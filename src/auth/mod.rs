//! 身份模块：注册、登录、登出、会话注入，以及全站**唯一**的权限判定入口。
//!
//! 六文件划分（需求硬约束）与本批进度：
//!
//! | 文件 | 职责 | 状态 |
//! |---|---|---|
//! | `model.rs` | `User`/`Role`/`UserStatus` 与查库函数 | ✅ 批次① |
//! | `policy.rs` | `can_view`/`can_write`/`can_moderate`/`can_admin` | ✅ 批次② |
//! | `service.rs` | 密码哈希、注册、登录、锁定、登出 | ✅ 批次③ |
//! | `middleware.rs` | 读 session → 查库 → 注入 `CurrentUser` | 批次④ |
//! | `handler.rs` | 表单处理与模板上下文 | 批次④ |
//! | `routes.rs` | 路由表 | 批次⑤ |
//!
//! 依赖方向：`auth → shared`。**反向禁止**——`shared` 一旦 `use crate::auth`，
//! 它就从"无业务判断的小零件"变成"带身份概念的中间层"，之后任何模块都会想往里塞权限代码。

pub mod handler;
pub mod middleware;
pub mod model;
pub mod policy;
pub mod routes;
pub mod service;
pub mod settings;

pub use model::{NewUser, Role, User, UserStatus};
pub use policy::{ContentCtx, CurrentUser, can_admin, can_moderate, can_view, can_write};
