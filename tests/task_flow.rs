//! M5 任务模块的集成测试。
//!
//! 与 forum_flow / group_flow / kb_flow 一致：辅助自备、用例造自己的数据、跑完不清理。
//! 这里覆盖 DESIGN §23.8 的十条验收，逐条对应一个 `#[tokio::test]`。

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};

use axum::Router;
use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt;
use nexus::app::build_router;
use nexus::auth::model::{Role, UserStatus};
use nexus::auth::policy::CurrentUser;
use nexus::auth::service as auth_service;
use nexus::config::Config;
use nexus::forum::service as forum_service;
use nexus::forum::service::{NewSpaceInput, NewThreadInput};
use nexus::group::service as group_service;
use nexus::group::service::NewGroupInput;
use nexus::shared::pagination::PageQuery;
use nexus::shared::visibility::Visibility;
use nexus::state::AppState;
use nexus::task::model::TaskStatus;
use nexus::task::service::{self as task_service, TaskInput};
use sqlx::MySqlPool;
use tower::ServiceExt;

// ---------------------------------------------------------------------------
// 基础设施
// ---------------------------------------------------------------------------

fn unique(prefix: &str) -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or_default();
    format!("tk_{prefix}_{nanos:x}_{n}")
}

fn db_url() -> Option<String> {
    std::env::var("TEST_DATABASE_URL")
        .ok()
        .filter(|value| !value.is_empty())
}

fn config_for(database_url: &str) -> Config {
    Config::from_source(|key| match key {
        "DATABASE_URL" => Some(database_url.to_owned()),
        "APP_BASE_URL" => Some("http://127.0.0.1:8080".to_owned()),
        "APP_ENV" => Some("dev".to_owned()),
        _ => None,
    })
    .expect("测试配置应当合法")
}

async fn harness(url: &str) -> (Router, MySqlPool) {
    let pool = nexus::db::init_pool(&config_for(url))
        .await
        .expect("测试库连接失败");
    let state = AppState::new(pool.clone(), config_for(url));
    (build_router(state), pool)
}

fn request(method: &str, uri: &str) -> axum::http::request::Builder {
    Request::builder()
        .method(method)
        .uri(uri)
        .extension(ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 45_678))))
}

fn get_request(uri: &str, cookie: Option<&str>) -> Request<Body> {
    let mut builder = request("GET", uri);
    if let Some(value) = cookie {
        builder = builder.header(header::COOKIE, value);
    }
    builder.body(Body::empty()).expect("请求构造失败")
}

fn form_request(uri: &str, body: &str, cookie: Option<&str>) -> Request<Body> {
    let mut builder =
        request("POST", uri).header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
    if let Some(value) = cookie {
        builder = builder.header(header::COOKIE, value);
    }
    builder
        .body(Body::from(body.to_owned()))
        .expect("请求构造失败")
}

fn cookie_of(response: &axum::http::Response<axum::body::Body>) -> Option<String> {
    response
        .headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .find_map(|value| value.split(';').next())
        .map(str::to_owned)
}

async fn body_of(response: axum::http::Response<axum::body::Body>) -> String {
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("读取响应体失败")
        .to_bytes();
    String::from_utf8_lossy(&bytes).into_owned()
}

fn csrf_of(html: &str) -> Option<String> {
    let marker = "name=\"csrf-token\" content=\"";
    let start = html.find(marker)? + marker.len();
    let rest = &html[start..];
    let end = rest.find('"')?;
    Some(rest[..end].to_owned())
}

async fn page_token(app: &Router, uri: &str, cookie: Option<&str>) -> (String, String) {
    let response = app
        .clone()
        .oneshot(get_request(uri, cookie))
        .await
        .expect("请求失败");
    let cookie = cookie_of(&response)
        .or_else(|| cookie.map(str::to_owned))
        .expect("页面应当带会话 cookie");
    let html = body_of(response).await;
    (html, cookie)
}

async fn make_user(pool: &MySqlPool, username: &str, role: &str) -> u64 {
    let hash = auth_service::hash_password("a-long-password")
        .await
        .expect("口令哈希失败");
    let result = sqlx::query(
        "INSERT INTO users (username, email, password_hash, display_name, role, status) \
         VALUES (?, ?, ?, ?, ?, 'active')",
    )
    .bind(username)
    .bind(format!("{username}@example.test"))
    .bind(hash)
    .bind(username)
    .bind(role)
    .execute(pool)
    .await
    .expect("插入用户失败");
    result.last_insert_id()
}

async fn login(app: &Router, username: &str) -> String {
    let (html, cookie) = page_token(app, "/login", None).await;
    let csrf = csrf_of(&html).expect("登录页应有 CSRF token");
    let body = format!("username={username}&password=a-long-password&_csrf={csrf}");
    let response = app
        .clone()
        .oneshot(form_request("/login", &body, Some(&cookie)))
        .await
        .expect("请求失败");
    assert_eq!(response.status(), StatusCode::SEE_OTHER, "登录应当成功");
    cookie_of(&response).unwrap_or(cookie)
}

/// 会话 cookie 里已经签发了 CSRF，直接从任意页面取一次即可。
async fn csrf_from_page(app: &Router, cookie: &str) -> String {
    let (html, _) = page_token(app, "/tasks", Some(cookie)).await;
    csrf_of(&html).expect("页面应有 CSRF token")
}

fn member(id: u64, groups: Vec<u64>) -> CurrentUser {
    CurrentUser {
        id,
        role: Role::Member,
        status: UserStatus::Active,
        group_ids: groups,
    }
}

fn admin(id: u64) -> CurrentUser {
    CurrentUser {
        id,
        role: Role::Admin,
        status: UserStatus::Active,
        group_ids: Vec::new(),
    }
}

async fn seed_task(pool: &MySqlPool, actor: &CurrentUser, title: &str) -> u64 {
    task_service::create(
        pool,
        actor,
        None,
        TaskInput {
            title,
            body: None,
            assignee_id: None,
            due_on: None,
        },
    )
    .await
    .expect("建任务失败")
}

async fn audit_count(pool: &MySqlPool, action: &str, target_id: u64) -> i64 {
    sqlx::query_scalar(
        "SELECT COUNT(*) FROM audit_logs WHERE action = ? AND target_type = 'task' AND target_id = ?",
    )
    .bind(action)
    .bind(target_id)
    .fetch_one(pool)
    .await
    .expect("查审计失败")
}

async fn audit_meta(pool: &MySqlPool, action: &str, target_id: u64) -> String {
    let meta: Option<String> = sqlx::query_scalar(
        "SELECT CAST(meta AS CHAR) FROM audit_logs \
         WHERE action = ? AND target_type = 'task' AND target_id = ? ORDER BY id DESC LIMIT 1",
    )
    .bind(action)
    .bind(target_id)
    .fetch_optional(pool)
    .await
    .expect("查审计失败")
    .flatten();
    meta.unwrap_or_default().replace(' ', "")
}

async fn task_status(pool: &MySqlPool, id: u64) -> String {
    sqlx::query_scalar("SELECT status FROM tasks WHERE id = ?")
        .bind(id)
        .fetch_one(pool)
        .await
        .expect("查任务失败")
}

async fn require_db() -> Option<(Router, MySqlPool, String)> {
    let url = db_url()?;
    let (app, pool) = harness(&url).await;
    Some((app, pool, url))
}

// ---------------------------------------------------------------------------
// 十条验收
// ---------------------------------------------------------------------------

/// 第 1 条：游客 303 去登录、普通 user 403、member 200。
#[tokio::test]
async fn guests_are_redirected_and_plain_users_are_forbidden() {
    let Some((app, pool, _)) = require_db().await else {
        return;
    };

    let response = app
        .clone()
        .oneshot(get_request("/tasks", None))
        .await
        .expect("请求失败");
    assert_eq!(
        response.status(),
        StatusCode::SEE_OTHER,
        "游客应当被送去登录"
    );
    let location = response
        .headers()
        .get(header::LOCATION)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    assert!(
        location.starts_with("/login?next="),
        "游客应当带 next 去登录页，实际 {location}"
    );

    let outsider = unique("outsider");
    make_user(&pool, &outsider, "user").await;
    let cookie = login(&app, &outsider).await;
    let response = app
        .clone()
        .oneshot(get_request("/tasks", Some(&cookie)))
        .await
        .expect("请求失败");
    assert_eq!(
        response.status(),
        StatusCode::FORBIDDEN,
        "普通 user 不该进任务板"
    );

    let insider = unique("member");
    make_user(&pool, &insider, "member").await;
    let cookie = login(&app, &insider).await;
    let response = app
        .clone()
        .oneshot(get_request("/tasks", Some(&cookie)))
        .await
        .expect("请求失败");
    assert_eq!(response.status(), StatusCode::OK, "member 应当能看任务板");
}

/// 第 2 条：新建任务落库默认值正确、审计一条。
#[tokio::test]
async fn creating_a_task_writes_the_row_and_one_audit_entry() {
    let Some((app, pool, _)) = require_db().await else {
        return;
    };

    let username = unique("creator");
    let creator_id = make_user(&pool, &username, "member").await;
    let cookie = login(&app, &username).await;
    let csrf = csrf_from_page(&app, &cookie).await;
    let title = unique("任务标题");

    let body = format!("title={title}&body=&assignee=&due_on=&_csrf={csrf}");
    let response = app
        .clone()
        .oneshot(form_request("/tasks/new", &body, Some(&cookie)))
        .await
        .expect("请求失败");
    assert_eq!(response.status(), StatusCode::SEE_OTHER, "创建应当 303");

    let (id, status, created_by): (u64, String, u64) = sqlx::query_as(
        "SELECT id, status, created_by FROM tasks WHERE title = ? ORDER BY id DESC LIMIT 1",
    )
    .bind(&title)
    .fetch_one(&pool)
    .await
    .expect("任务没有落库");
    assert_eq!(status, "todo", "新任务默认 todo");
    assert_eq!(created_by, creator_id, "created_by 应当是建它的人");
    assert_eq!(
        audit_count(&pool, "task.create", id).await,
        1,
        "应当写一条创建审计"
    );
}

/// 第 3 条：未指派任务在列表里显示「未指派」。
#[tokio::test]
async fn unassigned_tasks_show_up_as_unassigned() {
    let Some((app, pool, _)) = require_db().await else {
        return;
    };

    let username = unique("viewer");
    let viewer_id = make_user(&pool, &username, "member").await;
    let title = unique("未指派任务");
    seed_task(&pool, &member(viewer_id, Vec::new()), &title).await;

    let cookie = login(&app, &username).await;

    // 测试库是全工程共用且用例并行，第一页未必轮到这条任务；顺着页翻到它为止。
    // 这正是"分页必须真的能翻"的副产品验证：翻不到就是分页坏了。
    let mut found = false;
    let mut shown = String::new();
    for page in 1..=20 {
        let (html, _) = page_token(&app, &format!("/tasks?page={page}"), Some(&cookie)).await;
        if html.contains(&title) {
            found = true;
            shown = html;
            break;
        }
    }
    assert!(found, "翻遍 20 页都没找到刚建的任务");
    assert!(shown.contains("未指派"), "应当显示「未指派」");
}

/// 第 4 条：指派写审计，被指派人能在「我的任务」里看到。
#[tokio::test]
async fn assigning_writes_audit_and_shows_up_in_my_tasks() {
    let Some((app, pool, _)) = require_db().await else {
        return;
    };

    let owner_name = unique("owner");
    let owner_id = make_user(&pool, &owner_name, "member").await;
    let doer_name = unique("doer");
    let doer_id = make_user(&pool, &doer_name, "member").await;

    let title = unique("指派任务");
    let id = seed_task(&pool, &member(owner_id, Vec::new()), &title).await;

    let owner_cookie = login(&app, &owner_name).await;
    task_service::assign(
        &pool,
        &member(owner_id, Vec::new()),
        None,
        id,
        Some(doer_id),
    )
    .await
    .expect("指派失败");

    let meta = audit_meta(&pool, "task.assign", id).await;
    assert!(
        meta.contains(&format!("\"to\":{doer_id}")),
        "审计应带 to={doer_id}：{meta}"
    );

    let doer_cookie = login(&app, &doer_name).await;
    let (html, _) = page_token(&app, "/tasks?mine=1", Some(&doer_cookie)).await;
    assert!(
        html.contains(&title),
        "被指派人应当能在我的任务里看到：{html}"
    );
    let _ = owner_cookie;
}

/// 第 5 条：被指派人能改状态，但不能改派。
#[tokio::test]
async fn assignee_moves_status_but_cannot_reassign() {
    let Some((app, pool, _)) = require_db().await else {
        return;
    };

    let owner_name = unique("owner");
    let owner_id = make_user(&pool, &owner_name, "member").await;
    let doer_name = unique("doer");
    let doer_id = make_user(&pool, &doer_name, "member").await;
    let other_name = unique("other");
    let other_id = make_user(&pool, &other_name, "member").await;

    let id = seed_task(&pool, &member(owner_id, Vec::new()), &unique("进展")).await;
    task_service::assign(
        &pool,
        &member(owner_id, Vec::new()),
        None,
        id,
        Some(doer_id),
    )
    .await
    .expect("指派失败");

    let cookie = login(&app, &doer_name).await;
    let csrf = csrf_from_page(&app, &cookie).await;

    let body = format!("status=doing&_csrf={csrf}");
    let response = app
        .clone()
        .oneshot(form_request(
            &format!("/tasks/{id}/status"),
            &body,
            Some(&cookie),
        ))
        .await
        .expect("请求失败");
    assert_eq!(
        response.status(),
        StatusCode::SEE_OTHER,
        "负责人应当能改状态"
    );
    assert_eq!(task_status(&pool, id).await, "doing");

    let body = format!("assignee={other_id}&_csrf={csrf}");
    let response = app
        .clone()
        .oneshot(form_request(
            &format!("/tasks/{id}/assign"),
            &body,
            Some(&cookie),
        ))
        .await
        .expect("请求失败");
    assert_eq!(
        response.status(),
        StatusCode::FORBIDDEN,
        "被指派人不能把活转手给别人"
    );
}

/// 第 6 条：无关成员改状态 / 改派都 403，且状态不变。
#[tokio::test]
async fn unrelated_members_can_neither_move_status_nor_reassign() {
    let Some((app, pool, _)) = require_db().await else {
        return;
    };

    let owner_name = unique("owner");
    let owner_id = make_user(&pool, &owner_name, "member").await;
    let stranger_name = unique("stranger");
    make_user(&pool, &stranger_name, "member").await;

    let id = seed_task(&pool, &member(owner_id, Vec::new()), &unique("无关")).await;
    let cookie = login(&app, &stranger_name).await;
    let csrf = csrf_from_page(&app, &cookie).await;

    let body = format!("status=done&_csrf={csrf}");
    let response = app
        .clone()
        .oneshot(form_request(
            &format!("/tasks/{id}/status"),
            &body,
            Some(&cookie),
        ))
        .await
        .expect("请求失败");
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert_eq!(task_status(&pool, id).await, "todo", "状态不该被外人改动");

    let body = format!("assignee={owner_id}&_csrf={csrf}");
    let response = app
        .clone()
        .oneshot(form_request(
            &format!("/tasks/{id}/assign"),
            &body,
            Some(&cookie),
        ))
        .await
        .expect("请求失败");
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}

/// 第 7 条：todo → doing → done，两条审计的 from/to 与操作一致。
#[tokio::test]
async fn status_changes_are_audited_with_from_and_to() {
    let Some((_, pool, _)) = require_db().await else {
        return;
    };

    let name = unique("mover");
    let id_owner = make_user(&pool, &name, "member").await;
    let actor = member(id_owner, Vec::new());
    let id = seed_task(&pool, &actor, &unique("流转")).await;

    task_service::change_status(&pool, &actor, None, id, TaskStatus::Doing)
        .await
        .expect("改到 doing 失败");
    task_service::change_status(&pool, &actor, None, id, TaskStatus::Done)
        .await
        .expect("改到 done 失败");

    assert_eq!(task_status(&pool, id).await, "done");
    assert_eq!(
        audit_count(&pool, "task.status_change", id).await,
        2,
        "两次状态变更应当两条审计"
    );
    let meta = audit_meta(&pool, "task.status_change", id).await;
    assert!(
        meta.contains("\"from\":\"doing\"") && meta.contains("\"to\":\"done\""),
        "最后一条审计应是 doing→done：{meta}"
    );
}

/// 第 8 条：从议题生成继承标题与来源；看不见的议题 404 且不落行。
#[tokio::test]
async fn tasks_from_threads_inherit_the_title_and_respect_visibility() {
    let Some((_, pool, _)) = require_db().await else {
        return;
    };

    let admin_name = unique("admin");
    let admin_id = make_user(&pool, &admin_name, "admin").await;
    let admin_user = admin(admin_id);

    let slug = unique("grpcase").replace('_', "-");
    let title = unique("议题转任务");
    let space_id = forum_service::create_space(
        &pool,
        &admin_user,
        None,
        NewSpaceInput {
            name: "任务来源分区",
            slug: &slug,
            description: "为 M5 测试准备",
            visibility: Visibility::Internal,
        },
    )
    .await
    .expect("建分区失败");

    let thread_id = forum_service::create_thread(
        &pool,
        &admin_user,
        None,
        NewThreadInput {
            space_id,
            title: &title,
            body: Some("首楼正文，会变成任务的说明。"),
            body_html: None,
        },
    )
    .await
    .expect("建议题失败");

    let id = task_service::create_from_thread(&pool, &admin_user, None, thread_id)
        .await
        .expect("从议题生成任务失败");

    let (row_title, source): (String, Option<u64>) =
        sqlx::query_as("SELECT title, source_thread_id FROM tasks WHERE id = ?")
            .bind(id)
            .fetch_one(&pool)
            .await
            .expect("任务没有落库");
    assert_eq!(row_title, title, "标题应当继承议题");
    assert_eq!(source, Some(thread_id), "source_thread_id 应当落库");
    let meta = audit_meta(&pool, "task.create", id).await;
    assert!(
        meta.contains("\"source\":\"thread\""),
        "审计应标明来源：{meta}"
    );

    // 把一个分区收成小组可见，再让不在组里的成员来生成任务：应当 404 且不落行。
    let group_slug = unique("onlygroup").replace('_', "-");
    let group_id = group_service::create_group(
        &pool,
        &admin_user,
        None,
        NewGroupInput {
            name: "只给组看",
            slug: &group_slug,
            description: "",
        },
    )
    .await
    .expect("建小组失败");
    group_service::add_member(&pool, &admin_user, None, group_id, admin_id)
        .await
        .expect("加成员失败");
    forum_service::set_space_visibility(
        &pool,
        &admin_user,
        None,
        &slug,
        Visibility::Group,
        Some(group_id),
    )
    .await
    .expect("改可见性失败");

    let outsider_name = unique("nobody");
    let outsider_id = make_user(&pool, &outsider_name, "member").await;
    // 只数这条议题产生的任务：全表 COUNT(*) 会被同文件里并发跑的其他用例干扰，
    // 那种断言会在别人的用例建行时随机变红——等于没有断言。
    let before: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM tasks WHERE source_thread_id = ?")
        .bind(thread_id)
        .fetch_one(&pool)
        .await
        .expect("查任务数失败");

    let result =
        task_service::create_from_thread(&pool, &member(outsider_id, Vec::new()), None, thread_id)
            .await;
    assert!(
        matches!(result, Err(nexus::error::AppError::NotFound)),
        "看不见的议题必须回 404，实际 {result:?}"
    );

    let after: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM tasks WHERE source_thread_id = ?")
        .bind(thread_id)
        .fetch_one(&pool)
        .await
        .expect("查任务数失败");
    assert_eq!(before, after, "被拒之后不应该留下任何任务行");
}

/// 第 9 条：软删后行还在、列表与详情不可见、审计一条。
#[tokio::test]
async fn soft_delete_keeps_the_row_but_hides_it() {
    let Some((_app, pool, _)) = require_db().await else {
        return;
    };

    let name = unique("deleter");
    let owner_id = make_user(&pool, &name, "member").await;
    let actor = member(owner_id, Vec::new());
    let title = unique("将被删掉");
    let id = seed_task(&pool, &actor, &title).await;

    task_service::delete(&pool, &actor, None, id)
        .await
        .expect("删除失败");

    let deleted_at: Option<chrono::DateTime<chrono::Utc>> =
        sqlx::query_scalar("SELECT deleted_at FROM tasks WHERE id = ?")
            .bind(id)
            .fetch_one(&pool)
            .await
            .expect("行应当还在");
    assert!(deleted_at.is_some(), "软删除应当写 deleted_at");
    assert_eq!(audit_count(&pool, "task.delete", id).await, 1);

    // 软删之后连创建者也不该再拿到它：service 层必须是 NotFound，
    // 而不是返回一个 deleted_at 非空、界面上却照旧渲染的空壳。
    let shown = task_service::visible_task(&pool, &actor, id).await;
    assert!(shown.is_err(), "软删后的任务在 service 层必须是不可见");
}

/// 第 10 条：分页按 20 条切、两页不重叠、越界页为空、页面真渲染出 20 张卡片。
#[tokio::test]
async fn pagination_splits_twenty_and_the_rest() {
    let Some((app, pool, _)) = require_db().await else {
        return;
    };

    let name = unique("pager");
    let owner_id = make_user(&pool, &name, "member").await;
    let actor = member(owner_id, Vec::new());
    let marker = unique("pg");
    for i in 0..21 {
        seed_task(&pool, &actor, &format!("{marker}-{i:02}")).await;
    }

    let first = task_service::list_page(&pool, &actor, false, &PageQuery::new(1))
        .await
        .expect("第一页失败");
    assert_eq!(first.items.len(), 20, "第一页应当满 20 条");
    assert!(
        first.total >= 21,
        "新增 21 条后总数至少 21，实际 {}（并行用例还会继续加）",
        first.total
    );

    let second = task_service::list_page(&pool, &actor, false, &PageQuery::new(2))
        .await
        .expect("第二页失败");
    assert!(!second.items.is_empty(), "第二页不该是空的");

    let first_ids: Vec<u64> = first.items.iter().map(|item| item.id).collect();
    let second_ids: Vec<u64> = second.items.iter().map(|item| item.id).collect();
    assert!(
        first_ids.iter().all(|id| !second_ids.contains(id)),
        "两页不应出现同一条任务"
    );

    let far = task_service::list_page(&pool, &actor, false, &PageQuery::new(999))
        .await
        .expect("越界页失败");
    assert!(far.items.is_empty(), "越界页应当是空列表");

    // 模板里的分页控件是真跑过的：第一页必须渲染 20 张卡片。
    let cookie = login(&app, &name).await;
    let (html, _) = page_token(&app, "/tasks", Some(&cookie)).await;
    let cards = html.matches("class=\"card task-card\"").count();
    assert_eq!(cards, 20, "第一页应当渲染 20 张卡片");

    // 并行用例随时会在我们之后建任务，"刚建的必须在第一页"并不成立；
    // 能钉死的是：我们这批任务都能通过翻页找到。
    let mut found = false;
    for page in 1..=5 {
        let uri = if page == 1 {
            "/tasks".to_string()
        } else {
            format!("/tasks?page={page}")
        };
        let (html, _) = page_token(&app, &uri, Some(&cookie)).await;
        if html.contains(&format!("{marker}-20")) {
            found = true;
            break;
        }
    }
    assert!(found, "最新建的那条任务必须能通过翻页找到");
}
