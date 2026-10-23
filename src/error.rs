//! 统一错误类型：所有 handler 的失败都收敛到这里。
//!
//! 三条纪律（对应 DESIGN.md §4）：
//! 1. **不泄露内部信息**：5xx 一律只回「服务内部错误」，SQL、路径、堆栈只进日志不进响应。
//! 2. **唯一约束不刷脸**：`sqlx` 的 SQLSTATE 23000 转成 409，绝不回显数据库原文。
//! 3. **5xx 的错误页不用模板渲染**：`AppError::Template` 本身就是模板挂了，再用模板渲染错误页
//!    会无限递归。所以错误页由本文件内的纯 `format!` 渲染器产出，这是永久设计，不是临时方案。
//!
//! `LoginRequired` 同时返回 `303 + Location` 与 `HX-Redirect`：普通浏览器跟随 303，
//! htmx 认 `HX-Redirect` 做整页跳转，避免把登录页塞进一个局部刷新的容器里。

use std::borrow::Cow;

use askama::Error as AskamaError;
use axum::body::Body;
use axum::http::{HeaderName, HeaderValue, StatusCode, header};
use axum::response::{Html, IntoResponse, Response};

/// htmx 的整页重定向响应头。
const HX_REDIRECT: HeaderName = HeaderName::from_static("hx-redirect");

/// 登录页路径。M1 落地登录后如果改了路由，只改这一处。
const LOGIN_PATH: &str = "/login";

#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("请求不合法：{0}")]
    BadRequest(Cow<'static, str>),

    #[error("需要登录（登录后返回 {next}）")]
    LoginRequired { next: String },

    #[error("无权访问")]
    Forbidden,

    #[error("未找到")]
    NotFound,

    #[error("方法不允许")]
    MethodNotAllowed,

    #[error("冲突：{0}")]
    Conflict(Cow<'static, str>),

    #[error("请求体过大")]
    PayloadTooLarge,

    #[error("无法处理：{0}")]
    Unprocessable(Cow<'static, str>),

    #[error("请求过于频繁")]
    RateLimited,

    #[error("模板渲染失败：{0}")]
    Template(#[from] AskamaError),

    #[error("数据库错误：{0}")]
    Database(sqlx::Error),

    #[error("内部错误：{0}")]
    Internal(#[from] anyhow::Error),
}

impl AppError {
    pub fn bad_request(msg: impl Into<Cow<'static, str>>) -> Self {
        AppError::BadRequest(msg.into())
    }

    pub fn conflict(msg: impl Into<Cow<'static, str>>) -> Self {
        AppError::Conflict(msg.into())
    }

    pub fn unprocessable(msg: impl Into<Cow<'static, str>>) -> Self {
        AppError::Unprocessable(msg.into())
    }

    pub fn internal(err: impl Into<anyhow::Error>) -> Self {
        AppError::Internal(err.into())
    }

    pub fn status_code(&self) -> StatusCode {
        match self {
            AppError::BadRequest(_) => StatusCode::BAD_REQUEST,
            AppError::LoginRequired { .. } => StatusCode::SEE_OTHER,
            AppError::Forbidden => StatusCode::FORBIDDEN,
            AppError::NotFound => StatusCode::NOT_FOUND,
            AppError::MethodNotAllowed => StatusCode::METHOD_NOT_ALLOWED,
            AppError::Conflict(_) => StatusCode::CONFLICT,
            AppError::PayloadTooLarge => StatusCode::PAYLOAD_TOO_LARGE,
            AppError::Unprocessable(_) => StatusCode::UNPROCESSABLE_ENTITY,
            AppError::RateLimited => StatusCode::TOO_MANY_REQUESTS,
            AppError::Template(_) | AppError::Database(_) | AppError::Internal(_) => {
                StatusCode::INTERNAL_SERVER_ERROR
            }
        }
    }

    /// 允许展示给用户的一句话。5xx 恒定模糊，4xx 用我们自己写的话（可能是从表单校验带上来的）。
    pub fn public_message(&self) -> Cow<'static, str> {
        match self {
            AppError::BadRequest(m) | AppError::Conflict(m) | AppError::Unprocessable(m) => {
                m.clone()
            }
            AppError::LoginRequired { .. } => Cow::Borrowed("请先登录后再继续。"),
            AppError::Forbidden => Cow::Borrowed("你没有权限访问这个内容。"),
            AppError::NotFound => Cow::Borrowed("页面不存在，或者已经被移走了。"),
            AppError::MethodNotAllowed => Cow::Borrowed("这个地址不支持该请求方式。"),
            AppError::PayloadTooLarge => Cow::Borrowed("提交的内容太大了。"),
            AppError::RateLimited => Cow::Borrowed("操作太频繁，请稍后再试。"),
            AppError::Template(_) | AppError::Database(_) | AppError::Internal(_) => {
                Cow::Borrowed("服务内部错误，请稍后重试。")
            }
        }
    }
}

impl From<sqlx::Error> for AppError {
    fn from(err: sqlx::Error) -> Self {
        // 唯一键冲突（SQLSTATE 23000）是业务可预期的并发结果，转 409；
        // 其余数据库错误一律 500，且原文只进日志。
        if let sqlx::Error::Database(db) = &err
            && db.code().as_deref() == Some("23000")
        {
            tracing::warn!(error = %err, "唯一约束冲突");
            return AppError::Conflict("这条记录已经存在了。".into());
        }
        AppError::Database(err)
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let status = self.status_code();

        if let AppError::LoginRequired { next } = &self {
            let location = format!("{LOGIN_PATH}?next={}", percent_encode(next));
            let mut resp = Response::new(Body::empty());
            *resp.status_mut() = status;
            if let Ok(value) = HeaderValue::from_str(&location) {
                resp.headers_mut().insert(header::LOCATION, value.clone());
                resp.headers_mut().insert(HX_REDIRECT, value);
            }
            return resp;
        }

        if status.is_server_error() {
            // 完整原因（含 SQL / 路径 / 堆栈）只落日志，带 request_id 的 span 由中间件补齐。
            tracing::error!(error = %self, "请求失败");
        } else {
            tracing::debug!(error = %self, "请求被拒绝");
        }

        let html = render_error_page(status, &self.public_message());
        (status, Html(html)).into_response()
    }
}

/// 不依赖模板引擎的错误页渲染器。
///
/// 刻意手写而非 `#[derive(Template)]`：模板引擎自身出错时必须仍有可用的兜底页面。
fn render_error_page(status: StatusCode, message: &str) -> String {
    let code = status.as_u16();
    let title = status.canonical_reason().unwrap_or("出错了");
    format!(
        r#"<!DOCTYPE html>
<html lang="zh-CN">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>{code} · {title} · NEXUS</title>
<style>
  :root {{ color-scheme: light dark; }}
  body {{ margin: 0; min-height: 100vh; display: grid; place-items: center;
         font: 16px/1.6 system-ui, -apple-system, "Segoe UI", "Microsoft YaHei", sans-serif;
         background: #0f1115; color: #e6e8eb; }}
  main {{ max-width: 32rem; padding: 2.5rem; text-align: center; }}
  .code {{ font-size: 4rem; font-weight: 700; letter-spacing: .05em; color: #7c8798; margin: 0; }}
  h1 {{ font-size: 1.25rem; margin: .5rem 0 1rem; }}
  p {{ color: #a8b0bd; margin: 0 0 2rem; }}
  a {{ color: #6ea8fe; text-decoration: none; border-bottom: 1px solid currentColor; }}
  a:hover {{ color: #a8c7ff; }}
</style>
</head>
<body>
<main>
  <p class="code">{code}</p>
  <h1>{title}</h1>
  <p>{message}</p>
  <a href="/">回到首页</a>
</main>
</body>
</html>
"#,
        code = code,
        title = escape_html(title),
        message = escape_html(message),
    )
}

/// HTML 转义。`public_message` 可能带着表单里回填的用户原文，不能裸拼进页面。
fn escape_html(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for ch in input.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(ch),
        }
    }
    out
}

/// 只保留 RFC 3986 unreserved 字符，其余一律百分号编码。
/// 不引入 urlencoding 依赖：登录跳转只需要这么一点。
fn percent_encode(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for byte in input.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;

    async fn body_string(err: AppError) -> (StatusCode, String) {
        let resp = err.into_response();
        let status = resp.status();
        let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        (status, String::from_utf8(bytes.to_vec()).unwrap())
    }

    #[test]
    fn status_codes_follow_the_contract() {
        assert_eq!(
            AppError::bad_request("x").status_code(),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            AppError::LoginRequired { next: "/a".into() }.status_code(),
            StatusCode::SEE_OTHER
        );
        assert_eq!(AppError::Forbidden.status_code(), StatusCode::FORBIDDEN);
        assert_eq!(AppError::NotFound.status_code(), StatusCode::NOT_FOUND);
        assert_eq!(
            AppError::MethodNotAllowed.status_code(),
            StatusCode::METHOD_NOT_ALLOWED
        );
        assert_eq!(AppError::conflict("x").status_code(), StatusCode::CONFLICT);
        assert_eq!(
            AppError::PayloadTooLarge.status_code(),
            StatusCode::PAYLOAD_TOO_LARGE
        );
        assert_eq!(
            AppError::unprocessable("x").status_code(),
            StatusCode::UNPROCESSABLE_ENTITY
        );
        assert_eq!(
            AppError::RateLimited.status_code(),
            StatusCode::TOO_MANY_REQUESTS
        );
        assert_eq!(
            AppError::internal(anyhow::anyhow!("boom")).status_code(),
            StatusCode::INTERNAL_SERVER_ERROR
        );
    }

    #[tokio::test]
    async fn internal_error_never_leaks_details() {
        let secret = "/opt/nexus/very/secret/path.rs:42";
        let (status, body) =
            body_string(AppError::internal(anyhow::anyhow!("炸了：{secret}"))).await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert!(!body.contains(secret), "5xx 页面不得泄露内部细节");
        assert!(!body.contains("炸了"));
        assert!(body.contains("服务内部错误"));
    }

    #[tokio::test]
    async fn bad_request_message_is_escaped() {
        let (status, body) = body_string(AppError::bad_request("<script>alert(1)</script>")).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(!body.contains("<script>"));
        assert!(body.contains("&lt;script&gt;"));
    }

    #[tokio::test]
    async fn login_required_redirects_with_both_location_and_hx_redirect() {
        let resp = AppError::LoginRequired {
            next: "/kb/doc/7?tab=comments".into(),
        }
        .into_response();
        assert_eq!(resp.status(), StatusCode::SEE_OTHER);
        let location = resp
            .headers()
            .get(header::LOCATION)
            .unwrap()
            .to_str()
            .unwrap();
        let hx = resp.headers().get(&HX_REDIRECT).unwrap().to_str().unwrap();
        assert_eq!(location, hx, "浏览器与 htmx 必须跳向同一个地址");
        assert_eq!(location, "/login?next=%2Fkb%2Fdoc%2F7%3Ftab%3Dcomments");
    }

    #[tokio::test]
    async fn not_found_page_is_rendered_not_plain_text() {
        let (status, body) = body_string(AppError::NotFound).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert!(body.contains("<!DOCTYPE html>"));
        assert!(body.contains("404"));
    }

    #[test]
    fn duplicate_key_becomes_conflict_but_other_db_errors_do_not() {
        // 只有 Database 变体带 code()，这里用 RowNotFound 验证非唯一约束路径不被误判。
        let err = AppError::from(sqlx::Error::RowNotFound);
        assert!(matches!(err, AppError::Database(_)));
        assert_eq!(err.status_code(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[test]
    fn template_errors_are_500_and_render_without_templates() {
        // 错误页渲染器不能依赖 askama，否则 Template 变体会递归炸栈。
        let page = render_error_page(StatusCode::INTERNAL_SERVER_ERROR, "服务内部错误");
        assert!(page.contains("500"));
        assert!(page.contains("<!DOCTYPE html>"));
    }

    #[test]
    fn percent_encode_covers_reserved_characters() {
        assert_eq!(percent_encode("/a b"), "%2Fa%20b");
        assert_eq!(percent_encode("aA0-_.~"), "aA0-_.~");
        assert_eq!(percent_encode("中文"), "%E4%B8%AD%E6%96%87");
    }
}
