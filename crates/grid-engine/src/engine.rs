//! 网格引擎主循环
//!
//! - 每个 symbol 独立 tokio task 并发处理
//! - 每个 grid 按状态机流转：new→open→closed→new（反向）
//! - 使用乐观锁（version）防止并发写入竞争
//! - 价格持续超出网格范围超过 RANGE_DELAY_SECS 后触发滑动窗口

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// 价格超出网格范围持续多久后触发滑动窗口（5 分钟）
const RANGE_DELAY_SECS: u64 = 300;

use anyhow::Context;
use binance_client::types::{OpenOrder, OrderSide, OrderStatus, PlaceOrderRequest, SymbolFilters};
use binance_client::{split_symbol, BinanceClient, BinanceError, CANCEL_ORDERS_SETTLE_SECS};
use db::models::{trade_history, user_grid};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use sqlx::PgPool;
use tracing::{error, info, warn};

use crate::strategy::{allocate_grids, build_auto_grids, Allocation, AutoGridLayout};

/// 某个 symbol 的范围监控状态（供前端展示价格是否超出网格）
#[derive(Debug, Clone, serde::Serialize)]
pub struct ReCenterStatus {
    pub out_of_range: bool,
    pub minutes_out: u64,
}

/// 网格引擎，通过 `Arc<GridEngine>` 在任务间共享。
pub struct GridEngine {
    pub binance: Arc<BinanceClient>,
    pub db: PgPool,
    /// 每轮轮询间隔（默认 3 秒）
    pub poll_interval: Duration,
    /// 各 symbol 价格首次超出网格范围的时间戳
    /// Key: symbol, Value: 超出范围起始 Instant
    out_of_range_since: Mutex<HashMap<String, Instant>>,
    /// 每个 symbol 一把互斥锁，串行化所有会对该 symbol 下单或重建的入口。
    ///
    /// 目前有三个：引擎自身的轮询 task、HTTP 的 resume_grids（走 `process_symbol`），
    /// 以及 HTTP 的 recenter（走 `trigger_recenter_now`）。**新增入口必须在此登记并加锁。**
    ///
    /// 两类竞态都只有这把锁能挡住，乐观锁不行：
    /// - 两个调用方读到 version 相同的快照，各自挂出一笔真实订单。乐观锁只拦得住后写库的
    ///   那一方——订单已经在交易所上了。
    /// - 重建先 `cancel_all_orders` 再批量软删（该 SQL 按 user_id + symbol 更新，不校验
    ///   version），中间隔着等待资金结算的 2 秒。轮询若在这个窗口里挂出新单，那笔单会连
    ///   `order_id` 一起被抹成 NULL —— 交易所上留一笔活单，库里再无任何线索。
    symbol_locks: tokio::sync::Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    /// 各 symbol 的交易所下单约束缓存（stepSize / tickSize / minNotional）。
    /// 交易规则极少变动，每轮重取只是白费权重，故进程内缓存一次。
    filters: tokio::sync::Mutex<HashMap<String, SymbolFilters>>,
}

impl GridEngine {
    pub fn new(binance: Arc<BinanceClient>, db: PgPool) -> Self {
        Self {
            binance,
            db,
            poll_interval: Duration::from_secs(3),
            out_of_range_since: Mutex::new(HashMap::new()),
            symbol_locks: tokio::sync::Mutex::new(HashMap::new()),
            filters: tokio::sync::Mutex::new(HashMap::new()),
        }
    }

    /// 取得某 symbol 的串行锁（不存在则创建）
    async fn symbol_lock(&self, symbol: &str) -> Arc<tokio::sync::Mutex<()>> {
        let mut map = self.symbol_locks.lock().await;
        Arc::clone(
            map.entry(symbol.to_string())
                .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(()))),
        )
    }

    /// 取得某 symbol 的串行锁，供 HTTP 层与引擎轮询互斥。
    ///
    /// 凡是「先撤交易所挂单、再改库清 `order_id`」的两步操作都必须持有它。这类操作没有
    /// 乐观锁保护——清 `order_id` 的 SQL 按 id 或 (user_id, symbol) 更新，不校验 version。
    /// 轮询若挤在两步之间挂出新单，那笔真实订单会连 `order_id` 一起被抹成 NULL：
    /// 交易所上留一笔活单，库里再无任何线索。
    ///
    /// 取锁后请重新读取网格快照——取锁前读到的 `order_id` 可能已被轮询改写。
    pub async fn lock_symbol(&self, symbol: &str) -> tokio::sync::OwnedMutexGuard<()> {
        self.symbol_lock(symbol).await.lock_owned().await
    }

    /// 清除某 symbol 的超范围计时器（重建/滑窗完成后调用）
    fn clear_out_of_range(&self, symbol: &str) {
        self.out_of_range_since
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(symbol);
    }

    /// 取得某 symbol 的下单约束，首次访问时从交易所拉取并缓存
    pub async fn symbol_filters(&self, symbol: &str) -> anyhow::Result<SymbolFilters> {
        if let Some(f) = self.filters.lock().await.get(symbol) {
            return Ok(*f);
        }
        let f = self
            .binance
            .get_symbol_filters(symbol)
            .await
            .with_context(|| format!("{symbol} 获取交易规则失败"))?;
        info!(
            symbol = %symbol,
            step_size = %f.step_size,
            tick_size = %f.tick_size,
            min_qty = %f.min_qty,
            min_notional = %f.min_notional,
            "已缓存交易规则"
        );
        self.filters.lock().await.insert(symbol.to_string(), f);
        Ok(f)
    }

    // ── 引擎状态查询与配置 ──────────────────────────────────────────────────────

    /// 获取某个 symbol 的范围监控状态（供 Web API 查询）
    pub fn get_recenter_status(&self, symbol: &str) -> ReCenterStatus {
        let elapsed: Option<Duration> = self
            .out_of_range_since
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(symbol)
            .map(|since| since.elapsed());

        ReCenterStatus {
            out_of_range: elapsed.is_some(),
            minutes_out: elapsed.map_or(0, |e| e.as_secs() / 60),
        }
    }

    /// 立即触发指定 symbol 的重新居中（供手动触发）
    pub async fn trigger_recenter_now(&self, user_id: i32, symbol: &str) -> anyhow::Result<()> {
        // 与轮询共用同一把锁：重建会先撤光挂单、再批量软删（不校验 version），
        // 轮询若挤进这两步之间挂出新单，那笔真实订单会连 order_id 一起被抹掉。
        let lock = self.symbol_lock(symbol).await;
        let _guard = lock.lock().await;

        // 只取调用者自己的网格：HTTP 可达路径不得触碰他人的格子
        let grids: Vec<_> = user_grid::find_by_symbol(&self.db, symbol)
            .await?
            .into_iter()
            .filter(|g| g.user_id == user_id)
            .collect();
        anyhow::ensure!(!grids.is_empty(), "没有活跃的网格: {symbol}");
        let current_price = self
            .binance
            .get_ticker_price(symbol)
            .await
            .with_context(|| format!("{symbol} 获取当前价格失败"))?;
        info!(symbol = %symbol, current_price = %current_price, "手动触发立即重建");
        self.recenter_grids(symbol, &grids, current_price).await
    }

    /// 每隔多久重新对账一次 symbol 列表与 task 存活状态
    const SUPERVISE_INTERVAL: Duration = Duration::from_secs(30);

    /// 监管循环：持续保证「每个活跃 symbol 恰有一个存活的轮询 task」。
    ///
    /// 每 SUPERVISE_INTERVAL 对账一次：接管新增 symbol、重启已死的 task、
    /// 停掉已无网格的 symbol。查询失败只记录并沿用上一轮集合，不终止引擎。
    pub async fn run(self: Arc<Self>) -> anyhow::Result<()> {
        let mut tasks: HashMap<String, tokio::task::JoinHandle<()>> = HashMap::new();
        let mut ticker = tokio::time::interval(Self::SUPERVISE_INTERVAL);

        loop {
            // DB 抖动不该终止引擎：记录后沿用上一轮的 symbol 集合继续跑
            let symbols = match user_grid::get_active_symbols(&self.db).await {
                Ok(s) => s,
                Err(e) => {
                    error!(error = %e, "获取活跃 symbol 列表失败，保持现有 task 并稍后重试");
                    ticker.tick().await;
                    continue;
                }
            };

            // 已无网格的 symbol：停掉对应 task，避免空转消耗 API 权重
            let live: std::collections::HashSet<&str> =
                symbols.iter().map(|s| s.as_str()).collect();
            tasks.retain(|symbol, handle| {
                if live.contains(symbol.as_str()) {
                    true
                } else {
                    info!(symbol = %symbol, "该 symbol 已无活跃网格，停止其轮询 task");
                    handle.abort();
                    false
                }
            });

            // 新增的 symbol 拉起 task；已死的（panic 或被取消）重新拉起
            for symbol in symbols {
                let needs_spawn = match tasks.get(&symbol) {
                    None => true,
                    Some(h) if h.is_finished() => {
                        error!(symbol = %symbol, "轮询 task 已意外退出，正在重启");
                        true
                    }
                    Some(_) => false,
                };
                if needs_spawn {
                    let engine = self.clone();
                    let name = symbol.clone();
                    tasks.insert(
                        symbol,
                        tokio::spawn(async move { engine.run_symbol_loop(name).await }),
                    );
                }
            }

            if tasks.is_empty() {
                info!(
                    "暂无活跃 symbol，{}s 后重新检查",
                    Self::SUPERVISE_INTERVAL.as_secs()
                );
            }

            ticker.tick().await;
        }
    }

    /// 单个 symbol 的无限轮询循环
    async fn run_symbol_loop(&self, symbol: String) {
        loop {
            if let Err(e) = self.process_symbol(&symbol).await {
                error!(symbol = %symbol, error = %e, "symbol 处理失败");
            }
            tokio::time::sleep(self.poll_interval).await;
        }
    }

    /// 处理单个 symbol 下的所有网格（单轮）
    ///
    /// 全程持有该 symbol 的串行锁：引擎轮询与 HTTP resume 若同时进入，
    /// 会各自读到相同 version 的快照并各挂一笔真实订单（乐观锁只能拦住后写库的一方，
    /// 订单却已经挂在交易所上了）。
    pub async fn process_symbol(&self, symbol: &str) -> anyhow::Result<()> {
        let lock = self.symbol_lock(symbol).await;
        let _guard = lock.lock().await;
        self.process_symbol_locked(symbol).await
    }

    async fn process_symbol_locked(&self, symbol: &str) -> anyhow::Result<()> {
        let grids = user_grid::find_by_symbol(&self.db, symbol).await?;

        // 只在有 open 状态网格时才拉取一次开放订单，避免 N 次重复 API 调用
        let open_orders: Vec<OpenOrder> = if grids.iter().any(|g| g.status == "open") {
            self.binance.get_open_orders(symbol).await?
        } else {
            vec![]
        };

        // 如果本轮有 new 状态的 buy 格，预取一次 quote 余额，供所有买格共用
        // 避免 effective_buy_amount 在每格各自调用 get_spot_balances（权重 20/次）
        let cached_quote_free: Option<Decimal> =
            if grids.iter().any(|g| g.status == "new" && g.side == "buy") {
                if let Some((_, quote)) = split_symbol(symbol) {
                    self.binance.get_asset_free(quote).await.ok()
                } else {
                    None
                }
            } else {
                None
            };

        // 计价币预算是本轮**共享**的一份余额，必须随每笔挂单递减。
        // 否则每个买格都拿同一个初值去判断"够不够"，第 2 格开始的判断全部失真，
        // 这个用来避免 -2010 的护栏就形同虚设。
        let mut quote_budget = cached_quote_free;

        for grid in &grids {
            let before = quote_budget;
            if let Err(e) = self
                .process_single_grid(grid, &open_orders, &mut quote_budget)
                .await
            {
                // 出错时预算的扣减不可信，回退到本格处理前的值
                quote_budget = before;
                // -2010: 余额不足——理论上 place_and_open 已主动调整金额，此处仅作保底日志
                if matches!(
                    e.downcast_ref::<BinanceError>(),
                    Some(BinanceError::Api { code: -2010, .. })
                ) {
                    warn!(
                        grid_id = grid.id,
                        symbol = %symbol,
                        "余额不足（-2010），本轮跳过，等待余额恢复后自动重试"
                    );
                } else {
                    warn!(
                        grid_id = grid.id,
                        symbol = %symbol,
                        error = %e,
                        "单网格处理失败，跳过"
                    );
                }
            }
        }

        // 每轮检查价格是否长期超出网格范围，必要时重新居中
        if let Err(e) = self.check_and_recenter_if_needed(symbol, &grids).await {
            warn!(symbol = %symbol, error = %e, "范围检查失败，跳过");
        }

        Ok(())
    }

    // ── 范围检测与自动重新居中 ──────────────────────────────────────────────────

    /// 检查价格是否超出网格范围，超出持续 RANGE_DELAY_SECS 后触发滑动窗口。
    async fn check_and_recenter_if_needed(
        &self,
        symbol: &str,
        grids: &[user_grid::UserGrid],
    ) -> anyhow::Result<()> {
        if grids.is_empty() {
            return Ok(());
        }

        // 计算当前网格覆盖范围（grids 非空已在上方检查，fold 安全）
        let (min_buy, max_sell) = grids
            .iter()
            .fold((Decimal::MAX, Decimal::MIN), |(lo, hi), g| {
                (lo.min(g.buy_price), hi.max(g.sell_price))
            });

        let current_price = self
            .binance
            .get_ticker_price(symbol)
            .await
            .with_context(|| format!("{symbol} 获取当前价格失败"))?;

        let out_of_range = current_price < min_buy || current_price > max_sell;

        // 价格超出范围持续超过 RANGE_DELAY_SECS → 触发滑动窗口
        let should_slide = {
            let mut timers = self
                .out_of_range_since
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if out_of_range {
                let elapsed_secs = timers
                    .entry(symbol.to_string())
                    .or_insert_with(Instant::now)
                    .elapsed()
                    .as_secs();

                let minutes = elapsed_secs / 60;
                if minutes > 0 && minutes.is_multiple_of(10) {
                    warn!(
                        symbol = %symbol,
                        current_price = %current_price,
                        range = %format!("[{min_buy}, {max_sell}]"),
                        minutes_out = minutes,
                        "价格超出网格范围，等待滑动窗口触发"
                    );
                }
                elapsed_secs >= RANGE_DELAY_SECS
            } else {
                if timers.remove(symbol).is_some() {
                    info!(symbol = %symbol, current_price = %current_price, "价格回归网格范围，重置超范围计时器");
                }
                false
            }
        };

        if should_slide {
            self.slide_window(symbol, grids, current_price, min_buy, max_sell)
                .await?;
        }

        Ok(())
    }

    /// 拉取历史 K 线并返回 P50 中位数作为 p_floor 的锚点价格。
    ///
    /// K 线不可用时使用 `current_price` 作为稳定降级值。
    async fn resolve_anchor(&self, symbol: &str, current_price: Decimal) -> Decimal {
        match self.binance.get_klines(symbol, "1h", 24).await {
            Ok(closes) if !closes.is_empty() => {
                let median = compute_median(&closes).unwrap_or(current_price);
                info!(
                    symbol = %symbol,
                    p50_median = %median,
                    "24H K线收盘价中位数"
                );
                median
            }
            Err(e) => {
                warn!(symbol = %symbol, error = %e, "K线拉取失败，降级到当前价");
                current_price
            }
            _ => {
                warn!(symbol = %symbol, "K线返回为空，降级到当前价");
                current_price
            }
        }
    }

    /// 滑动窗口：价格超出范围时，只移动 1 格，保持格子总数不变。
    ///
    /// - 价格突破上边界 → 移除 buy_price 最低的未成交 buy 格，在顶部新建 sell 格
    /// - 价格跌破下边界 → 移除 sell_price 最高的未成交 sell 格，在底部新建 buy 格
    ///
    /// 无需等待资金结算：撤 buy 释放的是计价币，新 sell 锁定的是基础币，两种资产不冲突。
    /// 若对应方向已无未成交格可移除，降级到全量重建。
    async fn slide_window(
        &self,
        symbol: &str,
        grids: &[user_grid::UserGrid],
        current_price: Decimal,
        min_buy: Decimal,
        max_sell: Decimal,
    ) -> anyhow::Result<()> {
        let Some(first) = grids.first() else {
            return Ok(());
        };
        let spacing = first.sell_price - first.buy_price;
        let user_id = first.user_id;

        // 上滑与下滑互为镜像：撤掉最外侧的反向格，在另一端补一格。
        // 仅方向相关的四个量不同，其余（撤单 / 软删 / 建格）完全共用。
        let up = if current_price > max_sell {
            true
        } else if current_price < min_buy {
            false
        } else {
            // 调用方只在价格超出 [min_buy, max_sell] 时进入，这里是防御性分支
            self.clear_out_of_range(symbol);
            return Ok(());
        };
        let (dir, remove_side, new_side) = if up {
            ("up", "buy", "sell")
        } else {
            ("down", "sell", "buy")
        };

        {
            // 优先选 status=open（有活跃挂单），其次 status=new（还没挂单）
            let candidates = grids
                .iter()
                .filter(|g| g.side == remove_side && (g.status == "open" || g.status == "new"));
            let remove = if up {
                candidates.min_by_key(|g| g.buy_price)
            } else {
                candidates.max_by_key(|g| g.sell_price)
            };

            let Some(remove) = remove else {
                warn!(symbol = %symbol, direction = dir, "滑窗：无可移除的 {remove_side} 格，降级全量重建");
                return self.recenter_grids(symbol, grids, current_price).await;
            };

            // 新格金额沿用同方向已有格子，没有则沿用被移除格
            let amount = grids
                .iter()
                .find(|g| g.side == new_side && (g.status == "open" || g.status == "new"))
                .map(|g| g.amount)
                .unwrap_or(remove.amount);

            // 上滑在顶部补 [max_sell, max_sell+spacing]，下滑在底部补 [min_buy-spacing, min_buy]
            let (new_buy, new_sell) = if up {
                (max_sell, max_sell + spacing)
            } else {
                (min_buy - spacing, min_buy)
            };
            let remove_price = if up {
                remove.buy_price
            } else {
                remove.sell_price
            };

            info!(
                symbol = %symbol,
                direction = dir,
                current_price = %current_price,
                range = %format!("[{min_buy}, {max_sell}]"),
                remove_id = remove.id,
                remove_price = %remove_price,
                new_buy_at = %new_buy,
                new_sell_at = %new_sell,
                "滑窗：移除最外侧 {remove_side} 格，另一端新建 {new_side} 格"
            );

            // `grids` 是本轮开头的快照，而循环体可能已经给这一格挂上了真实订单。
            // 拿快照里的 status/order_id 决定撤不撤，会漏撤刚挂出的那笔。动手前重读。
            let Some(fresh) = user_grid::find_by_id(&self.db, remove.id).await? else {
                warn!(grid_id = remove.id, "待移除的格子已消失，跳过本次滑窗");
                return Ok(());
            };
            if fresh.status == "deleted" {
                warn!(
                    grid_id = remove.id,
                    "待移除的格子已被并发删除，跳过本次滑窗"
                );
                return Ok(());
            }

            if fresh.status == "open" {
                if let Some(ref oid) = fresh.order_id {
                    match oid.parse::<i64>() {
                        Ok(order_id) => {
                            // 撤单失败就中止滑窗：若继续软删，这笔活单将永久失去 order_id 线索
                            self.cancel_or_abort(symbol, order_id, fresh.id).await?;
                        }
                        Err(_) => {
                            warn!(grid_id = fresh.id, order_id = %oid, "order_id 解析失败，跳过撤单，Binance 挂单可能残留")
                        }
                    }
                }
            }
            let replacement = user_grid::NewUserGrid {
                user_id,
                symbol: symbol.to_string(),
                amount,
                buy_price: new_buy,
                sell_price: new_sell,
                side: new_side.into(),
                source: "auto".into(),
            };
            let Some(new_id) = user_grid::replace_one(&self.db, fresh.id, replacement).await?
            else {
                warn!(grid_id = fresh.id, "待移除网格已被并发修改，跳过本次滑窗");
                return Ok(());
            };

            info!(
                symbol = %symbol,
                direction = dir,
                new_id,
                buy_price = %new_buy,
                sell_price = %new_sell,
                "滑窗完成"
            );
        }

        self.clear_out_of_range(symbol);
        Ok(())
    }

    /// 重新居中：撤单 → 查余额 → 删旧格 → 按余额分买卖方向新建
    async fn recenter_grids(
        &self,
        symbol: &str,
        grids: &[user_grid::UserGrid],
        current_price: Decimal,
    ) -> anyhow::Result<()> {
        // 从现有网格推导参数（spacing 来自格子，amount 重新按余额平均）
        let Some(first) = grids.first() else {
            return Ok(());
        };
        let spacing = first.sell_price - first.buy_price;
        let grid_count = grids.len() as u32;

        // 解析 base/quote 资产名（如 USDCUSDT → USDC, USDT）
        let (base_asset, quote_asset) = binance_client::split_symbol(symbol)
            .ok_or_else(|| anyhow::anyhow!("无法解析 symbol 的 base/quote: {symbol}"))?;

        info!(
            symbol = %symbol,
            current_price = %current_price,
            "触发重新居中：撤单 → 查余额 → 以当前价新建"
        );

        // 1. 撤销所有挂单。失败必须中止重建：下一步会把持有 order_id 的行全部软删，
        //    若撤单其实没成功，那些真实挂单会失去追踪线索，且资金仍被锁定在交易所。
        self.binance
            .cancel_all_orders(symbol)
            .await
            .with_context(|| format!("{symbol} 撤单失败，中止重建以免丢失挂单线索"))?;
        // 等待 Binance 将刚撤销的挂单资金归还到 free 余额
        tokio::time::sleep(tokio::time::Duration::from_secs(CANCEL_ORDERS_SETTLE_SECS)).await;

        // 2. 查余额（仅 free，不含 locked），避免将其他挂单的 locked 误计入分配。
        //    一次账户快照取两个资产：分开查等于把同一份响应买两遍（权重 20 → 40）。
        let (base_balance, quote_balance) = self
            .binance
            .get_asset_free_pair(base_asset, quote_asset)
            .await
            .with_context(|| format!("查询余额失败 ({base_asset}/{quote_asset})"))?;

        // 3. 按余额比例分配格数，每边各自计算 amount
        //    base 与 quote 是不同货币，必须先用 current_price 折算到同一单位再比例分配
        let filters = self.symbol_filters(symbol).await?;
        let Some((sell_count, buy_count, sell_amount, buy_amount)) = allocate_grids(Allocation {
            base: base_balance,
            quote: quote_balance,
            price: current_price,
            grid_count,
            min_qty: filters.min_qty,
            min_notional: filters.min_notional,
        }) else {
            warn!(symbol = %symbol, "余额为零，跳过重新居中");
            self.clear_out_of_range(symbol);
            return Ok(());
        };

        info!(
            symbol = %symbol,
            base_asset,
            base_balance = %base_balance,
            quote_asset,
            quote_balance = %quote_balance,
            sell_count,
            buy_count,
            sell_amount = %sell_amount,
            buy_amount = %buy_amount,
            "余额分析完成（按比例分配格数）"
        );

        // 4. 按余额生成新格子。先完整构造并校验，再删除旧格，避免无效布局清空策略。
        let user_id = first.user_id;
        // 使用过去 24H 1H K 线收盘价中位数作为锚点，比瞬时价更稳定
        // K 线失败时 resolve_anchor 使用当前价，确保重建继续执行
        let anchor = self.resolve_anchor(symbol, current_price).await;
        let p_floor = (anchor / spacing).ceil() * spacing;

        let new_grids = build_auto_grids(
            AutoGridLayout {
                user_id,
                p_floor,
                spacing,
                sell_count,
                buy_count,
                sell_amount,
                buy_amount,
            },
            symbol,
        );
        anyhow::ensure!(
            !new_grids.is_empty(),
            "{symbol} 当前 spacing、余额与锚点会生成非正价格或零金额网格"
        );

        // 5. 原子替换旧网格；任何插入错误都会回滚软删除。
        let (deleted, ids) =
            user_grid::replace_active_by_user_and_symbol(&self.db, user_id, symbol, new_grids)
                .await?;
        info!(symbol = %symbol, user_id, deleted, "旧网格已原子替换（暂停中的格子保留）");
        info!(
            symbol = %symbol,
            user_id,
            new_ids = ?ids,
            p_floor = %p_floor,
            "网格重新居中完成"
        );

        // 7. 清除超出范围计时器
        self.clear_out_of_range(symbol);
        Ok(())
    }

    /// 处理单个网格的状态机流转
    async fn process_single_grid(
        &self,
        grid: &user_grid::UserGrid,
        open_orders: &[OpenOrder],
        quote_budget: &mut Option<Decimal>,
    ) -> anyhow::Result<()> {
        let status =
            user_grid::GridStatus::try_from(grid.status.as_str()).map_err(anyhow::Error::msg)?;
        let side = user_grid::GridSide::try_from(grid.side.as_str()).map_err(anyhow::Error::msg)?;

        match (status, side) {
            (user_grid::GridStatus::New, user_grid::GridSide::Buy) => {
                let spent = self
                    .place_and_open(
                        grid,
                        OrderSide::Buy,
                        grid.buy_price,
                        grid.amount,
                        *quote_budget,
                    )
                    .await?;
                // 从本轮共享预算中扣掉这笔实际花费，供后续买格判断
                if let (Some(b), Some(s)) = (quote_budget.as_mut(), spent) {
                    *b = (*b - s).max(Decimal::ZERO);
                }
            }
            (user_grid::GridStatus::New, user_grid::GridSide::Sell) => {
                self.place_and_open(grid, OrderSide::Sell, grid.sell_price, grid.amount, None)
                    .await?;
            }
            (user_grid::GridStatus::Open, _) => {
                if let Some(settled_grid) =
                    self.check_and_close_if_filled(grid, open_orders).await?
                {
                    // 成交后立即翻转并挂反向单，不等下一轮轮询
                    // close_grid_with_trade_amount 已将 version 增了 1，所以传 version+1
                    self.flip_and_place_reverse(&settled_grid, grid.version + 1)
                        .await?;
                }
            }
            (user_grid::GridStatus::Closed, _) => {
                // grid 已处于 closed 状态（来自上一轮），立即翻转并挂单
                self.flip_and_place_reverse(grid, grid.version).await?;
            }
            (user_grid::GridStatus::Deleted, _) => {
                warn!(grid_id = grid.id, "已删除网格意外进入调度集合，本轮跳过");
            }
        }
        Ok(())
    }

    /// 翻转网格方向（closed→new）并立即挂反向单
    ///
    /// `flip_version` 是此时 DB 中的 version（乐观锁检查值）：
    ///   - 若调用前刚执行了 close_grid_with_trade_amount，传 `grid.version + 1`
    ///   - 若 grid 已是 closed 状态（PlaceReverse 分支），传 `grid.version`
    async fn flip_and_place_reverse(
        &self,
        grid: &user_grid::UserGrid,
        flip_version: i64,
    ) -> anyhow::Result<()> {
        let current_side =
            user_grid::GridSide::try_from(grid.side.as_str()).map_err(anyhow::Error::msg)?;
        let reverse_side = match current_side {
            user_grid::GridSide::Buy => user_grid::GridSide::Sell,
            user_grid::GridSide::Sell => user_grid::GridSide::Buy,
        };

        // amount 换算：buy 格的 amount 是计价币花费额，sell 格的是基础币数量，两者单位不同。
        //   buy 成交  → 实际买到 round_quantity(amount / buy_price) 个基础币，卖腿卖这么多
        //   sell 成交 → 实际卖出 amount 个基础币、收回 amount × sell_price 计价币，买腿花这么多
        // 反向腿使用成交后换算出的对应币种数量，确保订单数量与可用余额一致。
        let filters = self.symbol_filters(&grid.symbol).await?;
        let reverse_amount = if reverse_side == user_grid::GridSide::Sell {
            let bought = grid.amount.checked_div(grid.buy_price).ok_or_else(|| {
                anyhow::anyhow!("grid_id={} buy_price 为零，无法换算翻转数量", grid.id)
            })?;
            filters.round_quantity(bought)
        } else {
            grid.amount * grid.sell_price
        };

        let flipped = user_grid::flip_to_reverse(
            &self.db,
            grid.id,
            reverse_side.as_str(),
            reverse_amount,
            flip_version,
        )
        .await?;
        if !flipped {
            warn!(
                grid_id = grid.id,
                "flip_to_reverse 乐观锁冲突（version 不匹配），跳过"
            );
            return Ok(());
        }

        info!(
            grid_id = grid.id,
            old_side = %grid.side,
            new_side = %reverse_side.as_str(),
            symbol = %grid.symbol,
            buy = %grid.buy_price,
            sell = %grid.sell_price,
            "✓ 订单成交，翻转 {old}→{new}，立即挂反向单",
            old = grid.side,
            new = reverse_side.as_str(),
        );

        // flip_to_reverse 也将 version 增 1，构造更新后的 grid 供 place_and_open 使用
        let updated = user_grid::UserGrid {
            side: reverse_side.as_str().to_string(),
            status: "new".to_string(),
            version: flip_version + 1,
            order_id: None,
            amount: reverse_amount,
            ..grid.clone()
        };

        let (order_side, price) = match reverse_side {
            user_grid::GridSide::Buy => (OrderSide::Buy, grid.buy_price),
            user_grid::GridSide::Sell => (OrderSide::Sell, grid.sell_price),
        };
        self.place_and_open(&updated, order_side, price, reverse_amount, None)
            .await?;
        Ok(())
    }

    /// 买单场景下，将请求金额收敛到实际 free balance（避免 -2010）。
    ///
    /// `cached_quote_free`：由 `process_symbol` 预取，同轮所有买格共用，避免重复 API 调用。
    /// `None` 表示调用方无缓存（如 flip_and_place_reverse），此时直接使用 requested 金额。
    fn effective_buy_amount(
        &self,
        requested: Decimal,
        cached_quote_free: Option<Decimal>,
    ) -> Decimal {
        let Some(free) = cached_quote_free else {
            return requested; // 无缓存时直接信任 requested（flip 场景）
        };

        if free >= requested {
            return requested; // 余额充足，无需调整
        }

        // 留 0.05% 缓冲，防止时序差异导致的二次 -2010
        let adjusted = (free * dec!(0.9995)).floor();
        warn!(
            requested = %requested,
            free = %free,
            adjusted = %adjusted,
            "买单余额不足，自动降额至实际可用 free balance"
        );
        adjusted
    }

    /// 挂单并用乐观锁将 status 从 new → open。
    ///
    /// 返回本次实际占用的**计价币**金额（仅买单有值），供调用方递减本轮共享预算。
    /// 卖单花的是基础币，不影响计价币预算，故返回 `None`。
    async fn place_and_open(
        &self,
        grid: &user_grid::UserGrid,
        side: OrderSide,
        price: rust_decimal::Decimal,
        amount: rust_decimal::Decimal,
        cached_quote_free: Option<Decimal>,
    ) -> anyhow::Result<Option<Decimal>> {
        // 买单：主动收敛到实际 free balance，防止余额被其他网格占用时报 -2010
        let effective_amount = if side == OrderSide::Buy {
            self.effective_buy_amount(amount, cached_quote_free)
        } else {
            amount
        };

        // 交易所约束：数量按 stepSize、价格按 tickSize 向下对齐，
        // 并确保名义价值不低于 minNotional；BTC 等小步长资产必须保留小数数量。
        let filters = self.symbol_filters(&grid.symbol).await?;
        let price = filters.round_price(price);
        if price <= Decimal::ZERO {
            anyhow::bail!("grid_id={} 价格取整后为零，跳过", grid.id);
        }

        // amount 的语义：
        //   buy  格：amount = 计价币要花费的金额，需除以价格得到基础币数量
        //   sell 格：amount = 基础币要卖出的数量，直接使用
        //
        // checked_div：buy_price 为 0 时返回 None 而不是 panic。API 层已拒绝非正价格，
        // 但历史数据或直接改库仍可能留下 0，而一次除零 panic 会永久杀死该 symbol 的 task。
        let raw_quantity = match side {
            OrderSide::Buy => effective_amount.checked_div(price).ok_or_else(|| {
                anyhow::anyhow!("grid_id={} 价格为零，无法换算数量，跳过", grid.id)
            })?,
            OrderSide::Sell => effective_amount,
        };
        let quantity = filters.round_quantity(raw_quantity);

        if quantity <= Decimal::ZERO {
            anyhow::bail!(
                "计算得到挂单数量为零，跳过 grid_id={} side={:?} amount={} price={} step={}",
                grid.id,
                side,
                amount,
                price,
                filters.step_size
            );
        }
        // 提前拦下必被交易所拒绝的订单，避免无谓的 API 调用与噪声告警
        if let Err(reason) = filters.validate(quantity, price) {
            anyhow::bail!("grid_id={} 不满足交易所下单约束：{reason}", grid.id);
        }

        // 幂等键：同一 (grid, version) 重复提交会被 Binance 以 -2010 duplicate 拒绝，
        // 因此"下单请求超时、实际已成交"这类模糊结果重试时不会变成两笔真实订单。
        let client_order_id = format!("g{}v{}", grid.id, grid.version);
        let req = PlaceOrderRequest {
            symbol: grid.symbol.clone(),
            side: side.clone(),
            quantity,
            price,
            client_order_id: Some(client_order_id.clone()),
        };

        let resp = match self.binance.place_order(req).await {
            Ok(resp) => resp,
            Err(place_error) => match self
                .binance
                .query_order_by_client_id(&grid.symbol, &client_order_id)
                .await
            {
                Ok(existing) => {
                    warn!(
                        grid_id = grid.id,
                        order_id = existing.order_id,
                        client_order_id,
                        error = %place_error,
                        "下单响应不确定，已按客户端幂等键恢复交易所订单"
                    );
                    binance_client::types::OrderResponse {
                        order_id: existing.order_id,
                    }
                }
                Err(BinanceError::Api { code: -2013, .. }) => return Err(place_error.into()),
                Err(recovery_error) => {
                    return Err(anyhow::anyhow!(
                        "下单失败且按 client_order_id 恢复失败；place={place_error}; recovery={recovery_error}"
                    ));
                }
            },
        };
        let order_id_str = resp.order_id.to_string();

        // 订单此刻已在交易所生效，但数据库还不知道它。以下三条分支必须保证
        // "库里记不下这笔单" ⇒ "立刻把它从交易所撤掉"，否则就是一笔无人跟踪的活单。
        match user_grid::set_order_open(&self.db, grid.id, &order_id_str, grid.version).await {
            Ok(true) => {
                info!(
                    grid_id = grid.id,
                    order_id = %order_id_str,
                    symbol = %grid.symbol,
                    side = ?side,
                    price = %price,
                    quantity = %quantity,
                    "→ 挂单成功 new→open  [{:?}] {} @ {} qty={}",
                    side, grid.symbol, price, quantity,
                );
                // 买单占用计价币，回报花费供本轮预算递减；卖单花的是基础币，不计入
                Ok(match side {
                    OrderSide::Buy => Some(quantity * price),
                    OrderSide::Sell => None,
                })
            }
            Ok(false) => {
                warn!(
                    grid_id = grid.id,
                    order_id = %order_id_str,
                    "乐观锁冲突（version 不匹配），撤销刚挂出的订单"
                );
                self.cancel_untracked(&grid.symbol, resp.order_id, grid.id)
                    .await;
                // 订单已撤销，未真正占用资金
                Ok(None)
            }
            Err(e) => {
                error!(
                    grid_id = grid.id,
                    order_id = %order_id_str,
                    error = %e,
                    "写库失败，撤销刚挂出的订单"
                );
                self.cancel_untracked(&grid.symbol, resp.order_id, grid.id)
                    .await;
                Err(e.into())
            }
        }
    }

    /// 撤单，并区分"本就没有这笔单"与"撤单调用失败"。
    ///
    /// Binance 用 -2011 / -2013 表示订单不存在或已终结 —— 这两种可以安全吞掉，
    /// 结果与撤单成功一致。其余（网络错误、429、-1021 时间戳、-1003 限频）都意味着
    /// **这笔单很可能还活着**，此时绝不能继续往下删除持有 order_id 的数据库行，
    /// 否则这笔真实挂单将永久失去追踪线索。
    async fn cancel_or_abort(
        &self,
        symbol: &str,
        order_id: i64,
        grid_id: i32,
    ) -> anyhow::Result<()> {
        match self.binance.cancel_order(symbol, order_id).await {
            Ok(()) => Ok(()),
            Err(BinanceError::Api { code, ref msg }) if code == -2011 || code == -2013 => {
                info!(grid_id, order_id, code, msg = %msg, "订单已不存在，视为撤单成功");
                Ok(())
            }
            Err(e) => {
                error!(
                    grid_id,
                    order_id,
                    symbol = %symbol,
                    error = %e,
                    "撤单失败且订单可能仍然存活，中止本次操作以免丢失 order_id 线索"
                );
                Err(e.into())
            }
        }
    }

    /// 撤销一笔数据库未能记录的订单。撤不掉就把 order_id 打进 error 日志供人工核对 ——
    /// 这是这笔钱唯一的线索，绝不能只留一句泛泛的失败信息。
    async fn cancel_untracked(&self, symbol: &str, order_id: i64, grid_id: i32) {
        if let Err(e) = self.binance.cancel_order(symbol, order_id).await {
            error!(
                grid_id,
                order_id,
                symbol = %symbol,
                error = %e,
                "!! 撤销失败：交易所存在一笔数据库未记录的活单，请人工核对该 order_id"
            );
        }
    }

    /// 查询 Binance 判断订单是否已成交，成交后 open → closed 并记录交易历史
    ///
    /// 返回实际结算后的网格表示订单刚刚成交，调用方可立即挂反向单。
    /// `open_orders` 由 `process_symbol` 预先拉取，避免每个 grid 独立请求。
    async fn check_and_close_if_filled(
        &self,
        grid: &user_grid::UserGrid,
        open_orders: &[OpenOrder],
    ) -> anyhow::Result<Option<user_grid::UserGrid>> {
        // open 状态必须携带 order_id，两者在正常流转中由同一条 UPDATE 写入。
        // 缺少 order_id 时无法对账，重置为 new 后恢复挂单流程。
        let Some(ref order_id) = grid.order_id else {
            warn!(
                grid_id = grid.id,
                symbol = %grid.symbol,
                "open 状态但没有 order_id（异常态），重置为 new 以便重新挂单"
            );
            if !user_grid::reset_to_new(&self.db, grid.id, grid.version).await? {
                warn!(grid_id = grid.id, "reset_to_new 乐观锁冲突，下一轮重试");
            }
            return Ok(None);
        };

        // 解析一次，后续比较和 API 调用都复用
        let order_id_i64: i64 = order_id
            .parse()
            .with_context(|| format!("grid {} 存储了无法解析的 order_id: {}", grid.id, order_id))?;

        let still_open = open_orders.iter().any(|o| o.order_id == order_id_i64);
        if still_open {
            return Ok(None);
        }

        // 不在开放订单中，需查询具体状态（FILLED vs CANCELED/EXPIRED）
        let order = self.binance.query_order(&grid.symbol, order_id_i64).await?;
        let status = OrderStatus::try_from(order.status.as_str()).map_err(anyhow::Error::msg)?;
        if matches!(
            status,
            OrderStatus::New | OrderStatus::PartiallyFilled | OrderStatus::PendingCancel
        ) {
            warn!(
                grid_id = grid.id,
                order_id = %order_id,
                status = %order.status,
                "订单仍处于活跃状态，保留 open 与 order_id 等待下一轮对账"
            );
            return Ok(None);
        }

        let terminal_without_fill = matches!(
            status,
            OrderStatus::Canceled
                | OrderStatus::Rejected
                | OrderStatus::Expired
                | OrderStatus::ExpiredInMatch
        ) && order.executed_qty <= Decimal::ZERO;
        if terminal_without_fill {
            // 明确终态且没有任何成交：重置为 new 以便重新挂单
            warn!(
                grid_id = grid.id,
                order_id = %order_id,
                status = %order.status,
                "订单未成交（已撤销或过期），重置网格为 new"
            );
            if !user_grid::reset_to_new(&self.db, grid.id, grid.version).await? {
                warn!(
                    grid_id = grid.id,
                    "reset_to_new 乐观锁冲突（已被并发重建或删除），跳过"
                );
            }
            return Ok(None);
        }

        let is_partial_terminal = status != OrderStatus::Filled;
        let settled_amount = match user_grid::GridSide::try_from(grid.side.as_str())
            .map_err(anyhow::Error::msg)?
        {
            user_grid::GridSide::Buy if order.cummulative_quote_qty > Decimal::ZERO => {
                order.cummulative_quote_qty
            }
            user_grid::GridSide::Buy if is_partial_terminal => order.executed_qty * grid.buy_price,
            user_grid::GridSide::Sell if order.executed_qty > Decimal::ZERO => order.executed_qty,
            _ => grid.amount,
        };
        let mut settled_grid = grid.clone();
        settled_grid.amount = settled_amount;

        // 只在卖单成交时记录利润：卖出 = 换回计价币，差价才真正落袋。
        // 买单成交只是把计价币换成基础币，利润尚未实现，不计入交易历史。
        let trade = (settled_grid.side == "sell").then(|| trade_history::NewTradeHistory {
            user_id: grid.user_id,
            symbol: &grid.symbol,
            order_id,
            amount: settled_amount,
            buy_price: grid.buy_price,
            sell_price: grid.sell_price,
            source: "user_grid",
            source_id: grid.id,
        });

        // 状态流转与入账在同一事务内完成，避免"已 closed 但利润没记上"的永久丢失
        let closed = trade_history::close_grid_with_trade_amount(
            &self.db,
            grid.id,
            grid.version,
            trade,
            Some(settled_amount),
        )
        .await?;
        if closed {
            info!(
                grid_id = grid.id,
                order_id = %order_id,
                symbol = %grid.symbol,
                side = %grid.side,
                buy = %grid.buy_price,
                sell = %grid.sell_price,
                amount = %grid.amount,
                "★ 订单已成交 open→closed，side={} buy={} sell={}",
                grid.side, grid.buy_price, grid.sell_price,
            );
            return Ok(Some(settled_grid));
        }
        Ok(None)
    }
}

/// 计算一组价格的中位数（P50）。
///
/// 用于从历史 K 线收盘价中确定网格锚点，
/// 比瞬时价格更稳定，避免因短暂波动导致格子定位偏差。
pub fn compute_median(prices: &[Decimal]) -> Option<Decimal> {
    if prices.is_empty() {
        return None;
    }
    let mut sorted = prices.to_vec();
    sorted.sort();
    let mid = sorted.len() / 2;
    if sorted.len().is_multiple_of(2) {
        Some((sorted[mid - 1] + sorted[mid]) / dec!(2))
    } else {
        Some(sorted[mid])
    }
}
