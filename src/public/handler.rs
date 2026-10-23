//! 前厅的 HTTP 层：解析请求、调 service、渲染模板。
//!
//! 这一层不写库、不判业务权限（可见性交给 `service::resolve_target`）。
//! 每个整页模板都带五字段契约（`page_title` / `static_base` / `csrf_token` /
//! `nav_display_name` / `nav_is_admin`），与 layout.html 的约定一致。

use askama::Template;
use askama_web::WebTemplate;
use axum::Form;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use chrono::{DateTime, FixedOffset, Utc};
use serde::Deserialize;
use tower_sessions::Session;

use crate::auth::middleware::{self, ClientIp, MaybeUser, MaybeUserRow};
use crate::error::AppError;
use crate::forum::model as forum_model;
use crate::kb::model as kb_model;
use crate::public::model::{self as public_model, CommentTarget, Submission};
use crate::public::service;
use crate::shared::http::redirect_to;
use crate::shared::pagination::{Page, PageQuery};
use crate::state::AppState;

// ── 表单 ────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct CommentForm {
    pub body: String,
    #[serde(rename = "_csrf")]
    pub csrf: String,
}

#[derive(Debug, Deserialize)]
pub struct SubmitForm {
    pub title: String,
    pub body: String,
    #[serde(rename = "_csrf")]
    pub csrf: String,
}

#[derive(Debug, Deserialize)]
pub struct CsrfForm {
    #[serde(rename = "_csrf")]
    pub csrf: String,
}

// ── 视图模型 ────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct ThreadCardView {
    pub id: u64,
    pub title: String,
    pub space_name: String,
    pub reply_count: u64,
    pub last_post_at: String,
}

#[derive(Debug, Clone)]
pub struct DocCardView {
    pub id: u64,
    pub title: String,
    pub updated_at: String,
}

#[derive(Debug, Clone)]
pub struct CommentView {
    pub id: u64,
    pub author_name: String,
    pub body: String,
    pub created_at: String,
    pub can_delete: bool,
}

#[derive(Debug, Clone)]
pub struct PostView {
    pub author_name: String,
    pub body: String,
    pub created_at: String,
    pub is_first: bool,
}

#[derive(Debug, Clone)]
pub struct SubmissionView {
    pub id: u64,
    pub title: String,
    pub status: String,
    pub status_key: String,
    pub created_at: String,
}

/// 五个页面共用的框架字段，避免每个结构体抄五遍。
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
        nav_active: "lobby",
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

#[derive(Template, WebTemplate)]
#[template(path = "public/lobby.html")]
pub struct LobbyTemplate {
    pub page_title: String,
    pub static_base: String,
    pub csrf_token: String,
    pub nav_display_name: String,
    pub nav_is_admin: bool,
    pub nav_is_guest: bool,
    pub look: crate::shared::appearance::Appearance,
    pub nav_active: &'static str,
    pub threads: Page<ThreadCardView>,
    pub docs: Vec<DocCardView>,
}

#[derive(Template, WebTemplate)]
#[template(path = "public/thread.html")]
pub struct ThreadTemplate {
    pub page_title: String,
    pub static_base: String,
    pub csrf_token: String,
    pub nav_display_name: String,
    pub nav_is_admin: bool,
    pub nav_is_guest: bool,
    pub look: crate::shared::appearance::Appearance,
    pub nav_active: &'static str,
    pub id: u64,
    pub title: String,
    pub space_name: String,
    pub first_body: String,
    pub posts: Vec<PostView>,
    pub comments: Page<CommentView>,
    pub liked: bool,
    pub like_count: u64,
    pub logged_in: bool,
}

#[derive(Template, WebTemplate)]
#[template(path = "public/doc.html")]
pub struct DocTemplate {
    pub page_title: String,
    pub static_base: String,
    pub csrf_token: String,
    pub nav_display_name: String,
    pub nav_is_admin: bool,
    pub nav_is_guest: bool,
    pub look: crate::shared::appearance::Appearance,
    pub nav_active: &'static str,
    pub id: u64,
    pub title: String,
    pub body: String,
    pub author_name: String,
    pub updated_at: String,
    pub comments: Page<CommentView>,
    pub liked: bool,
    pub like_count: u64,
    pub logged_in: bool,
}

#[derive(Template, WebTemplate)]
#[template(path = "public/submit.html")]
pub struct SubmitTemplate {
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
    pub error: Option<String>,
}

#[derive(Template, WebTemplate)]
#[template(path = "public/mine.html")]
pub struct MineTemplate {
    pub page_title: String,
    pub static_base: String,
    pub csrf_token: String,
    pub nav_display_name: String,
    pub nav_is_admin: bool,
    pub nav_is_guest: bool,
    pub look: crate::shared::appearance::Appearance,
    pub nav_active: &'static str,
    pub submissions: Page<SubmissionView>,
}

#[derive(Template, WebTemplate)]
#[template(path = "public/queue.html")]
pub struct QueueTemplate {
    pub page_title: String,
    pub static_base: String,
    pub csrf_token: String,
    pub nav_display_name: String,
    pub nav_is_admin: bool,
    pub nav_is_guest: bool,
    pub look: crate::shared::appearance::Appearance,
    pub nav_active: &'static str,
    pub submissions: Page<SubmissionView>,
}

// ── 小工具 ──────────────────────────────────────────────────────────────

fn stamp(dt: DateTime<Utc>, tz: FixedOffset) -> String {
    dt.with_timezone(&tz).format("%Y-%m-%d %H:%M").to_string()
}

fn submission_view(item: &Submission, tz: FixedOffset) -> SubmissionView {
    SubmissionView {
        id: item.id,
        title: item.title.clone(),
        status: item.status.label().to_owned(),
        status_key: item.status.as_str().to_owned(),
        created_at: stamp(item.created_at, tz),
    }
}

fn is_logged_in(user: &MaybeUser) -> bool {
    user.user().map(|u| u.id != 0).unwrap_or(false)
}

/// 未登录的写操作一律 303 去登录页（不是 403）。
///
/// 前厅是对外页面：外人第一次点"评论"就撞上 403，会以为站点坏了；
/// 303 到登录页顺带把 `next` 带上，注册完能回到原处。
fn require_login(user: &MaybeUser, next: &str) -> Result<(), AppError> {
    if is_logged_in(user) {
        Ok(())
    } else {
        Err(AppError::LoginRequired {
            next: next.to_owned(),
        })
    }
}

// ── 页面 ────────────────────────────────────────────────────────────────

pub async fn lobby_page(
    State(state): State<AppState>,
    session: Session,
    user: MaybeUser,
    row: MaybeUserRow,
    Query(page): Query<PageQuery>,
) -> Result<LobbyTemplate, AppError> {
    let threads = forum_model::list_public_threads(&state.pool, &page).await?;
    let tz = state.config.display_tz;

    let thread_cards: Vec<ThreadCardView> = threads
        .items
        .iter()
        .map(|t| ThreadCardView {
            id: t.id,
            title: t.title.clone(),
            space_name: t.space_name.clone(),
            reply_count: t.reply_count,
            last_post_at: stamp(t.last_post_at, tz),
        })
        .collect();
    let thread_page = Page::new(thread_cards, threads.page, threads.total, threads.per_page);

    // 前厅只放最近 10 篇公开档案：真正的目录在 `/kb`，这里的作用是"让人看到有内容"。
    let docs = kb_model::list_recent(&state.pool, None, 10).await?;
    let doc_cards = docs
        .iter()
        .map(|d| DocCardView {
            id: d.id,
            title: d.title.clone(),
            updated_at: stamp(d.updated_at, tz),
        })
        .collect();

    let frame = base(&state, &user).with_csrf(&session, &row).await?;
    Ok(LobbyTemplate {
        page_title: "前厅 · NEXUS".to_owned(),
        static_base: frame.static_base,
        csrf_token: frame.csrf_token,
        nav_display_name: frame.nav_display_name,
        nav_is_admin: frame.nav_is_admin,
        nav_is_guest: frame.nav_is_guest,
        look: frame.look,
        nav_active: frame.nav_active,
        threads: thread_page,
        docs: doc_cards,
    })
}

pub async fn thread_page(
    State(state): State<AppState>,
    session: Session,
    user: MaybeUser,
    row: MaybeUserRow,
    Path(id): Path<u64>,
    Query(page): Query<PageQuery>,
) -> Result<ThreadTemplate, AppError> {
    let target =
        service::resolve_target(&state.pool, user.user(), CommentTarget::Thread, id).await?;
    let thread = forum_model::find_thread(&state.pool, id)
        .await?
        .ok_or(AppError::NotFound)?;
    let posts = forum_model::list_posts(&state.pool, id, &PageQuery::new(1)).await?;
    let tz = state.config.display_tz;

    let first_body = posts
        .items
        .first()
        .map(|p| p.body.clone())
        .unwrap_or_default();
    let post_views: Vec<PostView> = if posts.items.is_empty() || first_body.is_empty() {
        Vec::new()
    } else {
        posts
            .items
            .iter()
            .enumerate()
            .skip(1)
            .map(|(_, p)| PostView {
                author_name: p.author_name.clone(),
                body: p.body.clone(),
                created_at: stamp(p.created_at, tz),
                is_first: false,
            })
            .collect()
    };

    let comments = service::comments_of(&state.pool, CommentTarget::Thread, id, &page).await?;
    let comment_views: Vec<CommentView> = comments
        .items
        .iter()
        .map(|c| CommentView {
            id: c.id,
            author_name: c.author_name.clone(),
            body: c.body.clone(),
            created_at: stamp(c.created_at, tz),
            can_delete: service::may_delete(user.user(), c),
        })
        .collect();
    let comment_page = Page::new(
        comment_views,
        comments.page,
        comments.total,
        comments.per_page,
    );

    let logged_in = is_logged_in(&user);
    let (liked, like_count) = match user.user() {
        Some(current) if logged_in => (
            public_model::is_liked(&state.pool, CommentTarget::Thread, id, current.id).await?,
            public_model::like_count(&state.pool, CommentTarget::Thread, id).await?,
        ),
        _ => (
            false,
            public_model::like_count(&state.pool, CommentTarget::Thread, id).await?,
        ),
    };

    let frame = base(&state, &user).with_csrf(&session, &row).await?;
    Ok(ThreadTemplate {
        page_title: format!("{} · 前厅 · NEXUS", target.title),
        static_base: frame.static_base,
        csrf_token: frame.csrf_token,
        nav_display_name: frame.nav_display_name,
        nav_is_admin: frame.nav_is_admin,
        nav_is_guest: frame.nav_is_guest,
        look: frame.look,
        nav_active: frame.nav_active,
        id,
        title: thread.title,
        space_name: thread.space_name,
        first_body,
        posts: post_views,
        comments: comment_page,
        liked,
        like_count,
        logged_in,
    })
}

pub async fn doc_page(
    State(state): State<AppState>,
    session: Session,
    user: MaybeUser,
    row: MaybeUserRow,
    Path(id): Path<u64>,
    Query(page): Query<PageQuery>,
) -> Result<DocTemplate, AppError> {
    let target = service::resolve_target(&state.pool, user.user(), CommentTarget::Doc, id).await?;
    let doc = kb_model::find_doc(&state.pool, id)
        .await?
        .ok_or(AppError::NotFound)?;
    let tz = state.config.display_tz;

    let comments = service::comments_of(&state.pool, CommentTarget::Doc, id, &page).await?;
    let comment_views: Vec<CommentView> = comments
        .items
        .iter()
        .map(|c| CommentView {
            id: c.id,
            author_name: c.author_name.clone(),
            body: c.body.clone(),
            created_at: stamp(c.created_at, tz),
            can_delete: service::may_delete(user.user(), c),
        })
        .collect();
    let comment_page = Page::new(
        comment_views,
        comments.page,
        comments.total,
        comments.per_page,
    );

    let logged_in = is_logged_in(&user);
    let (liked, like_count) = match user.user() {
        Some(current) if logged_in => (
            public_model::is_liked(&state.pool, CommentTarget::Doc, id, current.id).await?,
            public_model::like_count(&state.pool, CommentTarget::Doc, id).await?,
        ),
        _ => (
            false,
            public_model::like_count(&state.pool, CommentTarget::Doc, id).await?,
        ),
    };

    let frame = base(&state, &user).with_csrf(&session, &row).await?;
    Ok(DocTemplate {
        page_title: format!("{} · 前厅 · NEXUS", target.title),
        static_base: frame.static_base,
        csrf_token: frame.csrf_token,
        nav_display_name: frame.nav_display_name,
        nav_is_admin: frame.nav_is_admin,
        nav_is_guest: frame.nav_is_guest,
        look: frame.look,
        nav_active: frame.nav_active,
        id,
        title: doc.title,
        body: doc.body,
        author_name: doc.author_name,
        updated_at: stamp(doc.updated_at, tz),
        comments: comment_page,
        liked,
        like_count,
        logged_in,
    })
}

pub async fn submit_page(
    State(state): State<AppState>,
    session: Session,
    user: MaybeUser,
    row: MaybeUserRow,
) -> Result<SubmitTemplate, AppError> {
    require_login(&user, "/lobby/submit")?;
    let frame = base(&state, &user).with_csrf(&session, &row).await?;
    Ok(SubmitTemplate {
        page_title: "投稿 · 前厅 · NEXUS".to_owned(),
        static_base: frame.static_base,
        csrf_token: frame.csrf_token,
        nav_display_name: frame.nav_display_name,
        nav_is_admin: frame.nav_is_admin,
        nav_is_guest: frame.nav_is_guest,
        look: frame.look,
        nav_active: frame.nav_active,
        title_value: String::new(),
        body_value: String::new(),
        error: None,
    })
}

pub async fn submit_submit(
    State(state): State<AppState>,
    session: Session,
    user: MaybeUser,
    row: MaybeUserRow,
    client_ip: ClientIp,
    Form(form): Form<SubmitForm>,
) -> Result<Response, AppError> {
    require_login(&user, "/lobby/submit")?;
    middleware::require_csrf(&session, Some(&form.csrf)).await?;

    let current = user
        .user()
        .ok_or_else(|| AppError::LoginRequired {
            next: "/lobby/submit".to_owned(),
        })?
        .clone();

    match service::submit(
        &state.pool,
        &current,
        client_ip.0.as_deref(),
        &form.title,
        &form.body,
    )
    .await
    {
        Ok(_) => Ok(redirect_to("/lobby/mine")),
        Err(AppError::Unprocessable(message)) => {
            let frame = base(&state, &user).with_csrf(&session, &row).await?;
            Ok((
                axum::http::StatusCode::UNPROCESSABLE_ENTITY,
                SubmitTemplate {
                    page_title: "投稿 · 前厅 · NEXUS".to_owned(),
                    static_base: frame.static_base,
                    csrf_token: frame.csrf_token,
                    nav_display_name: frame.nav_display_name,
                    nav_is_admin: frame.nav_is_admin,
                    nav_is_guest: frame.nav_is_guest,
                    look: frame.look,
                    nav_active: frame.nav_active,
                    title_value: form.title,
                    body_value: form.body,
                    error: Some(message.into_owned()),
                },
            )
                .into_response())
        }
        Err(other) => Err(other),
    }
}

pub async fn mine_page(
    State(state): State<AppState>,
    session: Session,
    user: MaybeUser,
    row: MaybeUserRow,
    Query(page): Query<PageQuery>,
) -> Result<MineTemplate, AppError> {
    require_login(&user, "/lobby/mine")?;
    let current = user
        .user()
        .ok_or_else(|| AppError::LoginRequired {
            next: "/lobby/mine".to_owned(),
        })?
        .clone();
    let items = service::my_submissions(&state.pool, &current, &page).await?;
    let tz = state.config.display_tz;
    let views: Vec<SubmissionView> = items.items.iter().map(|s| submission_view(s, tz)).collect();
    let frame = base(&state, &user).with_csrf(&session, &row).await?;

    Ok(MineTemplate {
        page_title: "我的投稿 · 前厅 · NEXUS".to_owned(),
        static_base: frame.static_base,
        csrf_token: frame.csrf_token,
        nav_display_name: frame.nav_display_name,
        nav_is_admin: frame.nav_is_admin,
        nav_is_guest: frame.nav_is_guest,
        look: frame.look,
        nav_active: frame.nav_active,
        submissions: Page::new(views, items.page, items.total, items.per_page),
    })
}

pub async fn queue_page(
    State(state): State<AppState>,
    session: Session,
    user: MaybeUser,
    row: MaybeUserRow,
    Query(page): Query<PageQuery>,
) -> Result<QueueTemplate, AppError> {
    let current = user
        .user()
        .ok_or_else(|| AppError::LoginRequired {
            next: "/lobby/queue".to_owned(),
        })?
        .clone();
    let items = service::queue(&state.pool, &current, &page).await?;
    let tz = state.config.display_tz;
    let views: Vec<SubmissionView> = items.items.iter().map(|s| submission_view(s, tz)).collect();
    let frame = base(&state, &user).with_csrf(&session, &row).await?;

    Ok(QueueTemplate {
        page_title: "待审队列 · 前厅 · NEXUS".to_owned(),
        static_base: frame.static_base,
        csrf_token: frame.csrf_token,
        nav_display_name: frame.nav_display_name,
        nav_is_admin: frame.nav_is_admin,
        nav_is_guest: frame.nav_is_guest,
        look: frame.look,
        nav_active: frame.nav_active,
        submissions: Page::new(views, items.page, items.total, items.per_page),
    })
}

// ── 写操作 ──────────────────────────────────────────────────────────────

pub async fn thread_comment(
    State(state): State<AppState>,
    session: Session,
    user: MaybeUser,
    client_ip: ClientIp,
    Path(id): Path<u64>,
    Form(form): Form<CommentForm>,
) -> Result<Response, AppError> {
    require_login(&user, &format!("/lobby/forum/{id}"))?;
    middleware::require_csrf(&session, Some(&form.csrf)).await?;
    let current = user
        .user()
        .ok_or_else(|| AppError::LoginRequired {
            next: format!("/lobby/forum/{id}"),
        })?
        .clone();

    service::comment(
        &state.pool,
        &current,
        client_ip.0.as_deref(),
        CommentTarget::Thread,
        id,
        &form.body,
    )
    .await?;

    Ok(redirect_to(&format!("/lobby/forum/{id}")))
}

pub async fn doc_comment(
    State(state): State<AppState>,
    session: Session,
    user: MaybeUser,
    client_ip: ClientIp,
    Path(id): Path<u64>,
    Form(form): Form<CommentForm>,
) -> Result<Response, AppError> {
    require_login(&user, &format!("/lobby/kb/{id}"))?;
    middleware::require_csrf(&session, Some(&form.csrf)).await?;
    let current = user
        .user()
        .ok_or_else(|| AppError::LoginRequired {
            next: format!("/lobby/kb/{id}"),
        })?
        .clone();

    service::comment(
        &state.pool,
        &current,
        client_ip.0.as_deref(),
        CommentTarget::Doc,
        id,
        &form.body,
    )
    .await?;

    Ok(redirect_to(&format!("/lobby/kb/{id}")))
}

pub async fn comment_delete(
    State(state): State<AppState>,
    session: Session,
    user: MaybeUser,
    client_ip: ClientIp,
    Path(comment_id): Path<u64>,
    Form(form): Form<CsrfForm>,
) -> Result<Response, AppError> {
    require_login(&user, "/lobby")?;
    middleware::require_csrf(&session, Some(&form.csrf)).await?;
    let current = user
        .user()
        .ok_or_else(|| AppError::LoginRequired {
            next: "/lobby".to_owned(),
        })?
        .clone();

    // 先取一次目标，删完能回到原页面；否则用户每删一条评论就被丢回前厅首页。
    let target = public_model::find_comment(&state.pool, comment_id).await;
    let back = match target {
        Ok(Some(comment)) => match comment.target_type {
            CommentTarget::Thread => format!("/lobby/forum/{}", comment.target_id),
            CommentTarget::Doc => format!("/lobby/kb/{}", comment.target_id),
        },
        _ => "/lobby".to_owned(),
    };

    service::delete_comment(&state.pool, &current, client_ip.0.as_deref(), comment_id).await?;
    Ok(redirect_to(&back))
}

pub async fn thread_like(
    State(state): State<AppState>,
    session: Session,
    user: MaybeUser,
    Path(id): Path<u64>,
    Form(form): Form<CsrfForm>,
) -> Result<Response, AppError> {
    require_login(&user, &format!("/lobby/forum/{id}"))?;
    middleware::require_csrf(&session, Some(&form.csrf)).await?;
    let current = user
        .user()
        .ok_or_else(|| AppError::LoginRequired {
            next: format!("/lobby/forum/{id}"),
        })?
        .clone();

    service::toggle_like(&state.pool, &current, CommentTarget::Thread, id).await?;
    Ok(redirect_to(&format!("/lobby/forum/{id}")))
}

pub async fn doc_like(
    State(state): State<AppState>,
    session: Session,
    user: MaybeUser,
    Path(id): Path<u64>,
    Form(form): Form<CsrfForm>,
) -> Result<Response, AppError> {
    require_login(&user, &format!("/lobby/kb/{id}"))?;
    middleware::require_csrf(&session, Some(&form.csrf)).await?;
    let current = user
        .user()
        .ok_or_else(|| AppError::LoginRequired {
            next: format!("/lobby/kb/{id}"),
        })?
        .clone();

    service::toggle_like(&state.pool, &current, CommentTarget::Doc, id).await?;
    Ok(redirect_to(&format!("/lobby/kb/{id}")))
}

// ── 申请加入（M7） ───────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct JoinForm {
    #[serde(default)]
    pub message: String,
    #[serde(rename = "_csrf")]
    pub csrf: String,
}

#[derive(Template, WebTemplate)]
#[template(path = "public/join.html")]
pub struct JoinTemplate {
    pub page_title: String,
    pub static_base: String,
    pub csrf_token: String,
    pub nav_display_name: String,
    pub nav_is_admin: bool,
    pub nav_is_guest: bool,
    pub look: crate::shared::appearance::Appearance,
    pub nav_active: &'static str,
    pub message_value: String,
    pub error: Option<String>,
    /// 已经是内厅成员：页面改成"你已经在里面了"，不再给申请表单。
    pub is_member: bool,
    pub already_pending: bool,
}

pub async fn join_page(
    State(state): State<AppState>,
    session: Session,
    user: MaybeUser,
    row: MaybeUserRow,
) -> Result<JoinTemplate, AppError> {
    require_login(&user, "/join")?;
    let current = user
        .user()
        .ok_or_else(|| AppError::LoginRequired {
            next: "/join".to_owned(),
        })?
        .clone();

    let already_pending = public_model::find_pending_join_request(&state.pool, current.id)
        .await?
        .is_some();
    let frame = base(&state, &user).with_csrf(&session, &row).await?;

    Ok(JoinTemplate {
        page_title: "申请加入 · NEXUS".to_owned(),
        static_base: frame.static_base,
        csrf_token: frame.csrf_token,
        nav_display_name: frame.nav_display_name,
        nav_is_admin: frame.nav_is_admin,
        nav_is_guest: frame.nav_is_guest,
        look: frame.look,
        nav_active: frame.nav_active,
        message_value: String::new(),
        error: None,
        is_member: !matches!(current.role, crate::auth::model::Role::User),
        already_pending,
    })
}

pub async fn join_submit(
    State(state): State<AppState>,
    session: Session,
    user: MaybeUser,
    row: MaybeUserRow,
    client_ip: ClientIp,
    Form(form): Form<JoinForm>,
) -> Result<Response, AppError> {
    require_login(&user, "/join")?;
    middleware::require_csrf(&session, Some(&form.csrf)).await?;
    let current = user
        .user()
        .ok_or_else(|| AppError::LoginRequired {
            next: "/join".to_owned(),
        })?
        .clone();

    match service::submit_join(&state.pool, &current, client_ip.0.as_deref(), &form.message).await {
        Ok(_) => Ok(redirect_to("/join")),
        // 正文校验与"已有待审申请"都回表单页：让人看见原因，而不是一张错误卡片。
        Err(AppError::Unprocessable(message)) => Ok((
            StatusCode::UNPROCESSABLE_ENTITY,
            JoinTemplate {
                page_title: "申请加入 · NEXUS".to_owned(),
                static_base: state.static_base().to_owned(),
                csrf_token: middleware::ensure_csrf(&session).await?,
                nav_display_name: String::new(),
                nav_is_admin: user.is_admin(),
                nav_is_guest: crate::auth::policy::is_guest(user.user()),
                look: row.look(),
                nav_active: "lobby",
                message_value: form.message,
                error: Some(message.into_owned()),
                is_member: false,
                already_pending: false,
            },
        )
            .into_response()),
        Err(AppError::Conflict(message)) => Ok((
            StatusCode::CONFLICT,
            JoinTemplate {
                page_title: "申请加入 · NEXUS".to_owned(),
                static_base: state.static_base().to_owned(),
                csrf_token: middleware::ensure_csrf(&session).await?,
                nav_display_name: String::new(),
                nav_is_admin: user.is_admin(),
                nav_is_guest: crate::auth::policy::is_guest(user.user()),
                look: row.look(),
                nav_active: "lobby",
                message_value: form.message,
                error: Some(message.into_owned()),
                is_member: false,
                already_pending: true,
            },
        )
            .into_response()),
        Err(other) => Err(other),
    }
}
