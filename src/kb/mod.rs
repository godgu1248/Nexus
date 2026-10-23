//! 档案（知识库）模块。
//!
//! 六文件职责（与需求 §105 一致）：
//!
//! | 文件 | 职责 | 禁止 |
//! | --- | --- | --- |
//! | `model.rs` | 行结构体 + SQL | 不做权限判断、不写审计 |
//! | `service.rs` | 业务规则 + 事务 + 审计 | 不碰 HTTP 类型 |
//! | `handler.rs` | 提取参数、调 service、渲染模板 | 不写 SQL、不直连 audit |
//! | `routes.rs` | 路由表 | 不写业务 |
//! | `mod.rs` | 模块声明与重导出 | 不写逻辑 |
//!
//! 本模块**不自建 policy.rs**：可见性判定全部收敛在 `crate::auth::policy`，
//! 与 forum / group 保持一致。多一个 policy 就多一个口径，迟早会分叉。
//!
//! 依赖方向：`kb → auth + shared + forum`（forum 只用于「从议题沉淀」只读取议题）。

pub mod handler;
pub mod model;
pub mod routes;
pub mod service;

pub use service::{DocInput, MAX_DEPTH};
