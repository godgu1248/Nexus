//! 媒体路由。
//!
//! 只有上传一条。图片本身不进 Rust：nginx 把 `UPLOAD_BASE_URL`（生产 `/uploads/`）
//! 直接指向 `UPLOAD_DIR`，Rust 不参与静态文件服务（DESIGN §10.3 的同一条约束）。

use axum::Router;
use axum::extract::DefaultBodyLimit;
use axum::routing::post;

use crate::state::AppState;

use super::handler;
use super::service::MAX_UPLOAD_BYTES;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/media/upload", post(handler::upload))
        // multipart 自带边界与字段头开销，给正文上限留 64 KiB 余量，
        // 否则一张正好 4 MB 的图会先被框架拒掉，用户看到的是"太大了"却查不出原因。
        .layer(DefaultBodyLimit::max(MAX_UPLOAD_BYTES + 64 * 1024))
}
