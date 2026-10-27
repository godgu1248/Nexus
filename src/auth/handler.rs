//! 身份模块的 HTTP 层：取表单、调 service、把结果翻译成响应。
//!
//! 这一层不许写 SQL、不许开事务、不许直接调 `audit::record`——所有需要"要么全做要么全不做"
//! 的事情都在 `service.rs` 里。handler 只负责三件它独占的事：
//!
//! 1. 把用户输入翻译成**字段级错误**（回填原值、不清空表单）。这类错误不走 `AppError`：
//!    422 的错误页会把用户刚填的内容全丢掉，而表单校验是最需要保留输入的地方。
//! 2. 决定"跳哪儿"，并且**只允许跳到站内相对路径**（`safe_next`）。
//! 3. 把服务层的枚举结果翻译成人话，同时保证不同失败原因说同一句话。

use std::net::SocketAddr;

use askama::Template;
use askama_web::WebTemplate;
use axum::extract::{ConnectInfo, Form, State};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use tower_sessions::Session;

use crate::auth::middleware::{self, MaybeUser};
use crate::auth::service::{self, LoginOutcome, RegisterInput};
use crate::error::AppError;
use crate::shared::validate::{self, FieldError};
use crate::state::AppState;

/// 登录失败时对外的**唯一**提示。
///
/// 账号不存在、密码错、账号被停用、密码已被清空——四种原因共用这一句。
/// 分开说等于免费提供一个账号枚举接口。
const LOGIN_FAILED_MESSAGE: &str = "用户名或密码不正确";

/// 被限速时的提示。这里可以给出剩余时间：攻击者本来就知道自己被限速了
/// （他已经失败很多次），而正常用户需要知道"等多久再试"，否则只会反复重试。
fn locked_message(retry_after: std::time::Duration) -> String {
    let seconds = retry_after.as_secs();
    let minutes = seconds.div_ceil(60).max(1);
    format!("尝试次数过多，请在 {minutes} 分钟后重试")
}

#[derive(Debug, Deserialize)]
pub struct LoginForm {
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub password: String,
    /// 登录后要去的站内路径。由 `safe_next` 归一化，非法值一律回落到首页。
    #[serde(default)]
    pub next: String,
    #[serde(default, rename = "_csrf")]
    pub csrf: String,
}

#[derive(Debug, Deserialize)]
pub struct RegisterForm {
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub email: String,
    #[serde(default)]
    pub password: String,
    #[serde(default)]
    pub display_name: String,
    #[serde(default, rename = "_csrf")]
    pub csrf: String,
}

#[derive(Debug, Deserialize)]
pub struct LogoutForm {
    #[serde(default, rename = "_csrf")]
    pub csrf: String,
}

#[derive(Template, WebTemplate)]
#[template(path = "auth/login.html")]
pub struct LoginTemplate {
    pub page_title: String,
    pub static_base: String,
    pub csrf_token: String,
    /// 见 `templates/layout.html` 顶部的五字段契约。登录页上它恒为空（游客态），
    /// 但仍然必须存在——否则 layout 引用不存在的字段会直接编译失败。
    pub nav_display_name: String,
    pub nav_is_admin: bool,
    pub nav_is_guest: bool,
    pub look: crate::shared::appearance::Appearance,
    pub nav_active: &'static str,
    pub username: String,
    pub next: String,
    pub error: Option<String>,
    pub registered: bool,
}

#[derive(Template, WebTemplate)]
#[template(path = "auth/register.html")]
pub struct RegisterTemplate {
    pub page_title: String,
    pub static_base: String,
    pub csrf_token: String,
    pub nav_display_name: String,
    pub nav_is_admin: bool,
    pub nav_is_guest: bool,
    pub look: crate::shared::appearance::Appearance,
    pub nav_active: &'static str,
    pub username: String,
    pub email: String,
    pub display_name: String,
    pub errors: Vec<FieldError>,
}

impl LoginTemplate {
    fn new(state: &AppState, csrf_token: String) -> Self {
        Self {
            page_title: "登录 · NEXUS".to_owned(),
            static_base: state.static_base().to_owned(),
            csrf_token,
            nav_display_name: String::new(),
            nav_is_admin: false,
            nav_is_guest: false,
            look: crate::shared::appearance::Appearance::DEFAULT,
            nav_active: "",
            username: String::new(),
            next: String::new(),
            error: None,
            registered: false,
        }
    }
}

impl RegisterTemplate {
    fn new(state: &AppState, csrf_token: String) -> Self {
        Self {
            page_title: "注册 · NEXUS".to_owned(),
            static_base: state.static_base().to_owned(),
            csrf_token,
            nav_display_name: String::new(),
            nav_is_admin: false,
            nav_is_guest: false,
            look: crate::shared::appearance::Appearance::DEFAULT,
            nav_active: "",
            username: String::new(),
            email: String::new(),
            display_name: String::new(),
            errors: Vec::new(),
        }
    }
}

/// 只允许跳到站内相对路径。
///
/// 三种必须拒掉的形状：`//evil.com`（协议相对 URL，浏览器会当成外站）、
/// `https://evil.com`（绝对 URL）、`/\evil.com`（反斜杠在部分浏览器里等价于 `/`）。
/// 登录后被弹到攻击者站点，是开放重定向最经典的用法。
fn safe_next(raw: &str) -> String {
    let candidate = raw.trim();
    let looks_internal = candidate.starts_with('/')
        && !candidate.starts_with("//")
        && !candidate.contains('\\')
        && !candidate.contains('\r')
        && !candidate.contains('\n');
    if looks_internal {
        candidate.to_owned()
    } else {
        "/".to_owned()
    }
}

/// 303 跳转 + `HX-Redirect` 已收到 `shared::http`，三处 handler 共用一份。
use crate::shared::http::redirect_to;

fn ip_of(
    headers: &axum::http::HeaderMap,
    state: &AppState,
    connect_info: Option<&ConnectInfo<SocketAddr>>,
) -> Option<String> {
    middleware::client_ip(headers, state.config.trust_proxy, connect_info)
}

/// GET /login
pub async fn login_page(
    State(state): State<AppState>,
    session: Session,
    user: MaybeUser,
) -> Result<Response, AppError> {
    if user.user().is_some() {
        return Ok(redirect_to("/"));
    }
    let csrf_token = middleware::ensure_csrf(&session).await?;
    Ok(LoginTemplate::new(&state, csrf_token).into_response())
}

/// POST /login
pub async fn login_submit(
    State(state): State<AppState>,
    session: Session,
    headers: axum::http::HeaderMap,
    connect_info: ConnectInfo<SocketAddr>,
    Form(form): Form<LoginForm>,
) -> Result<Response, AppError> {
    middleware::require_csrf(&session, Some(&form.csrf)).await?;

    let csrf_token = middleware::ensure_csrf(&session).await?;
    let next = safe_next(&form.next);

    let mut template = LoginTemplate::new(&state, csrf_token);
    template.username = form.username.clone();
    template.next = if next == "/" {
        String::new()
    } else {
        next.clone()
    };
    template.registered = false;

    // 空字段属于"填错了"，不是"服务器坏了"，所以走回填重渲染而不是 400 错误页。
    if form.username.trim().is_empty() || form.password.is_empty() {
        template.error = Some("请填写用户名和密码".to_owned());
        return Ok(template.into_response());
    }

    let ip = ip_of(&headers, &state, Some(&connect_info));

    match service::login(&state.pool, ip.as_deref(), &form.username, &form.password).await? {
        LoginOutcome::Ok { user_id, .. } => {
            middleware::start_session(&session, user_id).await?;
            Ok(redirect_to(&next))
        }
        LoginOutcome::Locked { retry_after } => {
            template.error = Some(locked_message(retry_after));
            Ok(template.into_response())
        }
        // 停用账号与密码错误返回同一句提示：让"这个账号存在但被封了"不可观测。
        LoginOutcome::BadCredentials | LoginOutcome::Disabled => {
            template.error = Some(LOGIN_FAILED_MESSAGE.to_owned());
            Ok(template.into_response())
        }
    }
}

/// GET /register
pub async fn register_page(
    State(state): State<AppState>,
    session: Session,
    user: MaybeUser,
) -> Result<Response, AppError> {
    if user.user().is_some() {
        return Ok(redirect_to("/"));
    }
    let csrf_token = middleware::ensure_csrf(&session).await?;
    Ok(RegisterTemplate::new(&state, csrf_token).into_response())
}

/// POST /register
pub async fn register_submit(
    State(state): State<AppState>,
    session: Session,
    headers: axum::http::HeaderMap,
    connect_info: ConnectInfo<SocketAddr>,
    Form(form): Form<RegisterForm>,
) -> Result<Response, AppError> {
    middleware::require_csrf(&session, Some(&form.csrf)).await?;

    let csrf_token = middleware::ensure_csrf(&session).await?;

    let display_name = form.display_name.trim();
    let mut errors = Vec::new();
    if let Err(err) = validate::username(&form.username) {
        errors.push(err);
    }
    if let Err(err) = validate::email(&form.email) {
        errors.push(err);
    }
    if let Err(err) = validate::password(&form.password) {
        errors.push(err);
    }
    if !display_name.is_empty()
        && let Err(err) = validate::non_empty("display_name", display_name, 32)
    {
        errors.push(err);
    }

    if !errors.is_empty() {
        let mut template = RegisterTemplate::new(&state, csrf_token);
        template.username = form.username.clone();
        template.email = form.email.clone();
        template.display_name = form.display_name.clone();
        template.errors = errors;
        return Ok(template.into_response());
    }

    let ip = ip_of(&headers, &state, Some(&connect_info));

    // 注册要限速：每次注册是一次 argon2（19 MiB 内存 + 两次迭代），
    // 不限速的话它就是一个不用登录就能触发的 CPU 消耗点。
    // 键用出口 IP——注册时还没有身份可用；地址先按 /64 归并，
    // 否则 IPv6 上换个后缀就是一个新额度（见 `ratelimit::address_key`）。
    let rate_key = ip
        .as_deref()
        .map(crate::shared::ratelimit::address_key)
        .unwrap_or_else(|| "-".to_owned());
    state
        .rate
        .guard(crate::shared::ratelimit::Kind::Register, &rate_key)?;

    let input = RegisterInput {
        username: form.username.trim(),
        email: form.email.trim(),
        password: &form.password,
        display_name: Some(display_name),
    };

    match service::register(&state.pool, audit_ctx(None, ip.as_deref()), input).await {
        Ok(_user_id) => {
            // 注册不自动登录：让用户亲手登录一次，才会记住"我的账号是什么"。
            Ok(redirect_to("/login?registered=1"))
        }
        Err(AppError::Unprocessable(message)) => {
            let mut template = RegisterTemplate::new(&state, csrf_token);
            template.username = form.username.clone();
            template.email = form.email.clone();
            template.display_name = form.display_name.clone();
            template.errors = vec![FieldError {
                field: "username".to_owned(),
                message: message.to_string(),
            }];
            Ok(template.into_response())
        }
        Err(err) => Err(err),
    }
}

/// POST /logout
///
/// 必须是 POST：`<a href="/logout">` 会让浏览器预取、让爬虫、让邮件客户端的链接预览
/// 把所有人踢下线。
pub async fn logout_submit(
    State(state): State<AppState>,
    session: Session,
    user: MaybeUser,
    headers: axum::http::HeaderMap,
    connect_info: ConnectInfo<SocketAddr>,
    Form(form): Form<LogoutForm>,
) -> Result<Response, AppError> {
    middleware::require_csrf(&session, Some(&form.csrf)).await?;

    if let Some(current) = user.user() {
        let ip = ip_of(&headers, &state, Some(&connect_info));
        // 审计失败也照样登出（理由见 service::logout）。
        service::logout(&state.pool, current.id, ip.as_deref()).await?;
    }

    middleware::end_session(&session).await?;
    Ok(redirect_to("/login"))
}

fn audit_ctx(actor_id: Option<u64>, ip: Option<&str>) -> crate::shared::audit::AuditCtx<'_> {
    crate::shared::audit::AuditCtx::new(actor_id, ip)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_next_keeps_internal_paths() {
        assert_eq!(safe_next("/forum?page=2"), "/forum?page=2");
        assert_eq!(safe_next("/"), "/");
    }

    #[test]
    fn safe_next_rejects_every_open_redirect_shape() {
        for hostile in [
            "//evil.com",
            "https://evil.com",
            "http://evil.com/x",
            "/\\evil.com",
            "/path\r\nSet-Cookie: x=y",
            "javascript:alert(1)",
            "",
            "   ",
        ] {
            assert_eq!(safe_next(hostile), "/", "这个形状必须被拒：{hostile:?}");
        }
    }

    #[test]
    fn locked_message_rounds_up_to_whole_minutes() {
        assert_eq!(
            locked_message(std::time::Duration::from_secs(1)),
            "尝试次数过多，请在 1 分钟后重试"
        );
        assert_eq!(
            locked_message(std::time::Duration::from_secs(300)),
            "尝试次数过多，请在 5 分钟后重试"
        );
        assert_eq!(
            locked_message(std::time::Duration::from_secs(61)),
            "尝试次数过多，请在 2 分钟后重试"
        );
    }
}
