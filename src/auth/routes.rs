//! 身份模块路由表。
//!
//! 只有三个路径、五个方法。**登出是 POST**，不是 GET：`<a href="/logout">` 会被浏览器预取、
//! 被邮件客户端预览、被爬虫抓取——任何一个都会把人莫名其妙踢下线，而且这类 bug 只在
//! 某些客户端上复现，极难排查。
//!
//! 路由表保持"一个路径一行"，是为了让"哪些入口存在"这件事可以一眼看完。M7 加管理路由时
//! 不要在这里堆筛选逻辑，那属于 `admin/routes.rs`。

use axum::Router;
use axum::routing::{get, post};

use crate::auth::handler;
use crate::auth::settings;
use crate::state::AppState;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/login",
            get(handler::login_page).post(handler::login_submit),
        )
        .route(
            "/register",
            get(handler::register_page).post(handler::register_submit),
        )
        .route("/logout", post(handler::logout_submit))
        // 个人设置：GET 看色卡，POST 存主题。都要求登录（未登录 303 去 /login）。
        .route(
            "/settings",
            get(settings::settings_page).post(settings::settings_submit),
        )
}
