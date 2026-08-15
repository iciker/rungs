use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use sqlx::PgPool;

#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct TradeHistory {
    pub id: i32,
    pub user_id: i32,
    pub symbol: String,
    pub order_id: Option<String>,
    pub amount: Decimal,
    pub buy_price: Decimal,
    pub sell_price: Decimal,
    pub profit: Option<Decimal>,
    pub source: Option<String>,
    pub source_id: Option<i32>,
    pub filled_at: Option<DateTime<Utc>>,
}

#[derive(Debug)]
pub struct NewTradeHistory<'a> {
    pub user_id: i32,
    pub symbol: &'a str,
    pub order_id: &'a str,
    pub amount: Decimal,
    pub buy_price: Decimal,
    pub sell_price: Decimal,
    pub source: &'a str,
    pub source_id: i32,
}

/// 在**同一个事务**内把网格标记为 closed 并写入成交记录。
///
/// 两步必须原子：若先把网格设为 closed、随后写 trade_history 失败，
/// 这笔已实现的利润就永久丢了（网格已是 closed，下一轮不会再检测这次成交）。
/// 合成一个事务后，要么两者都落地，要么网格仍是 open、下一轮重新识别为成交。
///
/// `trade` 为 `None` 时只做状态流转（买单成交尚未实现利润，不记账）。
/// 返回 false 表示乐观锁冲突，调用方应跳过。
pub async fn close_grid_with_trade(
    pool: &PgPool,
    grid_id: i32,
    version: i64,
    trade: Option<NewTradeHistory<'_>>,
) -> sqlx::Result<bool> {
    close_grid_with_trade_amount(pool, grid_id, version, trade, None).await
}

/// 与 [`close_grid_with_trade`] 相同，但允许部分成交时把网格金额收敛到实际成交额。
pub async fn close_grid_with_trade_amount(
    pool: &PgPool,
    grid_id: i32,
    version: i64,
    trade: Option<NewTradeHistory<'_>>,
    settled_amount: Option<Decimal>,
) -> sqlx::Result<bool> {
    let mut tx = pool.begin().await?;

    let result = sqlx::query!(
        r#"UPDATE user_grid
           SET status = 'closed', amount = COALESCE($3, amount),
               version = version + 1, updated_at = NOW()
           WHERE id = $1 AND version = $2 AND status = 'open'"#,
        grid_id,
        version,
        settled_amount,
    )
    .execute(&mut *tx)
    .await?;

    if result.rows_affected() != 1 {
        tx.rollback().await?;
        return Ok(false);
    }

    if let Some(new) = trade {
        let profit = (new.sell_price - new.buy_price) * new.amount;
        sqlx::query!(
            r#"INSERT INTO trade_history
                   (user_id, symbol, order_id, amount, buy_price, sell_price,
                    profit, source, source_id, filled_at)
               VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, NOW())"#,
            new.user_id,
            new.symbol,
            new.order_id,
            new.amount,
            new.buy_price,
            new.sell_price,
            profit,
            new.source,
            new.source_id
        )
        .execute(&mut *tx)
        .await?;
    }

    tx.commit().await?;
    Ok(true)
}

#[derive(Debug, serde::Serialize, Default)]
pub struct ProfitStats {
    pub total_profit: rust_decimal::Decimal,
    pub trade_count: i64,
}

pub async fn get_profit_stats(pool: &PgPool, user_id: i32) -> sqlx::Result<ProfitStats> {
    let row = sqlx::query!(
        r#"SELECT COALESCE(SUM(profit), 0) AS "total_profit!", COUNT(*) AS "trade_count!"
           FROM trade_history WHERE user_id = $1"#,
        user_id
    )
    .fetch_one(pool)
    .await?;

    Ok(ProfitStats {
        total_profit: row.total_profit,
        trade_count: row.trade_count,
    })
}

pub async fn find_by_user(
    pool: &PgPool,
    user_id: i32,
    limit: i64,
) -> sqlx::Result<Vec<TradeHistory>> {
    sqlx::query_as!(
        TradeHistory,
        r#"SELECT id, user_id, symbol, order_id, amount, buy_price, sell_price,
                  profit, source, source_id, filled_at
           FROM trade_history WHERE user_id = $1
           ORDER BY filled_at DESC NULLS LAST
           LIMIT $2"#,
        user_id,
        limit
    )
    .fetch_all(pool)
    .await
}
