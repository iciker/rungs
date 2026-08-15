-- 引擎热路径索引
--
-- find_by_symbol / get_active_symbols 是每 3 秒一轮、每个 symbol 都要跑的查询，
-- 谓词为 (symbol, status != 'deleted', is_paused = FALSE)。此前无任何索引支撑，
-- 只能全表扫描；而软删除的行永不清理，表只增不减，扫描代价会随时间单调上升。
--
-- 用部分索引：只收录仍在调度的行，索引体积与"活跃网格数"成正比，
-- 而不是与"历史上创建过的网格总数"成正比。
CREATE INDEX IF NOT EXISTS idx_user_grid_live
    ON user_grid (symbol)
    WHERE status <> 'deleted' AND is_paused = FALSE;

-- 按用户查询网格列表（GET /api/user/grids）的支撑索引
CREATE INDEX IF NOT EXISTS idx_user_grid_user_active
    ON user_grid (user_id, symbol)
    WHERE status <> 'deleted';
