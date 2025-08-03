use axum::Router;

use crate::state::AppState;

pub mod v1;
pub fn router() -> Router<AppState> {
    Router::new().nest("/v1", v1::router())
}
