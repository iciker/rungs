use std::fmt;

use rust_decimal::Decimal;
use serde::Deserialize;

// ── 订单侧 ───────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub enum OrderSide {
    Buy,
    Sell,
}

impl fmt::Display for OrderSide {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            OrderSide::Buy => "BUY",
            OrderSide::Sell => "SELL",
        })
    }
}

// ── 现货挂单请求（/api/v3/order）────────────────────────────────────────────
//
// 本项目只挂 GTC 限价单，timeInForce 由 place_order 无条件附加。

#[derive(Debug, Clone)]
pub struct PlaceOrderRequest {
    pub symbol: String,
    pub side: OrderSide,
    pub quantity: Decimal,
    pub price: Decimal,
    /// 幂等键（Binance `newClientOrderId`）。同一键重复提交会被交易所拒绝，
    /// 使"请求超时但实际已下单"的重试不会产生第二笔真实订单。
    pub client_order_id: Option<String>,
}

// ── 挂单响应 ──────────────────────────────────────────────────────────────────
//
// 以下响应结构只声明代码实际读取的字段，serde 默认忽略其余 JSON 键。

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OrderResponse {
    pub order_id: i64,
}

// ── 开放订单（GET /api/v3/openOrders）────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OpenOrder {
    pub order_id: i64,
    pub status: String,
    #[serde(default, with = "rust_decimal::serde::str")]
    pub executed_qty: Decimal,
    #[serde(default, with = "rust_decimal::serde::str")]
    pub cummulative_quote_qty: Decimal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrderStatus {
    New,
    PartiallyFilled,
    Filled,
    Canceled,
    PendingCancel,
    Rejected,
    Expired,
    ExpiredInMatch,
}

impl TryFrom<&str> for OrderStatus {
    type Error = String;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        match value {
            "NEW" => Ok(Self::New),
            "PARTIALLY_FILLED" => Ok(Self::PartiallyFilled),
            "FILLED" => Ok(Self::Filled),
            "CANCELED" => Ok(Self::Canceled),
            "PENDING_CANCEL" => Ok(Self::PendingCancel),
            "REJECTED" => Ok(Self::Rejected),
            "EXPIRED" => Ok(Self::Expired),
            "EXPIRED_IN_MATCH" => Ok(Self::ExpiredInMatch),
            other => Err(format!("未知 Binance 订单状态: {other}")),
        }
    }
}

// ── 账户余额 ──────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SpotBalance {
    pub asset: String,
    #[serde(with = "rust_decimal::serde::str")]
    pub free: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub locked: Decimal,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountInfo {
    pub balances: Vec<SpotBalance>,
}

// ── 交易规则过滤器（GET /api/v3/exchangeInfo）─────────────────────────────────

/// 单个交易对的下单约束。不满足这些约束的订单会被 Binance 直接拒绝。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SymbolFilters {
    /// 数量步长（LOT_SIZE.stepSize）：数量必须是它的整数倍
    pub step_size: Decimal,
    /// 最小数量（LOT_SIZE.minQty）
    pub min_qty: Decimal,
    /// 价格步长（PRICE_FILTER.tickSize）：价格必须是它的整数倍
    pub tick_size: Decimal,
    /// 最小名义价值（NOTIONAL.minNotional）：quantity × price 的下限
    pub min_notional: Decimal,
}

impl SymbolFilters {
    /// 把数量向下取整到 step_size 的整数倍。
    ///
    /// 注意不能用 `.floor()` 代替：那等价于假定 step_size == 1，
    /// 对 BTCUSDT（step 0.00001）会把任何小于 1 BTC 的量抹成 0。
    pub fn round_quantity(&self, qty: Decimal) -> Decimal {
        if self.step_size <= Decimal::ZERO {
            return qty;
        }
        (qty / self.step_size).floor() * self.step_size
    }

    /// 把价格向下取整到 tick_size 的整数倍
    pub fn round_price(&self, price: Decimal) -> Decimal {
        if self.tick_size <= Decimal::ZERO {
            return price;
        }
        (price / self.tick_size).floor() * self.tick_size
    }

    /// 校验一笔订单是否满足全部过滤器；不满足则返回人类可读的原因
    pub fn validate(&self, qty: Decimal, price: Decimal) -> Result<(), String> {
        if qty < self.min_qty {
            return Err(format!("数量 {qty} 低于 minQty {}", self.min_qty));
        }
        if qty * price < self.min_notional {
            return Err(format!(
                "名义价值 {} 低于 minNotional {}",
                qty * price,
                self.min_notional
            ));
        }
        Ok(())
    }
}

/// exchangeInfo 的解析中间结构（只取需要的字段）
#[derive(Debug, Deserialize)]
pub(crate) struct ExchangeInfo {
    pub symbols: Vec<SymbolInfo>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SymbolInfo {
    pub symbol: String,
    pub filters: Vec<serde_json::Value>,
}

// ── 最新成交价（GET /api/v3/ticker/price）────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct TickerPriceResponse {
    #[serde(with = "rust_decimal::serde::str")]
    pub price: Decimal,
}
