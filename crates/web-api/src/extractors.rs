//! axum 请求提取器

use async_trait::async_trait;
use axum::{extract::FromRequestParts, http::request::Parts};

use crate::{auth, error::AppError, state::AppState};

/// 从 Bearer token 中提取并验证 user_id 的 axum 提取器
///
/// 用法：在 handler 签名中加 `UserId(user_id): UserId`，
/// axum 自动验证 JWT 并注入 user_id，无需每个 handler 手动调用。
pub struct UserId(pub i32);

#[async_trait]
impl FromRequestParts<AppState> for UserId {
    type Rejection = AppError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let auth = parts
            .headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .ok_or(AppError::Unauthorized)?;

        let token = auth.strip_prefix("Bearer ").ok_or(AppError::Unauthorized)?;

        let claims = auth::verify(token, &state.jwt_secret).map_err(|_| AppError::Unauthorized)?;

        Ok(UserId(claims.sub))
    }
}
