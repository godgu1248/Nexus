//! HTTP 层的共用小函数。
//!
//! 这里只放"没有任何业务含义、但每个 handler 都要写一遍"的东西。
//! 一旦某个函数开始问"这个人能不能做这件事"，它该去 `auth::policy`。

use axum::http::HeaderValue;
use axum::response::{IntoResponse, Redirect, Response};

/// 303 跳转，并同时给出 `HX-Redirect`。
///
/// 普通表单提交跟 303 走；如果哪天同一个表单被 `hx-post` 接管，htmx 会读 `HX-Redirect`
/// 做整页跳转，否则它会把目标页塞进当前容器里——登录成功后嵌一个页面碎片进去是很难查的 bug。
///
/// 这段实现原本在 `auth/handler.rs` 与 `forum/handler.rs` 各有一份，M5 加任务模块时
/// 变成第三份，于是收在这里：不是"顺手重构"，是三处必须行为一致的代码不能有三份。
pub fn redirect_to(to: &str) -> Response {
    let mut response = Redirect::to(to).into_response();
    if let Ok(value) = HeaderValue::from_str(to) {
        response.headers_mut().insert("hx-redirect", value);
    }
    response
}
