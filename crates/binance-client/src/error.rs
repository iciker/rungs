use thiserror::Error;

#[derive(Debug, Error)]
pub enum BinanceError {
    #[error("HTTP 错误: {0}")]
    Http(#[from] reqwest::Error),

    #[error("Binance API 错误 {code}: {msg}")]
    Api { code: i32, msg: String },

    #[error("速率限制 (429)，等待 {retry_after_ms}ms")]
    RateLimit { retry_after_ms: u64 },

    #[error("响应解析错误: {0}")]
    Parse(String),
}

/// Binance 错误响应体
#[derive(serde::Deserialize, Debug)]
pub struct BinanceApiError {
    pub code: i32,
    pub msg: String,
}
