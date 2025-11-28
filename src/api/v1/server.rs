use crate::error::AppError;
use crate::history2::{get_blob_raw, Status as Status2};
use crate::history3::{WorkflowHistoryMetadata, WorkflowHistoryMetadataVersion};
use crate::state::AppState;
use crate::utils::log::fetch_log_history_from_redis;
use crate::ws::FetchLogs;
use crate::{
    history::{ActivityHistory, Status, WorkflowHistory, WorkflowHistoryVersion},
    ActivitySchema, WfSchema,
};
use axum::extract::Path;
use axum::http::header;
use axum::{
    extract::{Query, State},
    response::IntoResponse,
    Json,
};
use chrono::{DateTime, Utc};
use o2o::o2o;
use serde::{Deserialize, Serialize};
// use serde_json::Value;
use simd_json::OwnedValue;
use std::collections::HashMap;
use uuid::Uuid;
#[derive(Debug, Clone, Default, Serialize)]
struct Worker {
    id: String,
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
    task_queues: Option<String>,
    status: Option<Status2>,
}

#[derive(Deserialize, Debug)]
pub struct BlobRef {
    path: String,
    encode: Option<bool>,
}

#[derive(o2o, Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "version", content = "spec")]
#[from_owned(WorkflowHistoryVersion)]
enum ApiWorkflowHistoryVersion {
    V1(#[from(~.into())] ApiWorkflowHistory),
}

#[derive(o2o, Debug, Clone, Serialize, Deserialize)]
#[from_owned(WorkflowHistory)]
struct ApiWorkflowHistory {
    pub args: Vec<OwnedValue>,
    pub workflow_id: String,
    pub workflow_type: String,
    #[from(~.into())]
    pub status: ApiStatus,
    pub activities: Vec<ActivityHistory>,
    pub start_time: DateTime<Utc>,
    pub end_time: Option<DateTime<Utc>>,
    pub task_queue: Option<String>,
    pub worker_id: Option<String>,
    // pub status: Status,
}

#[derive(o2o, Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "spec")]
#[from_owned(Status)]
pub enum ApiStatus {
    Running,
    // #[serde(with = "serde_bytes")]
    Completed(OwnedValue),
    // Completed(#[from(simd_json::from_str::<OwnedValue>(&mut ~.clone()).unwrap().clone())] OwnedValue),
    Failed(String),
}

pub async fn delete_history(
    State(state): State<AppState>,
    Path(workflow_id): Path<Uuid>,
) -> impl IntoResponse {
    match state
        .immortal_service
        .history
        .delete_history(&workflow_id)
        .await
    {
        Ok(_history) => Json(()),
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
            (header::CONTENT_TYPE, "application/octet-stream; charset=utf-8"),
            (
                header::CONTENT_DISPOSITION,
                "attachment; filename=\"blob_ref\"",
            ),
        ];

        (headers, data.unwrap().into_response())
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
        params.status,
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
    for (worker_id, worker) in workers.iter() {
        registered_workers.push(Worker {
            registered_on: worker.registered_on,
            id: worker_id.clone(),
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
                    f.1.iter()
                        .map(|f| (f.0.clone(), f.1.clone()))
                        .collect::<Vec<_>>(),
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
