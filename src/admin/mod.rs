//! `admin` 模块：管理区（M7）。
//!
//! 职责边界（需求 L123）：`admin → 各模块 service`。它是**唯一的编排层**：
//! 成员审批要同时改 `join_requests` 与 `users`，投稿通过要同时建档案草稿并回填投稿行——
//! 这些动作跨了两个聚合，只有这里能把它们串进一个事务或一个流程里。
//!
//! 六文件职责：
//!
//! | 文件 | 职责 | 禁止 |
//! | --- | --- | --- |
//! | `model.rs` | 管理区的只读查询（成员列表、审计列表） | 业务规则、写业务表 |
//! | `service.rs` | 编排：审批、改角色/状态、审核投稿、审计页 | Axum 类型 |
//! | `policy.rs` | —— 权限全部复用 `auth::policy::can_admin` | —— |
//! | `handler.rs` | 解析请求、调 service、渲染模板 | 手写权限判断 |
//! | `routes.rs` | 组装 Router | 业务逻辑 |
//! | `middleware.rs` | —— 复用 `auth::middleware` | —— |
//!
//! 为什么没有 `policy.rs`：管理区的权限只有一条——"是不是 admin"，
//! `auth::policy::can_admin` 已经覆盖。在这里再包一层只会让"两处判断谁先谁后"变成新问题。
//!
//! 为什么写操作不在 `model.rs`：本模块**不拥有**任何业务表。成员行是 auth 的、
//! 申请与投稿是 public 的、草稿是 kb 的。写它们要通过各自的 model/service，
//! 见 `service.rs` 里每个函数的注释。

pub mod handler;
pub mod model;
pub mod routes;
pub mod service;

pub use routes::routes;
