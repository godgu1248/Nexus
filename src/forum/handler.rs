//! 论坛的 HTTP 层：取表单、调 service、渲染模板。
//!
//! 这一层**不写库、不判业务权限**。它只做三件事：
//!
//! 1. 把请求里的东西变成 Rust 值（表单、路径参数、查询串）；
//! 2. 调 `service`，把 `Result` 原样往上抛（`AppError` 自己会变成响应）；
//! 3. 把结果塞进模板结构体。
//!
//! 每个整页模板都必须带五个字段：`page_title` / `static_base` / `csrf_token` /
//! `nav_display_name` / `nav_is_admin`。少一个，导航就会渲染成游客态——
//! 这是 M1 定下的契约（DESIGN §10.1）。

use askama::Template;
use askama_web::WebTemplate;
use axum::Form;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use tower_sessions::Session;

use crate::auth::middleware::{self, ClientIp, MaybeUser, MaybeUserRow};
use crate::auth::policy::{self, ContentCtx, CurrentUser};
use crate::error::AppError;
use crate::forum::model::{Post, Space, ThreadRow};
use crate::forum::service::{self, NewSpaceInput, NewThreadInput};
use crate::shared::pagination::{Page, PageQuery};
use crate::shared::visibility::Visibility;
use crate::state::AppState;

// ---------------------------------------------------------------------------
// 表单
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct NewThreadForm {
    pub title: String,
    /// 纯文本回退（脚本没跑起来时用户直接在这里打字）。
    #[serde(default)]
    pub body: String,
    /// 富文本 HTML。编辑器有 JS 时才会出现在表单里。
    #[serde(default)]
    pub body_html: String,
    #[serde(rename = "_csrf")]
    pub csrf: String,
}

#[derive(Debug, Deserialize)]
pub struct ReplyForm {
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub body_html: String,
    #[serde(rename = "_csrf")]
    pub csrf: String,
}

#[derive(Debug, Deserialize)]
pub struct ActionForm {
    /// 语义由路由决定（resolve 传 "on"/"off"），缺省按 "on" 处理。
    #[serde(default)]
    pub value: String,
    #[serde(rename = "_csrf")]
    pub csrf: String,
}

#[derive(Debug, Deserialize)]
pub struct NewSpaceForm {
    pub name: String,
    pub slug: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub visibility: String,
    #[serde(rename = "_csrf")]
    pub csrf: String,
}

/// 改分区可见性。
///
/// `group_id` 收字符串而不是 `Option<u64>`：非小组时表单里根本没有这个 input，
/// 用 `Option<u64>` 配 `Form` 会把"字段缺失"变成反序列化失败而不是 None。
#[derive(Debug, Deserialize)]
pub struct VisibilityForm {
    pub visibility: String,
    #[serde(default)]
    pub group_id: String,
    #[serde(rename = "_csrf")]
    pub csrf: String,
}

// ---------------------------------------------------------------------------
// 视图模型
// ---------------------------------------------------------------------------

/// 空间卡片。只放模板要显示的东西——不直接塞 `Space`，
/// 因为模板里一旦能碰到 `created_by` 这类字段，早晚有人把它渲染出来。
#[derive(Debug, Clone)]
pub struct SpaceCard {
    pub name: String,
    pub slug: String,
    pub description: String,
    pub is_public: bool,
    /// 给人看的可见性标签：`公开` / `内部` / `小组`。
    pub visibility: String,
    /// 机器值 `public` / `internal` / `group`，表单回填 `selected` 时用。
    /// 拿标签去比较等于把展示文案当协议，改个措辞就悄悄改错了行为。
    pub visibility_key: String,
    pub group_id: Option<u64>,
    /// 小组名。只有管理员（能列出全部小组）才会拿到值；
    /// 普通成员拿 None 是有意的——组名本身只在组内可见。
    pub group_name: Option<String>,
    /// 热度两个数字：分区列表按「议题 + 回复」倒序，界面上把这两个数显示出来，
    /// 否则用户看到的顺序等于黑箱（DESIGN §31）。
    pub thread_count: u64,
    pub post_count: u64,
}

impl From<&Space> for SpaceCard {
    fn from(space: &Space) -> Self {
        Self {
            name: space.name.clone(),
            slug: space.slug.clone(),
            description: space.description.clone(),
            is_public: space.visibility == Visibility::Public,
            visibility: match space.visibility {
                Visibility::Public => "公开".to_owned(),
                Visibility::Internal => "内部".to_owned(),
                Visibility::Group => "小组".to_owned(),
            },
            visibility_key: space.visibility.as_str().to_owned(),
            group_id: space.group_id,
            group_name: None,
            thread_count: space.thread_count,
            post_count: space.post_count,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ThreadCard {
    pub id: u64,
    pub title: String,
    pub author_name: String,
    pub reply_count: u64,
    pub is_pinned: bool,
    pub is_resolved: bool,
    pub is_archived: bool,
    pub last_post_at: String,
}

#[derive(Debug, Clone)]
pub struct PostCard {
    pub id: u64,
    pub author_name: String,
    pub body: String,
    /// 读的时候再净化一次：库里存的已经是净化结果，但历史数据与手改库都绕不过这一层。
    pub body_html: Option<crate::shared::SafeHtml>,
    pub created_at: String,
    pub can_delete: bool,
}

/// 时间统一走这里格式化：库里的 `TIMESTAMP` 是 UTC，展示按 `DISPLAY_TZ_OFFSET`。
/// 直接 `format()` 一个 UTC 值，晚上八点后日期会显示成"明天"——这类 bug 最难发现。
fn stamp(dt: chrono::DateTime<chrono::Utc>, tz: chrono::FixedOffset) -> String {
    dt.with_timezone(&tz).format("%Y-%m-%d %H:%M").to_string()
}

// ---------------------------------------------------------------------------
// 模板
// ---------------------------------------------------------------------------

#[derive(Template, WebTemplate)]
#[template(path = "forum/spaces.html")]
pub struct SpacesTemplate {
    pub page_title: String,
    pub static_base: String,
    pub csrf_token: String,
    pub nav_display_name: String,
    pub nav_is_admin: bool,
    pub nav_is_guest: bool,
    pub look: crate::shared::appearance::Appearance,
    pub nav_active: &'static str,
    pub spaces: Vec<SpaceCard>,
    pub can_create_space: bool,
    pub is_logged_in: bool,
    /// 管理员改可见性时可选的小组。非管理员为空——模板不显示表单，也就不需要数据。
    pub groups: Vec<GroupOption>,
}

#[derive(Debug, Clone)]
pub struct GroupOption {
    pub id: u64,
    pub name: String,
}

#[derive(Template, WebTemplate)]
#[template(path = "forum/threads.html")]
pub struct ThreadsTemplate {
    pub page_title: String,
    pub static_base: String,
    pub csrf_token: String,
    pub nav_display_name: String,
    pub nav_is_admin: bool,
    pub nav_is_guest: bool,
    pub look: crate::shared::appearance::Appearance,
    pub nav_active: &'static str,
    pub space_name: String,
    pub space_slug: String,
    pub space_description: String,
    pub threads: Vec<ThreadCard>,
    pub page: Page<ThreadRow>,
    pub can_post: bool,
    pub is_logged_in: bool,
}

#[derive(Template, WebTemplate)]
#[template(path = "forum/thread.html")]
pub struct ThreadTemplate {
    pub page_title: String,
    pub static_base: String,
    pub csrf_token: String,
    pub nav_display_name: String,
    pub nav_is_admin: bool,
    pub nav_is_guest: bool,
    pub look: crate::shared::appearance::Appearance,
    pub nav_active: &'static str,
    pub thread_id: u64,
    pub title: String,
    pub author_name: String,
    pub space_name: String,
    pub space_slug: String,
    pub is_resolved: bool,
    pub is_archived: bool,
    pub created_at: String,
    pub posts: Vec<PostCard>,
    pub page: Page<Post>,
    pub can_reply: bool,
    pub can_resolve: bool,
    pub can_archive: bool,
    pub is_logged_in: bool,
    pub error: Option<String>,
}

#[derive(Template, WebTemplate)]
#[template(path = "forum/new_thread.html")]
pub struct NewThreadTemplate {
    pub page_title: String,
    pub static_base: String,
    pub csrf_token: String,
    pub nav_display_name: String,
    pub nav_is_admin: bool,
    pub nav_is_guest: bool,
    pub look: crate::shared::appearance::Appearance,
    pub nav_active: &'static str,
    pub space_name: String,
    pub space_slug: String,
    pub title_value: String,
    pub body_value: String,
    pub error: Option<String>,
}

#[derive(Template, WebTemplate)]
#[template(path = "forum/new_space.html")]
pub struct NewSpaceTemplate {
    pub page_title: String,
    pub static_base: String,
    pub csrf_token: String,
    pub nav_display_name: String,
    pub nav_is_admin: bool,
    pub nav_is_guest: bool,
    pub look: crate::shared::appearance::Appearance,
    pub nav_active: &'static str,
    pub name_value: String,
    pub slug_value: String,
    pub description_value: String,
    pub error: Option<String>,
}

// ---------------------------------------------------------------------------
// 小工具
// ---------------------------------------------------------------------------

/// 管理员专属页面的统一入口检查。
///
/// 顺序很重要：**未登录 → `LoginRequired`**（303 去登录页并带回跳地址），
/// **已登录但不是管理员 → `Forbidden`**（403）。
/// 反过来的话，游客点"新建分区"会被告知"禁止访问"——那看起来像账号出了问题，
/// 而实际上他只是还没登录。
fn require_admin(user: &MaybeUser, next: &str) -> Result<(), AppError> {
    match user.user() {
        None => Err(AppError::LoginRequired {
            next: next.to_owned(),
        }),
        Some(current) if policy::can_admin(Some(current)) => Ok(()),
        Some(_) => Err(AppError::Forbidden),
    }
}

// 303 + HX-Redirect 共用 `shared::http::redirect_to`：三处 handler 必须行为一致。
use crate::shared::http::redirect_to;

fn login_required(next: &str) -> AppError {
    AppError::LoginRequired {
        next: next.to_owned(),
    }
}

fn ctx_of(visibility: Visibility, owner_id: Option<u64>, group_id: Option<u64>) -> ContentCtx {
    ContentCtx::new(visibility, owner_id, group_id)
}

fn parse_bool(raw: &str) -> bool {
    !matches!(raw.trim(), "off" | "false" | "0" | "")
}

// ---------------------------------------------------------------------------
// GET
// ---------------------------------------------------------------------------

/// GET /forum —— 空间列表。
pub async fn spaces_page(
    State(state): State<AppState>,
    session: Session,
    user: MaybeUser,
    row: MaybeUserRow,
) -> Result<Response, AppError> {
    let actor = CurrentUser::of(user.user());
    let spaces = service::list_spaces(&state.pool, &actor).await?;
    let csrf_token = middleware::ensure_csrf(&session).await?;
    let is_admin = policy::can_admin(user.user());

    // 只有管理员会看到改可见性的表单，所以只有他才付这次查询。
    let groups: Vec<GroupOption> = if is_admin {
        crate::group::model::list_groups(&state.pool)
            .await?
            .into_iter()
            .map(|group| GroupOption {
                id: group.id,
                name: group.name,
            })
            .collect()
    } else {
        Vec::new()
    };

    let names: std::collections::HashMap<u64, String> = groups
        .iter()
        .map(|group| (group.id, group.name.clone()))
        .collect();

    let cards: Vec<SpaceCard> = spaces
        .iter()
        .map(|space| {
            let mut card = SpaceCard::from(space);
            card.group_name = space.group_id.and_then(|id| names.get(&id).cloned());
            card
        })
        .collect();

    let template = SpacesTemplate {
        page_title: "论坛".to_owned(),
        static_base: state.config.static_base_url.clone(),
        csrf_token,
        nav_display_name: row.display_name().to_owned(),
        nav_is_admin: user.is_admin(),
        nav_is_guest: crate::auth::policy::is_guest(user.user()),
        look: row.look(),
        nav_active: "forum",
        spaces: cards,
        can_create_space: is_admin,
        is_logged_in: user.user().is_some(),
        groups,
    };

    Ok(template.into_response())
}

/// GET /forum/new —— 新建空间（仅 admin）。
pub async fn new_space_page(
    State(state): State<AppState>,
    session: Session,
    user: MaybeUser,
    row: MaybeUserRow,
) -> Result<Response, AppError> {
    require_admin(&user, "/forum/new")?;
    let csrf_token = middleware::ensure_csrf(&session).await?;

    Ok(NewSpaceTemplate {
        page_title: "新建分区".to_owned(),
        static_base: state.config.static_base_url.clone(),
        csrf_token,
        nav_display_name: row.display_name().to_owned(),
        nav_is_admin: user.is_admin(),
        nav_is_guest: crate::auth::policy::is_guest(user.user()),
        look: row.look(),
        nav_active: "forum",
        name_value: String::new(),
        slug_value: String::new(),
        description_value: String::new(),
        error: None,
    }
    .into_response())
}

/// GET /forum/{slug} —— 某分区的议题列表。
pub async fn threads_page(
    State(state): State<AppState>,
    session: Session,
    user: MaybeUser,
    row: MaybeUserRow,
    Path(slug): Path<String>,
    Query(page): Query<PageQuery>,
) -> Result<Response, AppError> {
    let actor = CurrentUser::of(user.user());
    let (space, threads) = service::threads_of_space(&state.pool, &actor, &slug, &page).await?;
    let csrf_token = middleware::ensure_csrf(&session).await?;

    let cards = threads
        .items
        .iter()
        .map(|thread| ThreadCard {
            id: thread.id,
            title: thread.title.clone(),
            author_name: thread.author_name.clone(),
            reply_count: thread.reply_count,
            is_pinned: thread.is_pinned,
            is_resolved: thread.is_resolved,
            is_archived: thread.is_archived,
            last_post_at: stamp(thread.last_post_at, state.config.display_tz),
        })
        .collect();

    Ok(ThreadsTemplate {
        page_title: format!("{} · 论坛", space.name),
        static_base: state.config.static_base_url.clone(),
        csrf_token,
        nav_display_name: row.display_name().to_owned(),
        nav_is_admin: user.is_admin(),
        nav_is_guest: crate::auth::policy::is_guest(user.user()),
        look: row.look(),
        nav_active: "forum",
        space_name: space.name.clone(),
        space_slug: space.slug.clone(),
        space_description: space.description.clone(),
        threads: cards,
        page: threads,
        can_post: policy::can_write(user.user(), &ctx_of(space.visibility, None, space.group_id)),
        is_logged_in: user.user().is_some(),
    }
    .into_response())
}

/// GET /forum/t/{id} —— 议题详情。
pub async fn thread_page(
    State(state): State<AppState>,
    session: Session,
    user: MaybeUser,
    row: MaybeUserRow,
    Path(thread_id): Path<u64>,
    Query(page): Query<PageQuery>,
) -> Result<Response, AppError> {
    let actor = CurrentUser::of(user.user());
    let (thread, posts) = service::thread_detail(&state.pool, &actor, thread_id, &page).await?;
    let csrf_token = middleware::ensure_csrf(&session).await?;

    let viewer = user.user();
    let can_moderate = policy::can_moderate(
        viewer,
        &ctx_of(thread.visibility, Some(thread.author_id), thread.group_id),
    );
    let is_author = viewer.is_some_and(|me| me.id == thread.author_id);

    let cards = posts
        .items
        .iter()
        .map(|post: &Post| PostCard {
            id: post.id,
            author_name: post.author_name.clone(),
            body: post.body.clone(),
            body_html: post
                .body_html
                .as_ref()
                .map(|html| crate::shared::richtext::sanitize(html)),
            created_at: stamp(post.created_at, state.config.display_tz),
            can_delete: viewer.is_some_and(|me| me.id == post.author_id) || can_moderate,
        })
        .collect();

    Ok(ThreadTemplate {
        page_title: format!("{} · 论坛", thread.title),
        static_base: state.config.static_base_url.clone(),
        csrf_token,
        nav_display_name: row.display_name().to_owned(),
        nav_is_admin: user.is_admin(),
        nav_is_guest: crate::auth::policy::is_guest(user.user()),
        look: row.look(),
        nav_active: "forum",
        thread_id: thread.id,
        title: thread.title.clone(),
        author_name: thread.author_name.clone(),
        space_name: thread.space_name.clone(),
        space_slug: thread.space_slug.clone(),
        is_resolved: thread.is_resolved,
        is_archived: thread.is_archived,
        created_at: stamp(thread.created_at, state.config.display_tz),
        posts: cards,
        page: posts,
        can_reply: !thread.is_archived
            && policy::can_write(
                viewer,
                &ctx_of(thread.visibility, Some(thread.author_id), thread.group_id),
            ),
        can_resolve: is_author || can_moderate,
        can_archive: can_moderate,
        is_logged_in: viewer.is_some(),
        error: None,
    }
    .into_response())
}

/// GET /forum/{slug}/new —— 发议题。
pub async fn new_thread_page(
    State(state): State<AppState>,
    session: Session,
    user: MaybeUser,
    row: MaybeUserRow,
    Path(slug): Path<String>,
) -> Result<Response, AppError> {
    let next = format!("/forum/{slug}/new");
    let viewer = user.user().ok_or_else(|| login_required(&next))?;

    let space = crate::forum::model::find_space_by_slug(&state.pool, &slug)
        .await?
        .ok_or(AppError::NotFound)?;

    if !policy::can_view(
        Some(viewer),
        &ctx_of(space.visibility, None, space.group_id),
    ) {
        return Err(AppError::NotFound);
    }
    if !policy::can_write(
        Some(viewer),
        &ctx_of(space.visibility, None, space.group_id),
    ) {
        return Err(AppError::Forbidden);
    }

    let csrf_token = middleware::ensure_csrf(&session).await?;

    Ok(NewThreadTemplate {
        page_title: format!("在 {} 发议题", space.name),
        static_base: state.config.static_base_url.clone(),
        csrf_token,
        nav_display_name: row.display_name().to_owned(),
        nav_is_admin: user.is_admin(),
        nav_is_guest: crate::auth::policy::is_guest(user.user()),
        look: row.look(),
        nav_active: "forum",
        space_name: space.name,
        space_slug: space.slug,
        title_value: String::new(),
        body_value: String::new(),
        error: None,
    }
    .into_response())
}

// ---------------------------------------------------------------------------
// POST
// ---------------------------------------------------------------------------

/// POST /forum/new —— 建空间。
pub async fn new_space_submit(
    State(state): State<AppState>,
    session: Session,
    user: MaybeUser,
    client_ip: ClientIp,
    row: MaybeUserRow,
    Form(form): Form<NewSpaceForm>,
) -> Result<Response, AppError> {
    // 先判"有没有登录"再判"是不是管理员"：游客该被送去登录页，
    // 而不是被告知"禁止访问"——后者看起来像账号出了问题。
    require_admin(&user, "/forum/new")?;
    middleware::require_csrf(&session, Some(&form.csrf)).await?;

    let csrf_token = middleware::ensure_csrf(&session).await?;
    let actor = CurrentUser::of(user.user());

    // 表单类错误：**带上 422 状态码**把表单回填重渲染。
    // 只回 200 会让 htmx 认为"提交成功"从而把响应塞进容器，调用方也测不出这是失败；
    // 只回 422 又不带表单，用户得自己返回上一页重新填。两个都不是人想要的。
    let redo = |error: String| {
        (
            StatusCode::UNPROCESSABLE_ENTITY,
            NewSpaceTemplate {
                page_title: "新建分区".to_owned(),
                static_base: state.config.static_base_url.clone(),
                csrf_token: csrf_token.clone(),
                nav_display_name: row.display_name().to_owned(),
                nav_is_admin: user.is_admin(),
                nav_is_guest: crate::auth::policy::is_guest(user.user()),
                look: row.look(),
                nav_active: "forum",
                name_value: form.name.clone(),
                slug_value: form.slug.clone(),
                description_value: form.description.clone(),
                error: Some(error),
            },
        )
            .into_response()
    };

    let visibility = form
        .visibility
        .trim()
        .parse::<Visibility>()
        .unwrap_or(Visibility::Internal);

    let ip = client_ip.0;

    let created = service::create_space(
        &state.pool,
        &actor,
        ip.as_deref(),
        NewSpaceInput {
            name: &form.name,
            slug: &form.slug,
            description: &form.description,
            visibility,
        },
    )
    .await;

    match created {
        Ok(_) => Ok(redirect_to("/forum")),
        // 表单类错误回填重渲染；其余（Forbidden/数据库）继续往上抛。
        Err(AppError::Unprocessable(message)) => Ok(redo(message.into_owned())),
        Err(other) => Err(other),
    }
}

/// POST /forum/{slug}/new —— 发议题。
pub async fn new_thread_submit(
    State(state): State<AppState>,
    session: Session,
    user: MaybeUser,
    client_ip: ClientIp,
    Path(slug): Path<String>,
    row: MaybeUserRow,
    Form(form): Form<NewThreadForm>,
) -> Result<Response, AppError> {
    let next = format!("/forum/{slug}/new");
    let viewer = user.user().ok_or_else(|| login_required(&next))?.clone();

    middleware::require_csrf(&session, Some(&form.csrf)).await?;

    let space = crate::forum::model::find_space_by_slug(&state.pool, &slug)
        .await?
        .ok_or(AppError::NotFound)?;

    let csrf_token = middleware::ensure_csrf(&session).await?;
    let redo = |error: String| {
        (
            StatusCode::UNPROCESSABLE_ENTITY,
            NewThreadTemplate {
                page_title: format!("在 {} 发议题", space.name),
                static_base: state.config.static_base_url.clone(),
                csrf_token: csrf_token.clone(),
                nav_display_name: row.display_name().to_owned(),
                nav_is_admin: user.is_admin(),
                nav_is_guest: crate::auth::policy::is_guest(user.user()),
                look: row.look(),
                nav_active: "forum",
                space_name: space.name.clone(),
                space_slug: space.slug.clone(),
                title_value: form.title.clone(),
                body_value: form.body.clone(),
                error: Some(error),
            },
        )
            .into_response()
    };

    let ip = client_ip.0;
    let body = form.body.trim();

    let created = service::create_thread(
        &state.pool,
        &viewer,
        ip.as_deref(),
        NewThreadInput {
            space_id: space.id,
            title: &form.title,
            body: if body.is_empty() { None } else { Some(body) },
            body_html: Some(form.body_html.as_str()),
        },
    )
    .await;

    match created {
        Ok(thread_id) => Ok(redirect_to(&format!("/forum/t/{thread_id}"))),
        Err(AppError::Unprocessable(message)) => Ok(redo(message.into_owned())),
        Err(other) => Err(other),
    }
}

/// POST /forum/t/{id}/reply —— 回帖。
pub async fn reply_submit(
    State(state): State<AppState>,
    session: Session,
    user: MaybeUser,
    client_ip: ClientIp,
    Path(thread_id): Path<u64>,
    Form(form): Form<ReplyForm>,
) -> Result<Response, AppError> {
    let next = format!("/forum/t/{thread_id}");
    let viewer = user.user().ok_or_else(|| login_required(&next))?.clone();

    middleware::require_csrf(&session, Some(&form.csrf)).await?;

    let ip = client_ip.0;

    service::reply(
        &state.pool,
        &viewer,
        ip.as_deref(),
        thread_id,
        &form.body,
        Some(form.body_html.as_str()),
    )
    .await?;

    // 回到最后一页：新回复总是在末尾，跳回第一页会让人以为没发出去。
    let total = crate::forum::model::count_posts(&state.pool, thread_id).await?;
    let last_page = div_ceil(total, u64::from(crate::shared::pagination::PAGE_SIZE)).max(1);
    Ok(redirect_to(&format!(
        "/forum/t/{thread_id}?page={last_page}"
    )))
}

/// POST /forum/t/{id}/resolve —— 定论 / 取消定论。
pub async fn resolve_submit(
    State(state): State<AppState>,
    session: Session,
    user: MaybeUser,
    client_ip: ClientIp,
    Path(thread_id): Path<u64>,
    Form(form): Form<ActionForm>,
) -> Result<Response, AppError> {
    let viewer = user
        .user()
        .ok_or_else(|| login_required(&format!("/forum/t/{thread_id}")))?
        .clone();

    middleware::require_csrf(&session, Some(&form.csrf)).await?;
    let ip = client_ip.0;

    service::set_resolved(
        &state.pool,
        &viewer,
        ip.as_deref(),
        thread_id,
        parse_bool(&form.value),
    )
    .await?;

    Ok(redirect_to(&format!("/forum/t/{thread_id}")))
}

/// POST /forum/t/{id}/archive —— 归档 / 取消归档。
pub async fn archive_submit(
    State(state): State<AppState>,
    session: Session,
    user: MaybeUser,
    client_ip: ClientIp,
    Path(thread_id): Path<u64>,
    Form(form): Form<ActionForm>,
) -> Result<Response, AppError> {
    let viewer = user
        .user()
        .ok_or_else(|| login_required(&format!("/forum/t/{thread_id}")))?
        .clone();

    middleware::require_csrf(&session, Some(&form.csrf)).await?;
    let ip = client_ip.0;

    service::set_archived(
        &state.pool,
        &viewer,
        ip.as_deref(),
        thread_id,
        parse_bool(&form.value),
    )
    .await?;

    Ok(redirect_to(&format!("/forum/t/{thread_id}")))
}

/// POST /forum/p/{id}/delete —— 软删一条回复。
pub async fn delete_post_submit(
    State(state): State<AppState>,
    session: Session,
    user: MaybeUser,
    client_ip: ClientIp,
    Path(post_id): Path<u64>,
    Form(form): Form<ActionForm>,
) -> Result<Response, AppError> {
    let viewer = user.user().ok_or_else(|| login_required("/forum"))?.clone();

    middleware::require_csrf(&session, Some(&form.csrf)).await?;
    let ip = client_ip.0;

    let thread_id = service::delete_post(&state.pool, &viewer, ip.as_deref(), post_id).await?;
    Ok(redirect_to(&format!("/forum/t/{thread_id}")))
}

/// POST /forum/{slug}/visibility —— 改分区可见性与组归属（仅 admin）。
///
/// 成功后回分区首页而不是原地刷新：可见性一改，当前用户可能立刻看不见这个分区
/// （改成小组而他不在组里），留在原地会拿到一个 404，像操作失败。
pub async fn visibility_submit(
    State(state): State<AppState>,
    session: Session,
    user: MaybeUser,
    client_ip: ClientIp,
    Path(slug): Path<String>,
    Form(form): Form<VisibilityForm>,
) -> Result<Response, AppError> {
    require_admin(&user, &format!("/forum/{slug}"))?;
    let viewer = user.user().ok_or_else(|| login_required("/forum"))?.clone();

    middleware::require_csrf(&session, Some(&form.csrf)).await?;

    let visibility = form
        .visibility
        .trim()
        .parse::<Visibility>()
        .map_err(|_| AppError::bad_request("可见性只能是 public / internal / group"))?;

    let group_id = match form.group_id.trim() {
        "" => None,
        raw => Some(
            raw.parse::<u64>()
                .map_err(|_| AppError::bad_request("小组编号必须是数字"))?,
        ),
    };

    service::set_space_visibility(
        &state.pool,
        &viewer,
        client_ip.0.as_deref(),
        &slug,
        visibility,
        group_id,
    )
    .await?;

    Ok(redirect_to("/forum"))
}

fn div_ceil(total: u64, per_page: u64) -> u64 {
    if per_page == 0 {
        return 1;
    }
    total.div_ceil(per_page)
}
