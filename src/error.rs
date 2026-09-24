use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde_json::json;

pub type Result<T> = std::result::Result<T, AppError>;

#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("{0}")]
    Invalid(String),
    #[error("authentication required")]
    Unauthorized,
    #[error("permission denied")]
    Forbidden,
    #[error("resource not found")]
    NotFound,
    #[error("{0}")]
    Conflict(String),
    #[error("{0}")]
    Unavailable(String),
    #[error(transparent)]
    Database(#[from] sqlx::Error),
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

impl AppError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::Invalid(_) => "INVALID_ARGUMENT",
            Self::Unauthorized => "UNAUTHENTICATED",
            Self::Forbidden => "FORBIDDEN",
            Self::NotFound => "NOT_FOUND",
            Self::Conflict(_) => "VERSION_OR_IDEMPOTENCY_CONFLICT",
            Self::Unavailable(_) => "DEPENDENCY_UNAVAILABLE",
            Self::Database(_) | Self::Internal(_) => "INTERNAL_ERROR",
        }
    }
    pub fn safe_message(&self) -> String {
        match self {
            Self::Database(_) | Self::Internal(_) => "internal operation failed".into(),
            _ => self.to_string(),
        }
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let status = match self {
            Self::Invalid(_) => StatusCode::UNPROCESSABLE_ENTITY,
            Self::Unauthorized => StatusCode::UNAUTHORIZED,
            Self::Forbidden => StatusCode::FORBIDDEN,
            Self::NotFound => StatusCode::NOT_FOUND,
            Self::Conflict(_) => StatusCode::CONFLICT,
            Self::Unavailable(_) => StatusCode::SERVICE_UNAVAILABLE,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        };
        // Do not log SQL bind arguments, document content or bearer tokens.
        tracing::warn!(
            code = self.code(),
            status = status.as_u16(),
            "request failed"
        );
        (
            status,
            Json(json!({"error":{"code":self.code(),"message":self.safe_message()}})),
        )
            .into_response()
    }
}
