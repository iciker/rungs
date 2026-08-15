pub mod error;
pub mod types;

pub use error::BinanceError;

/// 撤单后等待 Binance 将资金归还到 free 余额的保守超时（通常 <1s）。
///
/// Binance 不保证 `cancel_all_orders` 返回时 `free` 余额已更新，
/// 保守等待此时长可避免余额读取竞态导致建仓失败（-2010）。
pub const CANCEL_ORDERS_SETTLE_SECS: u64 = 2;

/// 签名请求允许的时间偏差窗口（毫秒）。Binance 上限 60000。
const RECV_WINDOW_MS: u64 = 5000;

use error::BinanceApiError;
use types::*;

use hmac::{Hmac, Mac};
use serde::de::DeserializeOwned;
use sha2::Sha256;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::Semaphore;

type HmacSha256 = Hmac<Sha256>;

// ── 客户端结构 ────────────────────────────────────────────────────────────────

pub struct BinanceClient {
    http: reqwest::Client,
    api_key: String,
    secret: String,
    base_url: String,
    /// 并发请求数上限（Binance 限制 1200 weight/min）
    rate_limiter: Semaphore,
    /// 收到 429/418 后的自我节流截止时刻（epoch 毫秒）。
    /// 在此之前发出的请求会先等待，避免把限频升级成更长的 IP 封禁。
    blocked_until_ms: AtomicI64,
    /// 本地时钟与交易所时钟的毫秒偏差，由 -1021 触发自动校准。
    time_offset_ms: AtomicI64,
}

impl BinanceClient {
    pub fn new(
        api_key: impl Into<String>,
        secret: impl Into<String>,
        base_url: impl Into<String>,
    ) -> Self {
        Self {
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(10))
                .build()
                .expect("HTTP client 构建失败"),
            api_key: api_key.into(),
            secret: secret.into(),
            base_url: base_url.into(),
            rate_limiter: Semaphore::new(10),
            blocked_until_ms: AtomicI64::new(0),
            time_offset_ms: AtomicI64::new(0),
        }
    }

    /// 本地 epoch 毫秒（不含偏移校正）
    fn local_ms() -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("系统时钟在 UNIX_EPOCH 之前")
            .as_millis() as i64
    }

    /// 记录一次限频，设定自我节流截止时刻
    fn block_until(&self, retry_after_ms: u64) {
        let until = Self::local_ms() + retry_after_ms as i64;
        self.blocked_until_ms.fetch_max(until, Ordering::Relaxed);
    }

    /// 若仍在限频窗口内则等待其结束。所有出网请求都要先过这一关。
    async fn await_throttle(&self) {
        let until = self.blocked_until_ms.load(Ordering::Relaxed);
        let wait = until - Self::local_ms();
        if wait > 0 {
            tracing::warn!(wait_ms = wait, "处于限频节流窗口内，等待后再发请求");
            tokio::time::sleep(std::time::Duration::from_millis(wait as u64)).await;
        }
    }

    /// 用交易所时间校准本地时钟偏差（应对 -1021）。
    ///
    /// Binance 要求 `timestamp` 落在服务器时间的 recvWindow 内。宿主机时钟漂移超过
    /// 1 秒就会让所有签名请求被拒，而现象只是每格一条普通告警——交易实际已全面停摆。
    async fn sync_time_offset(&self) -> Result<i64, BinanceError> {
        #[derive(serde::Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct ServerTime {
            server_time: i64,
        }
        let resp = self
            .http
            .get(format!("{}/api/v3/time", self.base_url))
            .send()
            .await?;
        let t: ServerTime = self.parse_response(resp).await?;
        let offset = t.server_time - Self::local_ms();
        self.time_offset_ms.store(offset, Ordering::Relaxed);
        tracing::warn!(offset_ms = offset, "已按交易所时间校准本地时钟偏差");
        Ok(offset)
    }

    // ── 签名 ────────────────────────────────────────────────────────────────

    /// 对查询字符串做 HMAC-SHA256 签名，返回十六进制摘要。
    ///
    /// Binance 要求所有私有接口携带 `signature` 参数。密钥不会出现在返回值中。
    pub fn sign(&self, query: &str) -> String {
        let mut mac =
            HmacSha256::new_from_slice(self.secret.as_bytes()).expect("HMAC 接受任意长度 key");
        mac.update(query.as_bytes());
        hex::encode(mac.finalize().into_bytes())
    }

    fn timestamp(&self) -> i64 {
        Self::local_ms() + self.time_offset_ms.load(Ordering::Relaxed)
    }

    // ── 内部 HTTP 帮助方法 ──────────────────────────────────────────────────

    /// timestamp 追加 + 查询字符串构建 + HMAC 签名（三个 signed_* 方法共用）
    fn build_signed_query(&self, params: &mut Vec<(&str, String)>) -> (String, String) {
        // recvWindow：显式声明可容忍的时间偏差窗口，不依赖交易所默认值
        params.push(("recvWindow", RECV_WINDOW_MS.to_string()));
        params.push(("timestamp", self.timestamp().to_string()));
        let qs = params
            .iter()
            .map(|(k, v)| format!("{}={}", k, v))
            .collect::<Vec<_>>()
            .join("&");
        let sig = self.sign(&qs);
        (qs, sig)
    }

    /// 统一处理私有接口的签名、限流和 -1021 校时重试。
    async fn signed_req<T: DeserializeOwned>(
        &self,
        method: reqwest::Method,
        path: &str,
        params: &mut Vec<(&str, String)>,
    ) -> Result<T, BinanceError> {
        // 首次尝试；-1021（时间戳超出 recvWindow）时校准时钟后重试一次
        match self.signed_req_once(method.clone(), path, params).await {
            Err(BinanceError::Api { code: -1021, .. }) => {
                self.sync_time_offset().await?;
                // build_signed_query 每次都会追加 recvWindow/timestamp，重试前清掉旧的
                params.retain(|(k, _)| *k != "recvWindow" && *k != "timestamp");
                self.signed_req_once(method, path, params).await
            }
            other => other,
        }
    }

    async fn signed_req_once<T: DeserializeOwned>(
        &self,
        method: reqwest::Method,
        path: &str,
        params: &mut Vec<(&str, String)>,
    ) -> Result<T, BinanceError> {
        let (qs, sig) = self.build_signed_query(params);
        self.await_throttle().await;
        let _permit = self
            .rate_limiter
            .acquire()
            .await
            .expect("semaphore 不会关闭");
        let is_post = method == reqwest::Method::POST;
        let request = self
            .http
            .request(method, format!("{}{}", self.base_url, path))
            .header("X-MBX-APIKEY", &self.api_key);
        let request = if is_post {
            request
                .header("Content-Type", "application/x-www-form-urlencoded")
                .body(format!("{qs}&signature={sig}"))
        } else {
            request
                .query(params.as_slice())
                .query(&[("signature", &sig)])
        };
        let resp = request.send().await?;
        self.parse_response(resp).await
    }

    /// 统一处理公开 GET 接口的限流、并发控制和响应解析。
    async fn unsigned_get<T, Q>(&self, path: &str, query: &Q) -> Result<T, BinanceError>
    where
        T: DeserializeOwned,
        Q: serde::Serialize + ?Sized,
    {
        self.await_throttle().await;
        let _permit = self
            .rate_limiter
            .acquire()
            .await
            .expect("semaphore 不会关闭");
        let resp = self
            .http
            .get(format!("{}{}", self.base_url, path))
            .query(query)
            .send()
            .await?;
        self.parse_response(resp).await
    }

    async fn parse_response<T: DeserializeOwned>(
        &self,
        resp: reqwest::Response,
    ) -> Result<T, BinanceError> {
        let status = resp.status();
        // 429 = 触发限频，418 = 已被封禁 IP。两者都必须让客户端自己停手，
        // 否则 3 秒一轮的轮询会继续砸过去，把封禁时长一路升级。
        if status.as_u16() == 429 || status.as_u16() == 418 {
            // Retry-After 的单位是**秒**。缺失时退回 1 秒，而不是 1000
            //（旧代码 unwrap_or(1000) 后又 ×1000，等于报出 1000 秒的等待）。
            let retry_after_ms = resp
                .headers()
                .get("Retry-After")
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.parse::<u64>().ok())
                .map(|secs| secs * 1000)
                .unwrap_or(1000);

            self.block_until(retry_after_ms);
            tracing::warn!(
                status = status.as_u16(),
                retry_after_ms,
                "触发交易所限频，客户端进入自我节流"
            );
            return Err(BinanceError::RateLimit { retry_after_ms });
        }
        let text = resp.text().await?;
        if !status.is_success() {
            if let Ok(api_err) = serde_json::from_str::<BinanceApiError>(&text) {
                return Err(BinanceError::Api {
                    code: api_err.code,
                    msg: api_err.msg,
                });
            }
            return Err(BinanceError::Parse(text));
        }
        serde_json::from_str(&text).map_err(|e| BinanceError::Parse(e.to_string()))
    }

    // ── 现货业务方法（/api/v3/...）──────────────────────────────────────────

    /// 挂现货限价单（GTC，maker 0 手续费）
    pub async fn place_order(&self, req: PlaceOrderRequest) -> Result<OrderResponse, BinanceError> {
        let mut params: Vec<(&str, String)> = vec![
            ("symbol", req.symbol),
            ("side", req.side.to_string()),
            ("type", "LIMIT".into()),
            ("quantity", req.quantity.to_string()),
            ("price", req.price.to_string()),
            ("timeInForce", "GTC".into()),
        ];
        // 幂等键：同键重复提交会被交易所拒绝，使超时重试不会产生第二笔订单
        if let Some(coid) = req.client_order_id {
            params.push(("newClientOrderId", coid));
        }
        self.signed_req(reqwest::Method::POST, "/api/v3/order", &mut params)
            .await
    }

    /// 查询当前所有挂单（指定 symbol）
    pub async fn get_open_orders(&self, symbol: &str) -> Result<Vec<OpenOrder>, BinanceError> {
        self.signed_req(
            reqwest::Method::GET,
            "/api/v3/openOrders",
            &mut vec![("symbol", symbol.to_string())],
        )
        .await
    }

    /// 查询单笔订单状态
    pub async fn query_order(
        &self,
        symbol: &str,
        order_id: i64,
    ) -> Result<OpenOrder, BinanceError> {
        self.signed_req(
            reqwest::Method::GET,
            "/api/v3/order",
            &mut vec![
                ("symbol", symbol.to_string()),
                ("orderId", order_id.to_string()),
            ],
        )
        .await
    }

    /// 按客户端幂等键查询订单，用于恢复“交易所已接受、HTTP 响应丢失”的模糊结果。
    pub async fn query_order_by_client_id(
        &self,
        symbol: &str,
        client_order_id: &str,
    ) -> Result<OpenOrder, BinanceError> {
        self.signed_req(
            reqwest::Method::GET,
            "/api/v3/order",
            &mut vec![
                ("symbol", symbol.to_string()),
                ("origClientOrderId", client_order_id.to_string()),
            ],
        )
        .await
    }

    /// 撤销 symbol 下所有挂单
    /// 撤销 symbol 下所有挂单。
    ///
    /// **本就没有挂单时 Binance 返回 -2011「Unknown order sent」，这里视为成功。**
    /// 批量撤单的语义是「让该 symbol 的挂单归零」，没有单子时这个目标已经达成。
    /// 调用方（建仓 / 暂停 / 重建）都只关心「撤完之后没有活单」，把它当失败会让
    /// 一个全新账户连第一次建仓都做不了——这正是它最常被调用的场景。
    ///
    /// 其余错误一律上报：调用方随后会清空 order_id，撤单若真失败，那些真实挂单
    /// 会永久失去追踪线索。
    pub async fn cancel_all_orders(&self, symbol: &str) -> Result<(), BinanceError> {
        match self
            .signed_req::<serde_json::Value>(
                reqwest::Method::DELETE,
                "/api/v3/openOrders",
                &mut vec![("symbol", symbol.to_string())],
            )
            .await
        {
            Ok(_) => Ok(()),
            Err(BinanceError::Api { code: -2011, .. }) => {
                tracing::info!(symbol = %symbol, "该 symbol 本就没有挂单，撤单视为已完成");
                Ok(())
            }
            Err(e) => Err(e),
        }
    }

    /// 撤销单笔订单（按 order_id）
    pub async fn cancel_order(&self, symbol: &str, order_id: i64) -> Result<(), BinanceError> {
        let _: serde_json::Value = self
            .signed_req(
                reqwest::Method::DELETE,
                "/api/v3/order",
                &mut vec![
                    ("symbol", symbol.to_string()),
                    ("orderId", order_id.to_string()),
                ],
            )
            .await?;
        Ok(())
    }

    /// 获取 K 线历史数据（公开接口，无需签名，权重 2）
    ///
    /// - `interval`：`"1h"` / `"4h"` / `"1d"` 等
    /// - `limit`：返回条数上限（Binance 最大 1000）
    ///
    /// 用于计算历史中位数，作为网格 p_floor 的稳定锚点。
    ///
    /// 返回每根 K 线的**收盘价**：Binance 每行是 12 元素异构数组，close 在索引 4。
    pub async fn get_klines(
        &self,
        symbol: &str,
        interval: &str,
        limit: u32,
    ) -> Result<Vec<rust_decimal::Decimal>, BinanceError> {
        let rows: Vec<Vec<serde_json::Value>> = self
            .unsigned_get(
                "/api/v3/klines",
                &[
                    ("symbol", symbol),
                    ("interval", interval),
                    ("limit", limit.to_string().as_str()),
                ],
            )
            .await?;
        rows.iter()
            .map(|row| {
                row.get(4)
                    .and_then(|v| v.as_str())
                    .and_then(|s| s.parse().ok())
                    .ok_or_else(|| BinanceError::Parse("K线 close 字段缺失或非法".into()))
            })
            .collect()
    }

    /// 查询单个交易对的下单约束（公开接口，无需签名）。
    ///
    /// 调用方应缓存结果：交易规则极少变动，而每轮下单都取一次是白白的权重开销。
    pub async fn get_symbol_filters(&self, symbol: &str) -> Result<SymbolFilters, BinanceError> {
        let info: ExchangeInfo = self
            .unsigned_get("/api/v3/exchangeInfo", &[("symbol", symbol)])
            .await?;

        let s = info
            .symbols
            .into_iter()
            .find(|s| s.symbol.eq_ignore_ascii_case(symbol))
            .ok_or_else(|| BinanceError::Parse(format!("exchangeInfo 未返回 {symbol}")))?;

        // 按 filterType 取值；字段缺失时回落到"不约束"的中性值，绝不臆造限制
        let pick = |ty: &str, key: &str| -> Option<rust_decimal::Decimal> {
            s.filters
                .iter()
                .find(|f| f.get("filterType").and_then(|v| v.as_str()) == Some(ty))
                .and_then(|f| f.get(key))
                .and_then(|v| v.as_str())
                .and_then(|v| v.parse().ok())
        };

        Ok(SymbolFilters {
            step_size: pick("LOT_SIZE", "stepSize").unwrap_or(rust_decimal::Decimal::ZERO),
            min_qty: pick("LOT_SIZE", "minQty").unwrap_or(rust_decimal::Decimal::ZERO),
            tick_size: pick("PRICE_FILTER", "tickSize").unwrap_or(rust_decimal::Decimal::ZERO),
            // 新版 API 用 NOTIONAL，旧版用 MIN_NOTIONAL，两者都认
            min_notional: pick("NOTIONAL", "minNotional")
                .or_else(|| pick("MIN_NOTIONAL", "minNotional"))
                .unwrap_or(rust_decimal::Decimal::ZERO),
        })
    }

    /// 查询最新成交价（公开接口，无需签名）
    ///
    /// 用于 auto-center 功能：创建网格时以当前价为中心自动定位格子。
    pub async fn get_ticker_price(
        &self,
        symbol: &str,
    ) -> Result<rust_decimal::Decimal, BinanceError> {
        let ticker: TickerPriceResponse = self
            .unsigned_get("/api/v3/ticker/price", &[("symbol", symbol)])
            .await?;
        Ok(ticker.price)
    }

    /// 查询现货账户余额（仅返回 free > 0 的资产）
    pub async fn get_spot_balances(&self) -> Result<Vec<SpotBalance>, BinanceError> {
        let info: AccountInfo = self
            .signed_req(reqwest::Method::GET, "/api/v3/account", &mut vec![])
            .await?;
        let balances = info
            .balances
            .into_iter()
            .filter(|b| {
                b.free > rust_decimal::Decimal::ZERO || b.locked > rust_decimal::Decimal::ZERO
            })
            .collect();
        Ok(balances)
    }

    /// 查询单个资产的可用余额（仅 free，不含 locked）
    ///
    /// 建仓前调用 cancel_all_orders 并等待 Binance 处理后，
    /// 使用此方法获取真实可用量，避免将其他挂单的 locked 误计入分配。
    ///
    /// 注意：底层是一次 `/api/v3/account` 全账户快照（权重 20）。
    /// **需要两个资产时请改用 [`Self::get_asset_free_pair`]**，否则等于把同一份响应买两遍。
    pub async fn get_asset_free(&self, asset: &str) -> Result<rust_decimal::Decimal, BinanceError> {
        Ok(free_of(&self.get_spot_balances().await?, asset))
    }

    /// 一次账户快照取两个资产的可用余额（权重 20，而非两次调用的 40）。
    ///
    /// 建仓/重建都要同时读 base 与 quote，是这个 API 唯一的使用形态。
    pub async fn get_asset_free_pair(
        &self,
        base: &str,
        quote: &str,
    ) -> Result<(rust_decimal::Decimal, rust_decimal::Decimal), BinanceError> {
        let balances = self.get_spot_balances().await?;
        Ok((free_of(&balances, base), free_of(&balances, quote)))
    }
}

/// 从余额列表中取某资产的 free 数量，缺失视为 0
fn free_of(balances: &[SpotBalance], asset: &str) -> rust_decimal::Decimal {
    balances
        .iter()
        .find(|b| b.asset.eq_ignore_ascii_case(asset))
        .map(|b| b.free)
        .unwrap_or_default()
}

// ── 工具函数 ──────────────────────────────────────────────────────────────────

/// 从 Binance symbol 名称解析 (base, quote)
///
/// 优先匹配较长的后缀，避免 "USDCUSDT" 误匹配 "USD"。
/// 支持：USDT / BTC / ETH / BNB / USDC / BUSD / USD
///
/// ```
/// use binance_client::split_symbol;
/// assert_eq!(split_symbol("USDCUSDT"), Some(("USDC", "USDT")));
/// assert_eq!(split_symbol("BTCUSDT"),  Some(("BTC",  "USDT")));
/// assert_eq!(split_symbol("ETHBTC"),   Some(("ETH",  "BTC")));
/// ```
pub fn split_symbol(symbol: &str) -> Option<(&str, &str)> {
    // 由长到短排列，防止短后缀抢先匹配
    const QUOTES: &[&str] = &["USDT", "BUSD", "USDC", "BTC", "ETH", "BNB", "USD"];
    for &quote in QUOTES {
        if let Some(base) = symbol.strip_suffix(quote) {
            if !base.is_empty() {
                return Some((base, quote));
            }
        }
    }
    None
}
