//! The S3 source: listing, fidelity checks, object streaming and the
//! fingerprint (plan § 4.4).
//!
//! Split out of `lib.rs` so that the whole read side of the module is one file
//! and the I-IMMUT lint (`scripts/source_readonly_lint.sh`) can assert that
//! nothing in it names a writing S3 operation. The type does the same job at
//! compile time: the only handle here is [`ReadOnlyS3`], which exposes the
//! seven read operations the source needs and no way to reach the client that
//! could `PutObject`.

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
use tokio::io::AsyncReadExt;

use rb_core::channel::ChunkSink;
use rb_core::error::{BackupError, Phase, Result};
use rb_core::module::Source;
use rb_core::plan::{BackupMode, BackupPlan, IntegritySpec, PlanItem, PLAN_FORMAT_VERSION};
use rb_core::wire::CHUNK_SIZE;

use crate::{
    acl_is_owner_only_full_control, canonical_policy_bytes, client, decode_plan, now_rfc3339,
    unsupported_object_features, S3Object, S3Params, S3Plan,
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
}

impl S3Source {
    pub(crate) async fn new(params: S3Params) -> Self {
        let s3 = ReadOnlyS3::new(client(&params).await);
        Self { params, s3 }
    }
}

#[async_trait]
impl Source for S3Source {
    async fn analyze(&self) -> Result<BackupPlan> {
        let objects = list_objects(&self.s3, &self.params, Phase::Analyze).await?;
        validate_source_fidelity(&self.s3, &self.params, &objects).await?;
        let policy = self
            .s3
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
        for item in &plan.items {
            let object = payload
                .objects
                .get(item.id as usize)
                .ok_or_else(|| BackupError::phase(Phase::Transfer, "plan item has no S3 object"))?;
            let response = self
                .s3
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
        let objects = list_objects(&self.s3, &self.params, Phase::Analyze).await?;
        let policy = self
            .s3
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
        let tags = s3
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
        let acl = s3
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
            .map_err(|e| BackupError::phase(phase, format!("list S3 objects: {e}")))?;
        for object in page.contents() {
            let key = object
                .key()
                .ok_or_else(|| BackupError::phase(phase, "S3 object without key"))?;
            let head = s3
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
