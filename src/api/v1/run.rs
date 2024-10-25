use axum::{extract::State, http::StatusCode, response::IntoResponse, Json};


use crate::{immortal::ClientStartWorkflowOptionsVersion, ImmortalService};


pub async fn run(
    State(state): State<ImmortalService>,
    // this argument tells axum to parse the request body
    // as JSON into a `CreateUser` type
    Json(workflow_options): Json<ClientStartWorkflowOptionsVersion>,
) -> impl IntoResponse {
    println!("Received request to start a workflow: {:?}", workflow_options);
    state
        .start_workflow_internal(workflow_options)
        .await
        .unwrap();

    // this will be converted into a JSON response
    // with a status code of `201 Created`
    StatusCode::CREATED
}
