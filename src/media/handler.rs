//! 图片上传接口。
//!
//! 只有一个端点：`POST /media/upload`（`multipart/form-data`，字段名 `file`）。
//! 编辑器拿到 JSON 里的 `url` 后，把 `<img src=…>` 插到光标处。
//!
//! 为什么不用表单回跳（303 + 整页刷新）：插入图片是"往正文中间塞一段"，
//! 刷新会丢掉用户已经敲的字。上传走 XHR，页面不动。

use axum::Json;
use axum::extract::{Multipart, State};
use axum::http::HeaderMap;
use serde_json::{Value, json};
use tower_sessions::Session;

use crate::auth::middleware::{self, ClientIp, MaybeUser};
use crate::error::AppError;
use crate::state::AppState;

use super::service;

/// 编辑器在 `X-CSRF-Token` 里带上会话令牌（`static/editor.js` 负责）。
const CSRF_HEADER: &str = "x-csrf-token";

pub async fn upload(
    State(state): State<AppState>,
    session: Session,
    user: MaybeUser,
    client_ip: ClientIp,
    headers: HeaderMap,
    mut multipart: Multipart,
) -> Result<Json<Value>, AppError> {
    let Some(current) = user.user() else {
        return Err(AppError::LoginRequired {
            next: "/".to_owned(),
        });
    };

    let token = headers
        .get(CSRF_HEADER)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    middleware::require_csrf(&session, token.as_deref()).await?;

    let mut payload: Option<Vec<u8>> = None;
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|err| AppError::bad_request(format!("上传内容读不出来：{err}")))?
    {
        if field.name() != Some("file") {
            continue;
        }
        let bytes = field
            .bytes()
            .await
            .map_err(|err| AppError::bad_request(format!("图片读取失败：{err}")))?;
        payload = Some(bytes.to_vec());
        break;
    }

    let Some(data) = payload else {
        return Err(AppError::bad_request("请求里没有 file 字段"));
    };

    let stored = service::save_image(
        &state.pool,
        &state.config,
        current,
        client_ip.0.as_deref(),
        &data,
    )
    .await?;

    Ok(Json(json!({
        "url": stored.url,
        "bytes": stored.bytes,
        "reused": stored.reused,
    })))
}
