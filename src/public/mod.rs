//! `public` 模块：前厅（公开区）。
//!
//! 职责边界（需求 L123）：`public → auth(policy) + shared + forum/kb/task（只读）`。
//! 也就是说：**读别人，写自己**。本模块对 forum/kb/task 只调它们的 model 查询函数，
//! 从不写它们的表；所有写操作只落在 `public_comments` / `public_likes` / `public_submissions`。
//!
//! 六文件职责：
//!
//! | 文件 | 职责 | 禁止 |
//! | --- | --- | --- |
//! | `model.rs` | 表结构体、SQLx 查询 | 业务规则、HTTP 类型 |
//! | `service.rs` | 业务规则、事务编排、调 audit | Axum 类型 |
//! | `policy.rs` | —— 本模块不新增策略函数 | —— |
//! | `handler.rs` | 解析请求、调 service、渲染模板 | 手写 SQL 权限条件 |
//! | `routes.rs` | 组装 Router | 业务逻辑 |
//! | `middleware.rs` | —— 复用 `auth::middleware` | —— |
//!
//! 为什么没有 `policy.rs` 和 `middleware.rs`：前厅的权限只有两类——
//! 「公开内容谁能看」由 `auth::policy::can_view` 决定，「评论/点赞/投稿要不要登录」
//! 由 `auth::middleware::MaybeUser` 决定。在这里再写一份策略函数等于把同一套规则抄两遍，
//! 而抄两遍的规则一定会分叉。详情见 DESIGN §24.3。

pub mod handler;
pub mod model;
pub mod routes;
pub mod service;

pub use model::{CommentTarget, SubmissionStatus};
pub use routes::routes;
pub use service::PublicTarget;
