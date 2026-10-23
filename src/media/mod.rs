//! 图片上传与内容寻址存储（DESIGN §32）。
//!
//! 这个模块刻意没有 `policy.rs`：能上传的唯一条件是"已登录"，判断直接复用
//! `auth::policy`。新增一层"上传策略"只会让人以为规则比实际多。
//!
//! 也没有 `middleware.rs`：CSRF 与身份都来自 `auth::middleware` 的公共提取器。

pub mod handler;
pub mod model;
pub mod routes;
pub mod service;

pub use routes::routes;
