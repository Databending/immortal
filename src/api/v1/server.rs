use std::collections::HashMap;
use axum::{extract::State, response::IntoResponse, Json};
use immortal::models::{ActivitySchema, WfSchema};
use serde::Serialize;
use crate::ImmortalService;

#[derive(Debug, Clone, Default, Serialize)]
struct Worker {
    id: String,
    task_queue: String,
    workflows: HashMap<String, WfSchema>,
    activities: HashMap<String, ActivitySchema>,
    activity_capacity: i32,
    workflow_capacity: i32,
}

pub async fn get_history(
    State(state): State<ImmortalService>,
    // this argument tells axum to parse the request body
    // as JSON into a `CreateUser` type
) -> impl IntoResponse {
    let history = state.history.get_workflows(Some(100), None).await.unwrap();

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
            task_queue: worker.task_queue.clone(),
            activity_capacity: worker.activity_capacity,
            workflow_capacity: worker.workflow_capacity,
        });
    }

    // this will be converted into a JSON response
    // with a status code of `201 Created`
    Json(registered_workers)
}


pub async fn get_workflow_queue(
    State(state): State<ImmortalService>,
    // this argument tells axum to parse the request body
    // as JSON into a `CreateUser` type
) -> impl IntoResponse {
    Json(state.workflow_queue.lock().await.clone())
}

pub async fn get_activity_queue(
    State(state): State<ImmortalService>,
    // this argument tells axum to parse the request body
    // as JSON into a `CreateUser` type
) -> impl IntoResponse {
    Json(state.activity_queue.lock().await.clone())
}


