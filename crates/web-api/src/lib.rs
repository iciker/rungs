//! axum Web API 服务
//!
//! 路由定义见 [`create_router`]。未命中 API 的请求交给 `ServeDir("public/dist")`，
//! 并以 index.html 兜底，使 Vue Router 的 history 模式刷新时不会落到后端 404。

pub mod auth;
pub mod error;
pub mod extractors;
pub mod handlers;
pub mod state;

use axum::{
    routing::{get, post, put},
    Router,
};
use sqlx::PgPool;
use tower_governor::{governor::GovernorConfigBuilder, GovernorLayer};
use tower_http::services::{ServeDir, ServeFile};

pub use state::AppState;

/// 创建 axum Router
///
/// 路由优先级：API 路由 > 静态文件（public/dist/）> SPA fallback (index.html)
/// 这样 Vue Router 的 HTML5 history 模式可以正常工作：刷新页面返回 index.html，
/// 由前端路由接管，而不是后端 404。
pub fn create_router(state: AppState) -> Router {
    // 生产环境：frontend 打包到 public/dist/
    let serve_dir = ServeDir::new("public/dist").fallback(ServeFile::new("public/dist/index.html"));

    // 认证路由限流：每个来源 IP 平均 12 秒 1 次、突发 5 次。
    // 两个理由：其一，登录没有次数上限就等于允许无限次密码爆破；
    // 其二，每次登录都要跑一次 bcrypt（~200-400ms，占用 blocking 线程池），
    // 不限流的话仅靠登录请求就能把线程池打满，拖垮整个 API。
    let auth_governor = std::sync::Arc::new(
        GovernorConfigBuilder::default()
            .per_second(12)
            .burst_size(5)
            .finish()
            .expect("限流参数为编译期常量，构建不会失败"),
    );

    let auth_routes = Router::new()
        .route("/api/auth/register", post(handlers::auth::register))
        .route("/api/auth/login", post(handlers::auth::login))
        .layer(GovernorLayer {
            config: auth_governor,
        });

    Router::new()
        // 公开路由（带限流）
        .merge(auth_routes)
        // 需要认证的路由
        .route(
            "/api/user/grids/auto-center",
            post(handlers::user::auto_center_grids),
        )
        .route("/api/user/grids/pause", post(handlers::user::pause_grids))
        .route("/api/user/grids/resume", post(handlers::user::resume_grids))
        .route(
            "/api/user/grids",
            get(handlers::user::get_grids).post(handlers::user::create_grid),
        )
        .route(
            "/api/user/grids/:id",
            put(handlers::user::update_grid).delete(handlers::user::delete_grid),
        )
        .route(
            "/api/user/grids/:id/toggle-pause",
            post(handlers::user::toggle_grid_pause),
        )
        .route("/api/user/price/:symbol", get(handlers::user::get_price))
        .route("/api/user/trades", get(handlers::user::get_trades))
        .route(
            "/api/user/profit-stats",
            get(handlers::user::get_profit_stats),
        )
        .route("/api/user/balance", get(handlers::user::get_balance))
        .route(
            "/api/user/engine-status/:symbol",
            get(handlers::user::get_engine_status),
        )
        .route(
            "/api/user/recenter/:symbol",
            post(handlers::user::trigger_recenter),
        )
        // 无 CORS 层：生产同源（ServeDir），开发走 Vite 的 /api 代理，
        // 浏览器始终只请求自身 origin。若将来前端独立部署，再按需加回。
        .with_state(state)
        // API 路由匹配后，剩余请求由静态文件服务处理（含 SPA fallback）
        .fallback_service(serve_dir)
}

/// 启动 HTTP 服务器
pub async fn run(
    pool: PgPool,
    jwt_secret: String,
    bind_addr: &str,
    port: u16,
    binance: std::sync::Arc<binance_client::BinanceClient>,
    engine: std::sync::Arc<grid_engine::engine::GridEngine>,
) -> anyhow::Result<()> {
    let state = AppState {
        db: pool,
        jwt_secret,
        binance,
        engine,
    };
    let app = create_router(state);
    let addr = format!("{bind_addr}:{port}");
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    if bind_addr != "127.0.0.1" && bind_addr != "localhost" {
        tracing::warn!(
            %addr,
            "API 已绑定到非本地地址：该接口持有交易所下单与撤单权限，请确认前置了鉴权代理"
        );
    }
    tracing::info!("Web API 监听 {addr}");
    // 必须用 with_connect_info：限流层按对端 IP 分桶，缺了 ConnectInfo 会让
    // /api/auth/* 全部返回 500。
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .await?;
    Ok(())
}
