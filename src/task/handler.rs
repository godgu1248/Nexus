//! 任务的 HTTP 层。
//!
//! 这里只做三件事：把入参翻成 service 的入参、把 service 的错误翻成状态码、
//! 把结果渲染成模板。任何"这个人能不能做这件事"的判断都不在这里写第二遍。

use axum::Form;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use tower_sessions::Session;

use crate::auth::middleware::{ClientIp, MaybeUser, MaybeUserRow, ensure_csrf, require_csrf};
use crate::auth::policy::{self, ContentCtx, CurrentUser};
use crate::error::AppError;
use crate::group::model as group_model;
use crate::shared::http::redirect_to;
use crate::shared::pagination::{PAGE_SIZE, Page, PageQuery, normalize_page};
use crate::shared::visibility::Visibility;
use crate::state::AppState;
use crate::task::model::{Task, TaskCard, TaskStatus};
use crate::task::service::{self, TaskInput};

// ---------------------------------------------------------------------------
// 表单与视图模型
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct ListQuery {
    #[serde(default)]
    mine: Option<String>,
    #[serde(default)]
    page: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct NewTaskForm {
    #[serde(default)]
    title: String,
    #[serde(default)]
    body: String,
    #[serde(default)]
    assignee: String,
    #[serde(default)]
    due_on: String,
    #[serde(rename = "_csrf", default)]
    _csrf: String,
}

#[derive(Debug, Deserialize)]
pub struct AssignForm {
    #[serde(default)]
    assignee: String,
    #[serde(rename = "_csrf", default)]
    _csrf: String,
}

#[derive(Debug, Deserialize)]
pub struct StatusForm {
    #[serde(default)]
    status: String,
    #[serde(rename = "_csrf", default)]
    _csrf: String,
}

#[derive(Debug, Deserialize)]
pub struct CsrfForm {
    #[serde(rename = "_csrf", default)]
    _csrf: String,
}

/// 一个可被指派的人。
pub struct MemberOption {
    pub id: u64,
    pub name: String,
    pub selected: bool,
}

/// 状态下拉里的一个选项。用结构体而不是三元组：模板里写 `{% for s in statuses %}`
/// 比写元组解构稳，改字段时编译器也会替我找全所有引用点。
pub struct StatusOption {
    pub key: &'static str,
    pub label: &'static str,
    pub selected: bool,
}

pub struct TaskRowView {
    pub id: u64,
    pub title: String,
    pub status_label: &'static str,
    pub status_key: &'static str,
    pub assignee: Option<String>,
    pub due_on: Option<String>,
    pub updated_at: String,
}

pub struct Framework {
    static_base: String,
    csrf_token: String,
    nav_display_name: String,
    nav_is_admin: bool,
    nav_is_guest: bool,
    look: crate::shared::appearance::Appearance,
    nav_active: &'static str,
}

// ---------------------------------------------------------------------------
// 模板（五字段契约：page_title / static_base / csrf_token / nav_display_name / nav_is_admin）
// ---------------------------------------------------------------------------

#[derive(askama::Template, WebTemplate)]
#[template(path = "task/list.html")]
pub struct ListTemplate {
    pub page_title: String,
    pub static_base: String,
    pub csrf_token: String,
    pub nav_display_name: String,
    pub nav_is_admin: bool,
    pub nav_is_guest: bool,
    pub look: crate::shared::appearance::Appearance,
    pub nav_active: &'static str,
    pub page: Page<TaskRowView>,
    /// 分页链接要保留的查询串（`mine=1&` 或空串）。拼在模板里容易漏掉 `&`。
    pub page_query: String,
    pub mine: bool,
    pub can_write: bool,
}

#[derive(askama::Template, WebTemplate)]
#[template(path = "task/detail.html")]
pub struct DetailTemplate {
    pub page_title: String,
    pub static_base: String,
    pub csrf_token: String,
    pub nav_display_name: String,
    pub nav_is_admin: bool,
    pub nav_is_guest: bool,
    pub look: crate::shared::appearance::Appearance,
    pub nav_active: &'static str,
    pub task: Task,
    pub status_key: &'static str,
    pub status_label: &'static str,
    pub statuses: Vec<StatusOption>,
    pub assignee_label: String,
    /// 「未指派」这一项是否选中。模板里不去调 `Option::is_none()`
    /// ——模板能做的事越少，出错时越好查。
    pub no_assignee_selected: bool,
    pub due_on: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub members: Vec<MemberOption>,
    pub can_assign: bool,
    pub can_status: bool,
    pub can_delete: bool,
    pub source_thread_id: Option<u64>,
}

#[derive(askama::Template, WebTemplate)]
#[template(path = "task/new.html")]
pub struct NewTemplate {
    pub page_title: String,
    pub static_base: String,
    pub csrf_token: String,
    pub nav_display_name: String,
    pub nav_is_admin: bool,
    pub nav_is_guest: bool,
    pub look: crate::shared::appearance::Appearance,
    pub nav_active: &'static str,
    pub title: String,
    pub body: String,
    pub due_on: String,
    pub assignee: String,
    pub members: Vec<MemberOption>,
    pub error: Option<String>,
}

// ---------------------------------------------------------------------------
// 辅助
// ---------------------------------------------------------------------------

use askama_web::WebTemplate;

async fn framework(
    state: &AppState,
    session: &Session,
    user: &MaybeUser,
    row: &MaybeUserRow,
) -> Result<Framework, AppError> {
    let csrf_token = ensure_csrf(session).await?;
    Ok(Framework {
        static_base: state.config.static_base_url.clone(),
        csrf_token,
        nav_display_name: row.display_name().to_owned(),
        nav_is_admin: user.is_admin(),
        nav_is_guest: crate::auth::policy::is_guest(user.user()),
        look: row.look(),
        nav_active: "tasks",
    })
}

fn stamp(at: chrono::DateTime<chrono::Utc>, tz: chrono::FixedOffset) -> String {
    at.with_timezone(&tz).format("%Y-%m-%d %H:%M").to_string()
}

/// 未登录 → 登录页；普通 user → 403。任务板不对游客与外部注册用户开放。
fn require_reader(user: &MaybeUser, next: &str) -> Result<CurrentUser, AppError> {
    let Some(current) = user.user() else {
        return Err(AppError::LoginRequired {
            next: next.to_owned(),
        });
    };
    if policy::can_view(
        Some(current),
        &ContentCtx::new(Visibility::Internal, None, None),
    ) {
        Ok(current.clone())
    } else {
        Err(AppError::Forbidden)
    }
}

fn parse_id(raw: Option<&str>) -> Option<u64> {
    raw.map(str::trim)
        .filter(|value| !value.is_empty())
        .and_then(|value| value.parse::<u64>().ok())
}

fn row_view(card: &TaskCard, tz: chrono::FixedOffset) -> TaskRowView {
    TaskRowView {
        id: card.id,
        title: card.title.clone(),
        status_label: card.status.label(),
        status_key: card.status.as_str(),
        assignee: card.assignee_name.clone(),
        due_on: card.due_on.map(|date| date.format("%Y-%m-%d").to_string()),
        updated_at: stamp(card.updated_at, tz),
    }
}

async fn member_options(
    state: &AppState,
    selected: Option<u64>,
) -> Result<Vec<MemberOption>, AppError> {
    let users = group_model::list_active_users(&state.pool).await?;
    Ok(users
        .into_iter()
        .map(|user| MemberOption {
            id: user.user_id,
            name: if user.display_name.is_empty() {
                user.username
            } else {
                user.display_name
            },
            selected: selected == Some(user.user_id),
        })
        .collect())
}

/// 表单里没填日期时要按"没有截止日"处理，而不是把空串当非法输入打回。
fn parse_due_on(raw: &str) -> Result<Option<chrono::NaiveDate>, AppError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    chrono::NaiveDate::parse_from_str(trimmed, "%Y-%m-%d")
        .map(Some)
        .map_err(|_| AppError::unprocessable("截止日期要用 2026-10-30 这种格式"))
}

// ---------------------------------------------------------------------------
// handler
// ---------------------------------------------------------------------------

pub async fn list_page(
    State(state): State<AppState>,
    session: Session,
    user: MaybeUser,
    row: MaybeUserRow,
    Query(query): Query<ListQuery>,
) -> Result<ListTemplate, AppError> {
    let viewer = require_reader(&user, "/tasks")?;
    let framework = framework(&state, &session, &user, &row).await?;
    let tz = state.config.display_tz;

    let mine = query
        .mine
        .as_deref()
        .map(|value| matches!(value, "1" | "true" | "yes"))
        .unwrap_or(false);
    let page_query = PageQuery::new(normalize_page(query.page.as_deref()));

    let page = service::list_page(&state.pool, &viewer, mine, &page_query).await?;
    let items = page
        .items
        .iter()
        .map(|card| row_view(card, tz))
        .collect::<Vec<_>>();
    let page = Page::new(items, page.page, page.total, PAGE_SIZE);

    Ok(ListTemplate {
        page_title: if mine {
            "我的任务 · NEXUS".to_owned()
        } else {
            "任务 · NEXUS".to_owned()
        },
        static_base: framework.static_base,
        csrf_token: framework.csrf_token,
        nav_display_name: framework.nav_display_name,
        nav_is_admin: framework.nav_is_admin,
        nav_is_guest: framework.nav_is_guest,
        look: framework.look,
        nav_active: framework.nav_active,
        page,
        page_query: if mine {
            "mine=1&".to_owned()
        } else {
            String::new()
        },
        mine,
        can_write: policy::can_write(
            Some(&viewer),
            &ContentCtx::new(Visibility::Internal, None, None),
        ),
    })
}

pub async fn new_page(
    State(state): State<AppState>,
    session: Session,
    user: MaybeUser,
    row: MaybeUserRow,
) -> Result<NewTemplate, AppError> {
    let viewer = require_reader(&user, "/tasks/new")?;
    if !policy::can_write(
        Some(&viewer),
        &ContentCtx::new(Visibility::Internal, None, None),
    ) {
        return Err(AppError::Forbidden);
    }
    let framework = framework(&state, &session, &user, &row).await?;
    let members = member_options(&state, None).await?;

    Ok(NewTemplate {
        page_title: "新建任务 · NEXUS".to_owned(),
        static_base: framework.static_base,
        csrf_token: framework.csrf_token,
        nav_display_name: framework.nav_display_name,
        nav_is_admin: framework.nav_is_admin,
        nav_is_guest: framework.nav_is_guest,
        look: framework.look,
        nav_active: framework.nav_active,
        title: String::new(),
        body: String::new(),
        due_on: String::new(),
        assignee: String::new(),
        members,
        error: None,
    })
}

pub async fn new_submit(
    State(state): State<AppState>,
    session: Session,
    user: MaybeUser,
    row: MaybeUserRow,
    client_ip: ClientIp,
    Form(form): Form<NewTaskForm>,
) -> Result<Response, AppError> {
    let viewer = require_reader(&user, "/tasks/new")?;
    require_csrf(&session, Some(&form._csrf)).await?;
    let framework = framework(&state, &session, &user, &row).await?;

    let body = form.body.trim();
    let body = if body.is_empty() { None } else { Some(body) };
    let assignee_id = parse_id(Some(&form.assignee));

    let result = service::create(
        &state.pool,
        &viewer,
        client_ip.0.as_deref(),
        TaskInput {
            title: form.title.trim(),
            body,
            assignee_id,
            due_on: match parse_due_on(&form.due_on) {
                Ok(value) => value,
                Err(error) => {
                    // 日期格式错也要带回填的表单，否则用户白填一遍。
                    let template = NewTemplate {
                        page_title: "新建任务 · NEXUS".to_owned(),
                        static_base: framework.static_base,
                        csrf_token: framework.csrf_token,
                        nav_display_name: framework.nav_display_name,
                        nav_is_admin: framework.nav_is_admin,
                        nav_is_guest: framework.nav_is_guest,
                        look: framework.look,
                        nav_active: framework.nav_active,
                        title: form.title,
                        body: form.body,
                        due_on: form.due_on,
                        assignee: form.assignee,
                        members: member_options(&state, assignee_id).await?,
                        error: Some(error.public_message().into_owned()),
                    };
                    return Ok((StatusCode::UNPROCESSABLE_ENTITY, template).into_response());
                }
            },
        },
    )
    .await;

    match result {
        Ok(id) => Ok(redirect_to(&format!("/tasks/{id}"))),
        Err(AppError::Unprocessable(message)) => {
            let template = NewTemplate {
                page_title: "新建任务 · NEXUS".to_owned(),
                static_base: framework.static_base,
                csrf_token: framework.csrf_token,
                nav_display_name: framework.nav_display_name,
                nav_is_admin: framework.nav_is_admin,
                nav_is_guest: framework.nav_is_guest,
                look: framework.look,
                nav_active: framework.nav_active,
                title: form.title,
                body: form.body,
                due_on: form.due_on,
                assignee: form.assignee,
                members: member_options(&state, assignee_id).await?,
                error: Some(message.into_owned()),
            };
            Ok((StatusCode::UNPROCESSABLE_ENTITY, template).into_response())
        }
        Err(other) => Err(other),
    }
}

pub async fn detail_page(
    State(state): State<AppState>,
    session: Session,
    user: MaybeUser,
    row: MaybeUserRow,
    Path(task_id): Path<u64>,
) -> Result<DetailTemplate, AppError> {
    let viewer = require_reader(&user, "/tasks")?;
    let framework = framework(&state, &session, &user, &row).await?;
    let task = service::visible_task(&state.pool, &viewer, task_id).await?;
    let tz = state.config.display_tz;

    let is_admin = policy::can_admin(Some(&viewer));
    let can_assign = is_admin || task.created_by == viewer.id;
    let can_status = can_assign || task.assignee_id == Some(viewer.id);
    let can_delete = can_assign;
    let members = if can_assign {
        member_options(&state, task.assignee_id).await?
    } else {
        Vec::new()
    };

    let status_key = task.status.as_str();
    let status_label = task.status.label();
    let statuses = TaskStatus::ALL
        .iter()
        .map(|status| StatusOption {
            key: status.as_str(),
            label: status.label(),
            selected: status.as_str() == status_key,
        })
        .collect();

    Ok(DetailTemplate {
        page_title: format!("{} · NEXUS", task.title),
        static_base: framework.static_base,
        csrf_token: framework.csrf_token,
        nav_display_name: framework.nav_display_name,
        nav_is_admin: framework.nav_is_admin,
        nav_is_guest: framework.nav_is_guest,
        look: framework.look,
        nav_active: framework.nav_active,
        assignee_label: task
            .assignee_name
            .clone()
            .unwrap_or_else(|| "未指派".to_owned()),
        no_assignee_selected: task.assignee_id.is_none(),
        due_on: task.due_on.map(|date| date.format("%Y-%m-%d").to_string()),
        created_at: stamp(task.created_at, tz),
        updated_at: stamp(task.updated_at, tz),
        source_thread_id: task.source_thread_id,
        status_key,
        status_label,
        statuses,
        members,
        can_assign,
        can_status,
        can_delete,
        task,
    })
}

pub async fn status_submit(
    State(state): State<AppState>,
    session: Session,
    user: MaybeUser,
    client_ip: ClientIp,
    Path(task_id): Path<u64>,
    Form(form): Form<StatusForm>,
) -> Result<Response, AppError> {
    let viewer = require_reader(&user, "/tasks")?;
    require_csrf(&session, Some(&form._csrf)).await?;

    let status =
        TaskStatus::parse(&form.status).ok_or_else(|| AppError::unprocessable("未知的任务状态"))?;
    service::change_status(
        &state.pool,
        &viewer,
        client_ip.0.as_deref(),
        task_id,
        status,
    )
    .await?;

    Ok(redirect_to(&format!("/tasks/{task_id}")))
}

pub async fn assign_submit(
    State(state): State<AppState>,
    session: Session,
    user: MaybeUser,
    client_ip: ClientIp,
    Path(task_id): Path<u64>,
    Form(form): Form<AssignForm>,
) -> Result<Response, AppError> {
    let viewer = require_reader(&user, "/tasks")?;
    require_csrf(&session, Some(&form._csrf)).await?;

    let assignee = parse_id(Some(&form.assignee));
    service::assign(
        &state.pool,
        &viewer,
        client_ip.0.as_deref(),
        task_id,
        assignee,
    )
    .await?;

    Ok(redirect_to(&format!("/tasks/{task_id}")))
}

pub async fn delete_submit(
    State(state): State<AppState>,
    session: Session,
    user: MaybeUser,
    client_ip: ClientIp,
    Path(task_id): Path<u64>,
    Form(form): Form<CsrfForm>,
) -> Result<Response, AppError> {
    let viewer = require_reader(&user, "/tasks")?;
    require_csrf(&session, Some(&form._csrf)).await?;
    service::delete(&state.pool, &viewer, client_ip.0.as_deref(), task_id).await?;

    Ok(redirect_to("/tasks"))
}

pub async fn from_thread_submit(
    State(state): State<AppState>,
    session: Session,
    user: MaybeUser,
    client_ip: ClientIp,
    Path(thread_id): Path<u64>,
    Form(form): Form<CsrfForm>,
) -> Result<Response, AppError> {
    let viewer = require_reader(&user, "/tasks")?;
    require_csrf(&session, Some(&form._csrf)).await?;
    let id = service::create_from_thread(&state.pool, &viewer, client_ip.0.as_deref(), thread_id)
        .await?;

    Ok(redirect_to(&format!("/tasks/{id}")))
}
