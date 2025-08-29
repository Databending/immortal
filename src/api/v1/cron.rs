use axum::{extract::State, response::IntoResponse};

use crate::state::AppState;

pub async fn read_cron(State(state): State<AppState>) -> impl IntoResponse {}
