//! 统一错误响应：{ message }

use axum::{http::StatusCode, response::IntoResponse, Json};
use serde_json::json;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum AppError {
    #[error("未授权")]
    Unauthorized,
    #[error("用户名或密码错误")]
    InvalidCredentials,
    #[error("用户名已存在")]
    UsernameConflict,
    #[error("禁止访问: {0}")]
    Forbidden(String),
    #[error("资源不存在")]
    NotFound,
    #[error("请求参数错误: {0}")]
    BadRequest(String),
    #[error("内部错误: {0}")]
    Internal(#[from] anyhow::Error),
    #[error("数据库错误: {0}")]
    Database(#[from] sqlx::Error),
}

impl IntoResponse for AppError {
    fn into_response(self) -> axum::response::Response {
        let (status, message) = match &self {
            AppError::Unauthorized | AppError::InvalidCredentials => {
                (StatusCode::UNAUTHORIZED, self.to_string())
            }
            AppError::UsernameConflict => (StatusCode::CONFLICT, self.to_string()),
            AppError::Forbidden(_) => (StatusCode::FORBIDDEN, self.to_string()),
            AppError::NotFound => (StatusCode::NOT_FOUND, self.to_string()),
            AppError::BadRequest(_) => (StatusCode::BAD_REQUEST, self.to_string()),
            AppError::Internal(_) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "内部服务器错误".to_string(),
            ),
            AppError::Database(_) => (StatusCode::INTERNAL_SERVER_ERROR, "数据库错误".to_string()),
        };
        if status == StatusCode::INTERNAL_SERVER_ERROR {
            tracing::error!(error = %self, "内部错误");
        }
        (status, Json(json!({ "message": message }))).into_response()
    }
}
