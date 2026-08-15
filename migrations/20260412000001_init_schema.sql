-- 初始 schema：PostgreSQL
-- 包含全部表结构，全新部署只需运行此文件一次

-- ── users ─────────────────────────────────────────────────────────────────────

CREATE TABLE users (
    id         SERIAL PRIMARY KEY,
    username   VARCHAR(255) NOT NULL UNIQUE,
    password   VARCHAR(255) NOT NULL,
    email      VARCHAR(255) NOT NULL UNIQUE,
    created_at TIMESTAMPTZ  NOT NULL DEFAULT NOW()
);

-- ── user_grid ─────────────────────────────────────────────────────────────────
-- 网格策略档位，status: new → open → closed → deleted
-- is_paused = TRUE 时引擎跳过该格，恢复后从原状态继续
-- source: 'manual' 用户手动创建，'auto' 智能建仓批量创建

CREATE TABLE user_grid (
    id         SERIAL PRIMARY KEY,
    user_id    INTEGER        NOT NULL,
    order_id   VARCHAR(255),
    amount     NUMERIC(36, 8) NOT NULL,
    buy_price  NUMERIC(36, 8) NOT NULL,
    sell_price NUMERIC(36, 8) NOT NULL,
    side       VARCHAR(10)    NOT NULL,
    symbol     VARCHAR(20)    NOT NULL,
    status     VARCHAR(20)    NOT NULL DEFAULT 'new',
    is_paused  BOOLEAN        NOT NULL DEFAULT FALSE,
    source     VARCHAR(20)    NOT NULL DEFAULT 'manual',
    version    BIGINT         NOT NULL DEFAULT 0,
    created_at TIMESTAMPTZ    NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ    NOT NULL DEFAULT NOW()
);

CREATE INDEX idx_user_grid_user_symbol ON user_grid (user_id, symbol);
CREATE INDEX idx_user_grid_status      ON user_grid (status);

-- ── trade_history ─────────────────────────────────────────────────────────────
-- 已成交记录，profit = (sell_price - buy_price) * amount

CREATE TABLE trade_history (
    id         SERIAL PRIMARY KEY,
    user_id    INTEGER        NOT NULL,
    symbol     VARCHAR(20)    NOT NULL,
    order_id   VARCHAR(255),
    amount     NUMERIC(36, 8) NOT NULL,
    buy_price  NUMERIC(36, 8) NOT NULL,
    sell_price NUMERIC(36, 8) NOT NULL,
    profit     NUMERIC(36, 8),
    source     VARCHAR(50),
    source_id  INTEGER,
    filled_at  TIMESTAMPTZ
);

CREATE INDEX idx_trade_user_id     ON trade_history (user_id);
CREATE INDEX idx_trade_user_filled ON trade_history (user_id, filled_at);
