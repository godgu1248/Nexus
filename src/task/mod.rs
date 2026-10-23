//! M5 任务模块。
//!
//! 六文件职责（需求 §105）：
//! - `model.rs`    SQL 与行结构，不认识权限、不认识 HTTP
//! - `service.rs`   业务规则与事务，权限判断、审计落库都在这一层
//! - `handler.rs`   HTTP 入参出参、渲染
//! - `routes.rs`    路由表
//! - `mod.rs`       模块出口
//!
//! 依赖方向：`task → auth(policy) + shared`，外加**唯一一条**受控依赖
//! `task → forum`（仅「从议题生成任务」用，见需求 §122）。
//!
//! 进度：① model ✅ ② service ✅ ③ handler/routes ✅ ④ 模板 ✅ ⑤ 测试 进行中 ⑥ 验收记录

pub mod handler;
pub mod model;
pub mod routes;
pub mod service;
