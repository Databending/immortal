use axum::{
    routing::{get, post, delete, patch},
    Router,
};

use crate::AppState;

pub mod cron;
pub mod run;
pub mod server;

pub fn router() -> Router<AppState> {
    Router::new()
        // DEPRECATE
        .route("/run/workflow", post(run::run_workflow))
        .route("/workflow/kill/{id}", delete(server::kill_workflow))
        .route("/workflow/run", post(run::run_workflow))
        // DEPRECATE
        .route("/run/activity", post(run::run_activity))
        .route("/activity/run", post(run::run_activity))
        .route("/history", get(server::get_history))
        .route("/history/blob", get(server::get_blob_ref))
        .route("/history/blob/download", get(server::download_blob_ref))
        .route("/logs", post(server::get_logs))
        .route("/history/{id}", delete(server::delete_history))
        .route("/history/{id}", get(server::get_wf_history))
        .route("/workers", get(server::get_workers))
        .route("/crons", get(cron::get_crons))
        .route("/crons", post(cron::create_cron))
        .route("/crons/{id}", delete(cron::delete_cron))
        .route("/crons/{id}", patch(cron::update_cron))
        .route("/crons/{id}/status", patch(cron::update_cron_status))
        .route("/crons/{id}", get(cron::get_cron))
        .route("/task-queues", get(server::get_task_queues))
        .route("/workflows/{task_queue}", get(server::get_registered_workflows))
        .route("/activities/{task_queue}", get(server::get_registered_activities))
        .route("/workflow-queue", get(server::get_workflow_queue))
        .route("/activity-queue", get(server::get_activity_queue))
        .route("/running-activities", get(server::running_activities))
        .route("/running-calls", get(server::running_calls))
}
