use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;

pub mod models;

/// 初始化 PostgreSQL 连接池
pub async fn connect(database_url: &str) -> anyhow::Result<PgPool> {
    let pool = PgPoolOptions::new()
        .max_connections(10)
        .connect(database_url)
        .await?;
    Ok(pool)
}
