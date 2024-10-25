use axum::Router;

use crate::ImmortalService;

pub mod v1;
pub fn router() -> Router<ImmortalService> {
    Router::new().nest("/v1", v1::router())
}
