//! 论坛模块：空间 → 议题 → 回复。
//!
//! 六文件职责（需求 §105）：
//!
//! | 文件 | 职责 |
//! |---|---|
//! | `model.rs` | 结构体 + SQL，**不含业务判断** |
//! | `service.rs` | 事务、审计、权限编排 |
//! | `policy.rs` | 论坛特有的可见性推导（复用 `auth::policy` 的四个函数） |
//! | `handler.rs` | 提取器、表单、渲染模板 |
//! | `routes.rs` | 路由表 |
//! | `middleware.rs` | 本模块暂无（M0 的 session 层已覆盖） |
//!
//! 依赖方向：`forum` → `auth` + `shared`，**不许反向**。
//! `auth` 不知道论坛存在——一旦反向，登录页就得跟着论坛的可见性改动一起发版。
//!
//! 进度：① model ✅ ｜ ② service ✅ ｜ ③ handler/routes ✅ ｜ ④ 模板 ✅ ｜ ⑤ 集成测试 ｜ ⑥ 文档

pub mod handler;
pub mod model;
pub mod routes;
pub mod service;

pub use model::{
    NewPost, NewSpace, NewThread, Post, Space, ThreadRow, create_post, create_space, create_thread,
    find_post, find_space_by_id, find_space_by_slug, find_thread, list_posts, list_spaces,
    list_threads, set_archived, set_resolved, soft_delete_post, touch_thread,
};
pub use routes::routes;
pub use service::{NewSpaceInput, NewThreadInput};
