#![forbid(unsafe_code)]
#![cfg_attr(
    not(test),
    deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)
)]

//! S3-compatible, read-only source and streaming restore module.
//!
//! GET bodies are forwarded in wire-sized chunks. Restores use bounded (5 MiB)
//! multipart parts, never a staging file or a whole-object buffer.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::Arc;

use async_trait::async_trait;
use aws_config::BehaviorVersion;
use aws_credential_types::Credentials;
use aws_sdk_s3::error::DisplayErrorContext;
use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::types::{CompletedMultipartUpload, CompletedPart, ServerSideEncryption};
use aws_sdk_s3::Client;
use aws_types::region::Region;
use futures_util::stream::{self, StreamExt, TryStreamExt};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::sync::mpsc;

use rb_core::channel::{ChunkEvent, ChunkSink, ChunkSource};
use rb_core::error::{BackupError, Phase, Result};
use rb_core::module::{BackupModule, Destination, Source, TargetParams};
use rb_core::plan::{human_bytes, summarize, BackupPlan, PlanItem, Preflight};
use rb_core::verification::{RestoreEvidence, VerificationReport, VerificationSink};
use rb_core::wire::CHUNK_SIZE;

mod source;

use source::S3Source;

const PART_SIZE: usize = 5 * 1024 * 1024;
/// Bytes reserved for the part buffer before it grows. The part size itself is
/// derived from a peer-supplied item size, so it must never be handed to
/// `Vec::with_capacity` directly.
const INITIAL_PART_RESERVE: usize = 16 * CHUNK_SIZE;
const MAX_MULTIPART_PARTS: u64 = 10_000;
const MAX_S3_OBJECT_BYTES: u64 = 5 * 1024 * 1024 * 1024 * 1024;
/// The one server-side encryption mode a copy can reproduce: SSE-S3, whose
/// keys the provider owns. SSE-KMS names a key that exists in one account and
/// SSE-C needs the caller's key, so neither is portable.
const SSE_S3: &str = "AES256";

/// How many independent S3 requests the module keeps in flight at once: the
/// source's per-object probes, the destination's stale-key removal and its
/// read-back.
///
/// Every one of them is a network round-trip, and a bucket has as many objects
/// as it has. Issued one after another, a 1 000 000-object bucket cost roughly
/// five million *sequential* round-trips across the pre-run fingerprint,
/// analysis and the post-run fingerprint — days of wall clock spent almost
/// entirely waiting. The bound keeps that concurrency well inside any provider's
/// per-connection limits (and MinIO's).
pub(crate) const REQUEST_CONCURRENCY: usize = 16;

#[derive(Clone, Serialize, Deserialize)]
pub struct S3Params {
    #[serde(default)]
    pub endpoint: Option<String>,
    #[serde(default)]
    pub region: Option<String>,
    pub bucket: String,
    #[serde(default)]
    pub prefix: Option<String>,
    #[serde(default)]
    pub access_key: Option<String>,
    #[serde(default)]
    pub secret_key: Option<String>,
    /// Force path-style addressing. Unset means "path style when a custom
    /// endpoint is configured", which is what MinIO and most S3-compatible
    /// deployments need; an explicit value always wins so `path_style: false`
    /// against a custom endpoint is honoured instead of silently overridden.
    #[serde(default)]
    pub path_style: Option<bool>,
    /// Permit destination bucket creation when absent.
    #[serde(default)]
    pub create_bucket: bool,
    /// Permit replacement of an existing destination object.
    #[serde(default)]
    pub overwrite: bool,
}

impl fmt::Debug for S3Params {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("S3Params")
            .field("endpoint", &self.endpoint)
            .field("region", &self.region)
            .field("bucket", &self.bucket)
            .field("prefix", &self.prefix)
            .field("access_key", &self.access_key)
            .field(
                "secret_key",
                &self.secret_key.as_ref().map(|_| "[REDACTED]"),
            )
            .field("path_style", &self.path_style)
            .field("create_bucket", &self.create_bucket)
            .field("overwrite", &self.overwrite)
            .finish()
    }
}

/// Self-contained object catalog carried in [`BackupPlan::payload`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct S3Plan {
    #[serde(default)]
    pub source_endpoint: Option<String>,
    pub source_bucket: String,
    #[serde(default)]
    pub source_prefix: Option<String>,
    pub objects: Vec<S3Object>,
    /// Captured only when source credentials allow it.
    #[serde(default)]
    pub policy: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct S3Object {
    pub key: String,
    pub size: u64,
    #[serde(default)]
    pub etag: Option<String>,
    #[serde(default)]
    pub content_type: Option<String>,
    #[serde(default)]
    pub storage_class: Option<String>,
    /// Source SSE mode. It is deliberately rejected because destination SSE
    /// headers/keys are not portable across S3 providers.
    #[serde(default)]
    pub server_side_encryption: Option<String>,
    #[serde(default)]
    pub metadata: BTreeMap<String, String>,
    /// HTTP entity headers. They are part of the object, not decoration: an
    /// object stored gzip-compressed is unreadable without its
    /// `Content-Encoding`, and the payload digest cannot notice their loss
    /// because the bytes are identical either way.
    #[serde(default)]
    pub content_encoding: Option<String>,
    #[serde(default)]
    pub cache_control: Option<String>,
    #[serde(default)]
    pub content_disposition: Option<String>,
    #[serde(default)]
    pub content_language: Option<String>,
    #[serde(default)]
    pub expires: Option<String>,
    #[serde(default)]
    pub website_redirect_location: Option<String>,
}

impl S3Object {
    /// The entity headers that must survive a 1:1 restore, as (name, value)
    /// pairs, for diagnostics and comparison.
    fn entity_headers(&self) -> [(&'static str, Option<&str>); 6] {
        [
            ("content-type", self.content_type.as_deref()),
            ("content-encoding", self.content_encoding.as_deref()),
            ("cache-control", self.cache_control.as_deref()),
            ("content-disposition", self.content_disposition.as_deref()),
            ("content-language", self.content_language.as_deref()),
            (
                "website-redirect-location",
                self.website_redirect_location.as_deref(),
            ),
        ]
    }
}

pub struct Module;

#[async_trait]
impl BackupModule for Module {
    fn name(&self) -> &'static str {
        "s3"
    }

    fn max_carriers(&self) -> u32 {
        // Multipart upload processing follows strict plan order.
        1
    }
    fn version_support(&self) -> &'static str {
        "S3-compatible (AWS S3, MinIO)"
    }
    async fn open_source(&self, params: &TargetParams) -> Result<Box<dyn Source>> {
        let params: S3Params = params.deserialize()?;
        validate_params(&params)?;
        Ok(Box::new(S3Source::new(params).await))
    }
    async fn open_destination(&self, params: &TargetParams) -> Result<Box<dyn Destination>> {
        let params: S3Params = params.deserialize()?;
        validate_params(&params)?;
        Ok(Box::new(S3Destination { params }))
    }
}

pub fn module() -> Arc<dyn BackupModule> {
    Arc::new(Module)
}
struct S3Destination {
    params: S3Params,
}

fn validate_params(params: &S3Params) -> Result<()> {
    if params.bucket.trim().is_empty() {
        return Err(BackupError::Config("s3.bucket must not be empty".into()));
    }
    if params.access_key.is_some() != params.secret_key.is_some() {
        return Err(BackupError::Config(
            "s3.access_key and s3.secret_key must be supplied together".into(),
        ));
    }
    Ok(())
}

/// An AWS SDK failure as a phase-tagged error that says what went wrong.
///
/// The SDK's own `Display` names only the error's variant — `service error`,
/// `dispatch failure` — so `get S3 object "a/b": service error` was all an
/// operator ever saw of a missing key, a denied request or a refused
/// connection. [`DisplayErrorContext`] walks the source chain down to the
/// provider's code and message.
pub(crate) fn sdk_error<E: std::error::Error>(
    phase: Phase,
    context: impl fmt::Display,
    error: E,
) -> BackupError {
    BackupError::phase(phase, format!("{context}: {}", DisplayErrorContext(error)))
}

/// The HTTP status of a failed request, when a response came back at all.
pub(crate) fn http_status<E>(
    error: &aws_sdk_s3::error::SdkError<E, aws_sdk_s3::config::http::HttpResponse>,
) -> Option<u16> {
    error
        .raw_response()
        .map(|response| response.status().as_u16())
}

/// Create the destination bucket, supplying the location constraint every AWS
/// region except `us-east-1` requires. Without it AWS answers
/// `IllegalLocationConstraintException` and every restore into a missing bucket
/// outside `us-east-1` fails after the plan was already accepted.
async fn create_destination_bucket(client: &Client, params: &S3Params) -> Result<()> {
    let mut request = client.create_bucket().bucket(&params.bucket);
    if let Some(region) = client.config().region().map(|region| region.to_string()) {
        if region != "us-east-1" {
            request = request.create_bucket_configuration(
                aws_sdk_s3::types::CreateBucketConfiguration::builder()
                    .location_constraint(aws_sdk_s3::types::BucketLocationConstraint::from(
                        region.as_str(),
                    ))
                    .build(),
            );
        }
    }
    request.send().await.map_err(|e| {
        sdk_error(
            Phase::Apply,
            format!("create S3 bucket {:?}", params.bucket),
            e,
        )
    })?;
    Ok(())
}

/// Whether the destination bucket exists.
///
/// Only a 404 means "missing". Any other failure — a 403 from a credential
/// that may not `HeadBucket`, a refused connection — used to read as missing
/// too: preflight then promised `create_bucket` a bucket it could never
/// create, and the run failed at `CreateBucket` after the plan was accepted.
async fn bucket_exists(client: &Client, bucket: &str, phase: Phase) -> Result<bool> {
    match client.head_bucket().bucket(bucket).send().await {
        Ok(_) => Ok(true),
        Err(error)
            if error
                .as_service_error()
                .is_some_and(|service| service.is_not_found())
                || http_status(&error) == Some(404) =>
        {
            Ok(false)
        }
        Err(error) => Err(sdk_error(
            phase,
            format!("check destination S3 bucket {bucket:?}"),
            error,
        )),
    }
}

pub(crate) fn unsupported_object_features(object: &S3Object) -> Vec<String> {
    let mut unsupported = Vec::new();
    if object.size > MAX_S3_OBJECT_BYTES {
        unsupported.push("object exceeds S3's 5 TiB object limit".into());
    }
    if let Some(storage_class) = &object.storage_class {
        if storage_class != "STANDARD" {
            unsupported.push(format!("storage class {storage_class} is not preserved"));
        }
    }
    if let Some(mode) = &object.server_side_encryption {
        if mode != SSE_S3 {
            unsupported.push(format!(
                "server-side encryption {mode} is not preserved (only {SSE_S3}, SSE-S3, is)"
            ));
        }
    }
    if let Some(expires) = &object.expires {
        // S3 exposes `Expires` as an HTTP date on read but takes a timestamp on
        // write; rather than guess at a lossy re-encoding, refuse the object so
        // the header is never silently dropped.
        unsupported.push(format!("Expires header {expires:?} is not preserved"));
    }
    unsupported
}

pub(crate) fn acl_is_owner_only_full_control(
    owner_id: Option<&str>,
    grants: &[aws_sdk_s3::types::Grant],
) -> bool {
    let [grant] = grants else { return false };
    let Some(grantee) = grant.grantee() else {
        return false;
    };
    let owner_id = owner_id.filter(|id| !id.is_empty());
    let grantee_id = grantee.id().filter(|id| !id.is_empty());
    grant
        .permission()
        .is_some_and(|permission| permission.as_str() == "FULL_CONTROL")
        && grantee.r#type().as_str() == "CanonicalUser"
        && grantee_id == owner_id
        && grantee.email_address().is_none()
        && grantee.uri().is_none()
}

pub(crate) async fn client(params: &S3Params) -> Client {
    let region = Region::new(params.region.clone().unwrap_or_else(|| "us-east-1".into()));
    let mut loader = aws_config::defaults(BehaviorVersion::latest()).region(region);
    if let (Some(access), Some(secret)) = (&params.access_key, &params.secret_key) {
        loader = loader.credentials_provider(Credentials::new(
            access,
            secret,
            None,
            None,
            "rust-backup",
        ));
    }
    let shared = loader.load().await;
    let mut config = aws_sdk_s3::config::Builder::from(&shared);
    if let Some(endpoint) = &params.endpoint {
        config = config.endpoint_url(endpoint);
    }
    config = config.force_path_style(
        params
            .path_style
            .unwrap_or_else(|| params.endpoint.is_some()),
    );
    Client::from_conf(config.build())
}

async fn list_keys(
    client: &Client,
    bucket: &str,
    prefix: Option<&str>,
    phase: Phase,
) -> Result<BTreeSet<String>> {
    let mut token = None;
    let mut keys = BTreeSet::new();
    loop {
        let page = client
            .list_objects_v2()
            .bucket(bucket)
            .set_prefix(prefix.map(str::to_owned))
            .set_continuation_token(token)
            .send()
            .await
            .map_err(|error| sdk_error(phase, "list destination S3 scope", error))?;
        for object in page.contents() {
            if let Some(key) = object.key() {
                keys.insert(key.to_string());
            }
        }
        token = page.next_continuation_token().map(str::to_owned);
        if token.is_none() {
            break;
        }
    }
    Ok(keys)
}

pub(crate) fn now_rfc3339() -> String {
    let seconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    let days = seconds.div_euclid(86_400);
    let rem = seconds.rem_euclid(86_400);
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = mp + if mp < 10 { 3 } else { -9 };
    let y = y + if m <= 2 { 1 } else { 0 };
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

pub(crate) fn decode_plan(plan: &BackupPlan, phase: Phase) -> Result<S3Plan> {
    if plan.module != "s3" {
        return Err(BackupError::phase(phase, "plan is not an S3 plan"));
    }
    serde_json::from_value(plan.payload.clone())
        .map_err(|e| BackupError::phase(phase, format!("invalid S3 plan payload: {e}")))
}

fn target_key(payload: &S3Plan, target_prefix: Option<&str>, source_key: &str) -> Result<String> {
    let relative = match payload.source_prefix.as_deref() {
        Some(prefix) => source_key.strip_prefix(prefix).ok_or_else(|| {
            BackupError::phase(Phase::Validate, "S3 plan object lies outside source prefix")
        })?,
        None => source_key,
    };
    Ok(format!("{}{}", target_prefix.unwrap_or(""), relative))
}

/// Preserve bucket-policy semantics when source and destination bucket names
/// differ. S3 policies address the bucket by ARN, so copying the source JSON
/// verbatim would produce a policy that no longer governs the restored bucket.
fn translated_policy(
    policy: &str,
    source_bucket: &str,
    destination_bucket: &str,
    phase: Phase,
) -> Result<String> {
    let mut value: serde_json::Value = serde_json::from_str(policy).map_err(|error| {
        BackupError::phase(phase, format!("source S3 policy is invalid JSON: {error}"))
    })?;
    let source_arn = format!("arn:aws:s3:::{source_bucket}");
    let destination_arn = format!("arn:aws:s3:::{destination_bucket}");
    rewrite_bucket_arns(&mut value, &source_arn, &destination_arn);
    serde_json::to_string(&value)
        .map_err(|error| BackupError::phase_src(phase, "serialize translated S3 policy", error))
}

fn rewrite_bucket_arns(value: &mut serde_json::Value, source: &str, destination: &str) {
    match value {
        serde_json::Value::String(string) => {
            if string == source {
                *string = destination.to_string();
            } else if let Some(suffix) = string.strip_prefix(source) {
                if suffix.starts_with('/') {
                    *string = format!("{destination}{suffix}");
                }
            }
        }
        serde_json::Value::Array(values) => {
            for value in values {
                rewrite_bucket_arns(value, source, destination);
            }
        }
        serde_json::Value::Object(values) => {
            for value in values.values_mut() {
                rewrite_bucket_arns(value, source, destination);
            }
        }
        _ => {}
    }
}

fn canonicalize_policy(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Array(values) => {
            for value in values.iter_mut() {
                canonicalize_policy(value);
            }
            // IAM policy Statement/Action/Resource/condition-value arrays are
            // sets. Providers may return them in a different order after PUT.
            values.sort_by_key(|value| serde_json::to_string(value).unwrap_or_default());
        }
        serde_json::Value::Object(values) => {
            for value in values.values_mut() {
                canonicalize_policy(value);
            }
            // `serde_json/preserve_order` can be enabled transitively in the
            // full workspace.  Sort keys explicitly so fingerprint bytes do
            // not depend on provider or feature-set insertion order.
            values.sort_keys();
        }
        _ => {}
    }
}

pub(crate) fn canonical_policy_bytes(policy: Option<&str>, phase: Phase) -> Result<Vec<u8>> {
    let Some(policy) = policy else {
        return Ok(Vec::new());
    };
    let mut value: serde_json::Value = serde_json::from_str(policy).map_err(|error| {
        BackupError::phase(phase, format!("source S3 policy is invalid JSON: {error}"))
    })?;
    canonicalize_policy(&mut value);
    serde_json::to_vec(&value)
        .map_err(|error| BackupError::phase_src(phase, "serialize canonical S3 policy", error))
}

#[async_trait]
impl Destination for S3Destination {
    fn max_carriers(&self) -> usize {
        // One ordered multipart loop: `restore_object` consumes exactly one
        // item's events inline. Stated here because this is the value the
        // session actually negotiates against.
        1
    }

    async fn validate(&self, plan: &BackupPlan) -> Result<Preflight> {
        let payload = decode_plan(plan, Phase::Validate)?;
        let client = client(&self.params).await;
        let (exists, result) =
            match bucket_exists(&client, &self.params.bucket, Phase::Validate).await {
                Ok(true) => (
                    true,
                    Preflight::pass().check("bucket", true, "destination bucket is accessible"),
                ),
                Ok(false) if self.params.create_bucket => (
                    false,
                    Preflight::pass().check("bucket", true, "destination bucket will be created"),
                ),
                Ok(false) => (
                    false,
                    Preflight::pass().check(
                        "bucket",
                        false,
                        "destination bucket is missing; set create_bucket=true to permit creation",
                    ),
                ),
                Err(error) => (
                    false,
                    Preflight::pass().check("bucket", false, error.to_string()),
                ),
            };
        // The destination never trusts the source's own fidelity gate: the plan
        // arrives over the wire, so every bound the restore relies on is
        // re-checked here, before any byte is applied.
        let mut plan_faults: Vec<String> = Vec::new();
        for object in &payload.objects {
            for fault in unsupported_object_features(object) {
                plan_faults.push(format!("{}: {fault}", object.key));
            }
        }
        for item in &plan.items {
            match payload.objects.get(item.id as usize) {
                Some(object) if object.size == item.estimated_bytes => {}
                Some(object) => plan_faults.push(format!(
                    "{}: plan item {} declares {} bytes but the object catalog says {}",
                    object.key, item.id, item.estimated_bytes, object.size
                )),
                None => plan_faults.push(format!("plan item {} has no S3 object", item.id)),
            }
        }
        let mut result = result.check(
            "plan-fidelity",
            plan_faults.is_empty(),
            if plan_faults.is_empty() {
                "every plan item matches a restorable object catalog entry".to_string()
            } else {
                format!("unrestorable plan: {}", summarize(&plan_faults))
            },
        );

        // Two plan items that map to the same target key would silently
        // overwrite each other and still verify, because the expected key set
        // is a set.
        let mut target_keys = Vec::with_capacity(payload.objects.len());
        for object in &payload.objects {
            target_keys.push(target_key(
                &payload,
                self.params.prefix.as_deref(),
                &object.key,
            )?);
        }
        let mut duplicates: Vec<String> = Vec::new();
        {
            let mut seen = BTreeSet::new();
            for key in &target_keys {
                if !seen.insert(key.clone()) {
                    duplicates.push(key.clone());
                }
            }
        }
        // A multipart part is assembled whole in memory before it is uploaded,
        // and `part_size_for` scales it with the object so the part count stays
        // under S3's 10 000 limit — an object near the 5 TiB ceiling therefore
        // needs a ~524 MiB buffer on this host. Nothing on disk (I-NOTEMP
        // holds), but it is the destination's real peak and an operator must
        // see it before accepting the plan, not discover it as an OOM.
        let peak_part = peak_part_bytes(&payload.objects);
        result = result.check(
            "multipart-memory",
            true,
            format!(
                "largest object needs {} of destination memory for one multipart part",
                human_bytes(peak_part)
            ),
        );

        result = result.check(
            "target-keys",
            duplicates.is_empty(),
            if duplicates.is_empty() {
                "every source object maps to a distinct destination key".to_string()
            } else {
                format!(
                    "source objects collide on destination key(s): {}",
                    summarize(&duplicates)
                )
            },
        );

        if exists {
            let expected: BTreeSet<_> = target_keys.iter().cloned().collect();
            let actual = list_keys(
                &client,
                &self.params.bucket,
                self.params.prefix.as_deref(),
                Phase::Validate,
            )
            .await?;
            if !self.params.overwrite {
                // One aggregate check, not one per object: a 200 000-object
                // plan used to emit 200 000 log lines and bury the handful of
                // keys an operator actually has to act on.
                let collisions: Vec<String> = expected.intersection(&actual).cloned().collect();
                result = result.check(
                    "object-collisions",
                    collisions.is_empty(),
                    if collisions.is_empty() {
                        format!("none of the {} target keys exist yet", expected.len())
                    } else {
                        format!(
                            "{} target object(s) already exist; set overwrite=true to permit replacement: {}",
                            collisions.len(),
                            summarize(&collisions)
                        )
                    },
                );
            }
            let unexpected: Vec<_> = actual.difference(&expected).cloned().collect();
            result = result.check(
                "destination-scope",
                unexpected.is_empty() || self.params.overwrite,
                if unexpected.is_empty() {
                    "destination prefix contains no objects outside the backup plan".to_string()
                } else if self.params.overwrite {
                    format!(
                        "{} stale object(s) will be removed from the destination prefix (--overwrite)",
                        unexpected.len()
                    )
                } else {
                    format!(
                        "destination prefix contains {} stale object(s) not in the source: {}; use --overwrite to replace the scope",
                        unexpected.len(),
                        summarize(&unexpected)
                    )
                },
            );
        }
        Ok(result)
    }

    async fn stream_in(&self, plan: &BackupPlan, source: &mut dyn ChunkSource) -> Result<()> {
        let payload = decode_plan(plan, Phase::Apply)?;
        let client = client(&self.params).await;
        if !bucket_exists(&client, &self.params.bucket, Phase::Apply).await? {
            if !self.params.create_bucket {
                return Err(BackupError::phase(
                    Phase::Apply,
                    "destination S3 bucket does not exist",
                ));
            }
            create_destination_bucket(&client, &self.params).await?;
        }
        let expected_keys: BTreeSet<_> = payload
            .objects
            .iter()
            .map(|object| target_key(&payload, self.params.prefix.as_deref(), &object.key))
            .collect::<Result<_>>()?;
        if self.params.overwrite {
            let actual_keys = list_keys(
                &client,
                &self.params.bucket,
                self.params.prefix.as_deref(),
                Phase::Apply,
            )
            .await?;
            // Independent requests: one at a time, an --overwrite into a
            // prefix of 100 000 stale keys was 100 000 sequential round-trips
            // before the first payload byte.
            let removals: Vec<_> = actual_keys
                .difference(&expected_keys)
                .map(|stale| {
                    let request = client
                        .delete_object()
                        .bucket(&self.params.bucket)
                        .key(stale);
                    async move {
                        request.send().await.map(|_| ()).map_err(|error| {
                            sdk_error(
                                Phase::Apply,
                                format!("remove stale S3 object {stale:?}"),
                                error,
                            )
                        })
                    }
                })
                .collect();
            let removed = removals.len();
            stream::iter(removals)
                .buffer_unordered(REQUEST_CONCURRENCY)
                .try_collect::<Vec<()>>()
                .await?;
            if removed > 0 {
                tracing::info!(
                    removed,
                    bucket = %self.params.bucket,
                    "removed stale destination objects (--overwrite)"
                );
            }
        }
        for item in &plan.items {
            let object = payload
                .objects
                .get(item.id as usize)
                .ok_or_else(|| BackupError::phase(Phase::Apply, "plan item has no S3 object"))?;
            let key = target_key(&payload, self.params.prefix.as_deref(), &object.key)?;
            restore_object(&client, &self.params.bucket, &key, object, item, source).await?;
        }
        if let Some(policy) = &payload.policy {
            let policy = translated_policy(
                policy,
                &payload.source_bucket,
                &self.params.bucket,
                Phase::Apply,
            )?;
            client
                .put_bucket_policy()
                .bucket(&self.params.bucket)
                .policy(policy)
                .send()
                .await
                .map_err(|e| sdk_error(Phase::Apply, "restore bucket policy", e))?;
        }
        match source.next().await? {
            ChunkEvent::End => Ok(()),
            _ => Err(BackupError::phase(
                Phase::Apply,
                "received data beyond S3 plan",
            )),
        }
    }

    async fn verify(
        &self,
        plan: &BackupPlan,
        evidence: &RestoreEvidence,
    ) -> Result<VerificationReport> {
        verify_restored_objects(&self.params, plan, evidence).await
    }
}

async fn verify_restored_objects(
    params: &S3Params,
    plan: &BackupPlan,
    evidence: &RestoreEvidence,
) -> Result<VerificationReport> {
    let payload = decode_plan(plan, Phase::Verify)?;
    let client = client(params).await;
    let expected_keys: BTreeSet<_> = payload
        .objects
        .iter()
        .map(|object| target_key(&payload, params.prefix.as_deref(), &object.key))
        .collect::<Result<_>>()?;
    let actual_keys = list_keys(
        &client,
        &params.bucket,
        params.prefix.as_deref(),
        Phase::Verify,
    )
    .await?;
    if actual_keys != expected_keys {
        // The differences, not both whole sets: at 200 000 keys the old message
        // was megabytes long and hid the handful of keys that mattered.
        let missing: Vec<_> = expected_keys.difference(&actual_keys).cloned().collect();
        let extra: Vec<_> = actual_keys.difference(&expected_keys).cloned().collect();
        return Err(BackupError::phase(
            Phase::Verify,
            format!(
                "restored S3 prefix key set differs: {} missing ({}), {} unexpected ({})",
                missing.len(),
                summarize(&missing),
                extra.len(),
                summarize(&extra)
            ),
        ));
    }
    let mut reads = Vec::with_capacity(plan.items.len());
    for item in &plan.items {
        let object = payload
            .objects
            .get(item.id as usize)
            .ok_or_else(|| BackupError::phase(Phase::Verify, "plan item has no S3 object"))?;
        reads.push((
            item.id,
            target_key(&payload, params.prefix.as_deref(), &object.key)?,
        ));
    }

    // Read the objects back REQUEST_CONCURRENCY at a time; one verifier
    // consumes what they read. Strictly one object after another, read-back
    // was a round-trip per object for a bucket of small ones (two, with the
    // HEAD it used to issue first). The verifier tracks each item on its own,
    // so interleaved chunks are fine; the channel bound keeps the memory
    // in flight at REQUEST_CONCURRENCY chunks.
    let mut verifier = VerificationSink::new(evidence);
    let (events, mut received) = mpsc::channel::<ReadBack>(REQUEST_CONCURRENCY);
    //
    // The reads are made lazily: an unstarted read is an ~11 KiB future, so
    // building all of them up front cost gigabytes at the item ceiling.
    let (client, objects, bucket) = (&client, &payload.objects, params.bucket.as_str());
    let readers = async move {
        let read = stream::iter(reads)
            .map(move |(item_id, key)| {
                // Every id was resolved against the catalog above.
                let object = &objects[item_id as usize];
                read_back_object(client, bucket, key, object, item_id, events.clone())
            })
            .buffer_unordered(REQUEST_CONCURRENCY)
            .try_collect::<Vec<()>>()
            .await;
        // The stream — and the sender its closure owns — is gone by now, so
        // the consumer below sees the channel close once the last read ends.
        read.map(drop)
    };
    let consumer = async {
        while let Some(event) = received.recv().await {
            match event {
                ReadBack::Chunk {
                    item_id,
                    offset,
                    data,
                } => verifier.send_chunk(item_id, offset, &data).await?,
                ReadBack::End {
                    item_id,
                    total,
                    digest,
                } => verifier.finish_item(item_id, total, &digest).await?,
            }
        }
        Ok::<_, BackupError>(())
    };
    tokio::try_join!(readers, consumer)?;

    if let Some(expected_policy) = &payload.policy {
        let actual_policy = client
            .get_bucket_policy()
            .bucket(&params.bucket)
            .send()
            .await
            .map_err(|error| sdk_error(Phase::Verify, "read restored S3 policy", error))?
            .policy()
            .map(str::to_owned)
            .ok_or_else(|| BackupError::phase(Phase::Verify, "restored S3 policy is missing"))?;
        let expected_policy = translated_policy(
            expected_policy,
            &payload.source_bucket,
            &params.bucket,
            Phase::Verify,
        )?;
        let mut expected_json: serde_json::Value =
            serde_json::from_str(&expected_policy).map_err(|e| {
                BackupError::phase(
                    Phase::Verify,
                    format!("translated S3 policy is invalid JSON: {e}"),
                )
            })?;
        let mut actual_json: serde_json::Value =
            serde_json::from_str(&actual_policy).map_err(|e| {
                BackupError::phase(
                    Phase::Verify,
                    format!("destination S3 policy is invalid JSON: {e}"),
                )
            })?;
        canonicalize_policy(&mut expected_json);
        canonicalize_policy(&mut actual_json);
        if actual_json != expected_json {
            return Err(BackupError::phase(
                Phase::Verify,
                format!(
                    "restored S3 bucket policy differs from source: source={} destination={}",
                    compact_json(&expected_json),
                    compact_json(&actual_json)
                ),
            ));
        }
    }

    verifier.finish().await?;
    verifier.report("S3 object metadata, policy and GET read-back match")
}

/// What one read-back reader hands the verifier.
enum ReadBack {
    Chunk {
        item_id: u32,
        offset: u64,
        data: Vec<u8>,
    },
    End {
        item_id: u32,
        total: u64,
        digest: String,
    },
}

/// Read one restored object with a single `GetObject`, check the headers that
/// came with it against the plan, and stream its body to the verifier.
async fn read_back_object(
    client: &Client,
    bucket: &str,
    key: String,
    object: &S3Object,
    item_id: u32,
    events: mpsc::Sender<ReadBack>,
) -> Result<()> {
    let response = client
        .get_object()
        .bucket(bucket)
        .key(&key)
        .send()
        .await
        .map_err(|error| {
            sdk_error(
                Phase::Verify,
                format!("read restored S3 object {key:?}"),
                error,
            )
        })?;
    let size = response.content_length().unwrap_or(-1).max(0) as u64;
    let metadata: BTreeMap<_, _> = response
        .metadata()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .collect();
    let restored = S3Object {
        key: key.clone(),
        size,
        etag: None,
        content_type: response.content_type().map(str::to_owned),
        storage_class: None,
        server_side_encryption: response
            .server_side_encryption()
            .map(|mode| mode.as_str().to_owned()),
        metadata,
        content_encoding: response.content_encoding().map(str::to_owned),
        cache_control: response.cache_control().map(str::to_owned),
        content_disposition: response.content_disposition().map(str::to_owned),
        content_language: response.content_language().map(str::to_owned),
        expires: response.expires_string().map(str::to_owned),
        website_redirect_location: response.website_redirect_location().map(str::to_owned),
    };
    let faults = restored_object_faults(object, &restored);
    if !faults.is_empty() {
        return Err(BackupError::phase(
            Phase::Verify,
            format!(
                "restored S3 metadata mismatch for {key:?}: {}",
                faults.join("; ")
            ),
        ));
    }

    let mut body = response.body.into_async_read();
    let mut hasher = blake3::Hasher::new();
    let mut offset = 0_u64;
    let mut buffer = vec![0_u8; CHUNK_SIZE];
    loop {
        let read = read_full(&mut body, &mut buffer).await.map_err(|error| {
            BackupError::phase(
                Phase::Verify,
                format!("stream restored S3 object {key:?}: {error}"),
            )
        })?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        let data = buffer[..read].to_vec();
        events
            .send(ReadBack::Chunk {
                item_id,
                offset,
                data,
            })
            .await
            .map_err(|_| verifier_gone())?;
        offset += read as u64;
    }
    events
        .send(ReadBack::End {
            item_id,
            total: offset,
            digest: hasher.finalize().to_hex().to_string(),
        })
        .await
        .map_err(|_| verifier_gone())
}

/// The verifier stopped consuming, which it only does after failing; that
/// failure is the one `try_join!` reports.
fn verifier_gone() -> BackupError {
    BackupError::phase(Phase::Verify, "S3 read-back verifier stopped")
}

/// Every way a restored object's stored description differs from the plan's.
///
/// Server-side encryption is compared one way: an object that was SSE-S3 at
/// the source must be SSE-S3 at the destination, never silently decrypted. An
/// object without SSE may come back encrypted, because a destination bucket
/// with default encryption (every AWS bucket since January 2023) applies it to
/// whatever is stored there.
fn restored_object_faults(source: &S3Object, restored: &S3Object) -> Vec<String> {
    let mut faults = Vec::new();
    if restored.size != source.size {
        faults.push(format!("bytes={}/{}", restored.size, source.size));
    }
    if restored.metadata != source.metadata {
        faults.push("user metadata differs".to_string());
    }
    faults.extend(
        source
            .entity_headers()
            .into_iter()
            .zip(restored.entity_headers())
            .filter(|((_, source), (_, restored))| source != restored)
            .map(|((name, source), (_, restored))| {
                format!("{name}: source={source:?} destination={restored:?}")
            }),
    );
    if source.server_side_encryption.is_some()
        && restored.server_side_encryption != source.server_side_encryption
    {
        faults.push(format!(
            "server-side encryption: source={:?} destination={:?}",
            source.server_side_encryption, restored.server_side_encryption
        ));
    }
    faults
}

/// Fill `buffer` from `reader` until it is full or the reader is exhausted, and
/// return how many bytes it holds.
///
/// One `read` returns whatever the HTTP body has buffered — typically a few
/// KiB — so forwarding each read as-is cut a 1 MiB object into hundreds of
/// wire chunks, each with its own frame header and per-chunk digest.
pub(crate) async fn read_full<R: AsyncRead + Unpin>(
    reader: &mut R,
    buffer: &mut [u8],
) -> std::io::Result<usize> {
    let mut filled = 0;
    while filled < buffer.len() {
        match reader.read(&mut buffer[filled..]).await? {
            0 => break,
            read => filled += read,
        }
    }
    Ok(filled)
}

fn compact_json(value: &serde_json::Value) -> String {
    const MAX: usize = 500;
    let rendered = value.to_string();
    if rendered.chars().count() <= MAX {
        rendered
    } else {
        format!("{}…", rendered.chars().take(MAX).collect::<String>())
    }
}

/// Part size for an object: S3's 5 MiB minimum, raised just enough to keep the
/// part count within S3's 10 000-part limit.
fn part_size_for(total_bytes: u64) -> usize {
    let needed = total_bytes.div_ceil(MAX_MULTIPART_PARTS);
    (PART_SIZE as u64)
        .max(needed)
        .try_into()
        .unwrap_or(usize::MAX)
}

/// The largest single multipart part this plan will make the destination hold
/// in memory: one part of its biggest object, since `restore_object` assembles
/// a part whole before uploading it and never keeps two.
fn peak_part_bytes(objects: &[S3Object]) -> u64 {
    objects
        .iter()
        .map(|object| object.size)
        .max()
        .map(|largest| part_size_for(largest) as u64)
        .unwrap_or(0)
}

/// Refuse an object whose catalog size and plan-item size disagree.
///
/// Defence in depth: `validate` already refuses such a plan, but `restore_object`
/// derives the multipart part size and its closing byte-count assertion from
/// `item.estimated_bytes` while every piece of object metadata comes from
/// `object`. Re-checking at the point of use means no future caller can reach the
/// upload with the two out of step and store an object under a size the catalog
/// never declared.
fn ensure_declared_size_matches(key: &str, object: &S3Object, item: &PlanItem) -> Result<()> {
    if object.size != item.estimated_bytes {
        return Err(BackupError::phase(
            Phase::Apply,
            format!(
                "{key:?}: plan item {} declares {} bytes but the object catalog says {}",
                item.id, item.estimated_bytes, object.size
            ),
        ));
    }
    Ok(())
}

async fn restore_object(
    client: &Client,
    bucket: &str,
    key: &str,
    object: &S3Object,
    item: &PlanItem,
    source: &mut dyn ChunkSource,
) -> Result<()> {
    ensure_declared_size_matches(key, object, item)?;
    if item.estimated_bytes <= PART_SIZE as u64 {
        // An object that fits one part is one `PutObject`, issued only once the
        // whole item has arrived and its digest matched. A multipart upload of
        // it took three round-trips instead of one — the dominant cost of a
        // bucket of small objects — and stored it under a `<md5>-1` ETag where
        // the source had a plain MD5.
        let body = receive_whole_object(item, source).await?;
        return put_whole_object(client, bucket, key, object, body).await;
    }
    let upload = client
        .create_multipart_upload()
        .bucket(bucket)
        .key(key)
        .set_content_type(object.content_type.clone())
        .set_content_encoding(object.content_encoding.clone())
        .set_cache_control(object.cache_control.clone())
        .set_content_disposition(object.content_disposition.clone())
        .set_content_language(object.content_language.clone())
        .set_website_redirect_location(object.website_redirect_location.clone())
        .set_server_side_encryption(server_side_encryption(object))
        .set_metadata(Some(object.metadata.clone().into_iter().collect()))
        .send()
        .await
        .map_err(|e| sdk_error(Phase::Apply, format!("start multipart upload {key:?}"), e))?;
    let upload_id = upload
        .upload_id()
        .ok_or_else(|| BackupError::phase(Phase::Apply, "S3 did not return multipart upload id"))?
        .to_owned();
    let mut bytes = 0_u64;
    let mut hasher = blake3::Hasher::new();
    let part_size = part_size_for(item.estimated_bytes);
    // Reserve a bounded amount up front and let the buffer grow: `part_size`
    // is derived from a peer-supplied `estimated_bytes`, and a single
    // `with_capacity(part_size)` on a hostile value aborts the process.
    let part_reserve = part_size.min(INITIAL_PART_RESERVE);
    let mut part = Vec::with_capacity(part_reserve);
    let mut parts = Vec::new();
    let mut part_number = 1_i32;
    loop {
        // `next()` is the most failure-prone call here (transport read error,
        // idle timeout, digest mismatch, peer abort). Propagating it with `?`
        // left the multipart upload open, so a dropped relay mid-object leaked
        // every uploaded part as invisible, billed storage.
        let event = match source.next().await {
            Ok(event) => event,
            Err(error) => {
                let _: Result<()> =
                    abort_upload(client, bucket, key, &upload_id, &error.to_string()).await;
                return Err(error);
            }
        };
        match event {
            ChunkEvent::Chunk {
                item_id,
                offset,
                data,
            } => {
                if item_id != item.id || offset != bytes {
                    return abort_upload(
                        client,
                        bucket,
                        key,
                        &upload_id,
                        "non-contiguous S3 item stream",
                    )
                    .await;
                }
                bytes += data.len() as u64;
                hasher.update(&data);
                let mut rest = data.as_slice();
                while !rest.is_empty() {
                    if !fill_part(&mut part, part_size, &mut rest) {
                        continue;
                    }
                    let full = std::mem::replace(&mut part, Vec::with_capacity(part_reserve));
                    if let Err(error) = upload_part(
                        client,
                        bucket,
                        key,
                        &upload_id,
                        part_number,
                        full,
                        &mut parts,
                    )
                    .await
                    {
                        return abort_upload(client, bucket, key, &upload_id, &error.to_string())
                            .await;
                    }
                    if multipart_fault_after_part(part_number) {
                        return abort_upload(
                            client,
                            bucket,
                            key,
                            &upload_id,
                            "injected multipart failure after upload_part",
                        )
                        .await;
                    }
                    part_number += 1;
                }
            }
            ChunkEvent::ItemEnd {
                item_id,
                total,
                blake3,
            } => {
                if item_id != item.id
                    || total != bytes
                    || bytes != item.estimated_bytes
                    || blake3 != hasher.finalize().to_hex().as_str()
                {
                    return abort_upload(
                        client,
                        bucket,
                        key,
                        &upload_id,
                        "S3 item integrity mismatch",
                    )
                    .await;
                }
                break;
            }
            ChunkEvent::End => {
                return abort_upload(
                    client,
                    bucket,
                    key,
                    &upload_id,
                    "stream ended before S3 item completion",
                )
                .await
            }
        }
    }
    if !part.is_empty() {
        if let Err(error) = upload_part(
            client,
            bucket,
            key,
            &upload_id,
            part_number,
            part,
            &mut parts,
        )
        .await
        {
            return abort_upload(client, bucket, key, &upload_id, &error.to_string()).await;
        }
        if multipart_fault_after_part(part_number) {
            return abort_upload(
                client,
                bucket,
                key,
                &upload_id,
                "injected multipart failure after upload_part",
            )
            .await;
        }
    }
    let completed = CompletedMultipartUpload::builder()
        .set_parts(Some(parts))
        .build();
    let completed = client
        .complete_multipart_upload()
        .bucket(bucket)
        .key(key)
        .upload_id(&upload_id)
        .multipart_upload(completed)
        .send()
        .await;
    if let Err(error) = completed {
        return abort_upload(
            client,
            bucket,
            key,
            &upload_id,
            &format!(
                "complete multipart upload {key:?}: {}",
                DisplayErrorContext(error)
            ),
        )
        .await;
    }
    Ok(())
}

/// Move bytes from `rest` into `part` until the part holds exactly
/// `part_size` bytes or `rest` is empty, and say whether the part is full.
///
/// Parts are cut at exactly `part_size`: a chunk that straddles the boundary
/// used to overshoot the part's capacity and make the buffer reallocate (and
/// copy) to twice its size.
fn fill_part(part: &mut Vec<u8>, part_size: usize, rest: &mut &[u8]) -> bool {
    let take = part_size.saturating_sub(part.len()).min(rest.len());
    part.extend_from_slice(&rest[..take]);
    *rest = &rest[take..];
    part.len() >= part_size
}

/// The SSE mode to request for a restored object: the source's own, which the
/// fidelity gate has already narrowed to SSE-S3 or none.
fn server_side_encryption(object: &S3Object) -> Option<ServerSideEncryption> {
    object
        .server_side_encryption
        .as_deref()
        .map(ServerSideEncryption::from)
}

/// Collect one item that fits a single part, checking it exactly as the
/// multipart path does: contiguous chunks of this item only, never more bytes
/// than declared (the buffer is sized from the declaration, so a peer cannot
/// grow it), and a total and digest that match before anything is stored.
async fn receive_whole_object(item: &PlanItem, source: &mut dyn ChunkSource) -> Result<Vec<u8>> {
    let declared = item.estimated_bytes;
    let mut body = Vec::with_capacity(usize::try_from(declared).unwrap_or(PART_SIZE));
    let mut hasher = blake3::Hasher::new();
    loop {
        match source.next().await? {
            ChunkEvent::Chunk {
                item_id,
                offset,
                data,
            } => {
                let end = offset.saturating_add(data.len() as u64);
                if item_id != item.id || offset != body.len() as u64 || end > declared {
                    return Err(BackupError::phase(
                        Phase::Apply,
                        format!(
                            "non-contiguous S3 item stream: item {item_id} bytes {offset}..{end} \
                             (expected item {} at {} of {declared})",
                            item.id,
                            body.len()
                        ),
                    ));
                }
                hasher.update(&data);
                body.extend_from_slice(&data);
            }
            ChunkEvent::ItemEnd {
                item_id,
                total,
                blake3,
            } => {
                if item_id != item.id
                    || total != declared
                    || body.len() as u64 != declared
                    || blake3 != hasher.finalize().to_hex().as_str()
                {
                    return Err(BackupError::phase(
                        Phase::Apply,
                        format!(
                            "S3 item integrity mismatch: item {} received {} of {declared} bytes",
                            item.id,
                            body.len()
                        ),
                    ));
                }
                return Ok(body);
            }
            ChunkEvent::End => {
                return Err(BackupError::phase(
                    Phase::Apply,
                    "stream ended before S3 item completion",
                ))
            }
        }
    }
}

async fn put_whole_object(
    client: &Client,
    bucket: &str,
    key: &str,
    object: &S3Object,
    body: Vec<u8>,
) -> Result<()> {
    client
        .put_object()
        .bucket(bucket)
        .key(key)
        .set_content_type(object.content_type.clone())
        .set_content_encoding(object.content_encoding.clone())
        .set_cache_control(object.cache_control.clone())
        .set_content_disposition(object.content_disposition.clone())
        .set_content_language(object.content_language.clone())
        .set_website_redirect_location(object.website_redirect_location.clone())
        .set_server_side_encryption(server_side_encryption(object))
        .set_metadata(Some(object.metadata.clone().into_iter().collect()))
        .content_length(body.len() as i64)
        .body(ByteStream::from(body))
        .send()
        .await
        .map_err(|e| sdk_error(Phase::Apply, format!("put S3 object {key:?}"), e))?;
    Ok(())
}

/// Test-only process fault hook.  It is opt-in and deliberately outside the
/// public configuration surface; MinIO e2e uses it to exercise cleanup after a
/// real uploaded part without killing the process before `abort_upload` runs.
fn multipart_fault_after_part(number: i32) -> bool {
    std::env::var("RUST_BACKUP_S3_TEST_FAIL_AFTER_PART")
        .ok()
        .and_then(|value| value.parse::<i32>().ok())
        == Some(number)
}

async fn upload_part(
    client: &Client,
    bucket: &str,
    key: &str,
    upload_id: &str,
    number: i32,
    data: Vec<u8>,
    parts: &mut Vec<CompletedPart>,
) -> Result<()> {
    let output = client
        .upload_part()
        .bucket(bucket)
        .key(key)
        .upload_id(upload_id)
        .part_number(number)
        .content_length(data.len() as i64)
        .body(ByteStream::from(data))
        .send()
        .await
        .map_err(|e| sdk_error(Phase::Apply, format!("upload multipart part {number}"), e))?;
    let etag = output
        .e_tag()
        .ok_or_else(|| BackupError::phase(Phase::Apply, "S3 did not return multipart part ETag"))?;
    parts.push(
        CompletedPart::builder()
            .part_number(number)
            .e_tag(etag)
            .build(),
    );
    Ok(())
}

async fn abort_upload<T>(
    client: &Client,
    bucket: &str,
    key: &str,
    upload_id: &str,
    detail: &str,
) -> Result<T> {
    if let Err(error) = client
        .abort_multipart_upload()
        .bucket(bucket)
        .key(key)
        .upload_id(upload_id)
        .send()
        .await
    {
        // The primary failure is what the operator must act on, but a failed
        // abort leaves billed parts behind and must not be silent.
        tracing::error!(
            error = %DisplayErrorContext(error),
            bucket,
            key,
            upload_id,
            "failed to abort the multipart upload; incomplete parts remain and must be cleaned up"
        );
    }
    Err(BackupError::phase(Phase::Apply, detail))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn object(key: &str, size: u64) -> S3Object {
        S3Object {
            key: key.into(),
            size,
            etag: None,
            content_type: None,
            storage_class: None,
            server_side_encryption: None,
            metadata: BTreeMap::new(),
            content_encoding: None,
            cache_control: None,
            content_disposition: None,
            content_language: None,
            expires: None,
            website_redirect_location: None,
        }
    }

    fn plan_item(id: u32, estimated_bytes: u64) -> PlanItem {
        PlanItem {
            id,
            name: format!("item-{id}"),
            kind: "object".into(),
            ordinal: id,
            estimated_bytes,
            meta: serde_json::Value::Null,
        }
    }

    /// `validate` is not the only gate: the restore re-checks the two sizes it
    /// mixes, so a plan that reaches `restore_object` without a preflight cannot
    /// upload an object under a size the catalog never declared.
    #[test]
    fn a_restore_refuses_an_item_whose_size_the_catalog_contradicts() {
        let object = object("a/b.bin", 1024);
        assert!(ensure_declared_size_matches("a/b.bin", &object, &plan_item(3, 1024)).is_ok());
        let error = ensure_declared_size_matches("a/b.bin", &object, &plan_item(3, 2048))
            .expect_err("a contradicted size must be refused")
            .to_string();
        assert!(
            error.contains("declares 2048 bytes") && error.contains("catalog says 1024"),
            "the error must name both sizes: {error}"
        );
    }

    /// The preflight line an operator reads before accepting a plan must name
    /// the destination's real peak: one part of the biggest object, because a
    /// part is assembled whole in memory and `part_size_for` scales it with the
    /// object to stay under S3's 10 000-part limit.
    #[test]
    fn the_peak_part_is_one_part_of_the_largest_object() {
        assert_eq!(peak_part_bytes(&[]), 0);
        assert_eq!(
            peak_part_bytes(&[object("small", 1), object("tiny", 2)]),
            PART_SIZE as u64,
            "small objects still cost one minimum part"
        );
        assert_eq!(
            peak_part_bytes(&[
                object("small", 1),
                object("huge", MAX_S3_OBJECT_BYTES),
                object("medium", 4096),
            ]),
            MAX_S3_OBJECT_BYTES.div_ceil(MAX_MULTIPART_PARTS),
            "the largest object sets the peak, wherever it sits in the catalog"
        );
    }

    /// A part size derived from a peer-supplied item size must never reach
    /// `Vec::with_capacity` unbounded, and must still respect S3's limits.
    #[test]
    fn part_size_respects_s3_limits_and_stays_allocatable() {
        assert_eq!(part_size_for(0), PART_SIZE);
        assert_eq!(part_size_for(1), PART_SIZE);
        // 100 GiB needs parts above the 5 MiB floor to fit 10 000 of them.
        let huge = 100 * 1024 * 1024 * 1024_u64;
        let size = part_size_for(huge);
        assert!(size > PART_SIZE);
        assert!(huge.div_ceil(size as u64) <= MAX_MULTIPART_PARTS);
        // A hostile size still yields a value we only ever `min` before use.
        let hostile = part_size_for(u64::MAX);
        assert!(hostile > 0);
        assert!(hostile.min(INITIAL_PART_RESERVE) <= INITIAL_PART_RESERVE);
    }

    /// Entity headers are part of the object: their loss must be detectable.
    #[test]
    fn entity_headers_are_compared_field_by_field() {
        let mut source = object("assets/app.js", 10);
        source.content_encoding = Some("gzip".into());
        source.cache_control = Some("public, max-age=31536000".into());
        let restored = object("assets/app.js", 10);
        let faults: Vec<_> = source
            .entity_headers()
            .into_iter()
            .zip(restored.entity_headers())
            .filter(|((_, a), (_, b))| a != b)
            .map(|((name, _), _)| name)
            .collect();
        assert_eq!(faults, vec!["content-encoding", "cache-control"]);
    }

    /// `Expires` cannot be reproduced faithfully, so it must fail loudly rather
    /// than be dropped in silence.
    #[test]
    fn unpreservable_headers_are_reported_as_unsupported() {
        let mut with_expires = object("k", 1);
        with_expires.expires = Some("Wed, 21 Oct 2026 07:28:00 GMT".into());
        let faults = unsupported_object_features(&with_expires);
        assert_eq!(faults.len(), 1, "{faults:?}");
        assert!(faults[0].contains("Expires"));
        assert!(unsupported_object_features(&object("k", 1)).is_empty());
    }

    /// An explicit `path_style` must win over the endpoint-derived default.
    #[test]
    fn explicit_path_style_is_honoured() {
        let with_endpoint = |path_style| S3Params {
            endpoint: Some("https://s3.example.com".into()),
            region: None,
            bucket: "b".into(),
            prefix: None,
            access_key: None,
            secret_key: None,
            path_style,
            create_bucket: false,
            overwrite: false,
        };
        let resolve = |params: &S3Params| {
            params
                .path_style
                .unwrap_or_else(|| params.endpoint.is_some())
        };
        assert!(resolve(&with_endpoint(None)));
        assert!(resolve(&with_endpoint(Some(true))));
        assert!(!resolve(&with_endpoint(Some(false))));
    }

    #[test]
    fn bucket_policy_arns_follow_the_destination_bucket() {
        let source = r#"{
          "Statement": [{
            "Resource": ["arn:aws:s3:::source", "arn:aws:s3:::source/in/*"],
            "Unrelated": "arn:aws:s3:::source-extra/in/*"
          }]
        }"#;
        let translated = translated_policy(source, "source", "destination", Phase::Apply).unwrap();
        let json: serde_json::Value = serde_json::from_str(&translated).unwrap();
        assert_eq!(
            json["Statement"][0]["Resource"],
            serde_json::json!(["arn:aws:s3:::destination", "arn:aws:s3:::destination/in/*"])
        );
        assert_eq!(
            json["Statement"][0]["Unrelated"],
            "arn:aws:s3:::source-extra/in/*"
        );
    }

    #[test]
    fn policy_array_order_is_semantically_irrelevant() {
        let mut first = serde_json::json!({
            "Statement": [{"Action": ["s3:GetObject", "s3:ListBucket"]}, {"Sid": "b"}]
        });
        let mut second = serde_json::json!({
            "Statement": [{"Sid": "b"}, {"Action": ["s3:ListBucket", "s3:GetObject"]}]
        });
        canonicalize_policy(&mut first);
        canonicalize_policy(&mut second);
        assert_eq!(first, second);
    }

    #[test]
    fn source_fingerprint_canonicalizes_semantically_equal_policies() {
        let first = r#"{
            "Version": "2012-10-17",
            "Statement": [
                {"Sid": "b"},
                {"Action": ["s3:GetObject", "s3:ListBucket"]}
            ]
        }"#;
        let second = r#"{
            "Statement": [
                {"Action": ["s3:ListBucket", "s3:GetObject"]},
                {"Sid": "b"}
            ],
            "Version": "2012-10-17"
        }"#;
        assert_eq!(
            canonical_policy_bytes(Some(first), Phase::Analyze).unwrap(),
            canonical_policy_bytes(Some(second), Phase::Analyze).unwrap()
        );
    }
    #[test]
    fn destination_prefix_rewrites_only_source_prefix() {
        let plan = S3Plan {
            source_endpoint: None,
            source_bucket: "a".into(),
            source_prefix: Some("in/".into()),
            objects: vec![],
            policy: None,
        };
        assert_eq!(
            target_key(&plan, Some("out/"), "in/a.txt").unwrap(),
            "out/a.txt"
        );
        assert!(target_key(&plan, None, "outside").is_err());
    }
    #[tokio::test]
    async fn parameters_open_without_network() {
        let params = TargetParams::from_value(
            serde_json::json!({"bucket":"test","access_key":"a","secret_key":"b"}),
        );
        assert!(Module.open_source(&params).await.is_ok());
    }

    #[test]
    fn fidelity_rejects_unrestorable_object_features() {
        let standard = S3Object {
            key: "ok+%/unicode-è".into(),
            storage_class: Some("STANDARD".into()),
            ..object("ok+%/unicode-è", 0)
        };
        assert!(unsupported_object_features(&standard).is_empty());
        let glacier = S3Object {
            storage_class: Some("GLACIER".into()),
            ..standard.clone()
        };
        assert!(unsupported_object_features(&glacier)
            .join(" ")
            .contains("storage class"));
        let huge = S3Object {
            size: MAX_S3_OBJECT_BYTES + 1,
            ..standard
        };
        assert!(unsupported_object_features(&huge)
            .join(" ")
            .contains("5 TiB"));
    }

    #[test]
    fn fidelity_accepts_only_the_default_owner_acl() {
        fn grant(id: &str, permission: aws_sdk_s3::types::Permission) -> aws_sdk_s3::types::Grant {
            let grantee = aws_sdk_s3::types::Grantee::builder()
                .id(id)
                .r#type(aws_sdk_s3::types::Type::CanonicalUser)
                .build()
                .unwrap();
            aws_sdk_s3::types::Grant::builder()
                .grantee(grantee)
                .permission(permission)
                .build()
        }

        let owner = grant("owner", aws_sdk_s3::types::Permission::FullControl);
        assert!(acl_is_owner_only_full_control(
            Some("owner"),
            std::slice::from_ref(&owner)
        ));
        assert!(!acl_is_owner_only_full_control(Some("other"), &[owner]));
        let public = grant("owner", aws_sdk_s3::types::Permission::Read);
        assert!(!acl_is_owner_only_full_control(Some("owner"), &[public]));
        assert!(!acl_is_owner_only_full_control(Some("owner"), &[]));
        let minio_default = aws_sdk_s3::types::Grant::builder()
            .grantee(
                aws_sdk_s3::types::Grantee::builder()
                    .r#type(aws_sdk_s3::types::Type::CanonicalUser)
                    .build()
                    .unwrap(),
            )
            .permission(aws_sdk_s3::types::Permission::FullControl)
            .build();
        assert!(acl_is_owner_only_full_control(Some(""), &[minio_default]));
    }

    /// Replays a fixed list of events, then `End`.
    struct Scripted(std::collections::VecDeque<ChunkEvent>);

    #[async_trait]
    impl ChunkSource for Scripted {
        async fn next(&mut self) -> Result<ChunkEvent> {
            Ok(self.0.pop_front().unwrap_or(ChunkEvent::End))
        }
    }

    fn chunk(item_id: u32, offset: u64, data: &[u8]) -> ChunkEvent {
        ChunkEvent::Chunk {
            item_id,
            offset,
            data: data.to_vec(),
        }
    }

    fn item_end(item_id: u32, data: &[u8]) -> ChunkEvent {
        ChunkEvent::ItemEnd {
            item_id,
            total: data.len() as u64,
            blake3: blake3::hash(data).to_hex().to_string(),
        }
    }

    #[tokio::test]
    async fn a_whole_object_is_collected_only_when_its_digest_matches() {
        let mut source = Scripted(
            [
                chunk(3, 0, b"hello "),
                chunk(3, 6, b"world"),
                item_end(3, b"hello world"),
            ]
            .into(),
        );
        let body = receive_whole_object(&plan_item(3, 11), &mut source)
            .await
            .expect("a contiguous, matching item is accepted");
        assert_eq!(body, b"hello world");

        let mut empty = Scripted([item_end(4, b"")].into());
        assert!(receive_whole_object(&plan_item(4, 0), &mut empty)
            .await
            .expect("an empty object is one ItemEnd")
            .is_empty());

        let mut corrupted =
            Scripted([chunk(3, 0, b"hello world"), item_end(3, b"HELLO WORLD")].into());
        let error = receive_whole_object(&plan_item(3, 11), &mut corrupted)
            .await
            .expect_err("a digest mismatch must not reach PutObject");
        assert!(error.to_string().contains("integrity mismatch"), "{error}");
    }

    #[tokio::test]
    async fn a_whole_object_never_grows_past_its_declared_size() {
        let mut oversized = Scripted([chunk(1, 0, b"0123"), chunk(1, 4, b"4567")].into());
        let error = receive_whole_object(&plan_item(1, 5), &mut oversized)
            .await
            .expect_err("bytes beyond the declaration are refused as they arrive");
        assert!(error.to_string().contains("non-contiguous"), "{error}");

        for script in [
            vec![chunk(1, 1, b"x")],
            vec![chunk(2, 0, b"x")],
            vec![chunk(1, 0, b"x")],
        ] {
            let mut source = Scripted(script.into());
            assert!(
                receive_whole_object(&plan_item(1, 2), &mut source)
                    .await
                    .is_err(),
                "a gap, a foreign item or an early End is refused"
            );
        }
    }

    #[test]
    fn parts_are_cut_at_exactly_the_part_size() {
        let part_size = 10;
        let mut part = Vec::with_capacity(part_size);
        let mut parts = Vec::new();
        for chunk in [&[1_u8; 7][..], &[2; 7], &[3; 13], &[4; 1]] {
            let mut rest = chunk;
            while !rest.is_empty() {
                if fill_part(&mut part, part_size, &mut rest) {
                    assert_eq!(part.capacity(), part_size, "a part never reallocates");
                    parts.push(std::mem::replace(&mut part, Vec::with_capacity(part_size)));
                }
            }
        }
        assert_eq!(parts.len(), 2);
        assert!(parts.iter().all(|part| part.len() == part_size));
        assert_eq!(part.len(), 8, "the remainder stays for the last part");
        let joined: Vec<u8> = parts.concat().into_iter().chain(part).collect();
        let expected: Vec<u8> = [vec![1; 7], vec![2; 7], vec![3; 13], vec![4; 1]].concat();
        assert_eq!(joined, expected, "no byte is lost or reordered at a cut");
    }

    #[tokio::test]
    async fn read_full_fills_the_buffer_across_short_reads() {
        let data: Vec<u8> = (0..=255).collect();
        let mut reader = tokio_test_reader(&data, 7);
        let mut buffer = [0_u8; 100];
        assert_eq!(read_full(&mut reader, &mut buffer).await.unwrap(), 100);
        assert_eq!(&buffer[..], &data[..100]);
        assert_eq!(read_full(&mut reader, &mut buffer).await.unwrap(), 100);
        assert_eq!(read_full(&mut reader, &mut buffer).await.unwrap(), 56);
        assert_eq!(&buffer[..56], &data[200..]);
        assert_eq!(read_full(&mut reader, &mut buffer).await.unwrap(), 0);
    }

    /// A reader that returns at most `step` bytes per read, as an HTTP body does.
    fn tokio_test_reader(data: &[u8], step: usize) -> impl AsyncRead + Unpin {
        struct Trickle {
            data: Vec<u8>,
            position: usize,
            step: usize,
        }
        impl AsyncRead for Trickle {
            fn poll_read(
                mut self: std::pin::Pin<&mut Self>,
                _: &mut std::task::Context<'_>,
                buf: &mut tokio::io::ReadBuf<'_>,
            ) -> std::task::Poll<std::io::Result<()>> {
                let n = self
                    .step
                    .min(self.data.len() - self.position)
                    .min(buf.remaining());
                let start = self.position;
                buf.put_slice(&self.data[start..start + n]);
                self.position += n;
                std::task::Poll::Ready(Ok(()))
            }
        }
        Trickle {
            data: data.to_vec(),
            position: 0,
            step,
        }
    }

    #[test]
    fn sse_s3_is_restorable_and_other_modes_are_not() {
        let encrypted = S3Object {
            server_side_encryption: Some("AES256".into()),
            ..object("k", 1)
        };
        assert!(unsupported_object_features(&encrypted).is_empty());
        for mode in ["aws:kms", "aws:kms:dsse"] {
            let kms = S3Object {
                server_side_encryption: Some(mode.into()),
                ..object("k", 1)
            };
            let faults = unsupported_object_features(&kms).join(" ");
            assert!(
                faults.contains(mode) && faults.contains("not preserved"),
                "{faults}"
            );
        }
        assert_eq!(
            server_side_encryption(&encrypted),
            Some(ServerSideEncryption::Aes256)
        );
        assert_eq!(server_side_encryption(&object("k", 1)), None);
    }

    #[test]
    fn read_back_never_accepts_a_decrypted_copy() {
        let plain = object("k", 3);
        let encrypted = S3Object {
            server_side_encryption: Some("AES256".into()),
            ..plain.clone()
        };
        assert!(restored_object_faults(&encrypted, &encrypted).is_empty());
        let faults = restored_object_faults(&encrypted, &plain).join(" ");
        assert!(faults.contains("server-side encryption"), "{faults}");
        // A destination bucket that encrypts by default may encrypt a copy of
        // a plain object: that is not a loss.
        assert!(restored_object_faults(&plain, &encrypted).is_empty());
    }

    #[test]
    fn read_back_names_every_differing_field() {
        let source = S3Object {
            content_type: Some("text/plain".into()),
            metadata: BTreeMap::from([("origin".into(), "e2e".into())]),
            ..object("k", 3)
        };
        let restored = S3Object {
            size: 2,
            content_type: Some("binary/octet-stream".into()),
            ..object("k", 3)
        };
        let faults = restored_object_faults(&source, &restored).join("; ");
        assert!(faults.contains("bytes=2/3"), "{faults}");
        assert!(faults.contains("user metadata differs"), "{faults}");
        assert!(faults.contains("content-type"), "{faults}");
    }

    #[test]
    fn s3_params_debug_never_prints_the_secret_key() {
        let params: S3Params = serde_json::from_value(serde_json::json!({
            "bucket": "b",
            "access_key": "AKIAEXAMPLE",
            "secret_key": "wJalrXUtnFEMI-very-secret",
        }))
        .unwrap();
        let rendered = format!("{params:?}");
        assert!(!rendered.contains("very-secret"), "{rendered}");
        assert!(rendered.contains("[REDACTED]"), "{rendered}");
    }

    #[test]
    fn a_policy_read_fails_unless_the_policy_is_absent_or_unreadable() {
        use source::{policy_error_kind, PolicyError};
        assert_eq!(
            policy_error_kind(Some("NoSuchBucketPolicy"), Some(404)),
            PolicyError::Absent
        );
        assert!(matches!(
            policy_error_kind(Some("AccessDenied"), Some(403)),
            PolicyError::Unreadable(_)
        ));
        assert!(matches!(
            policy_error_kind(None, Some(501)),
            PolicyError::Unreadable(_)
        ));
        // Transient and unknown failures must never read as "no policy": that
        // dropped the policy from the plan, or faked a source mutation.
        for (code, status) in [
            (Some("SlowDown"), Some(503)),
            (Some("InternalError"), Some(500)),
            (Some("NoSuchBucket"), Some(404)),
            (None, None),
        ] {
            assert_eq!(policy_error_kind(code, status), PolicyError::Failed);
        }
    }

    #[test]
    fn plan_items_carry_no_second_copy_of_the_catalog() {
        let params: S3Params = serde_json::from_value(serde_json::json!({"bucket": "b"})).unwrap();
        let plan = source::build_plan(
            &params,
            vec![object("a", 1), object("b", 2)],
            None,
            "2026-10-21T00:00:00Z".into(),
        )
        .unwrap();
        assert_eq!(plan.estimated_bytes, 3);
        for (index, item) in plan.items.iter().enumerate() {
            assert_eq!(item.id as usize, index, "an item id indexes the catalog");
            assert!(item.meta.is_null());
            assert!(item.expects_data());
        }
        let payload = decode_plan(&plan, Phase::Validate).unwrap();
        assert_eq!(payload.objects[1].key, "b");
    }

    /// The S3 plan is the fattest plan shape; at the item ceiling, with keys
    /// and headers as a real bucket has them, it must still fit the plan frame.
    #[test]
    fn an_s3_plan_at_the_item_ceiling_fits_the_plan_frame() {
        let params: S3Params = serde_json::from_value(serde_json::json!({
            "bucket": "warehouse-events",
            "prefix": "warehouse/events/"
        }))
        .unwrap();
        let objects: Vec<S3Object> = (0..rb_core::channel::MAX_PLAN_ITEMS)
            .map(|i| S3Object {
                etag: Some("\"9bb58f26192e4ba00f01e2e7b136bbd8\"".into()),
                content_type: Some("application/octet-stream".into()),
                storage_class: Some("STANDARD".into()),
                server_side_encryption: Some("AES256".into()),
                ..object(
                    &format!("warehouse/events/2026/09/22/part-{i:06}.parquet"),
                    1_234_567,
                )
            })
            .collect();
        let plan =
            source::build_plan(&params, objects, None, "2026-10-21T00:00:00Z".into()).unwrap();
        rb_core::channel::validate_plan_bounds(&plan).unwrap();
        let encoded = serde_json::to_vec(&plan).unwrap().len();
        assert!(
            encoded <= rb_core::wire::PLAN_FRAME_LIMIT,
            "an S3 plan of {} objects encodes to {encoded} bytes, past PLAN_FRAME_LIMIT {}",
            rb_core::channel::MAX_PLAN_ITEMS,
            rb_core::wire::PLAN_FRAME_LIMIT
        );
    }
}
