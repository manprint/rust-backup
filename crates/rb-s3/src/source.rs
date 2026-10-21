//! The S3 source: listing, fidelity checks, object streaming and the
//! fingerprint (plan § 4.4).
//!
//! Split out of `lib.rs` so that the whole read side of the module is one file
//! and the I-IMMUT lint (`scripts/source_readonly_lint.sh`) can assert that
//! nothing in it names a writing S3 operation. The type does the same job at
//! compile time: the only handle here is [`ReadOnlyS3`], which exposes the
//! seven read operations the source needs and no way to reach the client that
//! could `PutObject`.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use async_trait::async_trait;
use aws_sdk_s3::operation::get_bucket_location::builders::GetBucketLocationFluentBuilder;
use aws_sdk_s3::operation::get_bucket_policy::builders::GetBucketPolicyFluentBuilder;
use aws_sdk_s3::operation::get_bucket_versioning::builders::GetBucketVersioningFluentBuilder;
use aws_sdk_s3::operation::get_object::builders::GetObjectFluentBuilder;
use aws_sdk_s3::operation::get_object_acl::builders::GetObjectAclFluentBuilder;
use aws_sdk_s3::operation::get_object_tagging::builders::GetObjectTaggingFluentBuilder;
use aws_sdk_s3::operation::head_object::builders::HeadObjectFluentBuilder;
use aws_sdk_s3::operation::list_objects_v2::builders::ListObjectsV2FluentBuilder;
use aws_sdk_s3::Client;
use futures_util::stream::{self, StreamExt, TryStreamExt};

use rb_core::channel::ChunkSink;
use rb_core::error::{BackupError, Phase, Result};
use rb_core::module::Source;
use rb_core::plan::{BackupMode, BackupPlan, IntegritySpec, PlanItem, PLAN_FORMAT_VERSION};
use rb_core::wire::CHUNK_SIZE;

use crate::{
    acl_is_owner_only_full_control, canonical_policy_bytes, client, decode_plan, http_status,
    now_rfc3339, read_full, sdk_error, unsupported_object_features, S3Object, S3Params, S3Plan,
    REQUEST_CONCURRENCY,
};

/// The only handle the S3 source holds on the bucket it reads.
///
/// A newtype rather than a trait: the AWS SDK's fluent builders are concrete
/// types, so the wrapper hands each one back unchanged and simply has no method
/// for any writing operation — nothing that stores, removes, copies or
/// multipart-uploads an object, and nothing that changes a bucket. There is no
/// accessor for the inner client either, so the read side of this crate cannot
/// reach one (I-IMMUT, layer 1). The names of the operations it refuses are in
/// `scripts/source_readonly_lint.sh`, which greps this file for them.
pub(crate) struct ReadOnlyS3 {
    inner: Client,
}

impl ReadOnlyS3 {
    pub(crate) fn new(inner: Client) -> Self {
        Self { inner }
    }

    pub(crate) fn list_objects_v2(&self) -> ListObjectsV2FluentBuilder {
        self.inner.list_objects_v2()
    }

    pub(crate) fn head_object(&self) -> HeadObjectFluentBuilder {
        self.inner.head_object()
    }

    pub(crate) fn get_object(&self) -> GetObjectFluentBuilder {
        self.inner.get_object()
    }

    pub(crate) fn get_object_tagging(&self) -> GetObjectTaggingFluentBuilder {
        self.inner.get_object_tagging()
    }

    pub(crate) fn get_object_acl(&self) -> GetObjectAclFluentBuilder {
        self.inner.get_object_acl()
    }

    pub(crate) fn get_bucket_policy(&self) -> GetBucketPolicyFluentBuilder {
        self.inner.get_bucket_policy()
    }

    pub(crate) fn get_bucket_versioning(&self) -> GetBucketVersioningFluentBuilder {
        self.inner.get_bucket_versioning()
    }

    /// Part of the read-only surface rather than of a call site: a read the
    /// wrapper does not expose is a read somebody would otherwise take from a
    /// raw client.
    #[allow(dead_code)]
    pub(crate) fn get_bucket_location(&self) -> GetBucketLocationFluentBuilder {
        self.inner.get_bucket_location()
    }
}

/// The S3 source. Owns the read-only handle every phase of the run uses.
pub(crate) struct S3Source {
    params: S3Params,
    s3: ReadOnlyS3,
    /// The bucket state the last fingerprint read, handed to the analysis
    /// that follows it.
    ///
    /// The session fingerprints the source and analyzes it straight after, and
    /// both need the same `ListObjectsV2` pages plus one `HeadObject` per key:
    /// reading them twice doubled the dominant pre-transfer cost of a large
    /// bucket. The plan is then also built from exactly the state the
    /// immutability baseline committed to. Every fingerprint still reads the
    /// bucket afresh — the post-run audit must never see a cached answer.
    baseline: Mutex<Option<BucketState>>,
    /// Whether the "bucket policy not readable" warning was already printed:
    /// the policy is read up to three times per run.
    policy_warned: AtomicBool,
}

/// What one read of the bucket returns: its objects in key order and its
/// policy.
struct BucketState {
    objects: Vec<S3Object>,
    policy: Option<String>,
}

impl S3Source {
    pub(crate) async fn new(params: S3Params) -> Self {
        let s3 = ReadOnlyS3::new(client(&params).await);
        Self {
            params,
            s3,
            baseline: Mutex::new(None),
            policy_warned: AtomicBool::new(false),
        }
    }

    async fn read_bucket(&self, phase: Phase) -> Result<BucketState> {
        let objects = list_objects(&self.s3, &self.params, phase).await?;
        let policy = self.read_policy(phase).await?;
        Ok(BucketState { objects, policy })
    }

    /// The bucket policy, `None` when the bucket has none.
    ///
    /// Only "there is no policy" may become `None`. Every error used to: a
    /// throttled or timed-out `GetBucketPolicy` silently dropped the policy
    /// from the plan, so the restore certified a bucket without it — or the
    /// post-run fingerprint read `None` where the baseline had a policy and
    /// reported a source mutation that never happened. A credential that may
    /// not read the policy still yields `None`, as documented, but now says so.
    async fn read_policy(&self, phase: Phase) -> Result<Option<String>> {
        let error = match self
            .s3
            .get_bucket_policy()
            .bucket(&self.params.bucket)
            .send()
            .await
        {
            Ok(response) => return Ok(response.policy().map(str::to_owned)),
            Err(error) => error,
        };
        let code = error
            .as_service_error()
            .and_then(|service| service.meta().code());
        match policy_error_kind(code, http_status(&error)) {
            PolicyError::Absent => Ok(None),
            PolicyError::Unreadable(why) => {
                if !self.policy_warned.swap(true, Ordering::Relaxed) {
                    tracing::warn!(
                        bucket = %self.params.bucket,
                        "{why}: the bucket policy is not part of this backup and the \
                         destination keeps its own"
                    );
                }
                Ok(None)
            }
            PolicyError::Failed => Err(sdk_error(
                phase,
                format!("read S3 bucket policy of {:?}", self.params.bucket),
                error,
            )),
        }
    }
}

/// How a failed `GetBucketPolicy` is taken.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum PolicyError {
    /// The bucket has no policy: nothing to copy.
    Absent,
    /// The policy exists or may exist but cannot be read here; documented as
    /// "captured only when the source credential allows it".
    Unreadable(&'static str),
    /// Anything else — throttling, a timeout, a 5xx — fails the read.
    Failed,
}

pub(crate) fn policy_error_kind(code: Option<&str>, status: Option<u16>) -> PolicyError {
    match (code, status) {
        (Some("NoSuchBucketPolicy"), _) => PolicyError::Absent,
        (Some("AccessDenied"), _) | (_, Some(403)) => {
            PolicyError::Unreadable("the source credential may not read the bucket policy")
        }
        (Some("NotImplemented"), _) | (_, Some(501)) => {
            PolicyError::Unreadable("the S3 provider does not implement bucket policies")
        }
        _ => PolicyError::Failed,
    }
}

#[async_trait]
impl Source for S3Source {
    async fn analyze(&self) -> Result<BackupPlan> {
        let baseline = self
            .baseline
            .lock()
            .ok()
            .and_then(|mut cached| cached.take());
        let state = match baseline {
            Some(state) => state,
            None => self.read_bucket(Phase::Analyze).await?,
        };
        validate_source_fidelity(&self.s3, &self.params, &state.objects).await?;
        build_plan(&self.params, state.objects, state.policy, now_rfc3339())
    }

    async fn stream_out(&self, plan: &BackupPlan, sink: &mut dyn ChunkSink) -> Result<()> {
        let payload = decode_plan(plan, Phase::Transfer)?;
        // One buffer for the whole run, filled to a full wire chunk per read.
        let mut buffer = vec![0_u8; CHUNK_SIZE];
        for item in &plan.items {
            let object = payload
                .objects
                .get(item.id as usize)
                .ok_or_else(|| BackupError::phase(Phase::Transfer, "plan item has no S3 object"))?;
            // `If-Match` pins the read to the object the plan describes. An
            // object replaced since analysis used to stream its new bytes
            // under the old catalog entry, and only the post-run audit,
            // after the whole transfer, said the source had changed.
            let response = self
                .s3
                .get_object()
                .bucket(&self.params.bucket)
                .key(&object.key)
                .set_if_match(object.etag.clone())
                .send()
                .await
                .map_err(|error| {
                    if http_status(&error) == Some(412) {
                        BackupError::SourceMutated(format!(
                            "S3 object {:?} changed after the plan was made (ETag no longer {})",
                            object.key,
                            object.etag.as_deref().unwrap_or("?")
                        ))
                    } else {
                        sdk_error(
                            Phase::Transfer,
                            format!("get S3 object {:?}", object.key),
                            error,
                        )
                    }
                })?;
            let mut body = response.body.into_async_read();
            let mut hasher = blake3::Hasher::new();
            let mut offset = 0_u64;
            loop {
                let read = read_full(&mut body, &mut buffer).await.map_err(|e| {
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
        let state = self.read_bucket(Phase::Analyze).await?;
        let objects = serde_json::to_vec(&state.objects).map_err(|error| {
            BackupError::phase_src(Phase::Analyze, "serialize S3 fingerprint listing", error)
        })?;
        let mut hash = blake3::Hasher::new();
        hash.update(b"rust-backup/s3-fingerprint/v2\n");
        hash.update(&objects);
        hash.update(b"\npolicy:");
        hash.update(&canonical_policy_bytes(
            state.policy.as_deref(),
            Phase::Analyze,
        )?);
        if let Ok(mut cached) = self.baseline.lock() {
            *cached = Some(state);
        }
        Ok(hash.finalize().to_hex().to_string())
    }
}

/// The plan for one read of the bucket.
///
/// The object catalog travels once, in the payload: every consumer — preflight,
/// restore and read-back — looks an item's object up there by id. Plan items
/// used to carry a second full copy of it in `meta`, which nothing read and
/// which doubled the plan; an S3 plan near the item ceiling then outgrew the
/// plan frame and the run died in the handshake.
pub(crate) fn build_plan(
    params: &S3Params,
    objects: Vec<S3Object>,
    policy: Option<String>,
    created_at: String,
) -> Result<BackupPlan> {
    let items = objects
        .iter()
        .enumerate()
        .map(|(id, object)| PlanItem {
            id: id as u32,
            ordinal: id as u32,
            kind: "object".into(),
            name: object.key.clone(),
            estimated_bytes: object.size,
            meta: serde_json::Value::Null,
        })
        .collect();
    let estimated_bytes = objects.iter().map(|o| o.size).sum();
    let payload = S3Plan {
        source_endpoint: params.endpoint.clone(),
        source_bucket: params.bucket.clone(),
        source_prefix: params.prefix.clone(),
        objects,
        policy,
    };
    Ok(BackupPlan {
        format_version: PLAN_FORMAT_VERSION,
        module: "s3".into(),
        mode: BackupMode::Copy1to1,
        created_at,
        source_summary: format!("S3 bucket {}", params.bucket),
        estimated_bytes,
        integrity: IntegritySpec::default(),
        items,
        payload: serde_json::to_value(payload)
            .map_err(|e| BackupError::phase(Phase::Analyze, format!("serialize S3 plan: {e}")))?,
    })
}

async fn validate_source_fidelity(
    s3: &ReadOnlyS3,
    params: &S3Params,
    objects: &[S3Object],
) -> Result<()> {
    let versioning = s3
        .get_bucket_versioning()
        .bucket(&params.bucket)
        .send()
        .await
        .map_err(|e| sdk_error(Phase::Analyze, "read S3 bucket versioning", e))?;
    if versioning.status().is_some() {
        return Err(BackupError::phase(
            Phase::Analyze,
            "versioned S3 buckets are unsupported: versions/delete markers cannot be reproduced",
        ));
    }
    // `objects` arrives in key order and `buffered` preserves it, so the object
    // named in the refusal is the first offending key and not whichever probe
    // happened to answer first. The probes are made lazily, REQUEST_CONCURRENCY
    // alive at a time: an unstarted probe is a ~6 KiB future, and collecting
    // one per object first cost over a gigabyte at the 200 000-item ceiling.
    stream::iter(0..objects.len())
        .map(move |index| check_object_fidelity(s3, params, &objects[index]))
        .buffered(REQUEST_CONCURRENCY)
        .try_collect::<Vec<()>>()
        .await?;
    Ok(())
}

/// The per-object half of [`validate_source_fidelity`]: everything this build
/// refuses to copy rather than drop silently.
async fn check_object_fidelity(
    s3: &ReadOnlyS3,
    params: &S3Params,
    object: &S3Object,
) -> Result<()> {
    let unsupported = unsupported_object_features(object);
    if !unsupported.is_empty() {
        return Err(BackupError::phase(
            Phase::Analyze,
            format!("S3 object {:?}: {}", object.key, unsupported.join("; ")),
        ));
    }
    let tags = s3
        .get_object_tagging()
        .bucket(&params.bucket)
        .key(&object.key)
        .send()
        .await
        .map_err(|e| {
            sdk_error(
                Phase::Analyze,
                format!("read S3 object tags {:?}", object.key),
                e,
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
    let acl = s3
        .get_object_acl()
        .bucket(&params.bucket)
        .key(&object.key)
        .send()
        .await
        .map_err(|e| {
            sdk_error(
                Phase::Analyze,
                format!("read S3 object ACL {:?}", object.key),
                e,
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
    Ok(())
}

async fn list_objects(s3: &ReadOnlyS3, params: &S3Params, phase: Phase) -> Result<Vec<S3Object>> {
    let mut token = None;
    let mut objects = Vec::new();
    loop {
        let page = s3
            .list_objects_v2()
            .bucket(&params.bucket)
            .set_prefix(params.prefix.clone())
            .set_continuation_token(token)
            .send()
            .await
            .map_err(|e| sdk_error(phase, format!("list S3 bucket {:?}", params.bucket), e))?;
        let keys = page
            .contents()
            .iter()
            .map(|object| {
                object
                    .key()
                    .map(str::to_owned)
                    .ok_or_else(|| BackupError::phase(phase, "S3 object without key"))
            })
            .collect::<Result<Vec<_>>>()?;
        // The HEAD per key is not avoidable — `ListObjectsV2` carries neither
        // the user metadata nor the entity headers a 1:1 copy has to reproduce
        // — but serialising it made the listing one round-trip deep per object.
        // `buffered` keeps the page's key order, so the plan stays identical to
        // what the serial loop produced.
        let mut heads = stream::iter(keys)
            .map(|key| describe_object(s3, params, key, phase))
            .buffered(REQUEST_CONCURRENCY);
        while let Some(object) = heads.next().await {
            objects.push(object?);
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

/// One object's restorable description, from a single `HeadObject`.
async fn describe_object(
    s3: &ReadOnlyS3,
    params: &S3Params,
    key: String,
    phase: Phase,
) -> Result<S3Object> {
    let head = s3
        .head_object()
        .bucket(&params.bucket)
        .key(&key)
        .send()
        .await
        .map_err(|e| sdk_error(phase, format!("head S3 object {key:?}"), e))?;
    Ok(S3Object {
        key,
        size: head.content_length().unwrap_or(0).max(0) as u64,
        etag: head.e_tag().map(str::to_owned),
        content_type: head.content_type().map(str::to_owned),
        storage_class: head.storage_class().map(|v| v.as_str().to_owned()),
        server_side_encryption: head.server_side_encryption().map(|v| v.as_str().to_owned()),
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
    })
}
