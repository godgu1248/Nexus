//! 前厅路由表。
//!
//! 只做装配，不写业务。注意 `/lobby/forum/{id}` 与 `/lobby/kb/{id}` 是两套前缀，
//! 不依赖 matchit 的静态优先级去区分——它们本来就不冲突。

use axum::Router;
use axum::routing::{get, post};

use crate::public::handler;
use crate::state::AppState;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/lobby", get(handler::lobby_page))
        .route("/lobby/forum/{id}", get(handler::thread_page))
        .route("/lobby/kb/{id}", get(handler::doc_page))
        .route("/lobby/forum/{id}/comments", post(handler::thread_comment))
        .route("/lobby/kb/{id}/comments", post(handler::doc_comment))
        .route(
            "/lobby/comments/{comment_id}/delete",
            post(handler::comment_delete),
        )
        .route("/lobby/forum/{id}/like", post(handler::thread_like))
        .route("/lobby/kb/{id}/like", post(handler::doc_like))
        .route(
            "/lobby/submit",
            get(handler::submit_page).post(handler::submit_submit),
        )
        .route("/lobby/mine", get(handler::mine_page))
        .route("/lobby/queue", get(handler::queue_page))
        // 申请加入（M7）：注册外人自助申请，审批在 /admin/joins。
        .route("/join", get(handler::join_page).post(handler::join_submit))
}
