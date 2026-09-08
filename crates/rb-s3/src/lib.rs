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
use std::sync::Arc;

use async_trait::async_trait;
use aws_config::BehaviorVersion;
use aws_credential_types::Credentials;
use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::types::{CompletedMultipartUpload, CompletedPart};
use aws_sdk_s3::Client;
use aws_types::region::Region;
use serde::{Deserialize, Serialize};
use tokio::io::AsyncReadExt;

use rb_core::channel::{ChunkEvent, ChunkSink, ChunkSource};
use rb_core::error::{BackupError, Phase, Result};
use rb_core::module::{BackupModule, Destination, Source, TargetParams};
use rb_core::plan::{
    BackupMode, BackupPlan, IntegritySpec, PlanItem, Preflight, PLAN_FORMAT_VERSION,
};
use rb_core::verification::{RestoreEvidence, VerificationReport, VerificationSink};
use rb_core::wire::CHUNK_SIZE;

const PART_SIZE: usize = 5 * 1024 * 1024;
/// Bytes reserved for the part buffer before it grows. The part size itself is
/// derived from a peer-supplied item size, so it must never be handed to
/// `Vec::with_capacity` directly.
const INITIAL_PART_RESERVE: usize = 16 * CHUNK_SIZE;
const MAX_MULTIPART_PARTS: u64 = 10_000;
const MAX_S3_OBJECT_BYTES: u64 = 5 * 1024 * 1024 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
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
        Ok(Box::new(S3Source { params }))
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
struct S3Source {
    params: S3Params,
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

/// Render at most a few entries of a fault list plus the total, so a preflight
/// line stays readable for a 100 000-object plan.
fn summarize(entries: &[String]) -> String {
    const SHOWN: usize = 5;
    if entries.len() <= SHOWN {
        return entries.join(", ");
    }
    format!(
        "{}, and {} more",
        entries[..SHOWN].join(", "),
        entries.len() - SHOWN
    )
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
    request
        .send()
        .await
        .map_err(|e| BackupError::phase(Phase::Apply, format!("create S3 bucket: {e}")))?;
    Ok(())
}

fn unsupported_object_features(object: &S3Object) -> Vec<String> {
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
        unsupported.push(format!("server-side encryption {mode} is not preserved"));
    }
    if let Some(expires) = &object.expires {
        // S3 exposes `Expires` as an HTTP date on read but takes a timestamp on
        // write; rather than guess at a lossy re-encoding, refuse the object so
        // the header is never silently dropped.
        unsupported.push(format!("Expires header {expires:?} is not preserved"));
    }
    unsupported
}

fn acl_is_owner_only_full_control(
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

async fn validate_source_fidelity(params: &S3Params, objects: &[S3Object]) -> Result<()> {
    let client = client(params).await;
    let versioning = client
        .get_bucket_versioning()
        .bucket(&params.bucket)
        .send()
        .await
        .map_err(|e| {
            BackupError::phase(Phase::Analyze, format!("read S3 bucket versioning: {e}"))
        })?;
    if versioning.status().is_some() {
        return Err(BackupError::phase(
            Phase::Analyze,
            "versioned S3 buckets are unsupported: versions/delete markers cannot be reproduced",
        ));
    }
    for object in objects {
        let unsupported = unsupported_object_features(object);
        if !unsupported.is_empty() {
            return Err(BackupError::phase(
                Phase::Analyze,
                format!("S3 object {:?}: {}", object.key, unsupported.join("; ")),
            ));
        }
        let tags = client
            .get_object_tagging()
            .bucket(&params.bucket)
            .key(&object.key)
            .send()
            .await
            .map_err(|e| {
                BackupError::phase(
                    Phase::Analyze,
                    format!("read S3 object tags {:?}: {e}", object.key),
                )
            })?;
        if !tags.tag_set().is_empty() {
            return Err(BackupError::phase(
                Phase::Analyze,
                format!(
                    "S3 object {:?} has tags; object tags are not preserved",
                    object.key
                ),
            ));
        }
        let acl = client
            .get_object_acl()
            .bucket(&params.bucket)
            .key(&object.key)
            .send()
            .await
            .map_err(|e| {
                BackupError::phase(
                    Phase::Analyze,
                    format!("read S3 object ACL {:?}: {e}", object.key),
                )
            })?;
        let owner_id = acl.owner().and_then(|owner| owner.id());
        if !acl_is_owner_only_full_control(owner_id, acl.grants()) {
            return Err(BackupError::phase(
                Phase::Analyze,
                format!(
                    "S3 object {:?} has a non-default ACL; object ACLs are not preserved",
                    object.key
                ),
            ));
        }
    }
    Ok(())
}

async fn client(params: &S3Params) -> Client {
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

async fn list_objects(params: &S3Params, phase: Phase) -> Result<Vec<S3Object>> {
    let client = client(params).await;
    let mut token = None;
    let mut objects = Vec::new();
    loop {
        let page = client
            .list_objects_v2()
            .bucket(&params.bucket)
            .set_prefix(params.prefix.clone())
            .set_continuation_token(token)
            .send()
            .await
            .map_err(|e| BackupError::phase(phase, format!("list S3 objects: {e}")))?;
        for object in page.contents() {
            let key = object
                .key()
                .ok_or_else(|| BackupError::phase(phase, "S3 object without key"))?;
            let head = client
                .head_object()
                .bucket(&params.bucket)
                .key(key)
                .send()
                .await
                .map_err(|e| BackupError::phase(phase, format!("head S3 object {key:?}: {e}")))?;
            objects.push(S3Object {
                key: key.to_owned(),
                size: head.content_length().unwrap_or(0).max(0) as u64,
                etag: head.e_tag().map(str::to_owned),
                content_type: head.content_type().map(str::to_owned),
                storage_class: head.storage_class().map(|v| v.as_str().to_owned()),
                server_side_encryption: head
                    .server_side_encryption()
                    .map(|v| v.as_str().to_owned()),
                metadata: head
                    .metadata()
                    .cloned()
                    .unwrap_or_default()
                    .into_iter()
                    .collect(),
                content_encoding: head.content_encoding().map(str::to_owned),
                cache_control: head.cache_control().map(str::to_owned),
                content_disposition: head.content_disposition().map(str::to_owned),
                content_language: head.content_language().map(str::to_owned),
                expires: head.expires_string().map(str::to_owned),
                website_redirect_location: head.website_redirect_location().map(str::to_owned),
            });
        }
        // Drive the loop off the continuation token alone. Treating a missing
        // `IsTruncated` as "complete" silently truncated the plan at 1 000 keys
        // — and because the plan, the destination scope and the read-back all
        // derive from this same listing, the run then certified itself as a
        // verified copy of a bucket it had only partly read. Trusting
        // `IsTruncated=true` with no token re-fetched page one forever.
        token = page.next_continuation_token().map(str::to_owned);
        if token.is_none() {
            break;
        }
    }
    objects.sort_by(|a, b| a.key.cmp(&b.key));
    Ok(objects)
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
            .map_err(|error| {
                BackupError::phase(phase, format!("list destination S3 scope: {error}"))
            })?;
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

fn now_rfc3339() -> String {
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

fn decode_plan(plan: &BackupPlan, phase: Phase) -> Result<S3Plan> {
    if plan.module != "s3" {
        return Err(BackupError::phase(phase, "plan is not an S3 plan"));
    }
    serde_json::from_value(plan.payload.clone())
        .map_err(|e| BackupError::phase(phase, format!("invalid S3 plan payload: {e}")))
}

#[async_trait]
impl Source for S3Source {
    async fn analyze(&self) -> Result<BackupPlan> {
        let objects = list_objects(&self.params, Phase::Analyze).await?;
        validate_source_fidelity(&self.params, &objects).await?;
        let policy = client(&self.params)
            .await
            .get_bucket_policy()
            .bucket(&self.params.bucket)
            .send()
            .await
            .ok()
            .and_then(|r| r.policy().map(str::to_owned));
        let payload = S3Plan {
            source_endpoint: self.params.endpoint.clone(),
            source_bucket: self.params.bucket.clone(),
            source_prefix: self.params.prefix.clone(),
            objects: objects.clone(),
            policy,
        };
        let items = objects
            .iter()
            .enumerate()
            .map(|(id, object)| PlanItem {
                id: id as u32,
                ordinal: id as u32,
                kind: "object".into(),
                name: object.key.clone(),
                estimated_bytes: object.size,
                meta: serde_json::to_value(object).unwrap_or(serde_json::Value::Null),
            })
            .collect();
        Ok(BackupPlan {
            format_version: PLAN_FORMAT_VERSION,
            module: "s3".into(),
            mode: BackupMode::Copy1to1,
            created_at: now_rfc3339(),
            source_summary: format!("S3 bucket {}", self.params.bucket),
            estimated_bytes: objects.iter().map(|o| o.size).sum(),
            integrity: IntegritySpec::default(),
            items,
            payload: serde_json::to_value(payload).map_err(|e| {
                BackupError::phase(Phase::Analyze, format!("serialize S3 plan: {e}"))
            })?,
        })
    }

    async fn stream_out(&self, plan: &BackupPlan, sink: &mut dyn ChunkSink) -> Result<()> {
        let payload = decode_plan(plan, Phase::Transfer)?;
        let client = client(&self.params).await;
        for item in &plan.items {
            let object = payload
                .objects
                .get(item.id as usize)
                .ok_or_else(|| BackupError::phase(Phase::Transfer, "plan item has no S3 object"))?;
            let response = client
                .get_object()
                .bucket(&self.params.bucket)
                .key(&object.key)
                .send()
                .await
                .map_err(|e| {
                    BackupError::phase(
                        Phase::Transfer,
                        format!("get S3 object {:?}: {e}", object.key),
                    )
                })?;
            let mut body = response.body.into_async_read();
            let mut hasher = blake3::Hasher::new();
            let mut offset = 0_u64;
            let mut buffer = vec![0_u8; CHUNK_SIZE];
            loop {
                let read = body.read(&mut buffer).await.map_err(|e| {
                    BackupError::phase(
                        Phase::Transfer,
                        format!("read S3 object {:?}: {e}", object.key),
                    )
                })?;
                if read == 0 {
                    break;
                }
                hasher.update(&buffer[..read]);
                sink.send_chunk(item.id, offset, &buffer[..read]).await?;
                offset += read as u64;
            }
            if offset != item.estimated_bytes {
                return Err(BackupError::phase(
                    Phase::Transfer,
                    format!("S3 object {:?} changed size during transfer", object.key),
                ));
            }
            let digest = hasher.finalize().to_hex();
            sink.finish_item(item.id, offset, digest.as_ref()).await?;
        }
        Ok(())
    }

    async fn fingerprint(&self) -> Result<String> {
        let objects = list_objects(&self.params, Phase::Analyze).await?;
        let policy = client(&self.params)
            .await
            .get_bucket_policy()
            .bucket(&self.params.bucket)
            .send()
            .await
            .ok()
            .and_then(|response| response.policy().map(str::to_owned));
        let mut hash = blake3::Hasher::new();
        hash.update(b"rust-backup/s3-fingerprint/v2\n");
        hash.update(&serde_json::to_vec(&objects).unwrap_or_default());
        hash.update(b"\npolicy:");
        hash.update(&canonical_policy_bytes(policy.as_deref(), Phase::Analyze)?);
        Ok(hash.finalize().to_hex().to_string())
    }
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

fn canonical_policy_bytes(policy: Option<&str>, phase: Phase) -> Result<Vec<u8>> {
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
        let exists = client
            .head_bucket()
            .bucket(&self.params.bucket)
            .send()
            .await
            .is_ok();
        let result = Preflight::pass().check(
            "bucket",
            exists || self.params.create_bucket,
            if exists {
                "destination bucket is accessible"
            } else if self.params.create_bucket {
                "destination bucket will be created"
            } else {
                "destination bucket is missing; set create_bucket=true to permit creation"
            },
        );
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
                // One aggregate check, not one per object: a 100 000-object
                // plan used to emit 100 000 log lines and bury the handful of
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
                        "destination prefix contains stale objects not in the source: {unexpected:?}; use --overwrite to replace the scope"
                    )
                },
            );
        }
        Ok(result)
    }

    async fn stream_in(&self, plan: &BackupPlan, source: &mut dyn ChunkSource) -> Result<()> {
        let payload = decode_plan(plan, Phase::Apply)?;
        let client = client(&self.params).await;
        if client
            .head_bucket()
            .bucket(&self.params.bucket)
            .send()
            .await
            .is_err()
        {
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
            for stale in actual_keys.difference(&expected_keys) {
                client
                    .delete_object()
                    .bucket(&self.params.bucket)
                    .key(stale)
                    .send()
                    .await
                    .map_err(|error| {
                        BackupError::phase(
                            Phase::Apply,
                            format!("remove stale S3 object {stale:?}: {error}"),
                        )
                    })?;
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
                .map_err(|e| {
                    BackupError::phase(Phase::Apply, format!("restore bucket policy: {e}"))
                })?;
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
        return Err(BackupError::phase(
            Phase::Verify,
            format!(
                "restored S3 prefix key set differs: source={expected_keys:?} destination={actual_keys:?}"
            ),
        ));
    }
    let mut verifier = VerificationSink::new(evidence);
    for item in &plan.items {
        let object = payload
            .objects
            .get(item.id as usize)
            .ok_or_else(|| BackupError::phase(Phase::Verify, "plan item has no S3 object"))?;
        let key = target_key(&payload, params.prefix.as_deref(), &object.key)?;
        let head = client
            .head_object()
            .bucket(&params.bucket)
            .key(&key)
            .send()
            .await
            .map_err(|error| {
                BackupError::phase(
                    Phase::Verify,
                    format!("head restored S3 object {key:?}: {error}"),
                )
            })?;
        let size = head.content_length().unwrap_or(-1).max(0) as u64;
        let metadata: BTreeMap<_, _> = head
            .metadata()
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .collect();
        let restored = S3Object {
            key: key.clone(),
            size,
            etag: None,
            content_type: head.content_type().map(str::to_owned),
            storage_class: None,
            server_side_encryption: None,
            metadata: metadata.clone(),
            content_encoding: head.content_encoding().map(str::to_owned),
            cache_control: head.cache_control().map(str::to_owned),
            content_disposition: head.content_disposition().map(str::to_owned),
            content_language: head.content_language().map(str::to_owned),
            expires: head.expires_string().map(str::to_owned),
            website_redirect_location: head.website_redirect_location().map(str::to_owned),
        };
        let header_faults: Vec<String> = object
            .entity_headers()
            .into_iter()
            .zip(restored.entity_headers())
            .filter(|((_, source), (_, destination))| source != destination)
            .map(|((name, source), (_, destination))| {
                format!("{name}: source={source:?} destination={destination:?}")
            })
            .collect();
        if size != object.size || metadata != object.metadata || !header_faults.is_empty() {
            return Err(BackupError::phase(
                Phase::Verify,
                format!(
                    "restored S3 metadata mismatch for {key:?}: bytes={size}/{} metadata_equal={} {}",
                    object.size,
                    metadata == object.metadata,
                    header_faults.join("; ")
                ),
            ));
        }

        let response = client
            .get_object()
            .bucket(&params.bucket)
            .key(&key)
            .send()
            .await
            .map_err(|error| {
                BackupError::phase(
                    Phase::Verify,
                    format!("read restored S3 object {key:?}: {error}"),
                )
            })?;
        let mut body = response.body.into_async_read();
        let mut hasher = blake3::Hasher::new();
        let mut offset = 0_u64;
        let mut buffer = vec![0_u8; CHUNK_SIZE];
        loop {
            let read = body.read(&mut buffer).await.map_err(|error| {
                BackupError::phase(
                    Phase::Verify,
                    format!("stream restored S3 object {key:?}: {error}"),
                )
            })?;
            if read == 0 {
                break;
            }
            hasher.update(&buffer[..read]);
            verifier
                .send_chunk(item.id, offset, &buffer[..read])
                .await?;
            offset += read as u64;
        }
        let digest = hasher.finalize().to_hex().to_string();
        verifier.finish_item(item.id, offset, &digest).await?;
    }

    if let Some(expected_policy) = &payload.policy {
        let actual_policy = client
            .get_bucket_policy()
            .bucket(&params.bucket)
            .send()
            .await
            .map_err(|error| {
                BackupError::phase(Phase::Verify, format!("read restored S3 policy: {error}"))
            })?
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

async fn restore_object(
    client: &Client,
    bucket: &str,
    key: &str,
    object: &S3Object,
    item: &PlanItem,
    source: &mut dyn ChunkSource,
) -> Result<()> {
    if item.estimated_bytes == 0 {
        match source.next().await? {
            ChunkEvent::ItemEnd {
                item_id,
                total,
                blake3,
            } if item_id == item.id
                && total == 0
                && blake3 == blake3::hash(b"").to_hex().to_string() => {}
            _ => {
                return Err(BackupError::phase(
                    Phase::Apply,
                    "invalid empty S3 object stream",
                ))
            }
        }
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
            .set_metadata(Some(object.metadata.clone().into_iter().collect()))
            .body(ByteStream::from(Vec::new()))
            .send()
            .await
            .map_err(|e| {
                BackupError::phase(Phase::Apply, format!("put empty S3 object {key:?}: {e}"))
            })?;
        return Ok(());
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
        .set_metadata(Some(object.metadata.clone().into_iter().collect()))
        .send()
        .await
        .map_err(|e| {
            BackupError::phase(Phase::Apply, format!("start multipart upload {key:?}: {e}"))
        })?;
    let upload_id = upload
        .upload_id()
        .ok_or_else(|| BackupError::phase(Phase::Apply, "S3 did not return multipart upload id"))?
        .to_owned();
    let mut bytes = 0_u64;
    let mut expected_offset = 0_u64;
    let mut hasher = blake3::Hasher::new();
    let part_size = part_size_for(item.estimated_bytes);
    // Reserve a bounded amount up front and let the buffer grow: `part_size`
    // is derived from a peer-supplied `estimated_bytes`, and a single
    // `with_capacity(part_size)` on a hostile value aborts the process.
    let mut part = Vec::with_capacity(part_size.min(INITIAL_PART_RESERVE));
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
                if item_id != item.id || offset != expected_offset {
                    return abort_upload(
                        client,
                        bucket,
                        key,
                        &upload_id,
                        "non-contiguous S3 item stream",
                    )
                    .await;
                }
                expected_offset += data.len() as u64;
                bytes += data.len() as u64;
                hasher.update(&data);
                part.extend_from_slice(&data);
                if part.len() >= part_size {
                    if let Err(error) = upload_part(
                        client,
                        bucket,
                        key,
                        &upload_id,
                        part_number,
                        std::mem::take(&mut part),
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
                    || blake3 != hasher.finalize().to_hex().to_string()
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
            &format!("complete multipart upload {key:?}: {error}"),
        )
        .await;
    }
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
        .map_err(|e| {
            BackupError::phase(Phase::Apply, format!("upload multipart part {number}: {e}"))
        })?;
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
            %error,
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

    /// Fault lists stay readable for a plan at the item cap.
    #[test]
    fn fault_summaries_are_bounded() {
        let many: Vec<String> = (0..100).map(|i| format!("k{i}")).collect();
        let rendered = summarize(&many);
        assert!(rendered.contains("and 95 more"), "{rendered}");
        assert_eq!(summarize(&many[..2]), "k0, k1");
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
}
