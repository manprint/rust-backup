#![forbid(unsafe_code)]

//! S3-compatible, read-only source and streaming restore module.
//!
//! GET bodies are forwarded in wire-sized chunks. Restores use bounded (5 MiB)
//! multipart parts, never a staging file or a whole-object buffer.

use std::collections::BTreeMap;
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
use rb_core::wire::CHUNK_SIZE;

const PART_SIZE: usize = 5 * 1024 * 1024;

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
    #[serde(default)]
    pub path_style: bool,
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
    #[serde(default)]
    pub metadata: BTreeMap<String, String>,
}

pub struct Module;

#[async_trait]
impl BackupModule for Module {
    fn name(&self) -> &'static str {
        "s3"
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
    config = config.force_path_style(params.path_style || params.endpoint.is_some());
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
                metadata: head
                    .metadata()
                    .cloned()
                    .unwrap_or_default()
                    .into_iter()
                    .collect(),
            });
        }
        if !page.is_truncated().unwrap_or(false) {
            break;
        }
        token = page.next_continuation_token().map(str::to_owned);
    }
    objects.sort_by(|a, b| a.key.cmp(&b.key));
    Ok(objects)
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
        let mut hash = blake3::Hasher::new();
        for object in objects {
            hash.update(object.key.as_bytes());
            hash.update(&[0]);
            hash.update(object.etag.as_deref().unwrap_or("").as_bytes());
            hash.update(&[0]);
            hash.update(&object.size.to_le_bytes());
        }
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

#[async_trait]
impl Destination for S3Destination {
    async fn validate(&self, plan: &BackupPlan) -> Result<Preflight> {
        let payload = decode_plan(plan, Phase::Validate)?;
        let client = client(&self.params).await;
        let exists = client
            .head_bucket()
            .bucket(&self.params.bucket)
            .send()
            .await
            .is_ok();
        let mut result = Preflight::pass().check(
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
        if exists && !self.params.overwrite {
            for object in &payload.objects {
                let key = target_key(&payload, self.params.prefix.as_deref(), &object.key)?;
                let collision = client
                    .head_object()
                    .bucket(&self.params.bucket)
                    .key(&key)
                    .send()
                    .await
                    .is_ok();
                result = result.check(
                    format!("object:{key}"),
                    !collision,
                    if collision {
                        "target object exists; set overwrite=true to permit replacement"
                    } else {
                        "target key is unused"
                    },
                );
            }
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
            client
                .create_bucket()
                .bucket(&self.params.bucket)
                .send()
                .await
                .map_err(|e| BackupError::phase(Phase::Apply, format!("create S3 bucket: {e}")))?;
        }
        for item in &plan.items {
            let object = payload
                .objects
                .get(item.id as usize)
                .ok_or_else(|| BackupError::phase(Phase::Apply, "plan item has no S3 object"))?;
            let key = target_key(&payload, self.params.prefix.as_deref(), &object.key)?;
            restore_object(&client, &self.params.bucket, &key, object, item, source).await?;
        }
        if let Some(policy) = payload.policy {
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
    let mut part = Vec::with_capacity(PART_SIZE);
    let mut parts = Vec::new();
    let mut part_number = 1_i32;
    loop {
        match source.next().await? {
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
                if part.len() >= PART_SIZE {
                    upload_part(
                        client,
                        bucket,
                        key,
                        &upload_id,
                        part_number,
                        std::mem::take(&mut part),
                        &mut parts,
                    )
                    .await?;
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
        upload_part(
            client,
            bucket,
            key,
            &upload_id,
            part_number,
            part,
            &mut parts,
        )
        .await?;
    }
    let completed = CompletedMultipartUpload::builder()
        .set_parts(Some(parts))
        .build();
    client
        .complete_multipart_upload()
        .bucket(bucket)
        .key(key)
        .upload_id(upload_id)
        .multipart_upload(completed)
        .send()
        .await
        .map_err(|e| {
            BackupError::phase(
                Phase::Apply,
                format!("complete multipart upload {key:?}: {e}"),
            )
        })?;
    Ok(())
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
    let _ = client
        .abort_multipart_upload()
        .bucket(bucket)
        .key(key)
        .upload_id(upload_id)
        .send()
        .await;
    Err(BackupError::phase(Phase::Apply, detail))
}

#[cfg(test)]
mod tests {
    use super::*;
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
}
