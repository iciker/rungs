use std::sync::Arc;

use anyhow::Result;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

#[tokio::main]
async fn main() -> Result<()> {
    // 必须先加载 .env，RUST_LOG 才能被 tracing_subscriber 正确读取
    dotenvy::dotenv().ok();

    // 文件日志：每天滚动，写入 ./logs/app.log
    let file_appender = tracing_appender::rolling::daily("./logs", "app.log");
    let (non_blocking, _guard) = tracing_appender::non_blocking(file_appender);

    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    tracing_subscriber::registry()
        // 控制台：带颜色
        .with(tracing_subscriber::fmt::layer().with_ansi(true))
        // 文件：无 ANSI 转义码
        .with(
            tracing_subscriber::fmt::layer()
                .with_ansi(false)
                .with_writer(non_blocking),
        )
        .with(env_filter)
        .init();

    let cfg = config::load()?;

    // JWT_SECRET 最短 32 字节：HMAC-SHA256 密钥短于签名输出（32 字节）时安全强度下降
    anyhow::ensure!(
        cfg.jwt_secret.len() >= 32,
        "JWT_SECRET 长度须 >= 32 字符（当前 {} 字符），请在 .env 中更新",
        cfg.jwt_secret.len()
    );

    tracing::info!(port = cfg.port, bind_addr = %cfg.bind_addr, "配置加载成功");

    let pool = db::connect(&cfg.database_url).await?;
    tracing::info!("数据库连接成功");

    // 单运营者不变式：全进程共用一套 Binance 密钥，引擎按 symbol（而非 user_id）调度，
    // 因此任何第二个账号的网格都会用运营者的资金成交。启动即拒绝这种配置。
    let owners = db::models::user_grid::distinct_owner_count(&pool).await?;
    anyhow::ensure!(
        owners <= 1,
        "检测到 {owners} 个不同用户持有网格，但本系统共用单套 Binance 密钥。\
         请清理多余账号的网格后再启动，否则他人的网格会动用你的资金。"
    );

    let binance = std::sync::Arc::new(binance_client::BinanceClient::new(
        &cfg.binance_apikey,
        &cfg.binance_secret,
        "https://api.binance.com",
    ));

    let engine = std::sync::Arc::new(grid_engine::engine::GridEngine::new(
        binance.clone(),
        pool.clone(),
    ));

    // 并发运行网格引擎与 Web API，任意一个退出或收到 Ctrl+C 则结束
    tokio::select! {
        res = web_api::run(pool, cfg.jwt_secret, &cfg.bind_addr, cfg.port, binance.clone(), engine.clone()) => {
            match res {
                Ok(()) => anyhow::bail!("Web API 意外停止"),
                Err(e) => Err(e.context("Web API 异常退出")),
            }
        }
        res = Arc::clone(&engine).run() => {
            match res {
                Ok(()) => anyhow::bail!("网格引擎意外停止"),
                Err(e) => Err(e.context("网格引擎异常退出")),
            }
        }
        _ = tokio::signal::ctrl_c() => {
            tracing::info!("收到 Ctrl+C，优雅退出");
            Ok(())
        }
    }
}
