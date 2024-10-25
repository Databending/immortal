use axum::{routing::{get, post}, Router};

use crate::ImmortalService;
pub mod run;
pub mod server;

pub fn router() -> Router<ImmortalService> {
    Router::new().route("/run", post(run::run)).route("/history", get(server::get_history)).route("/workers", get(server::get_workers))
}
