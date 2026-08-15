use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use sqlx::{PgPool, Postgres, Transaction};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GridSide {
    Buy,
    Sell,
}

impl GridSide {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Buy => "buy",
            Self::Sell => "sell",
        }
    }
}

impl TryFrom<&str> for GridSide {
    type Error = String;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        match value {
            "buy" => Ok(Self::Buy),
            "sell" => Ok(Self::Sell),
            other => Err(format!("未知网格方向: {other}")),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GridStatus {
    New,
    Open,
    Closed,
    Deleted,
}

impl TryFrom<&str> for GridStatus {
    type Error = String;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        match value {
            "new" => Ok(Self::New),
            "open" => Ok(Self::Open),
            "closed" => Ok(Self::Closed),
            "deleted" => Ok(Self::Deleted),
            other => Err(format!("未知网格状态: {other}")),
        }
    }
}

// ── 模型 ──────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct UserGrid {
    pub id: i32,
    pub user_id: i32,
    pub order_id: Option<String>,
    pub amount: Decimal,
    pub buy_price: Decimal,
    pub sell_price: Decimal,
    pub side: String,
    pub symbol: String,
    pub status: String,
    pub source: String,
    pub is_paused: bool,
    pub version: i64,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug)]
pub struct NewUserGrid {
    pub user_id: i32,
    pub amount: Decimal,
    pub buy_price: Decimal,
    pub sell_price: Decimal,
    pub side: String,
    pub symbol: String,
    /// 来源：'manual'（用户手动创建）或 'auto'（智能建仓）
    pub source: String,
}

#[derive(Debug)]
pub struct UpdateUserGrid {
    pub amount: Decimal,
    pub buy_price: Decimal,
    pub sell_price: Decimal,
    pub side: String,
}

// ── 查询 ──────────────────────────────────────────────────────────────────────

/// 返回 symbol 下所有非删除且未暂停的网格（供引擎状态机处理）
pub async fn find_by_symbol(pool: &PgPool, symbol: &str) -> sqlx::Result<Vec<UserGrid>> {
    sqlx::query_as!(
        UserGrid,
        r#"SELECT id, user_id, order_id, amount, buy_price, sell_price,
                  side, symbol, status, source, is_paused, version, created_at, updated_at
           FROM user_grid
           WHERE symbol = $1 AND status != 'deleted' AND is_paused = FALSE
           ORDER BY id"#,
        symbol
    )
    .fetch_all(pool)
    .await
}

/// 返回用户的所有非删除网格（用于 API 展示，包含已暂停的）
pub async fn find_by_user(pool: &PgPool, user_id: i32) -> sqlx::Result<Vec<UserGrid>> {
    sqlx::query_as!(
        UserGrid,
        r#"SELECT id, user_id, order_id, amount, buy_price, sell_price,
                  side, symbol, status, source, is_paused, version, created_at, updated_at
           FROM user_grid WHERE user_id = $1 AND status != 'deleted'
           ORDER BY id"#,
        user_id
    )
    .fetch_all(pool)
    .await
}

pub async fn find_by_id(pool: &PgPool, id: i32) -> sqlx::Result<Option<UserGrid>> {
    sqlx::query_as!(
        UserGrid,
        r#"SELECT id, user_id, order_id, amount, buy_price, sell_price,
                  side, symbol, status, source, is_paused, version, created_at, updated_at
           FROM user_grid WHERE id = $1"#,
        id
    )
    .fetch_optional(pool)
    .await
}

/// 需要引擎轮询的 symbol 集合。
///
/// 谓词必须与 `find_by_symbol` 完全一致，否则监管器会为一个"全部暂停"的 symbol
/// 持续拉起 task，而该 task 每 3 秒查回一个空结果集；同时也用不上
/// `idx_user_grid_live`（该索引的 WHERE 含 `is_paused = FALSE`）。
pub async fn get_active_symbols(pool: &PgPool) -> sqlx::Result<Vec<String>> {
    let rows = sqlx::query!(
        "SELECT DISTINCT symbol FROM user_grid
         WHERE status != 'deleted' AND is_paused = FALSE"
    )
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(|r| r.symbol).collect())
}

// ── 写入 ──────────────────────────────────────────────────────────────────────

/// 批量创建网格（一个事务，要么全部成功要么全部回滚）
pub async fn create_batch(pool: &PgPool, grids: Vec<NewUserGrid>) -> sqlx::Result<Vec<i32>> {
    let mut tx = pool.begin().await?;
    let ids = insert_batch(&mut tx, grids).await?;
    tx.commit().await?;
    Ok(ids)
}

async fn insert_batch(
    tx: &mut Transaction<'_, Postgres>,
    grids: Vec<NewUserGrid>,
) -> sqlx::Result<Vec<i32>> {
    let mut ids = Vec::with_capacity(grids.len());
    for grid in grids {
        let row = sqlx::query!(
            r#"INSERT INTO user_grid (user_id, amount, buy_price, sell_price, side, symbol, status, source)
               VALUES ($1, $2, $3, $4, $5, $6, 'new', $7)
               RETURNING id"#,
            grid.user_id,
            grid.amount,
            grid.buy_price,
            grid.sell_price,
            grid.side,
            grid.symbol,
            grid.source,
        )
        .fetch_one(&mut **tx)
        .await?;
        ids.push(row.id);
    }
    Ok(ids)
}

#[derive(Clone, Copy)]
enum ReplacementTarget<'a> {
    AutoByUserAndSymbol { user_id: i32, symbol: &'a str },
    ActiveByUserAndSymbol { user_id: i32, symbol: &'a str },
    One { id: i32 },
}

impl ReplacementTarget<'_> {
    async fn soft_delete(self, tx: &mut Transaction<'_, Postgres>) -> sqlx::Result<u64> {
        let result = match self {
            Self::AutoByUserAndSymbol { user_id, symbol } => {
                sqlx::query!(
                    "UPDATE user_grid SET status = 'deleted', order_id = NULL, version = version + 1,
                                              updated_at = NOW()
                     WHERE user_id = $1 AND symbol = $2 AND source = 'auto' AND status != 'deleted'",
                    user_id,
                    symbol
                )
                .execute(&mut **tx)
                .await?
            }
            Self::ActiveByUserAndSymbol { user_id, symbol } => {
                sqlx::query!(
                    "UPDATE user_grid SET status = 'deleted', order_id = NULL, version = version + 1,
                                              updated_at = NOW()
                     WHERE user_id = $1 AND symbol = $2 AND status != 'deleted' AND is_paused = FALSE",
                    user_id,
                    symbol
                )
                .execute(&mut **tx)
                .await?
            }
            Self::One { id } => {
                sqlx::query!(
                    "UPDATE user_grid SET status = 'deleted', order_id = NULL, version = version + 1,
                                              updated_at = NOW()
                     WHERE id = $1 AND status != 'deleted'",
                    id
                )
                .execute(&mut **tx)
                .await?
            }
        };
        Ok(result.rows_affected())
    }

    const fn requires_single_match(self) -> bool {
        matches!(self, Self::One { .. })
    }
}

async fn replace_grids(
    pool: &PgPool,
    target: ReplacementTarget<'_>,
    grids: Vec<NewUserGrid>,
) -> sqlx::Result<(u64, Vec<i32>)> {
    let mut tx = pool.begin().await?;
    let deleted = target.soft_delete(&mut tx).await?;
    if target.requires_single_match() && deleted != 1 {
        tx.rollback().await?;
        return Ok((deleted, Vec::new()));
    }

    let ids = insert_batch(&mut tx, grids).await?;
    tx.commit().await?;
    Ok((deleted, ids))
}

/// 创建单个网格（复用 create_batch 的 INSERT，避免两份 SQL 各自漂移）
pub async fn create(pool: &PgPool, new: NewUserGrid) -> sqlx::Result<i32> {
    Ok(create_batch(pool, vec![new]).await?[0])
}

/// 乐观锁更新：status new → open，同时记录 order_id
/// 返回 true 表示更新成功（version 匹配），false 表示被并发抢占
pub async fn set_order_open(
    pool: &PgPool,
    id: i32,
    order_id: &str,
    version: i64,
) -> sqlx::Result<bool> {
    let result = sqlx::query!(
        r#"UPDATE user_grid
           SET status = 'open', order_id = $1, version = version + 1, updated_at = NOW()
           WHERE id = $2 AND version = $3 AND status = 'new'"#,
        order_id,
        id,
        version
    )
    .execute(pool)
    .await?;
    Ok(result.rows_affected() == 1)
}

/// 订单被撤销或过期时重置为 new，以便引擎重新挂单（乐观锁）。
///
/// 守卫缺一不可：
/// - `status = 'open'`：只有在挂单中的行才谈得上"订单没了要重挂"。
/// - `status != 'deleted'` 由上一条蕴含：否则并发的重新居中刚软删掉的行会被复活，
///   变成一条谁也不再管理、却会被引擎重新挂单的幽灵网格。
/// - `version = $2`：拿着旧快照的轮次不得覆盖新状态。
///
/// 返回 false 表示被并发抢占，调用方应跳过本轮。
pub async fn reset_to_new(pool: &PgPool, id: i32, version: i64) -> sqlx::Result<bool> {
    let result = sqlx::query!(
        r#"UPDATE user_grid
           SET status = 'new', order_id = NULL, version = version + 1, updated_at = NOW()
           WHERE id = $1 AND version = $2 AND status = 'open'"#,
        id,
        version
    )
    .execute(pool)
    .await?;
    Ok(result.rows_affected() == 1)
}

/// 全量更新（用于管理后台手动编辑）。
///
/// 一并清空 `order_id` 并递增 `version`：编辑价格/数量后，原先那笔挂单的价格已与本行不符，
/// 若继续留着 order_id，引擎会把一笔条件完全不同的旧单当成本行的挂单来对账。
/// 调用方有责任在编辑前撤销交易所挂单。
pub async fn update_full(
    pool: &PgPool,
    id: i32,
    user_id: i32,
    update: &UpdateUserGrid,
) -> sqlx::Result<bool> {
    let result = sqlx::query!(
        "UPDATE user_grid SET amount = $1, buy_price = $2, sell_price = $3, \
         side = $4, status = 'new', order_id = NULL, version = version + 1, updated_at = NOW() \
         WHERE id = $5 AND user_id = $6 AND status != 'deleted'",
        update.amount,
        update.buy_price,
        update.sell_price,
        update.side,
        id,
        user_id
    )
    .execute(pool)
    .await?;
    Ok(result.rows_affected() == 1)
}

/// 订单成交后原地翻转方向，重置为 new 等待下一轮挂单（乐观锁）
///
/// 这是网格循环的核心操作：sell 成交 → 同一行变为 buy,new；
/// buy 成交 → 同一行变为 sell,new。保持 grid_id 不变，便于追踪历史。
/// `new_amount` 必须同步改写：`amount` 的计量单位随 side 变化 ——
/// buy 格记的是**计价币**花费额，sell 格记的是**基础币**数量。
/// 沿用旧值等于把两种货币的数字当同一个用，只要价格不等于 1 就必然错量。
pub async fn flip_to_reverse(
    pool: &PgPool,
    id: i32,
    new_side: &str,
    new_amount: rust_decimal::Decimal,
    version: i64,
) -> sqlx::Result<bool> {
    let result = sqlx::query!(
        r#"UPDATE user_grid
           SET side = $1, amount = $2, status = 'new', order_id = NULL,
               version = version + 1, updated_at = NOW()
           WHERE id = $3 AND version = $4 AND status = 'closed'"#,
        new_side,
        new_amount,
        id,
        version
    )
    .execute(pool)
    .await?;
    Ok(result.rows_affected() == 1)
}

/// 原子替换某用户某交易对的自动网格。
///
/// 软删除和批量插入必须同成同败，否则插入故障会让策略永久变成零活跃网格。
pub async fn replace_auto_by_user_and_symbol(
    pool: &PgPool,
    user_id: i32,
    symbol: &str,
    grids: Vec<NewUserGrid>,
) -> sqlx::Result<(u64, Vec<i32>)> {
    replace_grids(
        pool,
        ReplacementTarget::AutoByUserAndSymbol { user_id, symbol },
        grids,
    )
    .await
}

/// 持有未删除网格的不同 user_id 数量。
///
/// 引擎按 symbol 调度且共用单套交易所密钥，故系统只允许一个网格持有者；
/// app 启动时以此为断言，防止他人的网格动用运营者的资金。
pub async fn distinct_owner_count(pool: &PgPool) -> sqlx::Result<i64> {
    let row = sqlx::query!(
        r#"SELECT COUNT(DISTINCT user_id) AS "n!" FROM user_grid WHERE status != 'deleted'"#
    )
    .fetch_one(pool)
    .await?;
    Ok(row.n)
}

/// 原子替换某用户某交易对中正在调度的网格，暂停网格保持不变。
pub async fn replace_active_by_user_and_symbol(
    pool: &PgPool,
    user_id: i32,
    symbol: &str,
    grids: Vec<NewUserGrid>,
) -> sqlx::Result<(u64, Vec<i32>)> {
    replace_grids(
        pool,
        ReplacementTarget::ActiveByUserAndSymbol { user_id, symbol },
        grids,
    )
    .await
}

/// 原子移除一个网格并在同一事务中补入替代网格。
pub async fn replace_one(
    pool: &PgPool,
    old_id: i32,
    new_grid: NewUserGrid,
) -> sqlx::Result<Option<i32>> {
    let (deleted, mut ids) =
        replace_grids(pool, ReplacementTarget::One { id: old_id }, vec![new_grid]).await?;
    if deleted != 1 {
        return Ok(None);
    }
    Ok(ids.pop())
}

/// 软删除单格。
///
/// 同时清空 `order_id` 并递增 `version`：持有旧快照的并发引擎轮次会因版本不符而放弃写入，
/// 且不会再拿着一个已删除行的 order_id 去撤单。
/// 调用方有责任在此之前撤销交易所挂单（见 handlers::user::delete_grid）。
pub async fn soft_delete(pool: &PgPool, id: i32) -> sqlx::Result<bool> {
    let result = sqlx::query!(
        "UPDATE user_grid SET status = 'deleted', order_id = NULL, version = version + 1,
                              updated_at = NOW()
         WHERE id = $1 AND status != 'deleted'",
        id
    )
    .execute(pool)
    .await?;
    Ok(result.rows_affected() == 1)
}

/// 仅重新打上 `is_paused` 标志，不触碰 `status` / `order_id`，返回受影响行数。
///
/// 专供 resume 失败回滚使用：此时引擎可能已为部分格子挂出真实订单，
/// 若像 `pause_by_user_symbol` 那样把 `order_id` 清空，这些活单会失去追踪线索
/// —— 而回滚路径并没有撤销它们。真正要撤单的场景请走 `pause_by_user_symbol`。
pub async fn mark_paused_flag_by_user_symbol(
    pool: &PgPool,
    user_id: i32,
    symbol: &str,
) -> sqlx::Result<u64> {
    let result = sqlx::query!(
        r#"UPDATE user_grid
           SET is_paused = TRUE, version = version + 1, updated_at = NOW()
           WHERE user_id = $1 AND symbol = $2 AND status != 'deleted'"#,
        user_id,
        symbol,
    )
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
}

/// 暂停用户某 symbol 下所有网格：设 `is_paused = TRUE`，`open` 状态重置为 `new`
/// 并清空 `order_id`，返回被暂停的行数。
///
/// 清空 `order_id` 等于放弃对那笔挂单的追踪，因此调用方**必须**先撤销交易所挂单。
/// 不该撤单的场景（如 resume 失败回滚）请走 `mark_paused_flag_by_user_symbol`。
pub async fn pause_by_user_symbol(pool: &PgPool, user_id: i32, symbol: &str) -> sqlx::Result<u64> {
    let result = sqlx::query!(
        r#"UPDATE user_grid
           SET is_paused = TRUE,
               status = CASE WHEN status = 'open' THEN 'new' ELSE status END,
               order_id = CASE WHEN status = 'open' THEN NULL ELSE order_id END,
               version = version + 1,
               updated_at = NOW()
           WHERE user_id = $1 AND symbol = $2 AND status != 'deleted'"#,
        user_id,
        symbol,
    )
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
}

/// 暂停单个网格：设 is_paused=TRUE，open 状态重置为 new（清空 order_id）
pub async fn pause_single(pool: &PgPool, id: i32) -> sqlx::Result<()> {
    sqlx::query!(
        r#"UPDATE user_grid
           SET is_paused = TRUE,
               status = CASE WHEN status = 'open' THEN 'new' ELSE status END,
               order_id = CASE WHEN status = 'open' THEN NULL ELSE order_id END,
               version = version + 1,
               updated_at = NOW()
           WHERE id = $1 AND status != 'deleted'"#,
        id
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// 恢复单个网格：设 is_paused=FALSE，引擎下一轮重新挂单
pub async fn resume_single(pool: &PgPool, id: i32) -> sqlx::Result<()> {
    sqlx::query!(
        "UPDATE user_grid SET is_paused = FALSE, version = version + 1, updated_at = NOW()
         WHERE id = $1 AND status != 'deleted'",
        id
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// 恢复用户某 symbol 下所有已暂停的网格：设 is_paused=FALSE
/// 引擎下一轮即恢复挂单
/// 返回被恢复的行数
pub async fn resume_by_user_symbol(pool: &PgPool, user_id: i32, symbol: &str) -> sqlx::Result<u64> {
    let result = sqlx::query!(
        r#"UPDATE user_grid
           SET is_paused = FALSE, version = version + 1, updated_at = NOW()
           WHERE user_id = $1 AND symbol = $2 AND is_paused = TRUE AND status != 'deleted'"#,
        user_id,
        symbol,
    )
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
}
