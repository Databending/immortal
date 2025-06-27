use crate::ImmortalService;
use axum::{
    extract::{Query, State},
    response::IntoResponse,
    Json,
};
use immortal::models::{ActivitySchema, WfSchema};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Default, Serialize)]
struct Worker {
    id: String,
    task_queue: String,
    workflows: HashMap<String, WfSchema>,
    activities: HashMap<String, ActivitySchema>,
    activity_capacity: i32,
    workflow_capacity: i32,
    max_activity_capacity: i32,
    max_workflow_capacity: i32,
}

//struct StrippedActivityQueue(HashMap<String, Vec<(String, RequestStartActivityOptionsV1)>>);

#[derive(Deserialize)]
pub struct HistoryFilter {
    worker_id: Option<String>,
    task_queue: Option<String>,
}
pub async fn get_history(
    State(state): State<ImmortalService>,

    Query(params): Query<HistoryFilter>, // this argument tells axum to parse the request body
                                         // as JSON into a `CreateUser` type
) -> impl IntoResponse {
    match state
        .history
        .get_workflows(Some(100), None, params.task_queue, params.worker_id)
        .await
    {
        Ok(history) => Json(history),
        Err(e) => {
            println!("{:#?}", e);
            Json(vec![])
        }
    }

    // this will be converted into a JSON response
    // with a status code of `201 Created`
}

pub async fn get_workers(
    State(state): State<ImmortalService>,
    // this argument tells axum to parse the request body
    // as JSON into a `CreateUser` type
) -> impl IntoResponse {
    let workers = state.workers.read().await;
    let mut registered_workers = Vec::new();
    for (worker_id, worker) in workers.iter() {
        registered_workers.push(Worker {
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
    State(state): State<ImmortalService>,
    // this argument tells axum to parse the request body
    // as JSON into a `CreateUser` type
) -> impl IntoResponse {
    Json(
        state
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
    State(state): State<ImmortalService>,
    // this argument tells axum to parse the request body
    // as JSON into a `CreateUser` type
) -> impl IntoResponse {
    Json(
        state
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
    State(state): State<ImmortalService>,
    // this argument tells axum to parse the request body
    // as JSON into a `CreateUser` type
) -> impl IntoResponse {
    Json(
        state
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
    State(state): State<ImmortalService>,
    // this argument tells axum to parse the request body
    // as JSON into a `CreateUser` type
) -> impl IntoResponse {
    Json(
        state
            .running_calls
            .read()
            .await
            .iter()
            .map(|f| (f.0.clone(), f.1 .1.clone()))
            .collect::<HashMap<_, _>>(),
    )
}
