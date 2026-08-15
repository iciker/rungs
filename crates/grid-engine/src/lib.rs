//! 网格交易核心引擎
//!
//! 架构：每个交易对（symbol）独立 tokio task，循环处理该 symbol 下所有用户网格。
//! 订单状态机：new → open → closed → new（反向）
//! 并发安全：乐观锁（user_grid.version 字段）

pub mod engine;
pub mod strategy;

pub use strategy::{allocate_grids, build_auto_grids, Allocation, AutoGridLayout};
