use crate::{error::AppError, ImmortalService};
use axum::{
    extract::{FromRef, FromRequestParts},
    http::request::Parts,
};
use bb8_redis::{bb8::Pool, RedisConnectionManager};
use std::{fmt::Debug, sync::Arc};

#[derive(Clone, Debug)]
pub struct JwtPublicBytes(pub Vec<u8>);

#[derive(Clone, Debug, FromRef)]
pub struct AppState {
    pub pub_key: JwtPublicBytes,
    pub immortal_service: Arc<ImmortalService>,
    pub redis: Pool<RedisConnectionManager>,
    pub without_validation_arguments: (),
}
impl<S> FromRequestParts<S> for AppState
where
    Self: FromRef<S>, // <---- added this line
    S: Send + Sync + Debug,
{
    type Rejection = AppError;

    async fn from_request_parts(_parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        Ok(Self::from_ref(state)) // <---- added this line
    }
}
