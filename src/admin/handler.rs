//! 管理区 HTTP 层（M7）。
//!
//! 这一层的职责只有四件事：解析表单、取 CSRF、调 service、把结果塞进模板。
//! **一切判断（谁能做什么、最后一个管理员能不能动）都在 service**——
//! 放在这里的话，将来多一个入口（比如 JSON API）就得把规则抄一遍，
//! 而抄漏的那一处不会报错，只会静默放行。

use askama::Template;
use askama_web::WebTemplate;
use axum::Form;
use axum::extract::{Path, Query, State};
use serde::Deserialize;
use tower_sessions::Session;

use crate::admin::model::{AuditFilter, AuditRow, MemberRow};
use crate::admin::service;
use crate::auth::middleware::{self, ClientIp, MaybeUser, MaybeUserRow};
use crate::auth::model::{Role, UserStatus};
use crate::auth::policy::{self, CurrentUser};
use crate::error::AppError;
use crate::public::model::{JoinRow, Submission};
use crate::shared::http::redirect_to;
use crate::shared::pagination::{Page, PageQuery};
use crate::state::AppState;

const ADMIN_HOME: &str = "/admin";
const MEMBERS_PATH: &str = "/admin/members";
const JOINS_PATH: &str = "/admin/joins";
const SUBMISSIONS_PATH: &str = "/admin/submissions";
const AUDIT_PATH: &str = "/admin/audit";

// ---------- 表单与查询 ----------

#[derive(Debug, Deserialize)]
pub struct RoleForm {
    pub role: String,
    #[serde(rename = "_csrf")]
    pub _csrf: String,
}

#[derive(Debug, Deserialize)]
pub struct StatusForm {
    pub status: String,
    #[serde(rename = "_csrf")]
    pub _csrf: String,
}

#[derive(Debug, Deserialize)]
pub struct CsrfForm {
    #[serde(rename = "_csrf")]
    pub _csrf: String,
}

#[derive(Debug, Deserialize)]
pub struct NoteForm {
    #[serde(default)]
    pub note: String,
    #[serde(rename = "_csrf")]
    pub _csrf: String,
}

#[derive(Debug, Deserialize)]
pub struct MembersQuery {
    #[serde(default)]
    pub role: String,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub page: Option<u32>,
}

#[derive(Debug, Deserialize)]
pub struct AuditQuery {
    #[serde(default)]
    pub actor: String,
    #[serde(default)]
    pub action: String,
    #[serde(default)]
    pub target_type: String,
    #[serde(default)]
    pub target_id: String,
    #[serde(default)]
    pub from: String,
    #[serde(default)]
    pub until: String,
    #[serde(default)]
    pub page: Option<u32>,
}

#[derive(Debug, Deserialize)]
pub struct PageOnly {
    #[serde(default)]
    pub page: Option<u32>,
}

// ---------- 视图模型 ----------

#[derive(Debug, Clone)]
pub struct AuditView {
    pub id: u64,
    pub actor: String,
    pub action: String,
    pub target: String,
    pub ip: String,
    pub created_at: String,
}

#[derive(Debug, Clone)]
pub struct MemberView {
    pub id: u64,
    pub username: String,
    pub display_name: String,
    pub email: String,
    pub role_key: String,
    pub status_key: String,
    pub created_at: String,
    /// 自己那一行不给改：界面先挡住，service 里还有第二道。
    pub is_self: bool,
    pub role_selected: Vec<SelectOption>,
    pub status_selected: Vec<SelectOption>,
}

#[derive(Debug, Clone)]
pub struct JoinView {
    pub id: u64,
    pub username: String,
    pub display_name: String,
    pub email: String,
    pub message: String,
    pub created_at: String,
}

#[derive(Debug, Clone)]
pub struct SubmissionView {
    pub id: u64,
    pub title: String,
    pub author_name: String,
    pub created_at: String,
    pub preview: String,
}

#[derive(Debug, Clone)]
pub struct SelectOption {
    pub value: String,
    pub label: String,
    pub selected: bool,
}

// ---------- 模板 ----------

#[derive(Template, WebTemplate)]
#[template(path = "admin/overview.html")]
pub struct OverviewTemplate {
    pub page_title: String,
    pub static_base: String,
    pub csrf_token: String,
    pub nav_display_name: String,
    pub nav_is_admin: bool,
    pub nav_is_guest: bool,
    pub look: crate::shared::appearance::Appearance,
    pub nav_active: &'static str,
    pub pending_joins: u64,
    pub pending_submissions: u64,
    pub active_admins: u64,
    pub recent: Vec<AuditView>,
}

#[derive(Template, WebTemplate)]
#[template(path = "admin/members.html")]
pub struct MembersTemplate {
    pub page_title: String,
    pub static_base: String,
    pub csrf_token: String,
    pub nav_display_name: String,
    pub nav_is_admin: bool,
    pub nav_is_guest: bool,
    pub look: crate::shared::appearance::Appearance,
    pub nav_active: &'static str,
    pub members: Page<MemberView>,
    pub page_query: String,
    pub role_options: Vec<SelectOption>,
    pub status_options: Vec<SelectOption>,
}

#[derive(Template, WebTemplate)]
#[template(path = "admin/joins.html")]
pub struct JoinsTemplate {
    pub page_title: String,
    pub static_base: String,
    pub csrf_token: String,
    pub nav_display_name: String,
    pub nav_is_admin: bool,
    pub nav_is_guest: bool,
    pub look: crate::shared::appearance::Appearance,
    pub nav_active: &'static str,
    pub joins: Page<JoinView>,
    pub page_query: String,
}

#[derive(Template, WebTemplate)]
#[template(path = "admin/submissions.html")]
pub struct SubmissionsTemplate {
    pub page_title: String,
    pub static_base: String,
    pub csrf_token: String,
    pub nav_display_name: String,
    pub nav_is_admin: bool,
    pub nav_is_guest: bool,
    pub look: crate::shared::appearance::Appearance,
    pub nav_active: &'static str,
    pub submissions: Page<SubmissionView>,
    pub page_query: String,
}

#[derive(Template, WebTemplate)]
#[template(path = "admin/audit.html")]
pub struct AuditTemplate {
    pub page_title: String,
    pub static_base: String,
    pub csrf_token: String,
    pub nav_display_name: String,
    pub nav_is_admin: bool,
    pub nav_is_guest: bool,
    pub look: crate::shared::appearance::Appearance,
    pub nav_active: &'static str,
    pub rows: Page<AuditView>,
    pub page_query: String,
    pub actor_value: String,
    pub action_value: String,
    pub target_type_value: String,
    pub target_id_value: String,
    pub from_value: String,
    pub until_value: String,
}

// ---------- 辅助 ----------

/// 页面公共部分。集中一处，避免五个模板结构体各抄一遍取 token 的代码。
struct Chrome {
    static_base: String,
    csrf_token: String,
    nav_display_name: String,
    nav_is_admin: bool,
    nav_is_guest: bool,
    look: crate::shared::appearance::Appearance,
    nav_active: &'static str,
}

async fn chrome(
    state: &AppState,
    session: &Session,
    user: &MaybeUser,
    row: &MaybeUserRow,
) -> Result<Chrome, AppError> {
    Ok(Chrome {
        static_base: state.static_base().to_string(),
        csrf_token: middleware::ensure_csrf(session).await?,
        nav_display_name: row.display_name().to_string(),
        nav_is_admin: user.is_admin(),
        nav_is_guest: crate::auth::policy::is_guest(user.user()),
        look: row.look(),
        nav_active: "admin",
    })
}

fn stamp(dt: chrono::DateTime<chrono::Utc>, tz: chrono::FixedOffset) -> String {
    dt.with_timezone(&tz).format("%Y-%m-%d %H:%M").to_string()
}

/// 先登录再鉴权：游客点管理入口应该拿到去登录页的 303，而不是 403。
fn require_admin<'a>(user: &'a MaybeUser, next: &str) -> Result<&'a CurrentUser, AppError> {
    match user.user() {
        None => Err(AppError::LoginRequired {
            next: next.to_string(),
        }),
        Some(current) if policy::can_admin(Some(current)) => Ok(current),
        Some(_) => Err(AppError::Forbidden),
    }
}

fn parse_id(raw: &str) -> Result<u64, AppError> {
    raw.parse::<u64>()
        .map_err(|_| AppError::bad_request("编号不是合法数字"))
}

/// `YYYY-MM-DD` → 当天零点（UTC，与全站存储口径一致）。
fn parse_day(raw: &str) -> Result<Option<chrono::NaiveDateTime>, AppError> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Ok(None);
    }
    let date = chrono::NaiveDate::parse_from_str(raw, "%Y-%m-%d")
        .map_err(|_| AppError::bad_request("日期格式应为 YYYY-MM-DD"))?;
    Ok(date.and_hms_opt(0, 0, 0))
}

/// `until` 是"包含当天"，所以取次日零点用 `<` 比较。
fn parse_day_end(raw: &str) -> Result<Option<chrono::NaiveDateTime>, AppError> {
    match parse_day(raw)? {
        None => Ok(None),
        Some(start) => Ok(start
            .and_utc()
            .checked_add_days(chrono::Days::new(1))
            .map(|next| next.naive_utc())),
    }
}

fn map_page<T, U>(page: Page<T>, f: impl Fn(&T) -> U) -> Page<U> {
    let items: Vec<U> = page.items.iter().map(f).collect();
    Page::new(items, page.page, page.total, page.per_page)
}

fn audit_view(row: &AuditRow, tz: chrono::FixedOffset) -> AuditView {
    AuditView {
        id: row.id,
        actor: match row.actor_id {
            Some(id) => format!("用户#{id}"),
            None => "系统".to_string(),
        },
        action: row.action.clone(),
        target: match row.target_id {
            Some(id) => format!("{}#{}", row.target_type, id),
            None => row.target_type.clone(),
        },
        ip: row.ip.clone().unwrap_or_else(|| "—".to_string()),
        created_at: stamp(row.created_at, tz),
    }
}

fn options(pairs: &[(&str, &str)], selected: &str) -> Vec<SelectOption> {
    pairs
        .iter()
        .map(|(value, label)| SelectOption {
            value: (*value).to_string(),
            label: (*label).to_string(),
            selected: *value == selected,
        })
        .collect()
}

const ROLE_PAIRS: &[(&str, &str)] = &[
    ("", "全部角色"),
    ("user", "user"),
    ("member", "member"),
    ("admin", "admin"),
];

const STATUS_PAIRS: &[(&str, &str)] = &[
    ("", "全部状态"),
    ("active", "active"),
    ("pending", "pending"),
    ("suspended", "suspended"),
    ("deleted", "deleted"),
];

fn member_view(row: &MemberRow, viewer_id: u64, tz: chrono::FixedOffset) -> MemberView {
    MemberView {
        id: row.id,
        username: row.username.clone(),
        display_name: row.display_name.clone(),
        email: row.email.clone(),
        role_key: row.role.as_str().to_string(),
        status_key: row.status.as_str().to_string(),
        created_at: stamp(row.created_at, tz),
        is_self: row.id == viewer_id,
        role_selected: options(ROLE_PAIRS, row.role.as_str()),
        status_selected: options(STATUS_PAIRS, row.status.as_str()),
    }
}

fn join_view(row: &JoinRow, tz: chrono::FixedOffset) -> JoinView {
    JoinView {
        id: row.id,
        username: row.username.clone(),
        display_name: row.display_name.clone(),
        email: row.email.clone(),
        message: row.message.clone(),
        created_at: stamp(row.created_at, tz),
    }
}

fn submission_view(row: &Submission, tz: chrono::FixedOffset) -> SubmissionView {
    SubmissionView {
        id: row.id,
        title: row.title.clone(),
        author_name: row.author_name.clone(),
        created_at: stamp(row.created_at, tz),
        preview: row.body.chars().take(160).collect(),
    }
}

fn page_query(extra: &str, page: u32) -> String {
    if extra.is_empty() {
        format!("page={page}")
    } else {
        format!("{extra}&page={page}")
    }
}

// ---------- GET 页面 ----------

pub async fn overview_page(
    State(state): State<AppState>,
    session: Session,
    user: MaybeUser,
    row: MaybeUserRow,
) -> Result<OverviewTemplate, AppError> {
    let viewer = require_admin(&user, ADMIN_HOME)?;
    let data = service::overview(&state.pool, viewer).await?;
    let head = chrome(&state, &session, &user, &row).await?;
    let tz = state.config.display_tz;

    Ok(OverviewTemplate {
        page_title: "管理 · NEXUS".to_string(),
        static_base: head.static_base,
        csrf_token: head.csrf_token,
        nav_display_name: head.nav_display_name,
        nav_is_admin: head.nav_is_admin,
        nav_is_guest: head.nav_is_guest,
        look: head.look,
        nav_active: head.nav_active,
        pending_joins: data.pending_joins,
        pending_submissions: data.pending_submissions,
        active_admins: data.active_admins,
        recent: data.recent.iter().map(|r| audit_view(r, tz)).collect(),
    })
}

pub async fn members_page(
    State(state): State<AppState>,
    session: Session,
    user: MaybeUser,
    row: MaybeUserRow,
    Query(query): Query<MembersQuery>,
) -> Result<MembersTemplate, AppError> {
    let viewer = require_admin(&user, MEMBERS_PATH)?;
    let page = PageQuery::new(query.page.unwrap_or(1));
    let role = Some(query.role.as_str()).filter(|value| !value.is_empty());
    let status = Some(query.status.as_str()).filter(|value| !value.is_empty());

    let members = service::members(&state.pool, viewer, role, status, &page).await?;
    let head = chrome(&state, &session, &user, &row).await?;
    let tz = state.config.display_tz;
    let viewer_id = viewer.id;

    let mut extra = String::new();
    if let Some(role) = role {
        extra.push_str("role=");
        extra.push_str(role);
        extra.push('&');
    }
    if let Some(status) = status {
        extra.push_str("status=");
        extra.push_str(status);
        extra.push('&');
    }

    Ok(MembersTemplate {
        page_title: "成员 · 管理 · NEXUS".to_string(),
        static_base: head.static_base,
        csrf_token: head.csrf_token,
        nav_display_name: head.nav_display_name,
        nav_is_admin: head.nav_is_admin,
        nav_is_guest: head.nav_is_guest,
        look: head.look,
        nav_active: head.nav_active,
        members: map_page(members, |row| member_view(row, viewer_id, tz)),
        page_query: page_query(extra.trim_end_matches('&'), page.page),
        role_options: options(ROLE_PAIRS, query.role.as_str()),
        status_options: options(STATUS_PAIRS, query.status.as_str()),
    })
}

pub async fn joins_page(
    State(state): State<AppState>,
    session: Session,
    user: MaybeUser,
    row: MaybeUserRow,
    Query(query): Query<PageOnly>,
) -> Result<JoinsTemplate, AppError> {
    let viewer = require_admin(&user, JOINS_PATH)?;
    let page = PageQuery::new(query.page.unwrap_or(1));
    let joins = service::join_queue(&state.pool, viewer, &page).await?;
    let head = chrome(&state, &session, &user, &row).await?;
    let tz = state.config.display_tz;

    Ok(JoinsTemplate {
        page_title: "入队申请 · 管理 · NEXUS".to_string(),
        static_base: head.static_base,
        csrf_token: head.csrf_token,
        nav_display_name: head.nav_display_name,
        nav_is_admin: head.nav_is_admin,
        nav_is_guest: head.nav_is_guest,
        look: head.look,
        nav_active: head.nav_active,
        joins: map_page(joins, |row| join_view(row, tz)),
        page_query: page_query("", page.page),
    })
}

pub async fn submissions_page(
    State(state): State<AppState>,
    session: Session,
    user: MaybeUser,
    row: MaybeUserRow,
    Query(query): Query<PageOnly>,
) -> Result<SubmissionsTemplate, AppError> {
    let viewer = require_admin(&user, SUBMISSIONS_PATH)?;
    let page = PageQuery::new(query.page.unwrap_or(1));
    let submissions = service::submission_queue(&state.pool, viewer, &page).await?;
    let head = chrome(&state, &session, &user, &row).await?;
    let tz = state.config.display_tz;

    Ok(SubmissionsTemplate {
        page_title: "投稿审核 · 管理 · NEXUS".to_string(),
        static_base: head.static_base,
        csrf_token: head.csrf_token,
        nav_display_name: head.nav_display_name,
        nav_is_admin: head.nav_is_admin,
        nav_is_guest: head.nav_is_guest,
        look: head.look,
        nav_active: head.nav_active,
        submissions: map_page(submissions, |row| submission_view(row, tz)),
        page_query: page_query("", page.page),
    })
}

pub async fn audit_page(
    State(state): State<AppState>,
    session: Session,
    user: MaybeUser,
    row: MaybeUserRow,
    client_ip: ClientIp,
    Query(query): Query<AuditQuery>,
) -> Result<AuditTemplate, AppError> {
    let viewer = require_admin(&user, AUDIT_PATH)?;
    let page = PageQuery::new(query.page.unwrap_or(1));

    let actor_id = match query.actor.trim() {
        "" => None,
        raw => Some(parse_id(raw)?),
    };
    let target_id = match query.target_id.trim() {
        "" => None,
        raw => Some(parse_id(raw)?),
    };
    let action = Some(query.action.trim().to_string()).filter(|value| !value.is_empty());

    let filter = AuditFilter {
        actor_id,
        action: action.clone(),
        target_type: Some(query.target_type.trim().to_string()).filter(|value| !value.is_empty()),
        target_id,
        from: parse_day(&query.from)?,
        until: parse_day_end(&query.until)?,
    };

    let rows =
        service::audit_page(&state.pool, viewer, client_ip.0.as_deref(), &filter, &page).await?;
    let head = chrome(&state, &session, &user, &row).await?;
    let tz = state.config.display_tz;

    let mut extra = String::new();
    if let Some(id) = actor_id {
        extra.push_str(&format!("actor={id}&"));
    }
    if let Some(action) = &action {
        extra.push_str(&format!("action={action}&"));
    }
    if let Some(kind) = &filter.target_type {
        extra.push_str(&format!("target_type={kind}&"));
    }
    if let Some(id) = target_id {
        extra.push_str(&format!("target_id={id}&"));
    }
    if !query.from.trim().is_empty() {
        extra.push_str(&format!("from={}&", query.from.trim()));
    }
    if !query.until.trim().is_empty() {
        extra.push_str(&format!("until={}&", query.until.trim()));
    }

    Ok(AuditTemplate {
        page_title: "审计 · 管理 · NEXUS".to_string(),
        static_base: head.static_base,
        csrf_token: head.csrf_token,
        nav_display_name: head.nav_display_name,
        nav_is_admin: head.nav_is_admin,
        nav_is_guest: head.nav_is_guest,
        look: head.look,
        nav_active: head.nav_active,
        rows: map_page(rows, |row| audit_view(row, tz)),
        page_query: page_query(extra.trim_end_matches('&'), page.page),
        actor_value: query.actor.trim().to_string(),
        action_value: query.action.trim().to_string(),
        target_type_value: query.target_type.trim().to_string(),
        target_id_value: query.target_id.trim().to_string(),
        from_value: query.from.trim().to_string(),
        until_value: query.until.trim().to_string(),
    })
}

// ---------- POST 动作 ----------

pub async fn member_role_submit(
    State(state): State<AppState>,
    session: Session,
    user: MaybeUser,
    client_ip: ClientIp,
    Path(id): Path<String>,
    Form(form): Form<RoleForm>,
) -> Result<axum::response::Response, AppError> {
    let viewer = require_admin(&user, MEMBERS_PATH)?;
    middleware::require_csrf(&session, Some(&form._csrf)).await?;

    let role = form
        .role
        .trim()
        .parse::<Role>()
        .map_err(|err| AppError::unprocessable(format!("角色不合法：{err}")))?;
    service::change_role(
        &state.pool,
        viewer,
        client_ip.0.as_deref(),
        parse_id(&id)?,
        role,
    )
    .await?;
    Ok(redirect_to(MEMBERS_PATH))
}

pub async fn member_status_submit(
    State(state): State<AppState>,
    session: Session,
    user: MaybeUser,
    client_ip: ClientIp,
    Path(id): Path<String>,
    Form(form): Form<StatusForm>,
) -> Result<axum::response::Response, AppError> {
    let viewer = require_admin(&user, MEMBERS_PATH)?;
    middleware::require_csrf(&session, Some(&form._csrf)).await?;

    let status = UserStatus::from_db(form.status.trim())
        .ok_or_else(|| AppError::unprocessable("状态不合法：应为 active/pending/suspended"))?;
    service::change_status(
        &state.pool,
        viewer,
        client_ip.0.as_deref(),
        parse_id(&id)?,
        status,
    )
    .await?;
    Ok(redirect_to(MEMBERS_PATH))
}

pub async fn member_delete_submit(
    State(state): State<AppState>,
    session: Session,
    user: MaybeUser,
    client_ip: ClientIp,
    Path(id): Path<String>,
    Form(form): Form<CsrfForm>,
) -> Result<axum::response::Response, AppError> {
    let viewer = require_admin(&user, MEMBERS_PATH)?;
    middleware::require_csrf(&session, Some(&form._csrf)).await?;
    service::delete_user(&state.pool, viewer, client_ip.0.as_deref(), parse_id(&id)?).await?;
    Ok(redirect_to(MEMBERS_PATH))
}

pub async fn join_approve_submit(
    State(state): State<AppState>,
    session: Session,
    user: MaybeUser,
    client_ip: ClientIp,
    Path(id): Path<String>,
    Form(form): Form<CsrfForm>,
) -> Result<axum::response::Response, AppError> {
    let viewer = require_admin(&user, JOINS_PATH)?;
    middleware::require_csrf(&session, Some(&form._csrf)).await?;
    service::approve_join(&state.pool, viewer, client_ip.0.as_deref(), parse_id(&id)?).await?;
    Ok(redirect_to(JOINS_PATH))
}

pub async fn join_reject_submit(
    State(state): State<AppState>,
    session: Session,
    user: MaybeUser,
    client_ip: ClientIp,
    Path(id): Path<String>,
    Form(form): Form<NoteForm>,
) -> Result<axum::response::Response, AppError> {
    let viewer = require_admin(&user, JOINS_PATH)?;
    middleware::require_csrf(&session, Some(&form._csrf)).await?;
    service::reject_join(
        &state.pool,
        viewer,
        client_ip.0.as_deref(),
        parse_id(&id)?,
        form.note.trim(),
    )
    .await?;
    Ok(redirect_to(JOINS_PATH))
}

pub async fn submission_approve_submit(
    State(state): State<AppState>,
    session: Session,
    user: MaybeUser,
    client_ip: ClientIp,
    Path(id): Path<String>,
    Form(form): Form<CsrfForm>,
) -> Result<axum::response::Response, AppError> {
    let viewer = require_admin(&user, SUBMISSIONS_PATH)?;
    middleware::require_csrf(&session, Some(&form._csrf)).await?;
    service::approve_submission(&state.pool, viewer, client_ip.0.as_deref(), parse_id(&id)?)
        .await?;
    Ok(redirect_to(SUBMISSIONS_PATH))
}

pub async fn submission_reject_submit(
    State(state): State<AppState>,
    session: Session,
    user: MaybeUser,
    client_ip: ClientIp,
    Path(id): Path<String>,
    Form(form): Form<NoteForm>,
) -> Result<axum::response::Response, AppError> {
    let viewer = require_admin(&user, SUBMISSIONS_PATH)?;
    middleware::require_csrf(&session, Some(&form._csrf)).await?;
    service::reject_submission(
        &state.pool,
        viewer,
        client_ip.0.as_deref(),
        parse_id(&id)?,
        form.note.trim(),
    )
    .await?;
    Ok(redirect_to(SUBMISSIONS_PATH))
}
