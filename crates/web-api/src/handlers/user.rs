//! 用户数据 handler（需要 JWT 认证）

use axum::{
    extract::{Path, State},
    Json,
};
use rust_decimal::Decimal;
use serde::Deserialize;
use std::collections::BTreeMap;
use tokio::time::Duration;

use crate::{error::AppError, extractors::UserId, AppState};
use binance_client::{BinanceError, CANCEL_ORDERS_SETTLE_SECS};
use db::models::{trade_history, user_grid};
use grid_engine::{allocate_grids, build_auto_grids, Allocation, AutoGridLayout};

// ── 网格列表（按 symbol 分组）────────────────────────────────────────────────

/// GET /api/user/grids
pub async fn get_grids(
    State(state): State<AppState>,
    UserId(user_id): UserId,
) -> Result<Json<serde_json::Value>, AppError> {
    let grids = user_grid::find_by_user(&state.db, user_id).await?;

    let mut groups: BTreeMap<String, Vec<_>> = BTreeMap::new();
    for g in grids {
        groups.entry(g.symbol.clone()).or_default().push(g);
    }
    let result: Vec<_> = groups
        .into_iter()
        .map(|(name, grid)| serde_json::json!({ "name": name, "grid": grid }))
        .collect();

    Ok(Json(serde_json::json!(result)))
}

// ── 共用校验 ──────────────────────────────────────────────────────────────────

/// 网格价格与数量的公共校验，create/update 两条路径共用。
///
/// `buy_price` 会作为除数参与数量换算（engine.rs place_and_open），必须严格为正，
/// 否则引擎会 Decimal 除零并杀死该 symbol 的轮询 task。两处各写一份必然漂移。
fn validate_grid_input(
    amount: Decimal,
    buy_price: Decimal,
    sell_price: Decimal,
    side: &str,
) -> Result<(), AppError> {
    if buy_price <= Decimal::ZERO || sell_price <= Decimal::ZERO {
        return Err(AppError::BadRequest(
            "buy_price 和 sell_price 必须大于 0".into(),
        ));
    }
    if buy_price >= sell_price {
        return Err(AppError::BadRequest("buy_price 必须小于 sell_price".into()));
    }
    if amount <= Decimal::ZERO {
        return Err(AppError::BadRequest("amount 必须大于 0".into()));
    }
    if !matches!(side, "buy" | "sell") {
        return Err(AppError::BadRequest("side 只能为 buy 或 sell".into()));
    }
    Ok(())
}

/// 按 id 取网格，并要求归属于 `user_id`。
///
/// 越权与不存在都返回 `NotFound`，避免用状态码区分「他人的 id」与「不存在的 id」。
async fn owned_grid(
    state: &AppState,
    id: i32,
    user_id: i32,
) -> Result<user_grid::UserGrid, AppError> {
    user_grid::find_by_id(&state.db, id)
        .await?
        .filter(|g| g.user_id == user_id)
        .ok_or(AppError::NotFound)
}

/// 已按 symbol 串行化、并在取锁后重新读取的网格快照。
///
/// guard 与快照绑在一起返回，确保调用方完成撤单和数据库写入前不会意外释放 symbol 锁。
struct LockedOwnedGrid {
    grid: user_grid::UserGrid,
    _guard: tokio::sync::OwnedMutexGuard<()>,
}

async fn locked_owned_grid(
    state: &AppState,
    id: i32,
    user_id: i32,
) -> Result<LockedOwnedGrid, AppError> {
    let grid = owned_grid(state, id, user_id).await?;
    let guard = state.engine.lock_symbol(&grid.symbol).await;
    let grid = owned_grid(state, id, user_id).await?;
    Ok(LockedOwnedGrid {
        grid,
        _guard: guard,
    })
}

/// 撤销该网格在交易所上的挂单（若有）。只有成功或交易所明确确认订单不存在时才返回；
/// 模糊失败必须阻止调用方清除 `order_id`。
async fn cancel_grid_order(state: &AppState, grid: &user_grid::UserGrid) -> Result<(), AppError> {
    if grid.status != "open" {
        return Ok(());
    }
    let Some(order_id_str) = grid.order_id.as_deref() else {
        return Ok(());
    };
    let order_id = order_id_str.parse::<i64>().map_err(|e| {
        AppError::Internal(anyhow::anyhow!(
            "grid_id={} 存储了非法 order_id={order_id_str}: {e}",
            grid.id
        ))
    })?;
    match state.binance.cancel_order(&grid.symbol, order_id).await {
        Ok(()) => Ok(()),
        Err(BinanceError::Api { code, .. }) if code == -2011 || code == -2013 => Ok(()),
        Err(e) => Err(AppError::Internal(anyhow::anyhow!(
            "grid_id={} 撤单失败，保留数据库订单追踪: {e}",
            grid.id
        ))),
    }
}

// ── 创建网格 ──────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct CreateGridRequest {
    pub symbol: String,
    #[serde(with = "rust_decimal::serde::str")]
    pub amount: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub buy_price: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub sell_price: Decimal,
    pub side: String,
}

/// POST /api/user/grids
pub async fn create_grid(
    State(state): State<AppState>,
    UserId(user_id): UserId,
    Json(req): Json<CreateGridRequest>,
) -> Result<Json<serde_json::Value>, AppError> {
    validate_grid_input(req.amount, req.buy_price, req.sell_price, &req.side)?;

    let id = user_grid::create(
        &state.db,
        user_grid::NewUserGrid {
            user_id,
            symbol: req.symbol,
            amount: req.amount,
            buy_price: req.buy_price,
            sell_price: req.sell_price,
            side: req.side,
            source: "manual".into(),
        },
    )
    .await?;
    Ok(Json(serde_json::json!({ "id": id })))
}

// ── 更新网格 ──────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct UpdateGridRequest {
    #[serde(with = "rust_decimal::serde::str")]
    pub amount: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub buy_price: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub sell_price: Decimal,
    pub side: String,
}

/// PUT /api/user/grids/:id
pub async fn update_grid(
    State(state): State<AppState>,
    UserId(user_id): UserId,
    Path(id): Path<i32>,
    Json(req): Json<UpdateGridRequest>,
) -> Result<Json<serde_json::Value>, AppError> {
    validate_grid_input(req.amount, req.buy_price, req.sell_price, &req.side)?;
    let locked = locked_owned_grid(&state, id, user_id).await?;
    cancel_grid_order(&state, &locked.grid).await?;

    let updated = user_grid::update_full(
        &state.db,
        id,
        user_id,
        &user_grid::UpdateUserGrid {
            amount: req.amount,
            buy_price: req.buy_price,
            sell_price: req.sell_price,
            side: req.side,
        },
    )
    .await?;
    if updated {
        Ok(Json(serde_json::Value::Null))
    } else {
        Err(AppError::NotFound)
    }
}

// ── 删除网格 ──────────────────────────────────────────────────────────────────

/// DELETE /api/user/grids/:id
pub async fn delete_grid(
    State(state): State<AppState>,
    UserId(user_id): UserId,
    Path(id): Path<i32>,
) -> Result<Json<serde_json::Value>, AppError> {
    let locked = locked_owned_grid(&state, id, user_id).await?;

    // 先撤掉交易所上的挂单，再删库。顺序反了会让这笔真实挂单失去追踪线索（order_id 随行消失）
    cancel_grid_order(&state, &locked.grid).await?;

    let deleted = user_grid::soft_delete(&state.db, id).await?;
    if deleted {
        Ok(Json(serde_json::Value::Null))
    } else {
        Err(AppError::NotFound)
    }
}

// ── Binance 现货余额 ───────────────────────────────────────────────────────────

/// GET /api/user/balance
/// 返回 Binance 现货账户中 free > 0 的资产列表
pub async fn get_balance(
    State(state): State<AppState>,
    UserId(_): UserId,
) -> Result<Json<serde_json::Value>, AppError> {
    let balances = state
        .binance
        .get_spot_balances()
        .await
        .map_err(|e| AppError::Internal(anyhow::anyhow!(e)))?;
    Ok(Json(serde_json::json!(balances)))
}

// ── 自动居中批量创建网格 ──────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct AutoCenterRequest {
    pub symbol: String,
    /// 格子数量上限（1-20，实际格数受余额限制）
    pub grid_count: u32,
    /// 相邻格子间的价格步长（如 "0.0001"）
    #[serde(with = "rust_decimal::serde::str")]
    pub spacing: Decimal,
}

/// POST /api/user/grids/auto-center
pub async fn auto_center_grids(
    State(state): State<AppState>,
    UserId(user_id): UserId,
    Json(req): Json<AutoCenterRequest>,
) -> Result<Json<serde_json::Value>, AppError> {
    // ── 1. 参数校验 ───────────────────────────────────────────────────────────
    if req.symbol.is_empty() {
        return Err(AppError::BadRequest("symbol 不能为空".into()));
    }
    if req.grid_count == 0 || req.grid_count > 20 {
        return Err(AppError::BadRequest("grid_count 必须在 1-20 之间".into()));
    }
    if req.spacing <= Decimal::ZERO {
        return Err(AppError::BadRequest("spacing 必须大于 0".into()));
    }

    let (base_asset, quote_asset) = binance_client::split_symbol(&req.symbol).ok_or_else(|| {
        AppError::BadRequest(format!("无法解析 symbol 的 base/quote: {}", req.symbol))
    })?;

    // 与引擎轮询串行化。下面是「撤光挂单 → 等 2 秒结算 → 批量软删（清 order_id，不校验
    // version）」，轮询若挤进这个窗口挂出新单，那笔真实订单会连线索一起被抹掉。
    let _guard = state.engine.lock_symbol(&req.symbol).await;

    // ── 2. 撤销所有挂单（释放 locked 余额）───────────────────────────────────
    state
        .binance
        .cancel_all_orders(&req.symbol)
        .await
        .map_err(|e| AppError::Internal(anyhow::anyhow!("撤单失败，中止自动居中: {e}")))?;
    // 等待 Binance 将刚撤销的挂单资金归还到 free 余额
    tokio::time::sleep(Duration::from_secs(CANCEL_ORDERS_SETTLE_SECS)).await;

    // ── 3. 并发查询余额 + 当前价 + 交易规则（三路并发，互不依赖）────────────────
    // 只取 free 不含 locked，避免将其他挂单（非本次撤销）的额度误计入分配。
    // 余额用 get_asset_free_pair：一次账户快照拿两个资产，分开查会把同一份响应买两遍。
    // 交易规则走 engine 的进程内缓存，命中后零网络开销。
    let ((base_balance, quote_balance), current_price, filters) = tokio::try_join!(
        async {
            state
                .binance
                .get_asset_free_pair(base_asset, quote_asset)
                .await
                .map_err(|e| AppError::Internal(anyhow::anyhow!(e)))
        },
        async {
            state
                .binance
                .get_ticker_price(&req.symbol)
                .await
                .map_err(|e| AppError::Internal(anyhow::anyhow!(e)))
        },
        async {
            state
                .engine
                .symbol_filters(&req.symbol)
                .await
                .map_err(AppError::Internal)
        },
    )?;

    // ── 4. 按余额比例分配格数，每边各自计算 amount ────────────────────────────
    // base 与 quote 是不同货币，必须先按 current_price 折算到同一单位；
    // 同时用交易所的 minQty / minNotional 判定 dust 与每格可行性。
    let (sell_count, buy_count, sell_amount, buy_amount) = allocate_grids(Allocation {
        base: base_balance,
        quote: quote_balance,
        price: current_price,
        grid_count: req.grid_count,
        min_qty: filters.min_qty,
        min_notional: filters.min_notional,
    })
    .ok_or_else(|| {
        AppError::BadRequest(format!(
            "账户余额不足以建仓：{base_asset}={base_balance}, {quote_asset}={quote_balance}"
        ))
    })?;

    // ── 5. ceil 到最近的 spacing 边界作为价格锚点 ─────────────────────────────────
    // ceil 确保当前价落在 buy 区（sell 格全部在锚点之上），与引擎自动重建逻辑一致
    let p_floor = (current_price / req.spacing).ceil() * req.spacing;

    tracing::info!(
        symbol = %req.symbol,
        current_price = %current_price,
        p_floor = %p_floor,
        base = %base_balance,
        quote = %quote_balance,
        sell_count,
        buy_count,
        sell_amount = %sell_amount,
        buy_amount = %buy_amount,
        "auto-center: 余额比例分配完成"
    );

    // ── 6. 生成新格子（sell 在上，buy 在下，互不重叠）────────────────────────
    let grids = build_auto_grids(
        AutoGridLayout {
            user_id,
            p_floor,
            spacing: req.spacing,
            sell_count,
            buy_count,
            sell_amount,
            buy_amount,
        },
        &req.symbol,
    );
    if grids.is_empty() {
        return Err(AppError::BadRequest(
            "当前 spacing、余额与价格会生成无效网格，请减小 spacing 或格子数量".into(),
        ));
    }
    // ── 7. 原子替换旧的智能网格（手动网格不受影响）─────────────────────────────
    let (_, ids) =
        user_grid::replace_auto_by_user_and_symbol(&state.db, user_id, &req.symbol, grids).await?;

    Ok(Json(serde_json::json!({
        "ids": ids,
        "sell_count": sell_count,
        "buy_count": buy_count,
        "current_price": current_price.to_string(),
        "sell_amount": sell_amount.to_string(),
        "buy_amount": buy_amount.to_string(),
    })))
}

// ── 暂停 / 恢复网格 ────────────────────────────────────────────────────────────

#[derive(Debug, serde::Deserialize)]
pub struct PauseResumeRequest {
    pub symbol: String,
}

/// POST /api/user/grids/pause
pub async fn pause_grids(
    State(state): State<AppState>,
    UserId(user_id): UserId,
    Json(req): Json<PauseResumeRequest>,
) -> Result<Json<serde_json::Value>, AppError> {
    if req.symbol.is_empty() {
        return Err(AppError::BadRequest("symbol 不能为空".into()));
    }

    // 与引擎轮询串行化：撤单与「清 order_id 的批量暂停」之间不得被挂上新单
    let _guard = state.engine.lock_symbol(&req.symbol).await;

    // 先撤单；模糊失败时保留 DB 记录，避免真实挂单失去 order_id 线索
    state
        .binance
        .cancel_all_orders(&req.symbol)
        .await
        .map_err(|e| AppError::Internal(anyhow::anyhow!("撤单失败，中止暂停: {e}")))?;

    let count = user_grid::pause_by_user_symbol(&state.db, user_id, &req.symbol).await?;
    tracing::info!(symbol = %req.symbol, user_id, paused = count, "网格已暂停");
    Ok(Json(serde_json::json!({ "paused": count })))
}

/// POST /api/user/grids/resume
pub async fn resume_grids(
    State(state): State<AppState>,
    UserId(user_id): UserId,
    Json(req): Json<PauseResumeRequest>,
) -> Result<Json<serde_json::Value>, AppError> {
    if req.symbol.is_empty() {
        return Err(AppError::BadRequest("symbol 不能为空".into()));
    }

    // 1. 清除暂停标志（所有 is_paused=TRUE 的网格恢复为可调度状态）
    let count = user_grid::resume_by_user_symbol(&state.db, user_id, &req.symbol).await?;
    if count == 0 {
        return Ok(Json(
            serde_json::json!({ "resumed": 0, "message": "没有找到已暂停的网格" }),
        ));
    }

    // 2. 立即触发引擎处理该 symbol（同步挂单，不等下一轮 poll）
    if let Err(e) = state.engine.process_symbol(&req.symbol).await {
        // 回滚只重新打上 is_paused 标志，**不动 status / order_id**。
        // 引擎可能已经为其中一部分格子挂出了真实订单；若像 pause 那样把 order_id 清空，
        // 这些活单会立刻失去追踪线索，而我们并没有撤销它们。
        tracing::error!(symbol = %req.symbol, error = %e, "恢复挂单失败，回滚 is_paused 标志");
        if let Err(rb_err) =
            user_grid::mark_paused_flag_by_user_symbol(&state.db, user_id, &req.symbol).await
        {
            tracing::error!(symbol = %req.symbol, rollback_error = %rb_err, "回滚暂停标志失败，数据库状态可能不一致");
        }
        return Err(AppError::Internal(anyhow::anyhow!("恢复失败：{e}")));
    }

    tracing::info!(symbol = %req.symbol, user_id, resumed = count, "网格已恢复并完成挂单");
    Ok(Json(serde_json::json!({ "resumed": count })))
}

// ── 当前价格 ──────────────────────────────────────────────────────────────────

/// GET /api/user/price/:symbol
///
/// 返回指定交易对的最新成交价（公开接口，转发自 Binance）
pub async fn get_price(
    State(state): State<AppState>,
    UserId(_): UserId,
    Path(symbol): Path<String>,
) -> Result<Json<serde_json::Value>, AppError> {
    let price = state
        .binance
        .get_ticker_price(&symbol)
        .await
        .map_err(|e| AppError::Internal(anyhow::anyhow!(e)))?;
    Ok(Json(
        serde_json::json!({ "symbol": symbol, "price": price.to_string() }),
    ))
}

// ── 单格暂停 / 恢复 ────────────────────────────────────────────────────────────

/// POST /api/user/grids/:id/toggle-pause
///
/// 切换单个网格的暂停状态：
///   - 当前运行中 → 暂停：撤销 Binance 挂单 + is_paused=TRUE, status=new
///   - 当前已暂停 → 恢复：is_paused=FALSE，引擎下一轮自动挂单
pub async fn toggle_grid_pause(
    State(state): State<AppState>,
    UserId(user_id): UserId,
    Path(id): Path<i32>,
) -> Result<Json<serde_json::Value>, AppError> {
    let locked = locked_owned_grid(&state, id, user_id).await?;
    let grid = &locked.grid;

    if grid.is_paused {
        // 恢复：清除暂停标志，引擎下一轮重新挂单
        user_grid::resume_single(&state.db, id).await?;
        tracing::info!(grid_id = id, "单格已恢复");
        Ok(Json(serde_json::json!({ "is_paused": false })))
    } else {
        // 暂停：先撤销 Binance 挂单（如有），再标记暂停
        cancel_grid_order(&state, grid).await?;
        user_grid::pause_single(&state.db, id).await?;
        tracing::info!(grid_id = id, "单格已暂停");
        Ok(Json(serde_json::json!({ "is_paused": true })))
    }
}

// ── 引擎状态查询 ──────────────────────────────────────────────────────────────

/// GET /api/user/engine-status/:symbol
///
/// 返回该 symbol 的价格范围监控状态（是否超范围、超范围持续多久）。
pub async fn get_engine_status(
    State(state): State<AppState>,
    UserId(_): UserId,
    Path(symbol): Path<String>,
) -> Result<Json<serde_json::Value>, AppError> {
    let status = state.engine.get_recenter_status(&symbol);
    Ok(Json(serde_json::json!(status)))
}

/// POST /api/user/recenter/:symbol
///
/// 立即触发指定 symbol 的全量重建（手动触发）。
/// 流程：撤单 → 查余额 → 软删旧格 → 按当前价新建。
///
/// user_id 会一路下传到 engine，重建只影响调用者自己的网格。
pub async fn trigger_recenter(
    State(state): State<AppState>,
    UserId(user_id): UserId,
    Path(symbol): Path<String>,
) -> Result<Json<serde_json::Value>, AppError> {
    state.engine.trigger_recenter_now(user_id, &symbol).await?;
    Ok(Json(
        serde_json::json!({ "symbol": symbol, "status": "recentered" }),
    ))
}

// ── 收益统计 ──────────────────────────────────────────────────────────────────

/// GET /api/user/profit-stats
pub async fn get_profit_stats(
    State(state): State<AppState>,
    UserId(user_id): UserId,
) -> Result<Json<serde_json::Value>, AppError> {
    let stats = trade_history::get_profit_stats(&state.db, user_id).await?;
    Ok(Json(serde_json::json!(stats)))
}

// ── 交易历史 ──────────────────────────────────────────────────────────────────

/// GET /api/user/trades
pub async fn get_trades(
    State(state): State<AppState>,
    UserId(user_id): UserId,
) -> Result<Json<serde_json::Value>, AppError> {
    let trades = trade_history::find_by_user(&state.db, user_id, 100).await?;
    Ok(Json(serde_json::json!(trades)))
}
