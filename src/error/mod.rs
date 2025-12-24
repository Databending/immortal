use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use bb8_redis::bb8::RunError;
use redis::RedisError;
use std::env::VarError;
use strum::Display;
use thiserror::Error;

macro_rules! impl_from_error_for_apperror {
    ($err_type:ty) => {
        impl From<$err_type> for AppError {
            fn from(err: $err_type) -> Self {
                AppError::Error(err.into())
            }
        }
    };
}

#[macro_export]
macro_rules! impl_from_error_for_custom_error {
    ($custom_err:ty, $variant: ident, $err_type:ty) => {
        impl From<$err_type> for $custom_err {
            fn from(err: $err_type) -> Self {
                <$custom_err>::$variant(err.into())
            }
        }
    };
}

#[derive(Error, Debug, Display)]
pub enum AppError {
    InvalidUuid(#[from] uuid::Error),
    InvalidChrono(#[from] chrono::ParseError),
    InvalidBody(#[from] garde::Report),
    #[error(transparent)]
    Error(#[from] anyhow::Error),
}
impl_from_error_for_apperror!(VarError);
impl_from_error_for_apperror!(RedisError);
impl_from_error_for_apperror!(RunError<RedisError>);

// Tell axum how to convert `AppError` into a response.
// impl IntoResponse for AppError {
//     fn into_response(self) -> Response {
//         (
//             StatusCode::INTERNAL_SERVER_ERROR,
//             format!("Something went wrong")
//         )
//             .into_response()
//     }
// }

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        // How we want errors responses to be serialized

        match self {
            Self::InvalidUuid(e) => {
                // This error is caused by bad user input so don't log it
                (
                    StatusCode::BAD_REQUEST,
                    Json(simd_json::json!({
                        "type": "InvalidUuid",
                        "message": e.to_string()
                    })),
                )
                    .into_response()
            }
            Self::InvalidChrono(e) => {
                // This error is caused by bad user input so don't log it
                (
                    StatusCode::BAD_REQUEST,
                    Json(simd_json::json!({
                        "type": "InvalidChrono",
                        "message": e.to_string()
                    })),
                )
                    .into_response()
            }
            Self::InvalidBody(e) => {
                // This error is caused by bad user input so don't log it
                (
                    StatusCode::BAD_REQUEST,
                    Json(simd_json::json!({
                        "type": "InvalidBody",
                        "message": e.to_string()
                    })),
                )
                    .into_response()
            }

            Self::Error(err) => {
                let env = std::env::var("APP_ENV").unwrap_or("PROD".to_string());
                // Because `TraceLayer` wraps each request in a span that contains the request
                // method, uri, etc we don't need to include those details here
                tracing::error!(%err, "error from time_library");

                // Don't expose any details about the error to the client
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(
                        &(match env.as_str() {
                            "DEV" => simd_json::json!({
                                "message": "An error occurred".to_string(),
                                "error": Some(err.to_string()),
                                "backtrace": Some(err.backtrace().to_string()),
                            }),
                            _ => simd_json::json!({
                                "message": "Something went wrong.",
                            }),
                        }),
                    ),
                )
                    .into_response()
            }
        }
    }
}

pub struct AnyhowError(anyhow::Error);

// Tell axum how to convert `AppError` into a response.
impl IntoResponse for AnyhowError {
    fn into_response(self) -> Response {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Something went wrong: {}", self.0),
        )
            .into_response()
    }
}

impl<E> From<E> for AnyhowError
where
    E: Into<anyhow::Error>,
{
    fn from(err: E) -> Self {
        Self(err.into())
    }
}

// This enables using `?` on functions that return `Result<_, anyhow::Error>` to turn them into
// `Result<_, AppError>`. That way you don't need to do that manually.
// impl<E> From<E> for AppError
// where
//     E: Into<anyhow::Error>,
// {
//     fn from(err: E) -> Self {
//         Self::Error(err.into())
//     }
// }
