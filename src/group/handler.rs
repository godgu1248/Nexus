//! 小组的 HTTP 层：取表单、调 service、渲染模板。
//!
//! 这一层不写库、不判业务权限。它只做三件事：把请求变成 Rust 值、调 `service`、
//! 把结果塞进模板结构体。每个整页模板都带五字段契约
//! （`page_title` / `static_base` / `csrf_token` / `nav_display_name` / `nav_is_admin`）。

use askama::Template;
use askama_web::WebTemplate;
use axum::Form;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Redirect, Response};
use serde::Deserialize;
use tower_sessions::Session;

use crate::auth::middleware::{self, ClientIp, MaybeUser, MaybeUserRow};
use crate::auth::policy::{self, CurrentUser};
use crate::error::AppError;
use crate::group::model::{self, Candidate, Member};
use crate::group::service::{self, NewGroupInput};
use crate::state::AppState;

#[derive(Debug, Deserialize)]
pub struct NewGroupForm {
    pub name: String,
    pub slug: String,
    #[serde(default)]
    pub description: String,
    #[serde(rename = "_csrf")]
    pub csrf: String,
}

/// 成员页的两个动作共用一个表单：`action` 决定加还是移。
///
/// 不用两个路由，是因为它们作用在同一个对象（`(组, 人)` 这一对事实）上，
/// 分成两条路径会让"加"和"移"的权限判定各写一遍，早晚漂移。
#[derive(Debug, Deserialize)]
pub struct MemberForm {
    pub action: String,
    pub user_id: u64,
    #[serde(rename = "_csrf")]
    pub csrf: String,
}

#[derive(Debug, Clone)]
pub struct GroupCardView {
    pub name: String,
    pub slug: String,
    pub description: String,
    pub member_count: u64,
}

#[derive(Template, WebTemplate)]
#[template(path = "group/list.html")]
pub struct ListTemplate {
    pub page_title: String,
    pub static_base: String,
    pub csrf_token: String,
    pub nav_display_name: String,
    pub nav_is_admin: bool,
    pub nav_is_guest: bool,
    pub look: crate::shared::appearance::Appearance,
    pub nav_active: &'static str,
    pub groups: Vec<GroupCardView>,
    pub is_admin: bool,
}

#[derive(Template, WebTemplate)]
#[template(path = "group/new.html")]
pub struct NewTemplate {
    pub page_title: String,
    pub static_base: String,
    pub csrf_token: String,
    pub nav_display_name: String,
    pub nav_is_admin: bool,
    pub nav_is_guest: bool,
    pub look: crate::shared::appearance::Appearance,
    pub nav_active: &'static str,
    pub error: Option<String>,
    pub name: String,
    pub slug: String,
    pub description: String,
}

#[derive(Debug, Clone)]
pub struct MemberView {
    pub user_id: u64,
    pub display_name: String,
    pub username: String,
}

impl From<&Member> for MemberView {
    fn from(member: &Member) -> Self {
        Self {
            user_id: member.user_id,
            display_name: member.display_name.clone(),
            username: member.username.clone(),
        }
    }
}

impl From<&Candidate> for MemberView {
    fn from(candidate: &Candidate) -> Self {
        Self {
            user_id: candidate.user_id,
            display_name: candidate.display_name.clone(),
            username: candidate.username.clone(),
        }
    }
}

#[derive(Template, WebTemplate)]
#[template(path = "group/members.html")]
pub struct MembersTemplate {
    pub page_title: String,
    pub static_base: String,
    pub csrf_token: String,
    pub nav_display_name: String,
    pub nav_is_admin: bool,
    pub nav_is_guest: bool,
    pub look: crate::shared::appearance::Appearance,
    pub nav_active: &'static str,
    pub group_name: String,
    pub group_slug: String,
    pub error: Option<String>,
    pub members: Vec<MemberView>,
    pub candidates: Vec<MemberView>,
}

/// 管理动作的统一前置判定：没登录 → 去登录页；登录了但不是 admin → 403。
///
/// 与 `forum::handler::require_admin` 同构，但这里不去 `pub` 它——
/// 两个模块各有一份三行的判定，好过让 `group` 依赖 `forum` 的内部工具。
fn require_admin<'a>(user: &'a MaybeUser, next: &str) -> Result<&'a CurrentUser, AppError> {
    match user.user() {
        None => Err(AppError::LoginRequired {
            next: next.to_owned(),
        }),
        Some(current) if policy::can_admin(Some(current)) => Ok(current),
        Some(_) => Err(AppError::Forbidden),
    }
}

fn require_login<'a>(user: &'a MaybeUser, next: &str) -> Result<&'a CurrentUser, AppError> {
    user.user().ok_or_else(|| AppError::LoginRequired {
        next: next.to_owned(),
    })
}

fn base(state: &AppState, user: &MaybeUser) -> Framework {
    Framework {
        static_base: state.static_base().to_owned(),
        csrf_token: String::new(),
        nav_display_name: String::new(),
        nav_is_admin: user.is_admin(),
        nav_is_guest: crate::auth::policy::is_guest(user.user()),
        look: crate::shared::appearance::Appearance::DEFAULT,
        nav_active: "groups",
    }
}

/// 五字段里"跟页面无关的那四个"的打包。`csrf_token` 需要 await 才能拿，
/// 所以单独一步填进去，避免每个 handler 都重复四行。
struct Framework {
    static_base: String,
    csrf_token: String,
    nav_display_name: String,
    nav_is_admin: bool,
    nav_is_guest: bool,
    look: crate::shared::appearance::Appearance,
    nav_active: &'static str,
}

impl Framework {
    async fn with_csrf(mut self, session: &Session, row: &MaybeUserRow) -> Result<Self, AppError> {
        self.csrf_token = middleware::ensure_csrf(session).await?;
        self.nav_display_name = row.display_name().to_owned();
        self.look = row.look();
        Ok(self)
    }
}

pub async fn list_page(
    State(state): State<AppState>,
    session: Session,
    user: MaybeUser,
    row: MaybeUserRow,
) -> Result<ListTemplate, AppError> {
    let current = require_login(&user, "/groups")?;
    let framework = base(&state, &user).with_csrf(&session, &row).await?;

    let cards = if policy::can_admin(Some(current)) {
        model::list_groups_with_counts(&state.pool).await?
    } else {
        model::list_groups_for_user(&state.pool, current.id).await?
    };

    Ok(ListTemplate {
        page_title: "小组 · NEXUS".to_owned(),
        static_base: framework.static_base,
        csrf_token: framework.csrf_token,
        nav_display_name: framework.nav_display_name,
        nav_is_admin: framework.nav_is_admin,
        nav_is_guest: framework.nav_is_guest,
        look: framework.look,
        nav_active: framework.nav_active,
        groups: cards
            .iter()
            .map(|card| GroupCardView {
                name: card.group.name.clone(),
                slug: card.group.slug.clone(),
                description: card.group.description.clone(),
                member_count: card.member_count,
            })
            .collect(),
        is_admin: policy::can_admin(Some(current)),
    })
}

pub async fn new_page(
    State(state): State<AppState>,
    session: Session,
    user: MaybeUser,
    row: MaybeUserRow,
) -> Result<NewTemplate, AppError> {
    require_admin(&user, "/groups/new")?;
    let framework = base(&state, &user).with_csrf(&session, &row).await?;
    Ok(NewTemplate {
        page_title: "新建小组 · NEXUS".to_owned(),
        static_base: framework.static_base,
        csrf_token: framework.csrf_token,
        nav_display_name: framework.nav_display_name,
        nav_is_admin: framework.nav_is_admin,
        nav_is_guest: framework.nav_is_guest,
        look: framework.look,
        nav_active: framework.nav_active,
        error: None,
        name: String::new(),
        slug: String::new(),
        description: String::new(),
    })
}

pub async fn new_submit(
    State(state): State<AppState>,
    session: Session,
    user: MaybeUser,
    row: MaybeUserRow,
    client_ip: ClientIp,
    Form(form): Form<NewGroupForm>,
) -> Result<Response, AppError> {
    let current = require_admin(&user, "/groups/new")?;
    middleware::require_csrf(&session, Some(&form.csrf)).await?;

    let input = NewGroupInput {
        name: &form.name,
        slug: &form.slug,
        description: &form.description,
    };

    match service::create_group(&state.pool, current, client_ip.0.as_deref(), input).await {
        Ok(_) => Ok(Redirect::to(&format!("/groups/{}/members", form.slug)).into_response()),
        // 表单错误：回 422 **并且**带着用户填过的值重渲染。
        // 只回 200 会让 htmx 当成功处理；只回 422 不给表单，用户就得重填一遍。
        Err(AppError::Unprocessable(message)) => {
            let framework = base(&state, &user).with_csrf(&session, &row).await?;
            let template = NewTemplate {
                page_title: "新建小组 · NEXUS".to_owned(),
                static_base: framework.static_base,
                csrf_token: framework.csrf_token,
                nav_display_name: framework.nav_display_name,
                nav_is_admin: framework.nav_is_admin,
                nav_is_guest: framework.nav_is_guest,
                look: framework.look,
                nav_active: framework.nav_active,
                error: Some(message.to_string()),
                name: form.name,
                slug: form.slug,
                description: form.description,
            };
            Ok((StatusCode::UNPROCESSABLE_ENTITY, template).into_response())
        }
        Err(other) => Err(other),
    }
}

pub async fn members_page(
    State(state): State<AppState>,
    session: Session,
    user: MaybeUser,
    row: MaybeUserRow,
    Path(slug): Path<String>,
) -> Result<MembersTemplate, AppError> {
    require_admin(&user, &format!("/groups/{slug}/members"))?;
    let framework = base(&state, &user).with_csrf(&session, &row).await?;

    let group = model::find_group_by_slug(&state.pool, &slug)
        .await?
        .ok_or(AppError::NotFound)?;
    let members = model::members_of(&state.pool, group.id).await?;
    let candidates = model::list_active_users(&state.pool).await?;

    // 已经在组里的人不再出现在"可添加"列表里：界面上少一个必然失败的操作。
    let existing: Vec<u64> = members.iter().map(|m| m.user_id).collect();
    let candidates: Vec<MemberView> = candidates
        .iter()
        .filter(|candidate| !existing.contains(&candidate.user_id))
        .map(MemberView::from)
        .collect();

    Ok(MembersTemplate {
        page_title: format!("{} · 小组成员 · NEXUS", group.name),
        static_base: framework.static_base,
        csrf_token: framework.csrf_token,
        nav_display_name: framework.nav_display_name,
        nav_is_admin: framework.nav_is_admin,
        nav_is_guest: framework.nav_is_guest,
        look: framework.look,
        nav_active: framework.nav_active,
        group_name: group.name.clone(),
        group_slug: group.slug.clone(),
        error: None,
        members: members.iter().map(MemberView::from).collect(),
        candidates,
    })
}

pub async fn members_submit(
    State(state): State<AppState>,
    session: Session,
    user: MaybeUser,
    client_ip: ClientIp,
    Path(slug): Path<String>,
    Form(form): Form<MemberForm>,
) -> Result<Response, AppError> {
    let current = require_admin(&user, &format!("/groups/{slug}/members"))?;
    middleware::require_csrf(&session, Some(&form.csrf)).await?;

    let group = model::find_group_by_slug(&state.pool, &slug)
        .await?
        .ok_or(AppError::NotFound)?;
    let ip = client_ip.0.as_deref();

    match form.action.as_str() {
        "add" => {
            service::add_member(&state.pool, current, ip, group.id, form.user_id).await?;
        }
        "remove" => {
            service::remove_member(&state.pool, current, ip, group.id, form.user_id).await?;
        }
        other => {
            return Err(AppError::bad_request(format!("未知的成员操作：{other}")));
        }
    }

    Ok(Redirect::to(&format!("/groups/{slug}/members")).into_response())
}
