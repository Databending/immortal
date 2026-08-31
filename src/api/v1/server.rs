use crate::error::AppError;
use crate::history::{get_blob_raw, Status as Status2};
use crate::history_metadata::{WorkflowHistoryMetadata, WorkflowHistoryMetadataVersion};
use crate::state::AppState;
use crate::utils::log::{
    delete_workflow_logs_from_s3, fetch_log_history_from_redis, fetch_workflow_logs_from_s3,
    ARCHIVE_DELETE_PENDING_KEY,
};
use redis::AsyncCommands;
use crate::ws::FetchLogs;
use crate::{ActivitySchema, WfSchema};
use axum::extract::Path;
use axum::http::header;
use axum::{
    extract::{Query, State},
    response::IntoResponse,
    Json,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
// use serde_json::Value;
use simd_json::prelude::{ValueAsScalar, ValueObjectAccess};
use simd_json::OwnedValue;
use std::collections::HashMap;
use uuid::Uuid;
#[derive(Debug, Clone, Default, Serialize)]
struct Worker {
    id: Uuid,
    worker_id: String,
    registered_on: DateTime<Utc>,
    task_queue: String,
    workflows: HashMap<String, WfSchema>,
    activities: HashMap<String, ActivitySchema>,
    activity_capacity: i32,
    workflow_capacity: i32,
    max_activity_capacity: i32,
    max_workflow_capacity: i32,
}

//struct StrippedActivityQueue(HashMap<String, Vec<(String, RequestStartActivityOptionsV1)>>);

#[derive(Deserialize, Debug)]
pub struct HistoryFilter {
    worker_ids: Option<String>,
    worker_instance_ids: Option<String>,
    task_queues: Option<String>,
    status: Option<Status2>,
}

#[derive(Deserialize, Debug)]
pub struct BlobRef {
    path: String,
    encode: Option<bool>,
}

pub async fn delete_history(
    State(state): State<AppState>,
    Path(workflow_id): Path<Uuid>,
) -> impl IntoResponse {
    if let Ok(mut connection) = state.redis.get().await {
        if let Err(error) = connection
            .hset::<_, _, _, ()>(
                ARCHIVE_DELETE_PENDING_KEY,
                workflow_id.to_string(),
                Utc::now().to_rfc3339(),
            )
            .await
        {
            tracing::error!("unable to record durable archive deletion request: {error}");
        }
    }
    match state
        .immortal_service
        .history
        .delete_history(&workflow_id)
        .await
    {
        Ok(_history) => {
            if let Err(e) = delete_workflow_logs_from_s3(&workflow_id.to_string()).await {
                tracing::error!(
                    "workflow history was deleted from Redis but archive deletion failed: {e:#}"
                );
            } else if let Ok(mut connection) = state.redis.get().await {
                let _ = connection
                    .hdel::<_, _, ()>(ARCHIVE_DELETE_PENDING_KEY, workflow_id.to_string())
                    .await;
            }
            Json(())
        }
        Err(e) => {
            println!("{:#?}", e);
            Json(())
        }
    }
}

pub async fn kill_workflow(
    State(state): State<AppState>,
    Path(workflow_id): Path<Uuid>,
) -> impl IntoResponse {
    match state
        .immortal_service
        .kill_workflow(&workflow_id.to_string())
        .await
    {
        Ok(_history) => Json(()),
        Err(e) => {
            println!("{:#?}", e);
            Json(())
        }
    }
}

#[derive(Serialize)]
#[serde(untagged)]
pub enum BlobEncoding {
    Raw(Option<Vec<u8>>),
    String(Option<String>),
}

pub async fn get_blob_ref(
    State(state): State<AppState>,

    Query(params): Query<BlobRef>, // this argument tells axum to parse the request body
                                   // as JSON into a `CreateUser` type
) -> impl IntoResponse {
    let mut con = state.redis.get().await.unwrap();

    if params.encode.unwrap_or(false) {
        Json(BlobEncoding::String(
            get_blob_raw::<String>(&mut con, &params.path)
                .await
                .unwrap(),
        ))
    } else {
        Json(BlobEncoding::Raw(
            get_blob_raw::<Vec<u8>>(&mut con, &params.path)
                .await
                .unwrap(),
        ))
    }
}

pub async fn download_blob_ref(
    State(state): State<AppState>,

    Query(params): Query<BlobRef>, // this argument tells axum to parse the request body
) -> impl IntoResponse {
    let mut con = state.redis.get().await.unwrap();
    if params.encode.unwrap_or(false) {
        let data = get_blob_raw::<String>(&mut con, &params.path)
            .await
            .unwrap();

        let headers = [
            (header::CONTENT_TYPE, "text/plain; charset=utf-8"),
            (
                header::CONTENT_DISPOSITION,
                "attachment; filename=\"blob_ref.txt\"",
            ),
        ];

        (headers, data.unwrap().into_response())
    } else {
        let data = get_blob_raw::<Vec<u8>>(&mut con, &params.path)
            .await
            .unwrap();

        let headers = [
            (
                header::CONTENT_TYPE,
                "application/octet-stream; charset=utf-8",
            ),
            (
                header::CONTENT_DISPOSITION,
                "attachment; filename=\"blob_ref\"",
            ),
        ];

        (headers, data.unwrap().into_response())
    }
}

pub async fn get_wf_history(
    State(state): State<AppState>,
    Path(workflow_id): Path<Uuid>,
) -> impl IntoResponse {
    let mut con = state.immortal_service.history.get_con().await.unwrap();
    match WorkflowHistoryMetadata::get_opt(&mut con, &workflow_id.to_string(), true).await {
        Ok(history) => {
            // let mut api_histories: Vec<ApiWorkflowHistoryVersion> = vec![];
            // for x in history {
            //     api_histories.push(x.into());
            // }
            Json(history.map(|f| WorkflowHistoryMetadataVersion::V1(f)))
        }
        Err(e) => {
            println!("{}", e);
            Json(None)
        }
    }
}

pub async fn get_history(
    State(state): State<AppState>,

    Query(params): Query<HistoryFilter>, // this argument tells axum to parse the request body
                                         // as JSON into a `CreateUser` type
) -> impl IntoResponse {
    let mut con = state.immortal_service.history.get_con().await.unwrap();
    match WorkflowHistoryMetadata::get_all(
        &mut con,
        Some(100),
        None,
        params
            .task_queues
            .map(|f| f.split(",").map(|f| f.to_string()).collect()),
        params
            .worker_ids
            .map(|f| f.split(",").map(|f| f.to_string()).collect()),
        params
            .worker_instance_ids
            .map(|f| f.split(",").map(|f| f.to_string()).collect()),
        params.status,
        false,
    )
    .await
    {
        Ok(history) => {
            // let mut api_histories: Vec<ApiWorkflowHistoryVersion> = vec![];
            // for x in history {
            //     api_histories.push(x.into());
            // }
            Json(
                history
                    .into_iter()
                    .map(|f| WorkflowHistoryMetadataVersion::V1(f))
                    .collect::<Vec<_>>(),
            )
        }
        Err(e) => {
            println!("{}", e);
            Json(vec![])
        }
    }
}

#[derive(Deserialize)]
pub struct LogFilter {
    fetch_type: FetchLogs,
    cursor: Option<String>,
    limit: Option<i32>,
    // task_queues: Option<String>,
}

#[derive(Serialize, Deserialize)]
struct LogCursor {
    emitted_at: String,
    event_id: String,
}

#[derive(Serialize)]
pub struct TieredLogPage {
    logs: Vec<OwnedValue>,
    next_cursor: Option<String>,
}

fn log_field(log: &OwnedValue, field: &str) -> Option<String> {
    log.get(field)
        .and_then(|value| value.as_str())
        .map(str::to_owned)
}

fn log_order(log: &OwnedValue) -> (String, String) {
    (
        log_field(log, "emitted_at")
            .or_else(|| log_field(log, "when"))
            .unwrap_or_default(),
        log_field(log, "event_id").unwrap_or_default(),
    )
}

fn matches_log_filter(log: &OwnedValue, workflow: &crate::ws::FetchWorkflowLogs) -> bool {
    workflow
        .activity_id
        .as_ref()
        .is_none_or(|id| log_field(log, "activity_id").as_deref() == Some(id))
        && workflow
            .run_id
            .as_ref()
            .is_none_or(|id| log_field(log, "activity_run_id").as_deref() == Some(id))
}

fn merge_tiered_page(
    mut logs: Vec<OwnedValue>,
    workflow: &crate::ws::FetchWorkflowLogs,
    cursor: Option<&LogCursor>,
    limit: usize,
) -> TieredLogPage {
    let mut seen = std::collections::HashSet::new();
    logs.retain(|log| {
        seen.insert(log_field(log, "event_id").unwrap_or_else(|| format!("legacy:{:?}", log)))
            && matches_log_filter(log, workflow)
    });
    logs.sort_by_key(log_order);
    let mut logs: Vec<_> = logs
        .into_iter()
        .filter(|log| {
            cursor.is_none_or(|cursor| {
                log_order(log) > (cursor.emitted_at.clone(), cursor.event_id.clone())
            })
        })
        .take(limit + 1)
        .collect();
    let has_more = logs.len() > limit;
    logs.truncate(limit);
    let next_cursor = has_more.then(|| logs.last()).flatten().map(|log| {
        let (emitted_at, event_id) = log_order(log);
        serde_json::to_string(&LogCursor {
            emitted_at,
            event_id,
        })
        .expect("cursor serializes")
    });
    TieredLogPage { logs, next_cursor }
}

pub async fn get_logs_v2(
    State(state): State<AppState>,
    Json(payload): Json<LogFilter>,
) -> Result<Json<TieredLogPage>, AppError> {
    let FetchLogs::Workflow(workflow) = &payload.fetch_type else {
        return Ok(Json(TieredLogPage {
            logs: vec![],
            next_cursor: None,
        }));
    };
    let limit = payload.limit.unwrap_or(100).clamp(1, 1_000) as usize;
    let cursor = payload
        .cursor
        .as_deref()
        .and_then(|value| serde_json::from_str::<LogCursor>(value).ok());
    let mut connection = state.redis.get().await?;
    // Redis stream IDs are an implementation detail; fetch a bounded working range then apply
    // the permanent cross-tier ordering cursor below.
    let mut hot =
        fetch_log_history_from_redis(&payload.fetch_type, &mut connection, &Some(1_000), &None)
            .await?;
    let cursor_order = cursor
        .as_ref()
        .map(|cursor| (cursor.emitted_at.as_str(), cursor.event_id.as_str()));
    let cold = match fetch_workflow_logs_from_s3(
        &workflow.workflow_id,
        limit + 1,
        cursor_order,
        workflow.activity_id.as_deref(),
        workflow.run_id.as_deref(),
    )
    .await
    {
        Ok(logs) => logs,
        Err(error) => {
            tracing::warn!("S3 archive read failed; returning available hot logs: {error:#}");
            vec![]
        }
    };
    hot.extend(cold);
    Ok(Json(merge_tiered_page(
        hot,
        workflow,
        cursor.as_ref(),
        limit,
    )))
}
pub async fn get_logs(
    State(state): State<AppState>,
    Json(payload): Json<LogFilter>, // this argument tells axum to parse the request body
) -> Result<Json<Vec<OwnedValue>>, AppError> {
    let redis = &state.redis;
    let mut con = redis.get().await?;
    // let con = state.with_current_subscriber
    let logs = fetch_log_history_from_redis(
        &payload.fetch_type,
        &mut con,
        &payload.limit,
        &payload.cursor,
    )
    .await?;
    Ok(Json(logs))
}

pub async fn get_workers(
    State(state): State<AppState>,
    // this argument tells axum to parse the request body
    // as JSON into a `CreateUser` type
) -> impl IntoResponse {
    // println!("waiting to read workers");
    let workers = state.immortal_service.workers.read().await;

    // println!("workers read");
    let mut registered_workers = Vec::new();
    for (instance_id, worker) in workers.iter() {
        registered_workers.push(Worker {
            registered_on: worker.registered_on,
            id: instance_id.clone(),
            worker_id: worker.worker_id.clone(),
            workflows: worker.registered_workflows.clone(),
            activities: worker.registered_activities.clone(),
            task_queue: worker.task_queue.clone(),
            activity_capacity: worker.activity_capacity,
            workflow_capacity: worker.workflow_capacity,
            max_activity_capacity: worker.max_activity_capacity,
            max_workflow_capacity: worker.max_workflow_capacity,
        });
    }

    // this will be converted into a JSON response
    // with a status code of `201 Created`
    Json(registered_workers)
}

pub async fn get_logging_metrics() -> Json<crate::LoggingMetricsSnapshot> {
    Json(crate::logging_metrics_snapshot())
}

pub async fn get_task_queues(
    State(state): State<AppState>,
    // this argument tells axum to parse the request body
    // as JSON into a `CreateUser` type
) -> impl IntoResponse {
    // println!("waiting to read workers");
    let mut task_queues = vec![];
    let workers = state.immortal_service.workers.read().await;

    // println!("workers read");
    for (_worker_id, worker) in workers.iter() {
        if !task_queues.contains(&worker.task_queue) {
            task_queues.push(worker.task_queue.clone());
        }
    }

    // this will be converted into a JSON response
    // with a status code of `201 Created`
    Json(task_queues)
}

pub async fn get_registered_workflows(
    State(state): State<AppState>,
    Path(task_queue): Path<String>,
    // this argument tells axum to parse the request body
    // as JSON into a `CreateUser` type
) -> impl IntoResponse {
    // println!("waiting to read workers");
    let mut workflow_types = HashMap::new();
    let workers = state.immortal_service.workers.read().await;

    // println!("workers read");
    for (_worker_id, worker) in workers.iter() {
        if worker.task_queue == task_queue {
            let registered_workflows: Vec<_> = worker.registered_workflows.iter().collect();
            for (key, registered_workflow) in registered_workflows {
                if !workflow_types.contains_key(key) {
                    workflow_types.insert(key.clone(), registered_workflow.clone());
                }
            }
        }
    }

    // this will be converted into a JSON response
    // with a status code of `201 Created`
    Json(workflow_types)
}

pub async fn get_registered_activities(
    State(state): State<AppState>,
    Path(task_queue): Path<String>,
    // this argument tells axum to parse the request body
    // as JSON into a `CreateUser` type
) -> impl IntoResponse {
    // println!("waiting to read workers");
    let mut activity_types = HashMap::new();
    let workers = state.immortal_service.workers.read().await;

    // println!("workers read");
    for (_worker_id, worker) in workers.iter() {
        if worker.task_queue == task_queue {
            let registered_activities: Vec<_> = worker.registered_activities.iter().collect();
            for (key, registered_activity) in registered_activities {
                if !activity_types.contains_key(key) {
                    activity_types.insert(key.clone(), registered_activity.clone());
                }
            }
        }
    }

    // this will be converted into a JSON response
    // with a status code of `201 Created`
    Json(activity_types)
}

//
// pub async fn get_registered_notifications(
//     State(state): State<AppState>,
//     // this argument tells axum to parse the request body
//     // as JSON into a `CreateUser` type
// ) -> impl IntoResponse {
//     println!("waiting to read workers");
//     let mut activity_types = HashMap::new();
//     let workers = state.immortal_service.workers.read().await;
//
//     println!("workers read");
//     for (_worker_id, worker) in workers.iter() {
//         // if worker.task_queue == task_queue {
//             let registered_activities: Vec<_> = worker.registered_activities.iter().collect();
//             for (key, registered_activity) in registered_activities {
//                 if !activity_types.contains_key(key) {
//                     activity_types.insert(key.clone(), registered_activity.clone());
//                 }
//             }
//         // }
//     }
//
//     // this will be converted into a JSON response
//     // with a status code of `201 Created`
//     Json(activity_types)
// }

#[cfg(test)]
mod tiered_log_tests {
    use super::*;

    fn log(event_id: &str, emitted_at: &str, activity_id: &str) -> OwnedValue {
        simd_json::json!({
            "event_id": event_id,
            "emitted_at": emitted_at,
            "activity_id": activity_id,
            "activity_run_id": "run-1",
        })
    }

    #[test]
    fn tiered_page_deduplicates_filters_orders_and_advances_cursor() {
        let workflow = crate::ws::FetchWorkflowLogs {
            workflow_id: "workflow".into(),
            activity_id: Some("activity".into()),
            run_id: Some("run-1".into()),
        };
        let input = vec![
            log("event-3", "2025-01-01T00:00:03Z", "activity"),
            log("event-1", "2025-01-01T00:00:01Z", "activity"),
            log("event-1", "2025-01-01T00:00:01Z", "activity"),
            log("event-2", "2025-01-01T00:00:02Z", "other"),
        ];
        let first = merge_tiered_page(input.clone(), &workflow, None, 1);
        assert_eq!(log_field(&first.logs[0], "event_id").as_deref(), Some("event-1"));
        let cursor: LogCursor = serde_json::from_str(first.next_cursor.as_deref().unwrap()).unwrap();

        let second = merge_tiered_page(input, &workflow, Some(&cursor), 2);
        assert_eq!(second.logs.len(), 1);
        assert_eq!(log_field(&second.logs[0], "event_id").as_deref(), Some("event-3"));
        assert!(second.next_cursor.is_none());
    }
}

//pub async fn get_workflow_queue(
//    State(state): State<ImmortalService>,
//    // this argument tells axum to parse the request body
//    // as JSON into a `CreateUser` type
//) -> impl IntoResponse {
//    Json(state.workflow_queue.lock().await.clone())
//}
//
// pub async fn get_workflow_queue(
//     State(state): State<ImmortalService>,
//     // this argument tells axum to parse the request body
//     // as JSON into a `CreateUser` type
// ) -> impl IntoResponse {
//     Json(
//         state
//             .workflow_queue
//             .lock()
//             .await
//             .iter()
//             .map(|f| {
//                 (
//                     f.0.clone(),
//                     f.1.iter()
//                         .map(|f| (f.0.clone(), f.1.clone()))
//                         .collect::<Vec<_>>(),
//                 )
//             })
//             .collect::<HashMap<_, _>>(),
//     )
// }
//

pub async fn get_workflow_queue(
    State(state): State<AppState>,
    // this argument tells axum to parse the request body
    // as JSON into a `CreateUser` type
) -> impl IntoResponse {
    Json(
        state
            .immortal_service
            .workflow_queue
            .lock()
            .await
            .iter()
            .map(|f| {
                (
                    f.0.clone(),
                    f.1.iter()
                        .map(|f| (f.0.clone(), f.1.clone()))
                        .collect::<Vec<_>>(),
                )
            })
            .collect::<HashMap<_, _>>(),
    )
}

pub async fn get_activity_queue(
    State(state): State<AppState>,
    // this argument tells axum to parse the request body
    // as JSON into a `CreateUser` type
) -> impl IntoResponse {
    Json(
        state
            .immortal_service
            .activity_queue
            .lock()
            .await
            .iter()
            .map(|f| {
                (
                    f.0.clone(),
                    f.1.iter().map(|f| f.0.clone()).collect::<Vec<_>>(),
                )
            })
            .collect::<HashMap<_, _>>(),
    )
}

pub async fn get_call_queue(
    State(state): State<AppState>,
    // this argument tells axum to parse the request body
    // as JSON into a `CreateUser` type
) -> impl IntoResponse {
    Json(
        state
            .immortal_service
            .call_queue
            .lock()
            .await
            .iter()
            .map(|f| {
                (
                    f.0.clone(),
                    f.1.iter()
                        .map(|f| (f.0.clone(), f.1.clone()))
                        .collect::<Vec<_>>(),
                )
            })
            .collect::<HashMap<_, _>>(),
    )
}

pub async fn running_calls(
    State(state): State<AppState>,
    // this argument tells axum to parse the request body
    // as JSON into a `CreateUser` type
) -> impl IntoResponse {
    Json(
        state
            .immortal_service
            .running_calls
            .read()
            .await
            .iter()
            .map(|f| (f.0.clone(), f.1 .1.clone()))
            .collect::<HashMap<_, _>>(),
    )
}

pub async fn running_activities(
    State(state): State<AppState>,
    // this argument tells axum to parse the request body
    // as JSON into a `CreateUser` type
) -> impl IntoResponse {
    Json(
        state
            .immortal_service
            .running_activities
            .read()
            .await
            .iter()
            .map(|f| (f.0.clone(), f.1 .2.clone()))
            .collect::<HashMap<_, _>>(),
    )
}
//
// pub async fn running_workflows(
//     State(state): State<ImmortalService>,
//     // this argument tells axum to parse the request body
//     // as JSON into a `CreateUser` type
// ) -> impl IntoResponse {
//     Json(
//         state
//             .running_calls
//             .read()
//             .await
//             .iter()
//             .map(|f| (f.0.clone(), f.1.2.clone()))
//             .collect::<HashMap<_, _>>(),
//     )
// }
