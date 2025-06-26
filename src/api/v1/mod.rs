use axum::{
    routing::{get, post},
    Router,
};

use crate::ImmortalService;
pub mod run;
pub mod server;

pub fn router() -> Router<ImmortalService> {
    Router::new()
        .route("/run/workflow", post(run::run_workflow))
        .route("/run/activity", post(run::run_activity))
        .route("/history", get(server::get_history))
        .route("/workers", get(server::get_workers))
        .route("/workflow-queue", get(server::get_workflow_queue))
        .route("/activity-queue", get(server::get_activity_queue))
}
