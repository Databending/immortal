use axum::{
    routing::{get, post, delete},
    Router,
};

use crate::AppState;

pub mod cron;
pub mod run;
pub mod server;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/run/workflow", post(run::run_workflow))
        .route("/run/activity", post(run::run_activity))
        .route("/history", get(server::get_history))
        .route("/history/{id}", delete(server::delete_history))
        .route("/workers", get(server::get_workers))
        .route("/workflow-queue", get(server::get_workflow_queue))
        .route("/activity-queue", get(server::get_activity_queue))
        .route("/running-activities", get(server::running_activities))
        .route("/running-calls", get(server::running_calls))
}
