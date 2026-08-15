//! 认证 handler：注册 / 登录

use axum::{extract::State, Json};
use serde::{Deserialize, Serialize};

use crate::{auth, error::AppError, AppState};
use db::models::user;

/// bcrypt 是 CPU 密集型操作（~200-400ms），必须在 blocking 线程上运行
async fn bcrypt_blocking<F, T>(f: F) -> Result<T, AppError>
where
    F: FnOnce() -> Result<T, bcrypt::BcryptError> + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| AppError::Internal(anyhow::anyhow!(e)))?
        .map_err(|e| AppError::Internal(e.into()))
}

// ── 注册 ──────────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct RegisterRequest {
    pub username: String,
    pub password: String,
    pub email: String,
}

/// POST /api/auth/register — **仅用于首次 bootstrap**。
///
/// 本系统全程共用运营者的一套 Binance 密钥（见 crates/app/src/main.rs），
/// 任何账号都能对该账户下单、撤单、读取余额。因此注册只在 users 表为空时开放，
/// 建立唯一运营账号后即永久关闭；此后新增账号须由运维直接操作数据库。
pub async fn register(
    State(state): State<AppState>,
    Json(req): Json<RegisterRequest>,
) -> Result<Json<serde_json::Value>, AppError> {
    // bootstrap 闸门：已存在任何账号即拒绝，防止陌生人自助开户后操纵运营者资金
    if user::count(&state.db).await? > 0 {
        tracing::warn!("注册接口已关闭（账号已存在），拒绝请求");
        return Err(AppError::Forbidden(
            "注册已关闭：本系统为单运营者设计，账号已初始化".into(),
        ));
    }

    // 输入长度限制：bcrypt 超 72 字节静默截断，username/email 防止数据库溢出
    if req.username.is_empty() || req.username.len() > 64 {
        return Err(AppError::BadRequest("username 长度须在 1-64 之间".into()));
    }
    if req.password.len() < 8 || req.password.len() > 72 {
        return Err(AppError::BadRequest("password 长度须在 8-72 之间".into()));
    }
    if req.email.is_empty() || req.email.len() > 255 {
        return Err(AppError::BadRequest("email 长度须在 1-255 之间".into()));
    }

    if user::find_by_username(&state.db, &req.username)
        .await?
        .is_some()
    {
        return Err(AppError::UsernameConflict);
    }

    let hash = bcrypt_blocking(move || bcrypt::hash(&req.password, bcrypt::DEFAULT_COST)).await?;

    let Some(id) = user::create_first(&state.db, &req.username, &hash, &req.email).await? else {
        return Err(AppError::Forbidden(
            "注册已关闭：本系统为单运营者设计，账号已初始化".into(),
        ));
    };
    Ok(Json(serde_json::json!({ "id": id })))
}

// ── 登录 ──────────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct LoginRequest {
    pub username: String,
    pub password: String,
}

#[derive(Debug, Serialize)]
pub struct LoginResponse {
    pub access_token: String,
    pub username: String,
}

/// 用户名不存在时用来顶替的固定 bcrypt 哈希（明文为随机串，永不匹配）。
/// 作用是让"用户不存在"与"密码错误"两条路径耗时一致，避免通过响应时间枚举用户名。
const DUMMY_HASH: &str = "$2b$12$C6UzMDM.H6dfI/f/IKcEe.7Cn0Vv2S2xVQZBxHnQNvnBiHqTQFqIS";

pub async fn login(
    State(state): State<AppState>,
    Json(req): Json<LoginRequest>,
) -> Result<Json<LoginResponse>, AppError> {
    let u = user::find_by_username(&state.db, &req.username).await?;

    // 无论用户是否存在都执行一次 bcrypt，保证两条路径耗时相同
    let hash = u
        .as_ref()
        .map(|u| u.password.clone())
        .unwrap_or_else(|| DUMMY_HASH.to_string());
    let password = req.password;
    let verified = bcrypt_blocking(move || bcrypt::verify(&password, &hash)).await?;

    // 只有"用户存在"且"哈希匹配"才放行；缺一即报同一个错，不区分原因
    let Some(u) = u.filter(|_| verified) else {
        return Err(AppError::InvalidCredentials);
    };

    let token = auth::sign(u.id, &state.jwt_secret).map_err(AppError::Internal)?;

    Ok(Json(LoginResponse {
        access_token: token,
        username: u.username,
    }))
}
