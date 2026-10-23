//! 档案的 HTTP 层：取参数、调 service、渲染模板。
//!
//! 这一层不写库、不判业务权限、不写审计。每个整页模板都带五字段契约
//! （`page_title` / `static_base` / `csrf_token` / `nav_display_name` / `nav_is_admin`）。

use askama::Template;
use askama_web::WebTemplate;
use axum::Form;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Redirect, Response};
use chrono::{DateTime, FixedOffset, Utc};
use serde::Deserialize;
use tower_sessions::Session;

use crate::auth::middleware::{self, ClientIp, MaybeUser, MaybeUserRow};
use crate::auth::policy::{self, ContentCtx, CurrentUser};
use crate::error::AppError;
use crate::kb::model::{DocCard, KbDoc};
use crate::kb::service::{self, DocInput};
use crate::shared::pagination::{Page, PageQuery};
use crate::shared::redirect_to;
use crate::shared::visibility::Visibility;
use crate::state::AppState;

/// 新建 / 编辑共用的表单。`parent_id` 只在新建时有值。
#[derive(Debug, Deserialize)]
pub struct DocForm {
    #[serde(default)]
    pub parent_id: Option<u64>,
    pub title: String,
    pub body: String,
    /// 富文本 HTML（编辑器跑起来才会出现在表单里）。
    #[serde(default)]
    pub body_html: String,
    #[serde(default)]
    pub visibility: String,
    /// 空串表示"没有小组"。用 String 而不是 Option<u64>：
    /// 表单里 `<option value="">` 提交上来的是空串，直接反序列化成 Option<u64> 会失败。
    #[serde(default)]
    pub group_id: String,
    #[serde(default)]
    pub sort_order: String,
    #[serde(rename = "_csrf")]
    pub csrf: String,
}

#[derive(Debug, Deserialize)]
pub struct PublishForm {
    pub value: String,
    #[serde(rename = "_csrf")]
    pub csrf: String,
}

#[derive(Debug, Deserialize)]
pub struct CsrfForm {
    #[serde(rename = "_csrf")]
    pub csrf: String,
}

#[derive(Debug, Deserialize)]
pub struct SearchQuery {
    #[serde(default)]
    pub q: String,
    #[serde(default)]
    pub page: Option<u32>,
}

#[derive(Debug, Deserialize)]
pub struct ParentQuery {
    #[serde(default)]
    pub parent: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct CardView {
    pub id: u64,
    pub title: String,
    pub updated_at: String,
    pub visibility: String,
    pub visibility_key: String,
    pub is_published: bool,
    /// 置顶的文档在列表里也要看得出来，否则"排在最前面"和"碰巧更新过"分不清。
    pub is_pinned: bool,
    pub child_count: u64,
}

#[derive(Debug, Clone)]
pub struct CrumbView {
    pub id: u64,
    pub title: String,
}

#[derive(Debug, Clone)]
pub struct DocView {
    pub id: u64,
    pub title: String,
    pub body: String,
    /// 读时再净化一次：库里存的是净化结果，但历史数据与手改库都绕不过这一层。
    pub body_html: Option<crate::shared::SafeHtml>,
    pub author_name: String,
    pub updated_at: String,
    pub published_at: String,
    pub visibility: String,
    pub visibility_key: String,
    pub is_published: bool,
    pub view_count: u64,
    pub is_pinned: bool,
    pub pin_order: i32,
    /// 只有 admin 能改置顶；模板据此决定要不要渲染那个小表单。
    pub can_pin: bool,
}

#[derive(Debug, Clone)]
pub struct GroupOption {
    pub id: u64,
    pub name: String,
    /// True when the current form value points at this group.
    pub selected: bool,
}

#[derive(Template, WebTemplate)]
#[template(path = "kb/list.html")]
pub struct ListTemplate {
    pub page_title: String,
    pub static_base: String,
    pub csrf_token: String,
    pub nav_display_name: String,
    pub nav_is_admin: bool,
    pub nav_is_guest: bool,
    pub look: crate::shared::appearance::Appearance,
    pub nav_active: &'static str,
    pub roots: Vec<CardView>,
    pub recent: Vec<CardView>,
    pub can_write: bool,
}

#[derive(Template, WebTemplate)]
#[template(path = "kb/doc.html")]
pub struct DocTemplate {
    pub page_title: String,
    pub static_base: String,
    pub csrf_token: String,
    pub nav_display_name: String,
    pub nav_is_admin: bool,
    pub nav_is_guest: bool,
    pub look: crate::shared::appearance::Appearance,
    pub nav_active: &'static str,
    pub doc: DocView,
    pub crumbs: Vec<CrumbView>,
    pub children: Vec<CardView>,
    pub can_manage: bool,
}

#[derive(Template, WebTemplate)]
#[template(path = "kb/form.html")]
pub struct FormTemplate {
    pub page_title: String,
    pub static_base: String,
    pub csrf_token: String,
    pub nav_display_name: String,
    pub nav_is_admin: bool,
    pub nav_is_guest: bool,
    pub look: crate::shared::appearance::Appearance,
    pub nav_active: &'static str,
    pub heading: String,
    pub form_action: String,
    pub parent_id: Option<u64>,
    pub title: String,
    pub body: String,
    pub visibility: String,
    pub group_id: String,
    pub sort_order: String,
    pub groups: Vec<GroupOption>,
    pub error: Option<String>,
    pub cancel_href: String,
}

#[derive(Template, WebTemplate)]
#[template(path = "kb/search.html")]
pub struct SearchTemplate {
    pub page_title: String,
    pub static_base: String,
    pub csrf_token: String,
    pub nav_display_name: String,
    pub nav_is_admin: bool,
    pub nav_is_guest: bool,
    pub look: crate::shared::appearance::Appearance,
    pub nav_active: &'static str,
    pub q: String,
    pub query_prefix: String,
    pub result: Page<CardView>,
}

/// 五字段里"跟页面无关的那四个"，`csrf_token` 需要 await，单独填。
struct Framework {
    static_base: String,
    csrf_token: String,
    nav_display_name: String,
    nav_is_admin: bool,
    nav_is_guest: bool,
    look: crate::shared::appearance::Appearance,
    nav_active: &'static str,
}

fn base(state: &AppState, user: &MaybeUser) -> Framework {
    Framework {
        static_base: state.static_base().to_owned(),
        csrf_token: String::new(),
        nav_display_name: String::new(),
        nav_is_admin: user.is_admin(),
        nav_is_guest: crate::auth::policy::is_guest(user.user()),
        look: crate::shared::appearance::Appearance::DEFAULT,
        nav_active: "kb",
    }
}

impl Framework {
    async fn with_csrf(mut self, session: &Session, row: &MaybeUserRow) -> Result<Self, AppError> {
        self.csrf_token = middleware::ensure_csrf(session).await?;
        self.nav_display_name = row.display_name().to_owned();
        self.look = row.look();
        Ok(self)
    }
}

fn visible_label(visibility: Visibility) -> &'static str {
    match visibility {
        Visibility::Public => "公开",
        Visibility::Internal => "内部",
        Visibility::Group => "小组",
    }
}

fn stamp(dt: DateTime<Utc>, tz: FixedOffset) -> String {
    dt.with_timezone(&tz).format("%Y-%m-%d %H:%M").to_string()
}

fn card_of(card: &DocCard, tz: FixedOffset) -> CardView {
    CardView {
        id: card.id,
        title: card.title.clone(),
        updated_at: stamp(card.updated_at, tz),
        visibility: visible_label(card.visibility).to_owned(),
        visibility_key: card.visibility.as_str().to_owned(),
        is_published: card.is_published,
        is_pinned: card.is_pinned,
        child_count: card.child_count,
    }
}

fn doc_view(doc: &KbDoc, tz: FixedOffset) -> DocView {
    DocView {
        id: doc.id,
        title: doc.title.clone(),
        body: doc.body.clone(),
        body_html: doc
            .body_html
            .as_ref()
            .map(|html| crate::shared::richtext::sanitize(html)),
        author_name: doc.author_name.clone(),
        updated_at: stamp(doc.updated_at, tz),
        published_at: doc.published_at.map(|at| stamp(at, tz)).unwrap_or_default(),
        visibility: visible_label(doc.visibility).to_owned(),
        visibility_key: doc.visibility.as_str().to_owned(),
        is_published: doc.is_published,
        view_count: doc.view_count,
        is_pinned: doc.is_pinned,
        pin_order: doc.pin_order,
        can_pin: false,
    }
}

/// 写操作的前置判定：没登录 → 去登录页；登录了但没有写权限 → 403。
fn require_writer<'a>(user: &'a MaybeUser, next: &str) -> Result<&'a CurrentUser, AppError> {
    match user.user() {
        None => Err(AppError::LoginRequired {
            next: next.to_owned(),
        }),
        Some(current)
            if policy::can_write(
                Some(current),
                &ContentCtx::new(Visibility::Internal, None, None),
            ) =>
        {
            Ok(current)
        }
        Some(_) => Err(AppError::Forbidden),
    }
}

/// 改 / 发 / 删的前置判定：作者本人或 admin。
fn require_manager<'a>(
    user: &'a MaybeUser,
    doc: &KbDoc,
    next: &str,
) -> Result<&'a CurrentUser, AppError> {
    match user.user() {
        None => Err(AppError::LoginRequired {
            next: next.to_owned(),
        }),
        Some(current) if current.id == doc.author_id || policy::can_admin(Some(current)) => {
            Ok(current)
        }
        Some(_) => Err(AppError::Forbidden),
    }
}

/// 表单里的可见性 + 小组字段 → 服务层参数。解析失败按 422 处理，别让它变成 500。
fn parse_visibility(raw: &str, group_raw: &str) -> Result<(Visibility, Option<u64>), AppError> {
    let visibility: Visibility = raw
        .trim()
        .parse()
        .map_err(|_| AppError::unprocessable("可见性只能是 公开 / 内部 / 小组"))?;

    let group_id = match group_raw.trim() {
        "" => None,
        other => Some(
            other
                .parse::<u64>()
                .map_err(|_| AppError::unprocessable("小组参数不合法"))?,
        ),
    };

    Ok((visibility, group_id))
}

/// 可选的小组下拉项：成员看自己所在的组，admin 看全部。
async fn group_options(
    pool: &sqlx::MySqlPool,
    user: &CurrentUser,
    current_group: Option<u64>,
) -> Result<Vec<GroupOption>, AppError> {
    // Admin lists every group; members only their own. The two model functions
    // return different types (Group vs GroupCard), so normalise to (id, name)
    // here instead of leaking that difference into the template layer.
    let mut pairs: Vec<(u64, String)> = Vec::new();
    if policy::can_admin(Some(user)) {
        for group in crate::group::model::list_groups(pool).await? {
            pairs.push((group.id, group.name));
        }
    } else {
        for card in crate::group::model::list_groups_for_user(pool, user.id).await? {
            pairs.push((card.group.id, card.group.name));
        }
    }

    Ok(pairs
        .into_iter()
        .map(|(id, name)| GroupOption {
            id,
            name,
            selected: current_group == Some(id),
        })
        .collect())
}

pub async fn list_page(
    State(state): State<AppState>,
    session: Session,
    user: MaybeUser,
    row: MaybeUserRow,
) -> Result<ListTemplate, AppError> {
    let framework = base(&state, &user).with_csrf(&session, &row).await?;
    let tz = state.config.display_tz;
    let viewer = user.user();

    let roots = crate::kb::model::list_roots(&state.pool, viewer).await?;
    let recent = crate::kb::model::list_recent(&state.pool, viewer, 8).await?;
    let can_write = viewer
        .map(|current| {
            policy::can_write(
                Some(current),
                &ContentCtx::new(Visibility::Internal, None, None),
            )
        })
        .unwrap_or(false);

    Ok(ListTemplate {
        page_title: "档案 · NEXUS".to_owned(),
        static_base: framework.static_base,
        csrf_token: framework.csrf_token,
        nav_display_name: framework.nav_display_name,
        nav_is_admin: framework.nav_is_admin,
        nav_is_guest: framework.nav_is_guest,
        look: framework.look,
        nav_active: framework.nav_active,
        roots: roots.iter().map(|card| card_of(card, tz)).collect(),
        recent: recent.iter().map(|card| card_of(card, tz)).collect(),
        can_write,
    })
}

pub async fn search_page(
    State(state): State<AppState>,
    session: Session,
    user: MaybeUser,
    row: MaybeUserRow,
    Query(query): Query<SearchQuery>,
) -> Result<SearchTemplate, AppError> {
    let framework = base(&state, &user).with_csrf(&session, &row).await?;
    let tz = state.config.display_tz;
    let q = query.q.trim().to_owned();

    let page = PageQuery {
        page: query.page.unwrap_or(1),
    };
    let result = service::search_page(&state.pool, user.user(), &q, &page).await?;

    let cards: Vec<CardView> = result.items.iter().map(|card| card_of(card, tz)).collect();
    let result = Page::new(cards, result.page, result.total, result.per_page);

    Ok(SearchTemplate {
        page_title: "查档案 · NEXUS".to_owned(),
        static_base: framework.static_base,
        csrf_token: framework.csrf_token,
        nav_display_name: framework.nav_display_name,
        nav_is_admin: framework.nav_is_admin,
        nav_is_guest: framework.nav_is_guest,
        look: framework.look,
        nav_active: framework.nav_active,
        query_prefix: format!("q={}&", encode_component(&q)),
        q,
        result,
    })
}

pub async fn new_page(
    State(state): State<AppState>,
    session: Session,
    user: MaybeUser,
    row: MaybeUserRow,
    Query(query): Query<ParentQuery>,
) -> Result<FormTemplate, AppError> {
    let current = require_writer(&user, "/kb/new")?;
    let framework = base(&state, &user).with_csrf(&session, &row).await?;

    let mut visibility = Visibility::Internal;
    let mut group_id = String::new();
    let mut heading = "新建文档".to_owned();

    if let Some(parent_id) = query.parent {
        let parent = service::visible_detail(&state.pool, Some(current), parent_id).await?;
        visibility = parent.visibility;
        group_id = parent.group_id.map(|id| id.to_string()).unwrap_or_default();
        heading = format!("在《{}》下新建", parent.title);
    }

    Ok(FormTemplate {
        page_title: "新建文档 · NEXUS".to_owned(),
        static_base: framework.static_base,
        csrf_token: framework.csrf_token,
        nav_display_name: framework.nav_display_name,
        nav_is_admin: framework.nav_is_admin,
        nav_is_guest: framework.nav_is_guest,
        look: framework.look,
        nav_active: framework.nav_active,
        heading,
        form_action: "/kb/new".to_owned(),
        parent_id: query.parent,
        title: String::new(),
        body: String::new(),
        visibility: visibility.as_str().to_owned(),
        group_id: group_id.clone(),
        sort_order: "0".to_owned(),
        groups: group_options(&state.pool, current, group_id.parse::<u64>().ok()).await?,
        error: None,
        cancel_href: "/kb".to_owned(),
    })
}

pub async fn new_submit(
    State(state): State<AppState>,
    session: Session,
    user: MaybeUser,
    row: MaybeUserRow,
    client_ip: ClientIp,
    Form(form): Form<DocForm>,
) -> Result<Response, AppError> {
    let current = require_writer(&user, "/kb/new")?;
    middleware::require_csrf(&session, Some(&form.csrf)).await?;

    let (visibility, group_id) = parse_visibility(&form.visibility, &form.group_id)?;
    // 界面上已经没有"排序值"了：列表顺序由置顶 + 浏览/点赞/时间算出来。
    // sort_order 列还留着，只为历史数据与回滚，新写入一律 0。
    let sort_order = 0;

    let input = DocInput {
        parent_id: form.parent_id,
        title: &form.title,
        body: &form.body,
        body_html: Some(form.body_html.as_str()),
        visibility,
        group_id,
        sort_order,
    };

    match service::create_doc(&state.pool, current, client_ip.0.as_deref(), input).await {
        Ok(id) => Ok(Redirect::to(&format!("/kb/{id}")).into_response()),
        Err(AppError::Unprocessable(message)) => {
            let framework = base(&state, &user).with_csrf(&session, &row).await?;
            let groups =
                group_options(&state.pool, current, form.group_id.parse::<u64>().ok()).await?;
            let template = FormTemplate {
                page_title: "新建文档 · NEXUS".to_owned(),
                static_base: framework.static_base,
                csrf_token: framework.csrf_token,
                nav_display_name: framework.nav_display_name,
                nav_is_admin: framework.nav_is_admin,
                nav_is_guest: framework.nav_is_guest,
                look: framework.look,
                nav_active: framework.nav_active,
                heading: "新建文档".to_owned(),
                form_action: "/kb/new".to_owned(),
                parent_id: form.parent_id,
                title: form.title,
                body: form.body,
                visibility: visibility.as_str().to_owned(),
                group_id: form.group_id,
                sort_order: form.sort_order,
                groups,
                error: Some(message.to_string()),
                cancel_href: "/kb".to_owned(),
            };
            Ok((StatusCode::UNPROCESSABLE_ENTITY, template).into_response())
        }
        Err(err) => Err(err),
    }
}

pub async fn doc_page(
    State(state): State<AppState>,
    session: Session,
    user: MaybeUser,
    row: MaybeUserRow,
    Path(id): Path<u64>,
) -> Result<DocTemplate, AppError> {
    let framework = base(&state, &user).with_csrf(&session, &row).await?;
    let tz = state.config.display_tz;
    let viewer = user.user();

    let doc = service::visible_detail(&state.pool, viewer, id).await?;
    // 先记浏览再渲染：作者自己打开不计（见 service::count_view）。
    service::count_view(&state.pool, viewer, id).await?;
    let crumbs = crate::kb::model::breadcrumb(&state.pool, id).await?;
    let children = crate::kb::model::list_children(&state.pool, id, viewer).await?;

    let can_manage = viewer
        .map(|current| current.id == doc.author_id || policy::can_admin(Some(current)))
        .unwrap_or(false);

    Ok(DocTemplate {
        page_title: format!("{} · 档案 · NEXUS", doc.title),
        static_base: framework.static_base,
        csrf_token: framework.csrf_token,
        nav_display_name: framework.nav_display_name,
        nav_is_admin: framework.nav_is_admin,
        nav_is_guest: framework.nav_is_guest,
        look: framework.look,
        nav_active: framework.nav_active,
        doc: {
            let mut view = doc_view(&doc, tz);
            view.can_pin = viewer
                .map(|current| policy::can_admin(Some(current)))
                .unwrap_or(false);
            view
        },
        crumbs: crumbs
            .iter()
            .map(|crumb| CrumbView {
                id: crumb.id,
                title: crumb.title.clone(),
            })
            .collect(),
        children: children.iter().map(|card| card_of(card, tz)).collect(),
        can_manage,
    })
}

pub async fn edit_page(
    State(state): State<AppState>,
    session: Session,
    user: MaybeUser,
    row: MaybeUserRow,
    Path(id): Path<u64>,
) -> Result<FormTemplate, AppError> {
    let framework = base(&state, &user).with_csrf(&session, &row).await?;
    let doc = service::visible_detail(&state.pool, user.user(), id).await?;
    let current = require_manager(&user, &doc, &format!("/kb/{id}/edit"))?;

    Ok(FormTemplate {
        page_title: format!("编辑 {} · 档案 · NEXUS", doc.title),
        static_base: framework.static_base,
        csrf_token: framework.csrf_token,
        nav_display_name: framework.nav_display_name,
        nav_is_admin: framework.nav_is_admin,
        nav_is_guest: framework.nav_is_guest,
        look: framework.look,
        nav_active: framework.nav_active,
        heading: format!("编辑《{}》", doc.title),
        form_action: format!("/kb/{id}/edit"),
        parent_id: doc.parent_id,
        title: doc.title.clone(),
        body: doc.body.clone(),
        visibility: doc.visibility.as_str().to_owned(),
        group_id: doc.group_id.map(|gid| gid.to_string()).unwrap_or_default(),
        sort_order: doc.sort_order.to_string(),
        groups: group_options(&state.pool, current, doc.group_id).await?,
        error: None,
        cancel_href: format!("/kb/{id}"),
    })
}

pub async fn edit_submit(
    State(state): State<AppState>,
    session: Session,
    user: MaybeUser,
    row: MaybeUserRow,
    client_ip: ClientIp,
    Path(id): Path<u64>,
    Form(form): Form<DocForm>,
) -> Result<Response, AppError> {
    middleware::require_csrf(&session, Some(&form.csrf)).await?;

    let doc = service::visible_detail(&state.pool, user.user(), id).await?;
    let current = require_manager(&user, &doc, &format!("/kb/{id}/edit"))?;

    let (visibility, group_id) = parse_visibility(&form.visibility, &form.group_id)?;
    // 界面上已经没有"排序值"了：列表顺序由置顶 + 浏览/点赞/时间算出来。
    // sort_order 列还留着，只为历史数据与回滚，新写入一律 0。
    let sort_order = 0;

    let input = DocInput {
        parent_id: doc.parent_id,
        title: &form.title,
        body: &form.body,
        body_html: Some(form.body_html.as_str()),
        visibility,
        group_id,
        sort_order,
    };

    match service::update_doc(&state.pool, current, client_ip.0.as_deref(), id, input).await {
        Ok(()) => Ok(Redirect::to(&format!("/kb/{id}")).into_response()),
        Err(AppError::Unprocessable(message)) => {
            let framework = base(&state, &user).with_csrf(&session, &row).await?;
            let groups =
                group_options(&state.pool, current, form.group_id.parse::<u64>().ok()).await?;
            let template = FormTemplate {
                page_title: format!("编辑 {} · 档案 · NEXUS", doc.title),
                static_base: framework.static_base,
                csrf_token: framework.csrf_token,
                nav_display_name: framework.nav_display_name,
                nav_is_admin: framework.nav_is_admin,
                nav_is_guest: framework.nav_is_guest,
                look: framework.look,
                nav_active: framework.nav_active,
                heading: format!("编辑《{}》", doc.title),
                form_action: format!("/kb/{id}/edit"),
                parent_id: doc.parent_id,
                title: form.title,
                body: form.body,
                visibility: visibility.as_str().to_owned(),
                group_id: form.group_id,
                sort_order: form.sort_order,
                groups,
                error: Some(message.to_string()),
                cancel_href: format!("/kb/{id}"),
            };
            Ok((StatusCode::UNPROCESSABLE_ENTITY, template).into_response())
        }
        Err(err) => Err(err),
    }
}

pub async fn publish_submit(
    State(state): State<AppState>,
    session: Session,
    user: MaybeUser,
    client_ip: ClientIp,
    Path(id): Path<u64>,
    Form(form): Form<PublishForm>,
) -> Result<Response, AppError> {
    middleware::require_csrf(&session, Some(&form.csrf)).await?;

    let doc = service::visible_detail(&state.pool, user.user(), id).await?;
    let current = require_manager(&user, &doc, &format!("/kb/{id}"))?;
    let published = form.value.trim() == "1";

    service::publish(&state.pool, current, client_ip.0.as_deref(), id, published).await?;
    Ok(Redirect::to(&format!("/kb/{id}")).into_response())
}

pub async fn delete_submit(
    State(state): State<AppState>,
    session: Session,
    user: MaybeUser,
    client_ip: ClientIp,
    Path(id): Path<u64>,
    Form(form): Form<CsrfForm>,
) -> Result<Response, AppError> {
    middleware::require_csrf(&session, Some(&form.csrf)).await?;

    let doc = service::visible_detail(&state.pool, user.user(), id).await?;
    let current = require_manager(&user, &doc, &format!("/kb/{id}"))?;

    service::delete_doc(&state.pool, current, client_ip.0.as_deref(), id).await?;
    Ok(Redirect::to("/kb").into_response())
}

pub async fn from_thread_submit(
    State(state): State<AppState>,
    session: Session,
    user: MaybeUser,
    client_ip: ClientIp,
    Path(thread_id): Path<u64>,
    Form(form): Form<CsrfForm>,
) -> Result<Response, AppError> {
    middleware::require_csrf(&session, Some(&form.csrf)).await?;

    let current = require_writer(&user, &format!("/forum/t/{thread_id}"))?;
    let id = service::create_from_thread(&state.pool, current, client_ip.0.as_deref(), thread_id)
        .await?;

    // 沉淀出来的是草稿，直接进编辑页：刚搬过来的内容通常还要整理。
    Ok(Redirect::to(&format!("/kb/{id}/edit")).into_response())
}

/// 查询串里的值必须编码：`q=a&b` 不编码会把 `b` 变成另一个参数。
fn encode_component(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for byte in input.as_bytes() {
        let ch = *byte as char;
        if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.' | '~') {
            out.push(ch);
        } else {
            out.push('%');
            out.push_str(&format!("{byte:02X}"));
        }
    }
    out
}

#[derive(Debug, Deserialize)]
pub struct PinForm {
    pub value: String,
    #[serde(default)]
    pub order: String,
    #[serde(rename = "_csrf")]
    pub csrf: String,
}

/// 置顶 / 取消置顶。只有 admin 能进——service 里还会再判一次。
pub async fn pin_submit(
    State(state): State<AppState>,
    session: Session,
    user: MaybeUser,
    client_ip: ClientIp,
    Path(id): Path<u64>,
    Form(form): Form<PinForm>,
) -> Result<Response, AppError> {
    let current = require_writer(&user, &format!("/kb/{id}"))?;
    middleware::require_csrf(&session, Some(&form.csrf)).await?;

    let pinned = match form.value.trim() {
        "on" => true,
        "off" => false,
        other => {
            return Err(AppError::bad_request(format!(
                "未知的置顶操作：{other}（只接受 on / off）"
            )));
        }
    };
    let order = if pinned {
        form.order
            .trim()
            .parse::<i32>()
            .map_err(|_| AppError::unprocessable("置顶顺序要填一个整数，越小越靠前"))?
    } else {
        0
    };

    service::set_pinned(
        &state.pool,
        current,
        client_ip.0.as_deref(),
        id,
        pinned,
        order,
    )
    .await?;
    Ok(redirect_to(&format!("/kb/{id}")))
}
