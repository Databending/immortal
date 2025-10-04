use redis::{
    aio::MultiplexedConnection,
    streams::{StreamId, StreamRangeReply},
    AsyncCommands,
};
use simd_json::OwnedValue;

use crate::ws::FetchLogs;
use simd_json::prelude::ValueAsMutObject;

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
                .await
                .expect("read");
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
                    } else {
                        panic!("Weird data")
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
