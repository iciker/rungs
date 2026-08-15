//! 网格策略：计算下一步挂单方向和价格

use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;

/// 分配所需的全部输入。
///
/// `base` 与 `quote` 是**两种不同货币**的数量，必须先用 `price` 折算到同一单位
/// 才能比较、求和、算比例 —— 直接相加会让高价基础币的权重被压到几乎为零。
pub struct Allocation {
    /// 基础币可用余额（如 BTC）
    pub base: Decimal,
    /// 计价币可用余额（如 USDT）
    pub quote: Decimal,
    /// 当前价，用于把 base 折算成计价币
    pub price: Decimal,
    pub grid_count: u32,
    /// 交易所允许的最小下单数量（LOT_SIZE.minQty）
    pub min_qty: Decimal,
    /// 交易所允许的最小名义价值（NOTIONAL.minNotional），同时用作 dust 阈值
    pub min_notional: Decimal,
}

/// 按余额比例分配买卖格数，每边各自计算 amount。
///
/// 核心思路（比例基于**折算到计价币后**的价值，而非两种货币的裸数字相加）：
///   - sell_grids = round(grid_count × base_value / total_value)，保证 quote>0 时至少留 1 买格
///   - buy_grids  = grid_count - sell_grids
///   - sell_amount = base  / sell_grids（每卖格投入，基础币计）
///   - buy_amount  = quote / buy_grids（每买格投入，计价币计）
///
/// 这样即使余额极度不均衡（如 4500:500），两边都能充分利用余额并满足 grid_count。
///
/// 返回 `(sell_grids, buy_grids, sell_amount, buy_amount)`；余额不足以建仓时返回 `None`。
pub fn allocate_grids(a: Allocation) -> Option<(u32, u32, Decimal, Decimal)> {
    if a.grid_count == 0 || a.price <= Decimal::ZERO {
        return None;
    }

    // dust 判定按**名义价值**统一口径：0.0001 BTC 和 5 USDT 都该按其 USDT 价值衡量，
    // 而不是拿一个裸数字 10 同时套在两种货币上。
    let dust = a.min_notional.max(Decimal::ZERO);
    let base_notional = a.base * a.price;
    let base = if base_notional < dust {
        Decimal::ZERO
    } else {
        a.base
    };
    let quote = if a.quote < dust {
        Decimal::ZERO
    } else {
        a.quote
    };

    // 折算到计价币后才能相加
    let base_value = base * a.price;
    let total_value = base_value + quote;
    if total_value == Decimal::ZERO {
        return None;
    }

    // 按价值比例分配格数
    let grid_count = a.grid_count;
    let initial_sell_grids = if base == Decimal::ZERO {
        0u32
    } else if quote == Decimal::ZERO {
        grid_count
    } else {
        let proportional = (Decimal::from(grid_count) * base_value / total_value).round();
        let mut n = proportional.to_u32().unwrap_or(0).min(grid_count);
        // 两边都有余额时，各自至少保留 1 格
        if n == 0 {
            n = 1;
        }
        if n == grid_count {
            n = grid_count - 1;
        }
        n
    };

    let mut sell_grids = initial_sell_grids;
    let mut buy_grids = grid_count - initial_sell_grids;

    // 每个卖格必须同时满足 minQty 与 minNotional，否则交易所会拒单。
    // 不满足就减少卖格数（每格分到更多），直到可下单或全部转为买格。
    while sell_grids > 0 {
        let per_grid = base / Decimal::from(sell_grids);
        if per_grid >= a.min_qty && per_grid * a.price >= dust {
            break;
        }
        sell_grids -= 1;
    }

    // 买格的 amount 是计价币预算；同样必须满足 minNotional，且换算出的基础币
    // 数量不得低于 minQty。卖格被缩减时不把空位强塞给买格，因为 quote 余额
    // 并不会随之增加。
    while buy_grids > 0 {
        let per_grid = quote / Decimal::from(buy_grids);
        let base_qty = per_grid / a.price;
        if per_grid >= dust && base_qty >= a.min_qty {
            break;
        }
        buy_grids -= 1;
    }

    if sell_grids == 0 && buy_grids == 0 {
        return None;
    }

    let sell_amount = if sell_grids > 0 {
        base / Decimal::from(sell_grids)
    } else {
        Decimal::ZERO
    };
    let buy_amount = if buy_grids > 0 {
        quote / Decimal::from(buy_grids)
    } else {
        Decimal::ZERO
    };

    Some((sell_grids, buy_grids, sell_amount, buy_amount))
}

/// 构造 `build_auto_grids` 所需的布局参数
pub struct AutoGridLayout {
    pub user_id: i32,
    pub p_floor: Decimal,
    pub spacing: Decimal,
    pub sell_count: u32,
    pub buy_count: u32,
    pub sell_amount: Decimal,
    pub buy_amount: Decimal,
}

/// 根据 p_floor + spacing 生成 sell + buy 格的 NewUserGrid 列表
///
/// sell 格：p_floor 往上，从索引 0 开始
/// buy  格：p_floor 往下，从 p_floor-spacing 开始（与 sell_0 不重叠）
pub fn build_auto_grids(
    layout: AutoGridLayout,
    symbol: &str,
) -> Vec<db::models::user_grid::NewUserGrid> {
    let AutoGridLayout {
        user_id,
        p_floor,
        spacing,
        sell_count,
        buy_count,
        sell_amount,
        buy_amount,
    } = layout;

    let lowest_buy = p_floor - spacing * Decimal::from(buy_count);
    let invalid = p_floor <= Decimal::ZERO
        || spacing <= Decimal::ZERO
        || (buy_count > 0 && (buy_amount <= Decimal::ZERO || lowest_buy <= Decimal::ZERO))
        || (sell_count > 0 && sell_amount <= Decimal::ZERO);
    if invalid {
        return Vec::new();
    }

    let mut grids = Vec::with_capacity((sell_count + buy_count) as usize);

    for i in 0..sell_count {
        let buy_price = p_floor + spacing * Decimal::from(i);
        grids.push(db::models::user_grid::NewUserGrid {
            user_id,
            symbol: symbol.to_string(),
            amount: sell_amount,
            buy_price,
            sell_price: buy_price + spacing,
            side: "sell".into(),
            source: "auto".into(),
        });
    }

    for i in 0..buy_count {
        let buy_price = p_floor - spacing * Decimal::from(i + 1);
        grids.push(db::models::user_grid::NewUserGrid {
            user_id,
            symbol: symbol.to_string(),
            amount: buy_amount,
            buy_price,
            sell_price: buy_price + spacing,
            side: "buy".into(),
            source: "auto".into(),
        });
    }

    grids
}
