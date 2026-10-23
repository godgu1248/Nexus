//! 档案路由表。
//!
//! 路由只做拼装，不含逻辑。`/kb/from-thread/{thread_id}` 必须写在 `/kb/{id}` 之前的意义在于
//! 可读性——axum 0.8 的 matchit 会优先匹配静态段，顺序不影响结果，但人读的时候会安心。

use axum::Router;
use axum::routing::{get, post};

use crate::state::AppState;

use super::handler;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/kb", get(handler::list_page))
        .route("/kb/search", get(handler::search_page))
        .route("/kb/new", get(handler::new_page).post(handler::new_submit))
        .route(
            "/kb/from-thread/{thread_id}",
            post(handler::from_thread_submit),
        )
        .route("/kb/{id}", get(handler::doc_page))
        .route(
            "/kb/{id}/edit",
            get(handler::edit_page).post(handler::edit_submit),
        )
        .route("/kb/{id}/pin", post(handler::pin_submit))
        .route("/kb/{id}/publish", post(handler::publish_submit))
        .route("/kb/{id}/delete", post(handler::delete_submit))
}
