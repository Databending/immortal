use axum::{extract::State, http::StatusCode, response::IntoResponse, Json};
use serde::Serialize;


use crate::{immortal::ClientStartWorkflowOptionsVersion, ImmortalService};

#[derive(Debug, Clone, Default, Serialize)]
struct Worker {
    id: String,
    workflows: Vec<String>,
    activities: Vec<String>,
}

pub async fn get_history(
    State(state): State<ImmortalService>,
    // this argument tells axum to parse the request body
    // as JSON into a `CreateUser` type
) -> impl IntoResponse {
    let history = state.history.lock().await.clone();

    // this will be converted into a JSON response
    // with a status code of `201 Created`
    Json(history)
}


pub async fn get_workers(
    State(state): State<ImmortalService>,
    // this argument tells axum to parse the request body
    // as JSON into a `CreateUser` type
) -> impl IntoResponse {
    let workers = state.workers.lock().await;
    let mut registered_workers = Vec::new();
    for (worker_id, worker) in workers.iter() {
        registered_workers.push(Worker {
            id: worker_id.clone(),
            workflows: worker.registered_workflows.clone(),
            activities: worker.registered_activities.clone(),
        });
    }

    // this will be converted into a JSON response
    // with a status code of `201 Created`
    Json(registered_workers)
}
