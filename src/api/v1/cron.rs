use crate::{
    cron::{CronSpec, CronStatus},
    error::AnyhowError,
    state::AppState,
};
use axum::{
    extract::{Path, State},
    response::IntoResponse,
    Json,
};
use uuid::Uuid;

pub async fn get_crons(State(state): State<AppState>) -> impl IntoResponse {
    let crons: Vec<CronSpec>;
    {
        let cron_manager = state.immortal_service.cron_manager.lock().await;
        crons = cron_manager.installed3.iter().map(|f| f.clone()).collect();
    }
    Json(crons)
}

pub async fn get_cron(
    State(state): State<AppState>,
    Path(cron_id): Path<Uuid>,
) -> impl IntoResponse {
    let cron;
    {
        let cron_manager = state.immortal_service.cron_manager.lock().await;
        cron = cron_manager
            .installed3
            .iter()
            .find(|f| f.id == cron_id)
            .cloned();
    }
    Json(cron)
}

pub async fn create_cron(
    State(state): State<AppState>,
    Json(cron): Json<CronSpec>,
) -> Result<Json<()>, AnyhowError> {
    {
        let cron_manager = state.immortal_service.cron_manager.lock().await;
        cron_manager.create_cron(cron).await?;
    }
    Ok(Json(()))
}

pub async fn update_cron(
    State(state): State<AppState>,
    Path(_cron_id): Path<Uuid>,
    Json(cron): Json<CronSpec>,
) -> Result<Json<()>, AnyhowError> {
    {
        let cron_manager = state.immortal_service.cron_manager.lock().await;
        cron_manager.update_cron(cron).await?;
    }
    Ok(Json(()))
}

pub async fn update_cron_status(
    State(state): State<AppState>,
    Path(cron_id): Path<Uuid>,
    Json(status): Json<CronStatus>,
) -> Result<Json<()>, AnyhowError> {
    {
        let cron_manager = state.immortal_service.cron_manager.lock().await;
        cron_manager.update_cron_status(cron_id, status).await?;
    }
    Ok(Json(()))
}

pub async fn delete_cron(
    State(state): State<AppState>,
    Path(cron_id): Path<Uuid>,
) -> Result<Json<()>, AnyhowError> {
    {
        let cron_manager = state.immortal_service.cron_manager.lock().await;
        cron_manager.delete_cron(cron_id).await?;
    }
    Ok(Json(()))
}
