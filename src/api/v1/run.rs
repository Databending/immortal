use axum::{extract::State, http::StatusCode, response::IntoResponse, Json};
use immortal::immortal::CallVersion;

use crate::{immortal::ClientStartWorkflowOptionsVersion, ImmortalService};

pub async fn run(
    State(state): State<ImmortalService>,
    // this argument tells axum to parse the request body
    // as JSON into a `CreateUser` type
    Json(workflow_options): Json<ClientStartWorkflowOptionsVersion>,
) -> impl IntoResponse {
    println!(
        "Received request to start a workflow: {:?}",
        workflow_options
    );
    state
        .start_workflow_internal(workflow_options, None)
        .await
        .unwrap();

    // this will be converted into a JSON response
    // with a status code of `201 Created`
    StatusCode::CREATED
}

pub async fn run_activity(
    State(state): State<ImmortalService>,
    // this argument tells axum to parse the request body
    // as JSON into a `CreateUser` type
    Json(workflow_options): Json<CallVersion>,
) -> impl IntoResponse {
    println!(
        "Received request to start a activity: {:?}",
        workflow_options
    );
    state
        .start_activity_internal(workflow_options)
        .await
        .unwrap();

    // this will be converted into a JSON response
    // with a status code of `201 Created`
    StatusCode::CREATED
}
