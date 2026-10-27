//! 图片上传接口。
//!
//! 只有一个端点：`POST /media/upload`（`multipart/form-data`，字段名 `file`）。
//! 编辑器拿到 JSON 里的 `url` 后，把 `<img src=…>` 插到光标处。
//!
//! 为什么不用表单回跳（303 + 整页刷新）：插入图片是"往正文中间塞一段"，
//! 刷新会丢掉用户已经敲的字。上传走 XHR，页面不动。

use axum::Json;
use axum::extract::{Multipart, Path, State};
use axum::http::HeaderMap;
use serde_json::{Value, json};
use tower_sessions::Session;

use crate::auth::middleware::{self, ClientIp, MaybeUser, MaybeUserRow};
use crate::error::AppError;
use crate::state::AppState;

use super::model;
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

    // 上传会落盘：4 MiB × 不限次数 = 把 40 GB 磁盘填满。
    // 键用用户 id（登录是硬前置），额度按人算。
    state.rate.guard(
        crate::shared::ratelimit::Kind::Upload,
        &format!("u{}", current.id),
    )?;

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

/// `POST /media/docx`：上传 Word 文档，返回查看器地址。
///
/// 与图片上传分开成两个端点而不是靠扩展名分流：两者的上限（8 MiB vs 4 MiB）、
/// 成功后的用法（插入 `<img>` vs 插入 `<iframe>`）都不一样，合在一起只会让
/// 「为什么这张图被拒绝了」变成一道要读代码才能回答的题。
pub async fn upload_docx(
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

    state.rate.guard(
        crate::shared::ratelimit::Kind::Upload,
        &format!("u{}", current.id),
    )?;

    let mut payload: Option<(Vec<u8>, Option<String>)> = None;
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|err| AppError::bad_request(format!("上传内容读不出来：{err}")))?
    {
        if field.name() != Some("file") {
            continue;
        }
        let file_name = field.file_name().map(str::to_owned);
        let bytes = field
            .bytes()
            .await
            .map_err(|err| AppError::bad_request(format!("文档读取失败：{err}")))?;
        payload = Some((bytes.to_vec(), file_name));
        break;
    }

    let Some((data, file_name)) = payload else {
        return Err(AppError::bad_request("请求里没有 file 字段"));
    };

    let stored = service::save_docx(
        &state.pool,
        &state.config,
        current,
        client_ip.0.as_deref(),
        file_name.as_deref(),
        &data,
    )
    .await?;

    Ok(Json(json!({
        // `html` 是编辑器真正要用的东西：就地插进正文，读起来像一份文档。
        // `url` 保留给"在新标签打开原文"的链接（正文末尾的来源行）。
        "html": stored.html,
        "url": stored.viewer_url,
        "title": stored.title,
        "bytes": stored.bytes,
        "reused": stored.reused,
        "notes": stored.notes,
    })))
}

/// Word 查看器页面。
///
/// 不做登录校验：上传走的是登录 + CSRF，但查看器要被嵌进正文里，而正文本身的
/// 可见性由论坛/档案/公告各自的策略决定。在这里再加一道登录，会让"公开议题里的
/// 附件来宾看不见"——与 `/uploads/` 图片（nginx 直出、同样不做登录）不一致。
/// 代价是"知道 sha256 就能看"，与图片的自洽取舍一样记在 DESIGN §41 的已知债务里。
pub async fn view_doc(
    State(state): State<AppState>,
    session: Session,
    user: MaybeUser,
    row: MaybeUserRow,
    Path(sha): Path<String>,
) -> Result<DocTemplate, AppError> {
    if !is_sha256_hex(&sha) {
        return Err(AppError::NotFound);
    }

    let Some(media) = model::find_by_sha(&state.pool, &sha).await? else {
        return Err(AppError::NotFound);
    };
    if media.kind != "docx" {
        return Err(AppError::NotFound);
    }

    let csrf_token = middleware::ensure_csrf(&session).await?;
    let look = row.look();
    let html = media
        .html
        .as_deref()
        .map(crate::shared::richtext::sanitize)
        .map(crate::shared::SafeHtml::into_string)
        .unwrap_or_default();
    let title = media
        .title
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "Word 文档".to_owned());

    Ok(DocTemplate {
        page_title: format!("{title} · NEXUS"),
        static_base: state.config.static_base_url.clone(),
        csrf_token,
        nav_display_name: row.display_name().to_owned(),
        nav_is_admin: user.is_admin(),
        nav_is_guest: crate::auth::policy::is_guest(user.user()),
        look,
        nav_active: "",
        title,
        html,
        size_label: human_size(media.bytes),
        back_href: "/lobby".to_owned(),
    })
}

fn is_sha256_hex(value: &str) -> bool {
    value.len() == 64 && value.chars().all(|c| c.is_ascii_hexdigit())
}

/// 人类可读的体积。
///
/// 除法会把小于 1 KB 的文件显示成 `0 KB`（一份几百字节的 docx 在页面上写着「0 KB」，
/// 看着像上传失败或者解析出了空文档），所以小于 1 KB 直接报字节数。
fn human_size(bytes: u32) -> String {
    if bytes >= 1024 * 1024 {
        format!("{:.1} MB", f64::from(bytes) / (1024.0 * 1024.0))
    } else if bytes >= 1024 {
        format!("{:.1} KB", f64::from(bytes) / 1024.0)
    } else {
        format!("{bytes} 字节")
    }
}

/// Word 查看器页面的模板上下文。
///
/// `nav_active` 是空串：`/doc/{sha}` 不属于任何一个导航分区，
/// 让导航一个都不高亮，比硬塞进"前厅"更诚实。
#[derive(askama::Template, askama_web::WebTemplate)]
#[template(path = "media/doc.html")]
pub struct DocTemplate {
    pub page_title: String,
    pub static_base: String,
    pub csrf_token: String,
    pub nav_display_name: String,
    pub nav_is_admin: bool,
    pub nav_is_guest: bool,
    pub look: crate::shared::appearance::Appearance,
    pub nav_active: &'static str,
    pub title: String,
    pub html: String,
    pub size_label: String,
    pub back_href: String,
}

#[cfg(test)]
mod tests {
    use super::human_size;

    #[test]
    fn small_files_are_not_rounded_to_zero() {
        assert_eq!(human_size(0), "0 字节");
        assert_eq!(human_size(395), "395 字节");
        assert_eq!(human_size(1023), "1023 字节");
        assert_eq!(human_size(1024), "1.0 KB");
        assert_eq!(human_size(3072), "3.0 KB");
        assert_eq!(human_size(1024 * 1024), "1.0 MB");
    }
}
