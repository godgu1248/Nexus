//! 公告路由。红点与横幅都是 `GET` 片段，写操作一律 `POST`。

use axum::Router;
use axum::routing::{get, post};

use crate::notice::handler;
use crate::state::AppState;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/notice", get(handler::list_page))
        .route("/notice/dot", get(handler::dot))
        .route("/notice/banner", get(handler::banner))
        .route(
            "/notice/new",
            get(handler::new_page).post(handler::new_submit),
        )
        .route("/notice/{id}/ack", post(handler::ack_submit))
        .route("/notice/{id}/delete", post(handler::delete_submit))
}
