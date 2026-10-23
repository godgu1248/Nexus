//! 小组模块。
//!
//! 职责边界：小组只解决一件事——**谁能看见 group 可见性的内容**。
//! 它不提供组内角色、不管内容归属、不做邀请审批（M7 统一做）。
//!
//! 依赖方向：`group → auth + shared`（组要知道用户是谁，审计走 shared）。
//! 反向依赖一律禁止：`auth` 不认识 `group`，`forum` 只在 service 层调用 `group` 的查询。
//!
//! 六个文件的分工见仓库根 DESIGN.md §105 的约定；本模块 M3 用到的四个：
//!
//! | 文件 | 职责 |
//! | --- | --- |
//! | `model.rs` | SQL 与行映射，不判权限 |
//! | `service.rs` | 事务、审计、权限判定 |
//! | `handler.rs` | 表单、模板上下文、跳转 |
//! | `routes.rs` | 路径表 |
//!
//! 进度：
//! - [x] ① `0003_groups.sql` + `model.rs`
//! - [ ] ② `service.rs`
//! - [ ] ③ `middleware` 加载 `group_ids` + policy group 分支
//! - [ ] ④ `handler.rs` + `routes.rs` + 模板
//! - [ ] ⑤ 论坛可见性编辑入口
//! - [ ] ⑥ `tests/group_flow.rs`

pub mod handler;
pub mod model;
pub mod routes;
pub mod service;

pub use model::{Group, Member, NewGroup};
pub use routes::routes;
pub use service::{NewGroupInput, add_member, create_group, delete_group, remove_member};
