//! 论坛路由表。
//!
//! 路径约定：`/forum/t/{id}` 与 `/forum/{slug}` 同时存在，靠 matchit 的静态优先规则区分
//! （`t` 是字面量段，永远赢过动态段）。这也是为什么议题详情不用 `/forum/{slug}/{id}`——
//! 两层动态段在有很多分区时容易误配，而且 URL 会变得很长。
//!
//! 所有写操作都是 POST：GET 只读，这是 CSRF 防护能成立的前提。

use axum::Router;
use axum::routing::{get, post};

use crate::forum::handler;
use crate::state::AppState;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/forum", get(handler::spaces_page))
        .route(
            "/forum/new",
            get(handler::new_space_page).post(handler::new_space_submit),
        )
        .route("/forum/{slug}", get(handler::threads_page))
        .route(
            "/forum/{slug}/new",
            get(handler::new_thread_page).post(handler::new_thread_submit),
        )
        .route("/forum/t/{id}", get(handler::thread_page))
        .route("/forum/t/{id}/reply", post(handler::reply_submit))
        .route("/forum/t/{id}/resolve", post(handler::resolve_submit))
        .route("/forum/t/{id}/archive", post(handler::archive_submit))
        .route("/forum/p/{id}/delete", post(handler::delete_post_submit))
        // 改可见性绑在分区上：议题没有独立可见性（§21.9 C2），
        // 所以这个入口只有分区一个，避免"同一件事两个地方能改"。
        .route("/forum/{slug}/visibility", post(handler::visibility_submit))
}
