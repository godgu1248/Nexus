//! 公告模块：管理员广播，成员逐条确认。
//!
//! 六文件模板里没有 `policy.rs` / `middleware.rs`：
//! 权限判定统一在 `auth::policy`（谁能发 = admin），请求级解析在 `auth::middleware`
//! （当前用户 + CSRF）。这里再抄一份只会出现"两处规则不一致"。

pub mod handler;
pub mod model;
pub mod routes;
pub mod service;
