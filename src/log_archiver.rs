use anyhow::{bail, Context, Result};
use chrono::{Datelike, Timelike, Utc};
use flate2::{write::GzEncoder, Compression};
use redis::streams::{
    StreamAutoClaimOptions, StreamAutoClaimReply, StreamId, StreamReadOptions, StreamReadReply,
};
use redis::{AsyncCommands, Value};
use s3::{creds::Credentials, Bucket, Region};
use serde_json::Value as JsonValue;
use std::collections::HashMap;
use std::io::Write;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use uuid::Uuid;

const GROUP: &str = "immortal-s3-archiver-v1";
static RUNNING: AtomicBool = AtomicBool::new(false);

pub fn is_running() -> bool {
    RUNNING.load(Ordering::Acquire)
}

struct RunningGuard;

impl Drop for RunningGuard {
    fn drop(&mut self) {
        RUNNING.store(false, Ordering::Release);
    }
}

struct Config {
    redis_url: String,
    bucket: Box<Bucket>,
    prefix: String,
    shards: usize,
    batch_records: usize,
    batch_bytes: usize,
    claim_idle_ms: usize,
    request_timeout: Duration,
}

impl Config {
    fn from_env(redis_url: String) -> Result<Self> {
        let bucket_name = std::env::var("IMMORTAL_LOG_S3_BUCKET")
            .context("IMMORTAL_LOG_S3_BUCKET is required")?;
        let region: Region = std::env::var("IMMORTAL_LOG_S3_REGION")
            .unwrap_or_else(|_| "us-east-1".into())
            .parse()
            .context("IMMORTAL_LOG_S3_REGION is invalid")?;
        let credentials = Credentials::default().context("unable to load S3 credentials")?;
        let shards = positive_env("IMMORTAL_LOG_ARCHIVE_SHARDS", 32)?;
        Ok(Self {
            redis_url,
            bucket: Bucket::new(&bucket_name, region, credentials)
                .context("unable to create S3 bucket client")?,
            prefix: std::env::var("IMMORTAL_LOG_S3_PREFIX")
                .unwrap_or_else(|_| "raw/v1".into())
                .trim_matches('/')
                .into(),
            shards,
            batch_records: positive_env("IMMORTAL_LOG_ARCHIVE_BATCH_RECORDS", 256)?,
            batch_bytes: positive_env("IMMORTAL_LOG_ARCHIVE_BATCH_BYTES", 8 * 1024 * 1024)?,
            claim_idle_ms: positive_env("IMMORTAL_LOG_ARCHIVE_CLAIM_IDLE_MS", 60_000)?,
            request_timeout: Duration::from_millis(positive_env(
                "IMMORTAL_LOG_ARCHIVE_REQUEST_TIMEOUT_MS",
                30_000,
            )? as u64),
        })
    }
}

pub fn enabled_from_env() -> Result<bool> {
    parse_enabled(
        std::env::var("IMMORTAL_LOG_ARCHIVER_ENABLED")
            .ok()
            .as_deref(),
    )
}

fn parse_enabled(value: Option<&str>) -> Result<bool> {
    match value {
        Some(value) => match value.trim().to_ascii_lowercase().as_str() {
            "true" | "1" | "yes" | "on" => Ok(true),
            "false" | "0" | "no" | "off" => Ok(false),
            _ => bail!("IMMORTAL_LOG_ARCHIVER_ENABLED must be true or false"),
        },
        None => Ok(false),
    }
}

fn positive_env(name: &str, default: usize) -> Result<usize> {
    let value = std::env::var(name).ok().map_or(Ok(default), |value| {
        value.parse::<usize>().context("must be a positive integer")
    })?;
    if value == 0 {
        bail!("{name} must be greater than zero");
    }
    Ok(value)
}

fn archive_key(shards: usize, shard: usize) -> String {
    format!("immortal:logs:archive:v1-s{shards}:{{{shard:02}}}")
}

fn invalid_key() -> &'static str {
    "immortal:logs:archive:invalid"
}

fn payload(id: &StreamId) -> Option<String> {
    match id.map.get("payload")? {
        Value::BulkString(bytes) => String::from_utf8(bytes.clone()).ok(),
        Value::SimpleString(value) => Some(value.clone()),
        _ => None,
    }
}

fn object_key(
    prefix: &str,
    workflow_id: &str,
    first_id: &str,
    last_id: &str,
    bytes: &[u8],
    ingested_at: chrono::DateTime<Utc>,
) -> String {
    let workflow_hash = blake3::hash(workflow_id.as_bytes()).to_hex().to_string();
    let content_hash = blake3::hash(bytes).to_hex().to_string();
    format!(
        "{prefix}/tenant=default/workflow_bucket={}/workflow_hash={}/dt={:04}-{:02}-{:02}/hour={:02}/part-{}-{}-{}.jsonl.gz",
        &workflow_hash[..2], workflow_hash, ingested_at.year(), ingested_at.month(), ingested_at.day(), ingested_at.hour(), first_id, last_id, content_hash,
    )
}

async fn archive_workflow(
    config: &Config,
    connection: &mut redis::aio::MultiplexedConnection,
    stream: &str,
    entries: Vec<(String, String)>,
) -> Result<()> {
    let workflow_id = serde_json::from_str::<JsonValue>(&entries[0].1)
        .ok()
        .and_then(|envelope| envelope.get("workflow_id")?.as_str().map(str::to_owned))
        .context("archive envelope lacks workflow_id")?;
    let ingested_at = serde_json::from_str::<JsonValue>(&entries[0].1)
        .ok()
        .and_then(|envelope| envelope.get("ingested_at")?.as_str().map(str::to_owned))
        .and_then(|value| chrono::DateTime::parse_from_rfc3339(&value).ok())
        .map(|value| value.with_timezone(&Utc))
        .unwrap_or_else(Utc::now);
    let mut gzip = GzEncoder::new(Vec::new(), Compression::default());
    for (_, payload) in &entries {
        gzip.write_all(payload.as_bytes())?;
        gzip.write_all(b"\n")?;
    }
    let body = gzip.finish()?;
    let key = object_key(
        &config.prefix,
        &workflow_id,
        &entries[0].0,
        &entries.last().expect("nonempty").0,
        &body,
        ingested_at,
    );
    let response = tokio::time::timeout(
        config.request_timeout,
        config
            .bucket
            .put_object_with_content_type(&key, &body, "application/gzip"),
    )
    .await
    .context("S3 PutObject timed out")??;
    if !(200..300).contains(&response.status_code()) {
        bail!("S3 PutObject returned {}", response.status_code());
    }
    let ids: Vec<&str> = entries.iter().map(|(id, _)| id.as_str()).collect();
    let _: usize = connection.xack(stream, GROUP, &ids).await?;
    // Archive streams are a durable handoff, not a second long-term log store. Deleting only
    // after XACK ensures a pending entry remains reclaimable until its S3 segment is durable.
    let _: usize = redis::cmd("XDEL")
        .arg(stream)
        .arg(&ids)
        .query_async(connection)
        .await?;
    Ok(())
}

async fn dead_letter(
    connection: &mut redis::aio::MultiplexedConnection,
    stream: &str,
    entry: &StreamId,
    reason: &str,
) -> Result<()> {
    let raw = format!("{:?}", entry.map);
    connection
        .xadd_maxlen::<_, &str, &str, _, ()>(
            invalid_key(),
            redis::streams::StreamMaxlen::Approx(10_000),
            "*",
            &[
                ("source_stream", stream),
                ("source_id", &entry.id),
                ("reason", reason),
                ("entry", &raw),
            ],
        )
        .await?;
    let _: usize = connection.xack(stream, GROUP, &[entry.id.as_str()]).await?;
    let _: usize = redis::cmd("XDEL")
        .arg(stream)
        .arg(&entry.id)
        .query_async(connection)
        .await?;
    Ok(())
}

async fn archive_entries(
    config: &Config,
    connection: &mut redis::aio::MultiplexedConnection,
    stream: &str,
    entries: impl IntoIterator<Item = StreamId>,
) -> Result<()> {
    let mut workflows: HashMap<String, Vec<(String, String)>> = HashMap::new();
    for entry in entries {
        let Some(payload) = payload(&entry) else {
            dead_letter(connection, stream, &entry, "missing payload").await?;
            continue;
        };
        let workflow = serde_json::from_str::<JsonValue>(&payload)
            .ok()
            .and_then(|value| value.get("workflow_id")?.as_str().map(str::to_owned));
        if let Some(workflow) = workflow {
            workflows
                .entry(workflow)
                .or_default()
                .push((entry.id, payload));
        } else {
            dead_letter(connection, stream, &entry, "invalid archive envelope").await?;
        }
    }
    for (_, entries) in workflows {
        for chunk in partition_entries(entries, config.batch_records, config.batch_bytes) {
            archive_workflow(config, connection, stream, chunk).await?;
        }
    }
    Ok(())
}

fn partition_entries(
    entries: Vec<(String, String)>,
    max_records: usize,
    max_bytes: usize,
) -> Vec<Vec<(String, String)>> {
    let mut chunks = Vec::new();
    let mut chunk = Vec::new();
    let mut bytes = 0usize;
    for entry in entries {
        let entry_bytes = entry.0.len().saturating_add(entry.1.len());
        if !chunk.is_empty()
            && (chunk.len() >= max_records || bytes.saturating_add(entry_bytes) > max_bytes)
        {
            chunks.push(std::mem::take(&mut chunk));
            bytes = 0;
        }
        bytes = bytes.saturating_add(entry_bytes);
        chunk.push(entry);
    }
    if !chunk.is_empty() {
        chunks.push(chunk);
    }
    chunks
}

fn retry_delay(failures: u32, jitter_ms: u64) -> Duration {
    let exponent = failures.saturating_sub(1).min(6);
    let base_ms = 250u64.saturating_mul(1u64 << exponent).min(30_000);
    Duration::from_millis(base_ms.saturating_add(jitter_ms.min(250)))
}

async fn run_shard(
    config: &Config,
    connection: &mut redis::aio::MultiplexedConnection,
    shard: usize,
    consumer: &str,
) -> Result<()> {
    let stream = archive_key(config.shards, shard);
    let create: redis::RedisResult<()> = redis::cmd("XGROUP")
        .arg("CREATE")
        .arg(&stream)
        .arg(GROUP)
        .arg("0")
        .arg("MKSTREAM")
        .query_async(connection)
        .await;
    if let Err(error) = create {
        if !error.to_string().contains("BUSYGROUP") {
            return Err(error.into());
        }
    }
    let reclaimed: StreamAutoClaimReply = connection
        .xautoclaim_options(
            &stream,
            GROUP,
            consumer,
            config.claim_idle_ms,
            "0-0",
            StreamAutoClaimOptions::default().count(config.batch_records),
        )
        .await?;
    archive_entries(config, connection, &stream, reclaimed.claimed).await?;

    let options = StreamReadOptions::default()
        .group(GROUP, consumer)
        .count(config.batch_records)
        .block(1_000);
    let reply: StreamReadReply = connection
        .xread_options(&[&stream], &[">"], &options)
        .await?;
    let mut entries = Vec::new();
    for key in reply.keys {
        for entry in key.ids {
            entries.push(entry);
        }
    }
    archive_entries(config, connection, &stream, entries).await
}

async fn run_shard_forever(
    config: Arc<Config>,
    client: redis::Client,
    shard: usize,
    consumer: String,
) {
    let mut failures = 0u32;
    loop {
        let mut connection = match client.get_multiplexed_async_connection().await {
            Ok(connection) => connection,
            Err(error) => {
                failures = failures.saturating_add(1);
                let delay = retry_delay(failures, rand::random_range(0..=250));
                tracing::error!("archive shard {shard} Redis connect failed: {error:#}; retrying in {delay:?}");
                tokio::time::sleep(delay).await;
                continue;
            }
        };
        loop {
            match run_shard(&config, &mut connection, shard, &consumer).await {
                Ok(()) => failures = 0,
                Err(error) => {
                    failures = failures.saturating_add(1);
                    let delay = retry_delay(failures, rand::random_range(0..=250));
                    tracing::error!("archive shard {shard} failed: {error:#}; reconnecting in {delay:?}");
                    tokio::time::sleep(delay).await;
                    break;
                }
            }
        }
    }
}

/// Independently supervised archive consumer that runs inside the server process. It owns no
/// server request state: Redis and S3 stalls remain isolated to these shard tasks.
#[derive(Clone)]
pub struct LogArchiverService {
    config: Arc<Config>,
    client: redis::Client,
    consumer: String,
}

impl LogArchiverService {
    pub fn from_env(redis_url: String) -> Result<Self> {
        let config = Arc::new(Config::from_env(redis_url)?);
        let client = redis::Client::open(config.redis_url.clone())?;
        let consumer = std::env::var("IMMORTAL_LOG_ARCHIVER_ID")
            .unwrap_or_else(|_| Uuid::new_v4().to_string());
        Ok(Self {
            config,
            client,
            consumer,
        })
    }

    pub async fn run(self) -> Result<()> {
        RUNNING.store(true, Ordering::Release);
        let _running_guard = RunningGuard;
        let mut tasks = tokio::task::JoinSet::new();
        for shard in 0..self.config.shards {
            tasks.spawn(run_shard_forever(
                Arc::clone(&self.config),
                self.client.clone(),
                shard,
                format!("{}-{shard:02}", self.consumer),
            ));
        }
        // Shard workers reconnect forever. A return therefore indicates an unexpected panic or
        // cancellation; surface it without taking down the server request plane.
        match tasks.join_next().await {
            Some(Ok(())) => bail!("log archiver shard task exited unexpectedly"),
            Some(Err(error)) => Err(error).context("log archiver shard task failed"),
            None => bail!("log archiver started without any shard tasks"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn archive_stream_keys_are_fixed_and_hash_tagged() {
        assert_eq!(archive_key(32, 0), "immortal:logs:archive:v1-s32:{00}");
        assert_eq!(archive_key(32, 31), "immortal:logs:archive:v1-s32:{31}");
    }

    #[test]
    fn malformed_entries_have_a_dedicated_dead_letter_stream() {
        assert_eq!(invalid_key(), "immortal:logs:archive:invalid");
    }

    #[test]
    fn object_key_hides_the_workflow_id_and_is_content_addressed() {
        let key = object_key(
            "raw/v1",
            "customer/workflow",
            "1-0",
            "2-0",
            b"content",
            chrono::DateTime::parse_from_rfc3339("2025-01-02T03:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
        );
        assert!(key.starts_with("raw/v1/tenant=default/workflow_bucket="));
        assert!(key.contains("workflow_hash="));
        assert!(!key.contains("customer/workflow"));
        assert!(key.ends_with(".jsonl.gz"));
        assert!(key.contains("part-1-0-2-0-"));
    }

    #[test]
    fn stream_payload_extracts_only_the_payload_field() {
        let mut map = HashMap::new();
        map.insert(
            "payload".into(),
            Value::BulkString(br#"{"workflow_id":"wf"}"#.to_vec()),
        );
        let entry = StreamId {
            id: "1-0".into(),
            map,
        };
        assert_eq!(payload(&entry).as_deref(), Some(r#"{"workflow_id":"wf"}"#));
    }

    #[test]
    fn batches_are_bounded_by_records_and_bytes() {
        let entries = (0..5)
            .map(|index| (format!("{index}-0"), "12345".to_owned()))
            .collect();
        let chunks = partition_entries(entries, 3, 18);
        assert_eq!(chunks.iter().map(Vec::len).collect::<Vec<_>>(), vec![2, 2, 1]);
        assert!(chunks.iter().all(|chunk| chunk.len() <= 3));
    }

    #[test]
    fn reconnect_backoff_is_capped_and_jitter_is_bounded() {
        assert_eq!(retry_delay(1, 0), Duration::from_millis(250));
        assert_eq!(retry_delay(2, 250), Duration::from_millis(750));
        assert!(retry_delay(100, 250) <= Duration::from_millis(30_250));
    }

    #[test]
    fn in_process_service_toggle_is_strict_and_defaults_off() {
        assert!(!parse_enabled(None).unwrap());
        assert!(parse_enabled(Some("true")).unwrap());
        assert!(parse_enabled(Some("YES")).unwrap());
        assert!(!parse_enabled(Some("0")).unwrap());
        assert!(parse_enabled(Some("sometimes")).is_err());
    }
}
