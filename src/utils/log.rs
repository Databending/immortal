use flate2::read::GzDecoder;
use redis::{
    aio::MultiplexedConnection,
    streams::{StreamId, StreamRangeReply},
    AsyncCommands,
};
use bb8_redis::{bb8, RedisConnectionManager};
use s3::{creds::Credentials, Bucket, Region};
use simd_json::OwnedValue;
use std::io::Read;
use std::sync::OnceLock;
use std::time::Duration;

use crate::ws::FetchLogs;
use simd_json::prelude::{ValueAsMutObject, ValueAsScalar, ValueObjectAccess};

static ARCHIVE_BUCKET: OnceLock<Result<Box<Bucket>, String>> = OnceLock::new();

fn archive_bucket() -> anyhow::Result<Option<&'static Bucket>> {
    let Ok(bucket_name) = std::env::var("IMMORTAL_LOG_S3_BUCKET") else {
        return Ok(None);
    };
    let bucket = ARCHIVE_BUCKET.get_or_init(|| {
        let region: Region = std::env::var("IMMORTAL_LOG_S3_REGION")
            .unwrap_or_else(|_| "us-east-1".into())
            .parse()
            .map_err(|error| format!("invalid S3 region: {error}"))?;
        let credentials = Credentials::default().map_err(|error| error.to_string())?;
        Bucket::new(&bucket_name, region, credentials).map_err(|error| error.to_string())
    });
    match bucket {
        Ok(bucket) => Ok(Some(bucket.as_ref())),
        Err(error) => anyhow::bail!("unable to initialize S3 archive client: {error}"),
    }
}

pub async fn fetch_log_history_from_redis(
    fetch_logs: &FetchLogs,
    con: &mut MultiplexedConnection,
    // immortal_service: &Arc<ImmortalService>,
    limit: &Option<i32>,
    cursor: &Option<String>,
) -> anyhow::Result<Vec<simd_json::value::OwnedValue>> {
    let mut logs = vec![];
    match fetch_logs {
        FetchLogs::Workflow(ref workflow) => {
            let srr: StreamRangeReply = con
                .xrevrange_count(
                    format!("immortal:logs:{}", workflow.workflow_id),
                    cursor.clone().unwrap_or("+".to_string()),
                    "-",
                    limit.unwrap_or(100),
                )
                .await?;
            // for StreamKey { key: _, ids } in srr.ids {
            for StreamId { id, map } in srr.ids {
                let mut parsed_map = simd_json::json!({
                    "id": id.clone()
                });

                if let Some(activity_id) = &workflow.activity_id {
                    if let Some(log_activity_id) = map.get("activity_id") {
                        if let redis::Value::BulkString(bytes) = log_activity_id.clone() {
                            let log_activity_id: String = String::from_utf8(bytes).unwrap();
                            if *activity_id != log_activity_id {
                                continue;
                            }
                        }
                    } else {
                        continue;
                    }
                }
                if let Some(activity_run_id) = &workflow.run_id {
                    if let Some(log_activity_id) = map.get("activity_run_id") {
                        if let redis::Value::BulkString(bytes) = log_activity_id.clone() {
                            let log_activity_id: String = String::from_utf8(bytes).unwrap();
                            if *activity_run_id != log_activity_id {
                                continue;
                            }
                        }
                    } else {
                        continue;
                    }
                }
                for (n, s) in map.clone() {
                    if let redis::Value::BulkString(mut bytes) = s {
                        if n == "metadata" {
                            parsed_map
                                .as_object_mut() // Get a mutable reference to the underlying Map
                                .unwrap() // Panics if not an object, which we've initialized it to be
                                .insert(
                                    n.to_owned(), // Convert String n to OwnedValue::String for the key, or use n.as_str()
                                    simd_json::from_slice(&mut bytes)
                                        .unwrap_or(OwnedValue::default()),
                                );
                        } else {
                            parsed_map.as_object_mut().unwrap().insert(
                                n.to_owned(), // Convert String n to OwnedValue::String for the key
                                OwnedValue::String(String::from_utf8(bytes)?),
                            );
                        }
                    }
                }
                logs.push(parsed_map);
            }
            // }
        }
        FetchLogs::Worker(ref _worker_ids) => {
            // let workflows = immortal_service
            //     .history
            //     .get_workflows(Some(1000), Some(0), None, Some(worker_ids.to_vec()))
            //     .await
            //     .unwrap();
            // let workflow_ids = workflows
            //     .iter()
            //     .map(|f| match f {
            //         WorkflowHistoryVersion::V1(v1) => v1.workflow_id.clone(),
            //     })
            //     .collect::<Vec<_>>();
            // merge_last_ids(last_ids, &workflow_ids);
            // let srr: StreamReadReply = con
            //     .xread_options(
            //         &workflow_ids
            //             .iter()
            //             .map(|f| format!("immortal:logs:{}", f))
            //             .collect::<Vec<_>>(),
            //         &sort_last_ids(&last_ids, &workflow_ids),
            //         &opts,
            //     )
            //     .await
            //     .expect("read");
            // for StreamKey { key, ids } in srr.keys {
            //     for StreamId { id, map } in ids {
            //         let key_id = key.split("immortal:logs:").collect::<Vec<_>>();
            //         let key_id = key_id.get(1).unwrap();
            //         if let Some(last_id) = last_ids.get_mut(&key_id.to_string()) {
            //             *last_id = id.clone();
            //         }
            //         // last_ids.get_mut("key") = id.clone();
            //         let mut parsed_map = simd_json::json!({
            //             "id": id.clone()
            //         });
            //         // read_and_send_log(&room_name, &mut parsed_map, &map, &io)
            //         //     .await
            //         //     .unwrap();
            //     }
            // }
        }
        FetchLogs::TaskQueue(ref _task_queues) => {}
    }
    Ok(logs)
}

pub async fn fetch_workflow_logs_from_s3(
    workflow_id: &str,
    limit: usize,
    after: Option<(&str, &str)>,
    activity_id: Option<&str>,
    activity_run_id: Option<&str>,
) -> anyhow::Result<Vec<OwnedValue>> {
    let Some(bucket) = archive_bucket()? else {
        return Ok(vec![]);
    };
    let prefix = std::env::var("IMMORTAL_LOG_S3_PREFIX").unwrap_or_else(|_| "raw/v1".into());
    let workflow_hash = blake3::hash(workflow_id.as_bytes()).to_hex().to_string();
    let object_prefix = format!(
        "{}/tenant=default/workflow_bucket={}/workflow_hash={}/",
        prefix.trim_matches('/'),
        &workflow_hash[..2],
        workflow_hash
    );
    let request_timeout = Duration::from_millis(
        std::env::var("IMMORTAL_LOG_S3_REQUEST_TIMEOUT_MS")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(30_000),
    );
    let pages = tokio::time::timeout(request_timeout, bucket.list(object_prefix, None)).await??;
    // Keep only the next page in memory while scanning every immutable object. This makes the
    // `(emitted_at,event_id)` cursor complete without loading an entire workflow archive.
    let mut logs = Vec::with_capacity(limit.saturating_add(1));
    for page in pages {
        for object in page.contents {
            let response = tokio::time::timeout(request_timeout, bucket.get_object(&object.key))
                .await??;
            if !(200..300).contains(&response.status_code()) {
                continue;
            }
            let mut decoder = GzDecoder::new(response.bytes().as_ref());
            let mut jsonl = String::new();
            decoder.read_to_string(&mut jsonl)?;
            for line in jsonl.lines() {
                let mut line = line.as_bytes().to_vec();
                if let Ok(log) = simd_json::from_slice::<OwnedValue>(&mut line) {
                    let field = |name: &str| log.get(name).and_then(|value| value.as_str());
                    if activity_id.is_some_and(|id| field("activity_id") != Some(id))
                        || activity_run_id
                            .is_some_and(|id| field("activity_run_id") != Some(id))
                    {
                        continue;
                    }
                    let order = (
                        field("emitted_at").unwrap_or_default(),
                        field("event_id").unwrap_or_default(),
                    );
                    if after.is_some_and(|cursor| order <= cursor) {
                        continue;
                    }
                    logs.push(log);
                    logs.sort_by(|left, right| archive_log_order(left).cmp(&archive_log_order(right)));
                    if logs.len() > limit {
                        logs.pop();
                    }
                }
            }
        }
    }
    Ok(logs)
}

fn archive_log_order(log: &OwnedValue) -> (String, String) {
    let field = |name: &str| {
        log.get(name)
            .and_then(|value| value.as_str())
            .unwrap_or_default()
            .to_owned()
    };
    (field("emitted_at"), field("event_id"))
}

pub async fn delete_workflow_logs_from_s3(workflow_id: &str) -> anyhow::Result<()> {
    let Some(bucket) = archive_bucket()? else {
        return Ok(());
    };
    let prefix = std::env::var("IMMORTAL_LOG_S3_PREFIX").unwrap_or_else(|_| "raw/v1".into());
    let hash = blake3::hash(workflow_id.as_bytes()).to_hex().to_string();
    let object_prefix = format!(
        "{}/tenant=default/workflow_bucket={}/workflow_hash={}/",
        prefix.trim_matches('/'),
        &hash[..2],
        hash
    );
    let timeout = Duration::from_millis(
        std::env::var("IMMORTAL_LOG_S3_REQUEST_TIMEOUT_MS")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(30_000),
    );
    let pages = tokio::time::timeout(timeout, bucket.list(object_prefix, None)).await??;
    for page in pages {
        for object in page.contents {
            tokio::time::timeout(timeout, bucket.delete_object(&object.key)).await??;
        }
    }
    Ok(())
}

pub const ARCHIVE_DELETE_PENDING_KEY: &str = "immortal:logs:archive:delete-pending:v1";

/// Retry durable archive deletion requests. The tombstone is removed only after every currently
/// listed object has been deleted successfully; rerunning deletion is therefore idempotent.
pub async fn reconcile_archive_deletions(pool: bb8::Pool<RedisConnectionManager>) {
    loop {
        let workflow_ids = async {
            let mut connection = pool.get().await?;
            let ids: Vec<String> = connection.hkeys(ARCHIVE_DELETE_PENDING_KEY).await?;
            Ok::<_, anyhow::Error>(ids)
        }
        .await;
        match workflow_ids {
            Ok(ids) => {
                for workflow_id in ids {
                    match delete_workflow_logs_from_s3(&workflow_id).await {
                        Ok(()) => match pool.get().await {
                            Ok(mut connection) => {
                                if let Err(error) = connection
                                    .hdel::<_, _, ()>(ARCHIVE_DELETE_PENDING_KEY, &workflow_id)
                                    .await
                                {
                                    tracing::warn!("unable to clear archive deletion tombstone: {error}");
                                }
                            }
                            Err(error) => tracing::warn!("unable to clear archive deletion tombstone: {error}"),
                        },
                        Err(error) => tracing::warn!(
                            "archive deletion for workflow {workflow_id} will be retried: {error:#}"
                        ),
                    }
                }
            }
            Err(error) => tracing::warn!("unable to read archive deletion tombstones: {error:#}"),
        }
        tokio::time::sleep(Duration::from_secs(60)).await;
    }
}
