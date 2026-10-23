//! 论坛流程的端到端集成测试：可见性、发帖、回帖、定论、归档、软删、分页。
//!
//! 和 `auth_flow.rs` 一样，走的是**线上同一条路由**（`nexus::app::build_router`）。
//! 需要 `TEST_DATABASE_URL`，没设时全部打印跳过并返回。
//!
//! 这些用例故意在"边界"上使劲，而不是复述正常流程：
//! 游客看不见内部区、普通用户看不见内部区、无权时返回 404 而不是 403、
//! 归档后回帖被拒、软删后行还在。正常流程人眼能看出来，边界只有测试能守住。
//!
//! **用例之间不共享数据，也不做批量清理**：所有名字都带纳秒时间戳，并行跑互不干扰。
//! 曾经写过"按前缀批量删"的收尾，结果是并行跑时把隔壁用例的数据一起删掉，
//! 7 个用例连环失败——这类"清理逻辑"比不清理更危险。测试库的残留行无害，
//! 要清就手工 `TRUNCATE`。

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};

use axum::Router;
use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt;
use nexus::app::build_router;
use nexus::auth::service;
use nexus::config::Config;
use nexus::state::AppState;
use sqlx::MySqlPool;
use tower::ServiceExt;

fn unique(prefix: &str) -> String {
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("ff_{prefix}_{nanos:x}_{n}")
}

fn db_url() -> Option<String> {
    match std::env::var("TEST_DATABASE_URL") {
        Ok(url) if !url.trim().is_empty() => Some(url),
        _ => {
            eprintln!("跳过：未设置 TEST_DATABASE_URL（这些用例需要真库）");
            None
        }
    }
}

fn config_for(database_url: &str) -> Config {
    Config::from_source(|key| match key {
        "DATABASE_URL" => Some(database_url.to_string()),
        "APP_BASE_URL" => Some("http://127.0.0.1:18080".to_string()),
        "SESSION_SECURE" => Some("false".to_string()),
        _ => None,
    })
    .expect("测试配置必须合法")
}

async fn harness(url: &str) -> (Router, MySqlPool) {
    let config = config_for(url);
    let pool = nexus::db::init_pool(&config).await.expect("建池失败");
    nexus::db::migrate(&pool).await.expect("迁移失败");
    let app = build_router(AppState::new(pool.clone(), config));
    (app, pool)
}

fn request(method: &str, uri: &str) -> axum::http::request::Builder {
    Request::builder()
        .method(method)
        .uri(uri)
        .extension(ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 45_678))))
}

fn get_request(uri: &str, cookie: Option<&str>) -> Request<Body> {
    let mut builder = request("GET", uri);
    if let Some(cookie) = cookie {
        builder = builder.header(header::COOKIE, cookie);
    }
    builder.body(Body::empty()).expect("构造请求失败")
}

fn form_request(uri: &str, body: &str, cookie: Option<&str>) -> Request<Body> {
    let mut builder =
        request("POST", uri).header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
    if let Some(cookie) = cookie {
        builder = builder.header(header::COOKIE, cookie);
    }
    builder
        .body(Body::from(body.to_string()))
        .expect("构造请求失败")
}

fn cookie_of(response: &axum::http::Response<axum::body::Body>) -> Option<String> {
    let raw = response.headers().get(header::SET_COOKIE)?.to_str().ok()?;
    let pair = raw.split(';').next()?.trim();
    (!pair.is_empty()).then(|| pair.to_string())
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
    const MARK: &str = "name=\"_csrf\" value=\"";
    let start = html.find(MARK)? + MARK.len();
    let rest = &html[start..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

/// 拿一个会话 cookie + CSRF token。`uri` 必须是渲染了表单的页面。
///
/// `cookie` 为 `None` 表示以游客身份取——登录/注册页就是这么用的。
/// 已登录的页面必须把自己的 cookie 传进来，否则会以游客身份拿到 404/403，
/// 而报错看起来像"页面不存在"，很容易误判成代码 bug（我第一次跑就是这么被误导的）。
async fn page_token(app: &Router, uri: &str, cookie: Option<&str>) -> (String, String) {
    let response = app
        .clone()
        .oneshot(get_request(uri, cookie))
        .await
        .expect("请求失败");
    assert_eq!(response.status(), StatusCode::OK, "{uri} 应可访问");
    let result_cookie = cookie_of(&response)
        .or_else(|| cookie.map(str::to_owned))
        .expect("页面必须下发会话 cookie，或调用方已经带了一个");
    let html = body_of(response).await;
    let token = csrf_of(&html).unwrap_or_else(|| panic!("{uri} 页面里没有 CSRF 字段：{html}"));
    (result_cookie, token)
}

/// 直接在库里造一个指定角色的账号（不经过注册页面——那是 auth_flow 的职责）。
async fn make_user(pool: &MySqlPool, username: &str, role: &str) -> String {
    let hash = service::hash_password("a-long-password")
        .await
        .expect("算密码哈希失败");

    sqlx::query(
        "INSERT INTO users (username, email, password_hash, display_name, role, status) \
         VALUES (?, ?, ?, ?, ?, 'active')",
    )
    .bind(username)
    .bind(format!("{username}@example.com"))
    .bind(&hash)
    .bind(username)
    .bind(role)
    .execute(pool)
    .await
    .expect("建用户失败");

    username.to_string()
}

/// 登录并返回会话 cookie。
async fn login(app: &Router, username: &str) -> String {
    let (cookie, token) = page_token(app, "/login", None).await;
    let body = format!("_csrf={token}&username={username}&password=a-long-password&next=");
    let response = app
        .clone()
        .oneshot(form_request("/login", &body, Some(&cookie)))
        .await
        .expect("请求失败");
    assert_eq!(
        response.status(),
        StatusCode::SEE_OTHER,
        "{username} 登录应成功"
    );
    cookie_of(&response).unwrap_or_else(|| panic!("{username} 登录后没有新 cookie"))
}

async fn seed_space(pool: &MySqlPool, slug: &str, name: &str, visibility: &str) -> u64 {
    // 审计要求 actor 存在，这里直接写库绕过 service，避免测试之间抢 admin 名额。
    let owner = ensure_owner(pool).await;

    let result = sqlx::query(
        "INSERT INTO forum_spaces (name, slug, description, sort_order, visibility, created_by) \
         VALUES (?, ?, '测试分区', 100, ?, ?)",
    )
    .bind(name)
    .bind(slug)
    .bind(visibility)
    .bind(owner)
    .execute(pool)
    .await
    .expect("建分区失败");

    result.last_insert_id()
}

/// 找一个已存在的用户当"作者"；库里一个都没有时先造一个。
///
/// 分区与议题的 `created_by` / `author_id` 都是真外键（RESTRICT），
/// 拿 0 去插会直接被数据库拒掉——所以这里不能图省事写死 1。
async fn ensure_owner(pool: &MySqlPool) -> u64 {
    if let Some(id) = sqlx::query_scalar::<_, u64>("SELECT id FROM users ORDER BY id ASC LIMIT 1")
        .fetch_optional(pool)
        .await
        .expect("查用户失败")
    {
        return id;
    }

    let username = unique("owner");
    make_user(pool, &username, "member").await;
    sqlx::query_scalar("SELECT id FROM users WHERE username = ?")
        .bind(&username)
        .fetch_one(pool)
        .await
        .expect("查新用户失败")
}

#[allow(dead_code)]
async fn cleanup(pool: &MySqlPool, prefix: &str) {
    let like = format!("{prefix}%");
    let _ = sqlx::query(
        "DELETE p FROM forum_posts p \
         JOIN forum_threads t ON t.id = p.thread_id \
         JOIN forum_spaces s ON s.id = t.space_id \
         WHERE s.slug LIKE ?",
    )
    .bind(&like)
    .execute(pool)
    .await;
    let _ = sqlx::query(
        "DELETE t FROM forum_threads t \
         JOIN forum_spaces s ON s.id = t.space_id \
         WHERE s.slug LIKE ?",
    )
    .bind(&like)
    .execute(pool)
    .await;
    let _ = sqlx::query("DELETE FROM forum_spaces WHERE slug LIKE ?")
        .bind(&like)
        .execute(pool)
        .await;
    let _ = sqlx::query("DELETE FROM users WHERE username LIKE ?")
        .bind(&like)
        .execute(pool)
        .await;
}

// ---------------------------------------------------------------------------
// 用例
// ---------------------------------------------------------------------------

#[tokio::test]
async fn guest_sees_public_space_but_internal_space_is_not_found() {
    let Some(url) = db_url() else { return };
    let (app, pool) = harness(&url).await;

    let public_slug = unique("pub");
    let internal_slug = unique("int");
    seed_space(&pool, &public_slug, "公开分区", "public").await;
    seed_space(&pool, &internal_slug, "内部分区", "internal").await;

    let response = app
        .clone()
        .oneshot(get_request("/forum", None))
        .await
        .expect("请求失败");
    assert_eq!(response.status(), StatusCode::OK);
    let html = body_of(response).await;
    assert!(html.contains(&public_slug), "游客应能看到公开分区：{html}");
    assert!(
        !html.contains(&internal_slug),
        "游客不该看到内部分区：{html}"
    );

    let response = app
        .clone()
        .oneshot(get_request(&format!("/forum/{internal_slug}"), None))
        .await
        .expect("请求失败");
    assert_eq!(
        response.status(),
        StatusCode::NOT_FOUND,
        "内部分区对游客必须是 404，不能是 403"
    );
}

#[tokio::test]
async fn plain_user_cannot_see_internal_space() {
    let Some(url) = db_url() else { return };
    let (app, pool) = harness(&url).await;

    let slug = unique("int");
    seed_space(&pool, &slug, "内部分区", "internal").await;

    let username = unique("u");
    make_user(&pool, &username, "user").await;
    let cookie = login(&app, &username).await;

    let response = app
        .clone()
        .oneshot(get_request(&format!("/forum/{slug}"), Some(&cookie)))
        .await
        .expect("请求失败");
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn member_can_create_thread_and_reply_with_audit_rows() {
    let Some(url) = db_url() else { return };
    let (app, pool) = harness(&url).await;

    let slug = unique("int");
    let space_id = seed_space(&pool, &slug, "内部分区", "internal").await;

    let username = unique("m");
    make_user(&pool, &username, "member").await;
    let cookie = login(&app, &username).await;

    // 发议题
    let response = app
        .clone()
        .oneshot(get_request(&format!("/forum/{slug}/new"), Some(&cookie)))
        .await
        .expect("请求失败");
    assert_eq!(response.status(), StatusCode::OK, "成员应能打开发议题页");
    let html = body_of(response).await;
    let token = csrf_of(&html).expect("发议题页应有 CSRF");

    let body = format!(
        "_csrf={token}&title=%E7%BC%96%E8%BE%91%E6%B5%81%E7%A8%8B%E7%A1%AE%E8%AE%A4&body=%E7%AC%AC%E4%B8%80%E7%89%88"
    );
    let response = app
        .clone()
        .oneshot(form_request(
            &format!("/forum/{slug}/new"),
            &body,
            Some(&cookie),
        ))
        .await
        .expect("请求失败");
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    let location = response
        .headers()
        .get(header::LOCATION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    assert!(
        location.starts_with("/forum/t/"),
        "应跳到议题详情：{location}"
    );

    let thread_id: u64 = sqlx::query_scalar(
        "SELECT id FROM forum_threads WHERE space_id = ? ORDER BY id DESC LIMIT 1",
    )
    .bind(space_id)
    .fetch_one(&pool)
    .await
    .expect("议题应已落库");

    // 详情页能把标题和首楼渲染出来
    let response = app
        .clone()
        .oneshot(get_request(&location, Some(&cookie)))
        .await
        .expect("请求失败");
    assert_eq!(response.status(), StatusCode::OK);
    let html = body_of(response).await;
    assert!(html.contains("编辑流程确认"), "标题应显示：{html}");
    assert!(html.contains("第一版"), "首楼正文应显示：{html}");

    // 回帖
    let token = csrf_of(&html).expect("详情页应有 CSRF");
    let body = format!("_csrf={token}&body=%E5%9B%9E%E5%A4%8D%E4%B8%80%E6%9D%A1");
    let response = app
        .clone()
        .oneshot(form_request(
            &format!("/forum/t/{thread_id}/reply"),
            &body,
            Some(&cookie),
        ))
        .await
        .expect("请求失败");
    assert_eq!(response.status(), StatusCode::SEE_OTHER);

    let posts: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM forum_posts WHERE thread_id = ?")
        .bind(thread_id)
        .fetch_one(&pool)
        .await
        .expect("查回复失败");
    assert_eq!(posts, 2, "首楼 + 一条回复");

    // 审计两条：建议题、发回复
    let created: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM audit_logs WHERE action = 'forum.thread_create' AND target_id = ?",
    )
    .bind(thread_id)
    .fetch_one(&pool)
    .await
    .expect("查审计失败");
    assert_eq!(created, 1, "建议题必须留审计");

    // 回帖审计只该属于这条议题：全表数 'forum.post_create' 会被并发用例抬高，
    // 写成 >= 1 又等于没断言，所以 join 回帖子表按 thread_id 精确数。
    let replied: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM audit_logs a JOIN forum_posts p ON p.id = a.target_id \
         WHERE a.action = 'forum.post_create' AND a.target_type = 'post' AND p.thread_id = ?",
    )
    .bind(thread_id)
    .fetch_one(&pool)
    .await
    .expect("查审计失败");
    assert_eq!(replied, 1, "回帖必须留且只留一条审计");
}

#[tokio::test]
async fn resolve_records_audit_and_only_author_or_admin_can_do_it() {
    let Some(url) = db_url() else { return };
    let (app, pool) = harness(&url).await;

    let slug = unique("int");
    let space_id = seed_space(&pool, &slug, "内部分区", "internal").await;

    let author = unique("a");
    make_user(&pool, &author, "member").await;
    let other = unique("b");
    make_user(&pool, &other, "member").await;

    let author_cookie = login(&app, &author).await;
    let other_cookie = login(&app, &other).await;

    let author_id: u64 = sqlx::query_scalar("SELECT id FROM users WHERE username = ?")
        .bind(&author)
        .fetch_one(&pool)
        .await
        .expect("查作者失败");

    let result = sqlx::query(
        "INSERT INTO forum_threads (space_id, author_id, title, visibility) VALUES (?, ?, '要定论的议题', 'internal')",
    )
    .bind(space_id)
    .bind(author_id)
    .execute(&pool)
    .await
    .expect("建议题失败");
    let thread_id = result.last_insert_id();

    // 别人不能定论
    let (_, token) = page_token(&app, &format!("/forum/t/{thread_id}"), Some(&other_cookie)).await;
    let body = format!("_csrf={token}&value=on");
    let response = app
        .clone()
        .oneshot(form_request(
            &format!("/forum/t/{thread_id}/resolve"),
            &body,
            Some(&other_cookie),
        ))
        .await
        .expect("请求失败");
    assert_eq!(response.status(), StatusCode::FORBIDDEN, "非作者不能定论");

    // 作者可以定论
    let (_, token) = page_token(&app, &format!("/forum/t/{thread_id}"), Some(&author_cookie)).await;
    let body = format!("_csrf={token}&value=on");
    let response = app
        .clone()
        .oneshot(form_request(
            &format!("/forum/t/{thread_id}/resolve"),
            &body,
            Some(&author_cookie),
        ))
        .await
        .expect("请求失败");
    assert_eq!(response.status(), StatusCode::SEE_OTHER);

    let resolved: i8 = sqlx::query_scalar("SELECT is_resolved FROM forum_threads WHERE id = ?")
        .bind(thread_id)
        .fetch_one(&pool)
        .await
        .expect("查定论状态失败");
    assert_eq!(resolved, 1);

    let audited: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM audit_logs WHERE action = 'forum.thread_resolve' AND target_id = ?",
    )
    .bind(thread_id)
    .fetch_one(&pool)
    .await
    .expect("查审计失败");
    assert_eq!(audited, 1, "定论必须留审计");
}

#[tokio::test]
async fn archived_thread_rejects_new_replies() {
    let Some(url) = db_url() else { return };
    let (app, pool) = harness(&url).await;

    let slug = unique("int");
    let space_id = seed_space(&pool, &slug, "内部分区", "internal").await;

    let writer = unique("w");
    make_user(&pool, &writer, "member").await;
    let cookie = login(&app, &writer).await;

    let writer_id: u64 = sqlx::query_scalar("SELECT id FROM users WHERE username = ?")
        .bind(&writer)
        .fetch_one(&pool)
        .await
        .expect("查用户失败");

    let result = sqlx::query(
        "INSERT INTO forum_threads (space_id, author_id, title, visibility, is_archived) \
         VALUES (?, ?, '已归档的议题', 'internal', 1)",
    )
    .bind(space_id)
    .bind(writer_id)
    .execute(&pool)
    .await
    .expect("建议题失败");
    let thread_id = result.last_insert_id();

    let (_, token) = page_token(&app, &format!("/forum/t/{thread_id}"), Some(&cookie)).await;
    let body = format!("_csrf={token}&body=%E6%83%B3%E5%9B%9E%E4%B8%80%E6%9D%A1");
    let response = app
        .clone()
        .oneshot(form_request(
            &format!("/forum/t/{thread_id}/reply"),
            &body,
            Some(&cookie),
        ))
        .await
        .expect("请求失败");
    assert_eq!(
        response.status(),
        StatusCode::UNPROCESSABLE_ENTITY,
        "归档后回帖应被拒"
    );

    let posts: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM forum_posts WHERE thread_id = ?")
        .bind(thread_id)
        .fetch_one(&pool)
        .await
        .expect("查回复失败");
    assert_eq!(posts, 0, "被拒的回复不能落库");
}

#[tokio::test]
async fn delete_post_soft_deletes_and_keeps_the_row() {
    let Some(url) = db_url() else { return };
    let (app, pool) = harness(&url).await;

    let slug = unique("int");
    let space_id = seed_space(&pool, &slug, "内部分区", "internal").await;

    let writer = unique("d");
    make_user(&pool, &writer, "member").await;
    let cookie = login(&app, &writer).await;

    let writer_id: u64 = sqlx::query_scalar("SELECT id FROM users WHERE username = ?")
        .bind(&writer)
        .fetch_one(&pool)
        .await
        .expect("查用户失败");

    let result = sqlx::query(
        "INSERT INTO forum_threads (space_id, author_id, title, visibility) VALUES (?, ?, '要删回复的议题', 'internal')",
    )
    .bind(space_id)
    .bind(writer_id)
    .execute(&pool)
    .await
    .expect("建议题失败");
    let thread_id = result.last_insert_id();

    let result = sqlx::query(
        "INSERT INTO forum_posts (thread_id, author_id, body) VALUES (?, ?, '这条会被删掉')",
    )
    .bind(thread_id)
    .bind(writer_id)
    .execute(&pool)
    .await
    .expect("建回复失败");
    let post_id = result.last_insert_id();

    let (_, token) = page_token(&app, &format!("/forum/t/{thread_id}"), Some(&cookie)).await;
    let body = format!("_csrf={token}");
    let response = app
        .clone()
        .oneshot(form_request(
            &format!("/forum/p/{post_id}/delete"),
            &body,
            Some(&cookie),
        ))
        .await
        .expect("请求失败");
    assert_eq!(response.status(), StatusCode::SEE_OTHER);

    let deleted_at: Option<chrono::DateTime<chrono::Utc>> =
        sqlx::query_scalar("SELECT deleted_at FROM forum_posts WHERE id = ?")
            .bind(post_id)
            .fetch_one(&pool)
            .await
            .expect("行必须还在——软删不是物理删除");

    assert!(deleted_at.is_some(), "deleted_at 应被置上");
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM forum_posts WHERE id = ?")
            .bind(post_id)
            .fetch_one(&pool)
            .await
            .expect("查行失败"),
        1,
        "软删之后行必须还在（审计与追溯要用）"
    );
}

#[tokio::test]
async fn duplicate_slug_is_rejected_with_422_and_rerenders_the_form() {
    let Some(url) = db_url() else { return };
    let (app, pool) = harness(&url).await;

    let admin = unique("adm");
    make_user(&pool, &admin, "admin").await;
    let cookie = login(&app, &admin).await;

    // slug 的字符集是 [a-z0-9-]，而 `unique()` 里带下划线——直接拿来当 slug 会被自己的
    // 校验挡下来。第一次跑就栽在这：断言"第一次应成功"却回了 200 的表单页（422 被
    // handler 翻译成"回填重渲染"），看起来像路由坏了，其实是测试数据不合法。
    let slug = unique("dup").replace('_', "-");

    let (_, token) = page_token(&app, "/forum/new", Some(&cookie)).await;
    let body = format!(
        "_csrf={token}&name=%E5%88%86%E5%8C%BA&slug={slug}&description=%E8%AF%B4%E6%98%8E&visibility=internal"
    );
    let response = app
        .clone()
        .oneshot(form_request("/forum/new", &body, Some(&cookie)))
        .await
        .expect("请求失败");
    assert_eq!(response.status(), StatusCode::SEE_OTHER, "第一次应成功");

    let (_, token) = page_token(&app, "/forum/new", Some(&cookie)).await;
    let body = format!(
        "_csrf={token}&name=%E5%88%86%E5%8C%BA&slug={slug}&description=%E8%AF%B4%E6%98%8E&visibility=internal"
    );
    let response = app
        .clone()
        .oneshot(form_request("/forum/new", &body, Some(&cookie)))
        .await
        .expect("请求失败");
    assert_eq!(
        response.status(),
        StatusCode::UNPROCESSABLE_ENTITY,
        "slug 重复应回 422 而不是 500"
    );

    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM forum_spaces WHERE slug = ?")
        .bind(&slug)
        .fetch_one(&pool)
        .await
        .expect("查分区失败");
    assert_eq!(count, 1, "重复提交不能建出第二个分区");
}

#[tokio::test]
async fn plain_user_cannot_create_space() {
    let Some(url) = db_url() else { return };
    let (app, pool) = harness(&url).await;

    let username = unique("u");
    make_user(&pool, &username, "user").await;
    let cookie = login(&app, &username).await;

    let response = app
        .clone()
        .oneshot(get_request("/forum/new", Some(&cookie)))
        .await
        .expect("请求失败");
    assert_eq!(
        response.status(),
        StatusCode::FORBIDDEN,
        "建分区只有 admin 能做"
    );
}

#[tokio::test]
async fn pagination_pages_do_not_overlap_and_out_of_range_is_empty() {
    let Some(url) = db_url() else { return };
    let (app, pool) = harness(&url).await;

    let slug = unique("pg");
    let space_id = seed_space(&pool, &slug, "分页分区", "public").await;
    let author = ensure_owner(&pool).await;

    for i in 0..25 {
        sqlx::query(
            "INSERT INTO forum_threads (space_id, author_id, title, visibility, last_post_at) \
             VALUES (?, ?, ?, 'public', CURRENT_TIMESTAMP)",
        )
        .bind(space_id)
        .bind(author)
        .bind(format!("议题 {i:02}"))
        .execute(&pool)
        .await
        .expect("建议题失败");
    }

    let page1 = app
        .clone()
        .oneshot(get_request(&format!("/forum/{slug}?page=1"), None))
        .await
        .expect("请求失败");
    assert_eq!(page1.status(), StatusCode::OK);
    let html1 = body_of(page1).await;

    let page2 = app
        .clone()
        .oneshot(get_request(&format!("/forum/{slug}?page=2"), None))
        .await
        .expect("请求失败");
    assert_eq!(page2.status(), StatusCode::OK);
    let html2 = body_of(page2).await;

    // 第一页 20 条、第二页 5 条。用标题前缀判断重叠，比数链接更稳。
    let count1 = (0..25)
        .filter(|i| html1.contains(&format!("议题 {i:02}")))
        .count();
    let count2 = (0..25)
        .filter(|i| html2.contains(&format!("议题 {i:02}")))
        .count();
    assert_eq!(count1, 20, "第一页应有 20 条");
    assert_eq!(count2, 5, "第二页应有 5 条");

    let overlap = (0..25)
        .filter(|i| {
            let title = format!("议题 {i:02}");
            html1.contains(&title) && html2.contains(&title)
        })
        .count();
    assert_eq!(overlap, 0, "两页不能出现同一条议题");

    let beyond = app
        .clone()
        .oneshot(get_request(&format!("/forum/{slug}?page=999"), None))
        .await
        .expect("请求失败");
    assert_eq!(
        beyond.status(),
        StatusCode::OK,
        "越界页应返回空列表，不是 500"
    );
    let html_beyond = body_of(beyond).await;
    assert!(!html_beyond.contains("议题 00"), "越界页不该显示任何议题");
}

#[tokio::test]
async fn reply_count_ignores_soft_deleted_posts() {
    let Some(url) = db_url() else { return };
    let (_, pool) = harness(&url).await;

    let slug = unique("rc");
    let space_id = seed_space(&pool, &slug, "计数分区", "public").await;
    let author = ensure_owner(&pool).await;

    let result = sqlx::query(
        "INSERT INTO forum_threads (space_id, author_id, title, visibility) VALUES (?, ?, '计数议题', 'public')",
    )
    .bind(space_id)
    .bind(author)
    .execute(&pool)
    .await
    .expect("建议题失败");
    let thread_id = result.last_insert_id();

    for _ in 0..2 {
        sqlx::query(
            "INSERT INTO forum_posts (thread_id, author_id, body) VALUES (?, ?, '正常回复')",
        )
        .bind(thread_id)
        .bind(author)
        .execute(&pool)
        .await
        .expect("建回复失败");
    }
    sqlx::query(
        "INSERT INTO forum_posts (thread_id, author_id, body, deleted_at) \
         VALUES (?, ?, '已删回复', CURRENT_TIMESTAMP)",
    )
    .bind(thread_id)
    .bind(author)
    .execute(&pool)
    .await
    .expect("建已删回复失败");

    // 这里直接打 model 层：计数是"列表页显示的数字"，做成页面断言要解析 HTML，
    // 反而看不出差异到底出在哪。
    let page = nexus::forum::model::list_threads(
        &pool,
        space_id,
        &nexus::auth::policy::CurrentUser::guest(),
        &nexus::shared::pagination::PageQuery { page: 1 },
    )
    .await
    .expect("查列表失败");

    let row = page.items.first().expect("应有一条议题");
    assert_eq!(
        row.reply_count, 2,
        "软删的回复不该算进计数（清单里是 3 行，用户看到的是 2）"
    );
}

/// 分区顺序看热度（未删议题 + 未删回复），不看人工 `sort_order`。
///
/// 这个用例故意让两个信号的结论相反：冷分区的人工排序是 1（旧逻辑里排第一），
/// 热分区是 999（旧逻辑里垫底）。只有真按热度排，热分区才会跑到前面。
#[tokio::test]
async fn spaces_are_ordered_by_heat_not_by_manual_sort_order() {
    let Some(url) = db_url() else {
        return;
    };
    let (app, pool) = harness(&url).await;
    let owner = ensure_owner(&pool).await;
    let tag = unique("heat");
    let cold = format!("{tag}-cold");
    let hot = format!("{tag}-hot");

    for (slug, order) in [(&cold, 1_i32), (&hot, 999_i32)] {
        sqlx::query(
            "INSERT INTO forum_spaces (name, slug, description, sort_order, visibility, created_by) \
             VALUES (?, ?, '热度排序测试', ?, 'public', ?)",
        )
        .bind(format!("热度测试 {slug}"))
        .bind(slug)
        .bind(order)
        .bind(owner)
        .execute(&pool)
        .await
        .expect("建分区失败");
    }

    let hot_id: u64 = sqlx::query_scalar("SELECT id FROM forum_spaces WHERE slug = ?")
        .bind(&hot)
        .fetch_one(&pool)
        .await
        .expect("查热分区失败");
    for index in 0..3 {
        sqlx::query(
            "INSERT INTO forum_threads (space_id, author_id, title, visibility) \
             VALUES (?, ?, ?, 'public')",
        )
        .bind(hot_id)
        .bind(owner)
        .bind(format!("热帖 {index}"))
        .execute(&pool)
        .await
        .expect("建议题失败");
    }

    // 游客看列表页：这里不能借 page_token——它要求页面里带 CSRF 字段，
    // 而游客页按设计不输出 csrf meta（空 token 不渲染），会直接在断言里炸。
    let response = app
        .clone()
        .oneshot(get_request("/forum", None))
        .await
        .expect("请求失败");
    assert_eq!(response.status(), StatusCode::OK, "/forum 应可访问");
    let html = body_of(response).await;
    let hot_at = html.find(&hot).expect("热分区应当出现在列表里");
    let cold_at = html.find(&cold).expect("冷分区应当出现在列表里");
    assert!(
        hot_at < cold_at,
        "三条议题的分区必须排在零议题分区之前（热度排序），实际 hot={hot_at} cold={cold_at}"
    );

    // 页面上把两个数字露出来，否则顺序对用户是黑箱。
    assert!(
        html.contains("3 议题") || html.contains("3 议题 / 0 回复"),
        "分区卡片应当显示议题数"
    );
}
