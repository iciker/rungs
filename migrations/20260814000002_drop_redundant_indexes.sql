-- 清理冗余索引
--
-- 每个索引都要在每次 UPDATE 时维护。user_grid 的 status 几乎每次状态流转都变，
-- 索引越多，单次成交的写放大越明显。以下三个都不承担任何查询：
--
-- 1. idx_user_grid_user_symbol (user_id, symbol)
--    被 20260814000001 的部分索引 idx_user_grid_user_active 完全覆盖 ——
--    同列同序，且所有读路径都带 status <> 'deleted' 谓词。
--
-- 2. idx_user_grid_status (status)
--    所有查询都是 `status != 'deleted'`（否定条件），4 个取值的低基数列，
--    规划器不会选用它。
--
-- 3. idx_trade_user_id (user_id)
--    是 idx_trade_user_filled (user_id, filled_at) 的严格前缀。
DROP INDEX IF EXISTS idx_user_grid_user_symbol;
DROP INDEX IF EXISTS idx_user_grid_status;
DROP INDEX IF EXISTS idx_trade_user_id;
