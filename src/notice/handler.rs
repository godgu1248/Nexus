//! 公告的 HTTP 层：列表、确认、导航红点、管理员发/撤。

use askama::Template;
use askama_web::WebTemplate;
use axum::Form;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};
use serde::Deserialize;
use tower_sessions::Session;

use crate::auth::middleware::{self, ClientIp, MaybeUser, MaybeUserRow};
use crate::auth::policy::{self, CurrentUser};
use crate::error::AppError;
use crate::notice::model::{self, Audience, Notice};
use crate::notice::service::{self, BODY_MAX, TITLE_MAX};
use crate::shared::redirect_to;
use crate::state::AppState;

const NOTICE_PATH: &str = "/notice";

#[derive(Debug, Deserialize)]
pub struct CsrfForm {
    #[serde(rename = "_csrf")]
    pub csrf: String,
}

#[derive(Debug, Deserialize)]
pub struct NewNoticeForm {
    pub title: String,
    #[serde(default)]
    pub body: String,
    /// 富文本 HTML（编辑器跑起来才会出现在表单里）。
    #[serde(default)]
    pub body_html: String,
    /// 受众：`all` 全体可见 / `team` 仅团队。缺省按 `all`——
    /// 表单没渲染出来（没有 JS 的降级路径）时，宁可发得比预期宽，
    /// 也不能默默发成"仅团队"然后以为所有人都收到了。
    #[serde(default)]
    pub audience: String,
    #[serde(rename = "_csrf")]
    pub csrf: String,
}

/// 发布表单里的一行受众选项。
///
/// 用结构体而不是元组：模板里 `a.label` 比 `a.1` 更能看出是什么，
/// 而且这些字段全是 `&'static str`，不会把用户输入带进页面。
#[derive(Debug, Clone)]
pub struct AudienceOption {
    pub key: &'static str,
    pub label: &'static str,
    pub hint: &'static str,
    pub selected: bool,
}

fn audience_options(current: &str) -> Vec<AudienceOption> {
    Audience::ALL
        .iter()
        .map(|audience| AudienceOption {
            key: audience.as_str(),
            label: audience.label(),
            hint: audience.hint(),
            selected: audience.as_str() == current,
        })
        .collect()
}

#[derive(Debug, Clone)]
pub struct NoticeView {
    pub id: u64,
    pub title: String,
    pub body: String,
    /// 读时再净化一次（见档案模块同名字段的注释）。
    pub body_html: Option<crate::shared::SafeHtml>,
    pub audience_label: &'static str,
    /// 只有"仅团队"那类才需要打标记：全体可见是默认，标出来只是噪音。
    pub team_only: bool,
    pub author_name: String,
    pub created_at: String,
}

/// 本地时区展示，和档案/任务模块同一套格式。
/// 不引第三方时间库，也不在这里做"刚刚/3 分钟前"的相对时间——
/// 公告是给人核对"什么时候发的"，绝对时间比相对时间更有用。
fn stamp(at: chrono::DateTime<chrono::Utc>, tz: chrono::FixedOffset) -> String {
    at.with_timezone(&tz).format("%Y-%m-%d %H:%M").to_string()
}

fn view(notice: &Notice, tz: chrono::FixedOffset) -> NoticeView {
    NoticeView {
        id: notice.id,
        title: notice.title.clone(),
        body: notice.body.clone(),
        body_html: notice
            .body_html
            .as_ref()
            .map(|html| crate::shared::richtext::sanitize(html)),
        audience_label: notice.audience.label(),
        team_only: notice.audience == Audience::Team,
        author_name: notice.author_name.clone(),
        created_at: stamp(notice.created_at, tz),
    }
}

#[derive(Template, WebTemplate)]
#[template(path = "notice/list.html")]
pub struct ListTemplate {
    pub page_title: String,
    pub static_base: String,
    pub csrf_token: String,
    pub nav_display_name: String,
    pub nav_is_admin: bool,
    pub nav_is_guest: bool,
    pub look: crate::shared::appearance::Appearance,
    pub nav_active: &'static str,
    pub unread: Vec<NoticeView>,
    pub all: Vec<NoticeView>,
    pub can_manage: bool,
}

#[derive(Template, WebTemplate)]
#[template(path = "notice/new.html")]
pub struct NewTemplate {
    pub page_title: String,
    pub static_base: String,
    pub csrf_token: String,
    pub nav_display_name: String,
    pub nav_is_admin: bool,
    pub nav_is_guest: bool,
    pub look: crate::shared::appearance::Appearance,
    pub nav_active: &'static str,
    pub title_value: String,
    pub body_value: String,
    pub audiences: Vec<AudienceOption>,
    pub title_max: usize,
    pub body_max: usize,
    pub error: Option<String>,
}

fn require_login(user: &MaybeUser) -> Result<&CurrentUser, AppError> {
    user.user().ok_or_else(|| AppError::LoginRequired {
        next: NOTICE_PATH.to_owned(),
    })
}

fn require_admin<'a>(user: &'a MaybeUser, next: &str) -> Result<&'a CurrentUser, AppError> {
    match user.user() {
        None => Err(AppError::LoginRequired {
            next: next.to_owned(),
        }),
        Some(current) if policy::can_admin(Some(current)) => Ok(current),
        Some(_) => Err(AppError::Forbidden),
    }
}

pub async fn list_page(
    State(state): State<AppState>,
    session: Session,
    user: MaybeUser,
    row: MaybeUserRow,
) -> Result<ListTemplate, AppError> {
    let current = require_login(&user)?;
    let tz = state.config.display_tz;
    let (unread, all) = service::panel(&state.pool, current).await?;

    Ok(ListTemplate {
        page_title: "公告 · NEXUS".to_owned(),
        static_base: state.static_base().to_owned(),
        csrf_token: middleware::ensure_csrf(&session).await?,
        nav_display_name: row.display_name().to_owned(),
        nav_is_admin: user.is_admin(),
        nav_is_guest: policy::is_guest(user.user()),
        look: row.look(),
        nav_active: "notice",
        unread: unread.iter().map(|n| view(n, tz)).collect(),
        all: all.iter().map(|n| view(n, tz)).collect(),
        can_manage: user.is_admin(),
    })
}

pub async fn ack_submit(
    State(state): State<AppState>,
    session: Session,
    user: MaybeUser,
    Path(id): Path<u64>,
    Form(form): Form<CsrfForm>,
) -> Result<Response, AppError> {
    let current = require_login(&user)?;
    middleware::require_csrf(&session, Some(&form.csrf)).await?;
    service::acknowledge(&state.pool, current, id).await?;
    Ok(redirect_to(NOTICE_PATH))
}

/// 导航上的小红点。返回一段 HTML 片段，由 htmx 在页面加载时换进去。
///
/// 为什么不用服务端把未读数塞进每个页面的模板上下文：那要给三十多个
/// 结构体再加两个字段，而红点本身是"锦上添花"的信息。片段的代价是
/// 一次额外的请求，但它只在导航可见时才发。
pub async fn dot(State(state): State<AppState>, user: MaybeUser) -> Html<String> {
    let unread = match user.user() {
        Some(current) => service::unread_count(&state.pool, current)
            .await
            .unwrap_or(0),
        None => 0,
    };
    if unread == 0 {
        return Html(r#"<span id="notice-dot" class="nav-dot-slot"></span>"#.to_owned());
    }
    Html(format!(
        r#"<span id="notice-dot" class="nav-dot-slot"><span class="nav-dot" title="{unread} 条未读公告" aria-label="{unread} 条未读公告"></span></span>"#
    ))
}

pub async fn new_page(
    State(state): State<AppState>,
    session: Session,
    user: MaybeUser,
    row: MaybeUserRow,
) -> Result<NewTemplate, AppError> {
    require_admin(&user, "/notice/new")?;
    Ok(NewTemplate {
        page_title: "发布公告 · NEXUS".to_owned(),
        static_base: state.static_base().to_owned(),
        csrf_token: middleware::ensure_csrf(&session).await?,
        nav_display_name: row.display_name().to_owned(),
        nav_is_admin: user.is_admin(),
        nav_is_guest: policy::is_guest(user.user()),
        look: row.look(),
        nav_active: "notice",
        title_value: String::new(),
        body_value: String::new(),
        audiences: audience_options(Audience::All.as_str()),
        title_max: TITLE_MAX,
        body_max: BODY_MAX,
        error: None,
    })
}

pub async fn new_submit(
    State(state): State<AppState>,
    session: Session,
    user: MaybeUser,
    row: MaybeUserRow,
    client_ip: ClientIp,
    Form(form): Form<NewNoticeForm>,
) -> Result<Response, AppError> {
    let current = require_admin(&user, "/notice/new")?.clone();
    middleware::require_csrf(&session, Some(&form.csrf)).await?;

    // 受众是枚举而不是字符串：解析失败就当场回 422，不要带着"猜的默认值"发出去。
    let audience = match Audience::parse(&form.audience).or_else(|| {
        // 空字符串 = 表单没带这个字段（无 JS 降级、或旧客户端），按"全体可见"处理。
        if form.audience.trim().is_empty() {
            Some(Audience::All)
        } else {
            None
        }
    }) {
        Some(audience) => audience,
        None => {
            let page = NewTemplate {
                page_title: "发布公告 · NEXUS".to_owned(),
                static_base: state.static_base().to_owned(),
                csrf_token: middleware::ensure_csrf(&session).await?,
                nav_display_name: row.display_name().to_owned(),
                nav_is_admin: user.is_admin(),
                nav_is_guest: policy::is_guest(user.user()),
                look: row.look(),
                nav_active: "notice",
                title_value: form.title,
                body_value: form.body,
                audiences: audience_options(Audience::All.as_str()),
                title_max: TITLE_MAX,
                body_max: BODY_MAX,
                error: Some("受众取值不认识，请重新选择。".to_owned()),
            };
            return Ok((StatusCode::UNPROCESSABLE_ENTITY, page).into_response());
        }
    };
    let options = audience_options(audience.as_str());

    match service::create(
        &state.pool,
        &current,
        client_ip.0.as_deref(),
        &form.title,
        &form.body,
        Some(form.body_html.as_str()),
        audience,
    )
    .await
    {
        Ok(_) => Ok(redirect_to(NOTICE_PATH)),
        Err(AppError::Unprocessable(message)) => {
            let page = NewTemplate {
                page_title: "发布公告 · NEXUS".to_owned(),
                static_base: state.static_base().to_owned(),
                csrf_token: middleware::ensure_csrf(&session).await?,
                nav_display_name: row.display_name().to_owned(),
                nav_is_admin: user.is_admin(),
                nav_is_guest: policy::is_guest(user.user()),
                look: row.look(),
                nav_active: "notice",
                title_value: form.title,
                body_value: form.body,
                audiences: options,
                title_max: TITLE_MAX,
                body_max: BODY_MAX,
                error: Some(message.to_string()),
            };
            Ok((StatusCode::UNPROCESSABLE_ENTITY, page).into_response())
        }
        Err(err) => Err(err),
    }
}

pub async fn delete_submit(
    State(state): State<AppState>,
    session: Session,
    user: MaybeUser,
    client_ip: ClientIp,
    Path(id): Path<u64>,
    Form(form): Form<CsrfForm>,
) -> Result<Response, AppError> {
    let current = require_admin(&user, NOTICE_PATH)?.clone();
    middleware::require_csrf(&session, Some(&form.csrf)).await?;
    service::delete(&state.pool, &current, client_ip.0.as_deref(), id).await?;
    Ok(redirect_to(NOTICE_PATH))
}

/// 首页/每个页面上方的公告横幅片段。
///
/// 只返回"这个人还没确认的"几条：确认过的公告继续挂在每个页面顶部，
/// 很快就会变成所有人都会无视的墙纸。
pub async fn banner(
    State(state): State<AppState>,
    session: Session,
    user: MaybeUser,
) -> Result<Html<String>, AppError> {
    let Some(current) = user.user() else {
        return Ok(Html(String::new()));
    };
    let unread =
        model::list_unacked(&state.pool, current.id, 3, service::sees_team(current)).await?;
    if unread.is_empty() {
        return Ok(Html(String::new()));
    }
    let token = middleware::ensure_csrf(&session).await?;
    let tz = state.config.display_tz;

    let mut html = String::from("<div class=\"notice-stack\">");
    for notice in unread {
        html.push_str(
            "<div class=\"notice notice--announcement\"><div class=\"notice__text\"><strong>",
        );
        html.push_str(&escape(&notice.title));
        if notice.audience == Audience::Team {
            // 团队公告只在团队侧可见，标出来让发的人确认"这条来宾看不到"。
            html.push_str(" <span class=\"badge badge--internal\">仅团队</span>");
        }
        html.push_str("</strong><span class=\"muted\"> · ");
        html.push_str(&escape(&notice.author_name));
        html.push_str(" · ");
        html.push_str(&stamp(notice.created_at, tz));
        html.push_str("</span><div class=\"notice__body rich-text\">");
        // 有富文本就渲染净化后的 HTML（sanitize 已经去掉一切危险标签与属性），
        // 否则退回转义后的纯文本。
        match notice.body_html.as_deref() {
            Some(raw) => html.push_str(crate::shared::richtext::sanitize(raw).as_str()),
            None => html.push_str(&escape(&notice.body)),
        }
        html.push_str("</div></div><form method=\"post\" action=\"/notice/");
        html.push_str(&notice.id.to_string());
        html.push_str("/ack\" class=\"inline\"><input type=\"hidden\" name=\"_csrf\" value=\"");
        html.push_str(&token);
        html.push_str("\"><button class=\"btn btn--ghost btn--small\" type=\"submit\">我知道了</button></form></div>");
    }
    html.push_str("</div>");
    Ok(Html(html))
}

fn escape(raw: &str) -> String {
    raw.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}
