//! 管理区路由（M7）。
//!
//! 都是"页面 GET + 动作 POST"的成对结构：动作用 POST，且都带 `_csrf`。
//! 没有 DELETE 动词——浏览器表单只支持 GET/POST，靠 htmx 或 fetch 才能发 DELETE，
//! 而这个站点在无 JS 时也必须能用。

use axum::Router;
use axum::routing::{get, post};

use crate::admin::handler;
use crate::state::AppState;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/admin", get(handler::overview_page))
        .route("/admin/members", get(handler::members_page))
        .route(
            "/admin/members/{id}/role",
            post(handler::member_role_submit),
        )
        .route(
            "/admin/members/{id}/status",
            post(handler::member_status_submit),
        )
        .route(
            "/admin/members/{id}/delete",
            post(handler::member_delete_submit),
        )
        .route("/admin/joins", get(handler::joins_page))
        .route(
            "/admin/joins/{id}/approve",
            post(handler::join_approve_submit),
        )
        .route(
            "/admin/joins/{id}/reject",
            post(handler::join_reject_submit),
        )
        .route("/admin/submissions", get(handler::submissions_page))
        .route(
            "/admin/submissions/{id}/approve",
            post(handler::submission_approve_submit),
        )
        .route(
            "/admin/submissions/{id}/reject",
            post(handler::submission_reject_submit),
        )
        .route("/admin/audit", get(handler::audit_page))
}
