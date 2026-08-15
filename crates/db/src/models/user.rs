use chrono::{DateTime, Utc};
use sqlx::PgPool;

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct User {
    pub id: i32,
    pub username: String,
    pub password: String,
    pub email: String,
    pub created_at: DateTime<Utc>,
}

pub async fn find_by_username(pool: &PgPool, username: &str) -> sqlx::Result<Option<User>> {
    sqlx::query_as!(
        User,
        "SELECT id, username, password, email, created_at FROM users WHERE username = $1",
        username
    )
    .fetch_optional(pool)
    .await
}

/// 账号总数。用于 bootstrap 注册闸门：仅当账号表为空时才允许注册。
pub async fn count(pool: &PgPool) -> sqlx::Result<i64> {
    let row = sqlx::query!(r#"SELECT COUNT(*) AS "n!" FROM users"#)
        .fetch_one(pool)
        .await?;
    Ok(row.n)
}

pub async fn create(
    pool: &PgPool,
    username: &str,
    password_hash: &str,
    email: &str,
) -> sqlx::Result<i32> {
    let row = sqlx::query!(
        "INSERT INTO users (username, password, email) VALUES ($1, $2, $3) RETURNING id",
        username,
        password_hash,
        email
    )
    .fetch_one(pool)
    .await?;
    Ok(row.id)
}

/// 原子创建全库第一个账号。
///
/// PostgreSQL 事务级 advisory lock 将所有 bootstrap 尝试串行化；锁内再次计数，
/// 因而即使多个请求在 bcrypt 前都观察到空表，也只有一个能真正插入。
pub async fn create_first(
    pool: &PgPool,
    username: &str,
    password_hash: &str,
    email: &str,
) -> sqlx::Result<Option<i32>> {
    const BOOTSTRAP_LOCK_ID: i64 = 0x7275_6e67_735f_7573;

    let mut tx = pool.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(BOOTSTRAP_LOCK_ID)
        .execute(&mut *tx)
        .await?;

    let row = sqlx::query!(r#"SELECT COUNT(*) AS "n!" FROM users"#)
        .fetch_one(&mut *tx)
        .await?;
    if row.n > 0 {
        tx.rollback().await?;
        return Ok(None);
    }

    let row = sqlx::query!(
        "INSERT INTO users (username, password, email) VALUES ($1, $2, $3) RETURNING id",
        username,
        password_hash,
        email
    )
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Some(row.id))
}
