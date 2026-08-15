-- 将交易状态机的不变量下沉到数据库边界，拒绝会被引擎误解释的非法数据。

ALTER TABLE user_grid
    ADD CONSTRAINT user_grid_side_valid
        CHECK (side IN ('buy', 'sell')),
    ADD CONSTRAINT user_grid_status_valid
        CHECK (status IN ('new', 'open', 'closed', 'deleted')),
    ADD CONSTRAINT user_grid_source_valid
        CHECK (source IN ('manual', 'auto')),
    ADD CONSTRAINT user_grid_amount_positive
        CHECK (amount > 0),
    ADD CONSTRAINT user_grid_prices_positive
        CHECK (buy_price > 0 AND sell_price > 0),
    ADD CONSTRAINT user_grid_price_order_valid
        CHECK (buy_price < sell_price);

ALTER TABLE trade_history
    ADD CONSTRAINT trade_history_amount_positive
        CHECK (amount > 0),
    ADD CONSTRAINT trade_history_prices_positive
        CHECK (buy_price > 0 AND sell_price > 0),
    ADD CONSTRAINT trade_history_price_order_valid
        CHECK (buy_price < sell_price);
