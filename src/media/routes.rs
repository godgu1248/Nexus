//! 媒体路由。
//!
//! 上传两条（图片 / Word 文档）＋ 一个查看器页面。图片与文档本身不进 Rust：
//! nginx 把 `UPLOAD_BASE_URL`（生产 `/uploads/`）直接指向 `UPLOAD_DIR`，
//! Rust 不参与静态文件服务（DESIGN §10.3 的同一条约束）。
//!
//! 查看器是个例外——它必须渲染 HTML，所以由 Rust 出页面；但页面里的正文来自
//! 上传时就已净化并入库的 `media.html`，渲染时不再解析文件。

use axum::Router;
use axum::extract::DefaultBodyLimit;
use axum::routing::{get, post};

use crate::state::AppState;

use super::handler;
use super::service::MAX_UPLOAD_BYTES;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/doc/{sha}", get(handler::view_doc))
        .merge(
            Router::new()
                .route("/media/upload", post(handler::upload))
                .route("/media/docx", post(handler::upload_docx))
                // multipart 自带边界与字段头开销，给正文上限留 64 KiB 余量，
                // 否则一张正好 4 MB 的图会先被框架拒掉，用户看到的是"太大了"却查不出原因。
                // 两条上传共用一层上限，取两者中较大的那个（文档 8 MiB）。
                .layer(DefaultBodyLimit::max(
                    crate::shared::docx::MAX_DOCX_BYTES.max(MAX_UPLOAD_BYTES) + 64 * 1024,
                )),
        )
}
