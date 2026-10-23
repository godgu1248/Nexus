//! 小组的路径表。
//!
//! 与论坛的路径不冲突：论坛在 `/forum/...` 下，小组在 `/groups` 下。
//! `/groups/new` 与 `/groups/{slug}/members` 能共存，靠的是 matchit 的静态段优先——
//! 静态的 `new` 永远先匹配，不会再被 `{slug}` 抢走。

use axum::Router;
use axum::routing::get;

use crate::group::handler;
use crate::state::AppState;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/groups", get(handler::list_page))
        .route(
            "/groups/new",
            get(handler::new_page).post(handler::new_submit),
        )
        .route(
            "/groups/{slug}/members",
            get(handler::members_page).post(handler::members_submit),
        )
}
