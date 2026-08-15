//! axum 共享状态

use std::sync::Arc;

use binance_client::BinanceClient;
use grid_engine::engine::GridEngine;
use sqlx::PgPool;

#[derive(Clone)]
pub struct AppState {
    pub db: PgPool,
    pub jwt_secret: String,
    pub binance: Arc<BinanceClient>,
    /// 与后台引擎共享同一实例，resume 时可直接触发立即挂单
    pub engine: Arc<GridEngine>,
}
