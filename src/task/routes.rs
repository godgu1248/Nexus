//! 任务路由。
//!
//! 全部 POST 都带 CSRF（表单里的 `_csrf`），全部写操作先过 `require_reader`
//! 再过 service 的权限判定——两层都要有：前者挡"这个模块对你不可见"，
//! 后者挡"这个动作你不能做"。

use axum::Router;
use axum::routing::{get, post};

use crate::state::AppState;
use crate::task::handler;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/tasks", get(handler::list_page))
        .route(
            "/tasks/new",
            get(handler::new_page).post(handler::new_submit),
        )
        .route(
            "/tasks/from-thread/{thread_id}",
            post(handler::from_thread_submit),
        )
        .route("/tasks/{id}", get(handler::detail_page))
        .route("/tasks/{id}/status", post(handler::status_submit))
        .route("/tasks/{id}/assign", post(handler::assign_submit))
        .route("/tasks/{id}/delete", post(handler::delete_submit))
}
