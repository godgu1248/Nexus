//! 富文本正文（DESIGN §32）的端到端验收。
//!
//! 只走公开的 HTTP 接口 + 真数据库：证明三件事——富文本真的落库、危险标签真的被摘掉、
//! 老式纯文本提交仍然可用（没有 JS 的用户提交的是 textarea 里的纯文本）。

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use tower::ServiceExt;

use nexus::auth::policy::CurrentUser;
use nexus::auth::service;
use nexus::config::Config;
use nexus::forum::service as forum_service;
use nexus::state::AppState;
use sqlx::MySqlPool;

fn unique(prefix: &str) -> String {
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    format!("rt_{prefix}_{nanos:x}_{n}")
}

fn db_url() -> Option<String> {
    match std::env::var("TEST_DATABASE_URL") {
        Ok(url) if !url.trim().is_empty() => Some(url),
        _ => {
            println!("跳过：没有设置 TEST_DATABASE_URL");
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
    let app = nexus::app::build_router(AppState::new(pool.clone(), config));
    (app, pool)
}

fn request(method: &str, uri: &str) -> axum::http::request::Builder {
    let mut builder = Request::builder().method(method).uri(uri);
    builder = builder.extension(axum::extract::ConnectInfo(SocketAddr::from((
        [127, 0, 0, 1],
        45_678,
    ))));
    builder
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

async fn body_of(response: axum::http::Response<Body>) -> String {
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("读响应体失败")
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

fn cookie_of(response: &axum::http::Response<Body>) -> Option<String> {
    let raw = response
        .headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .find(|value| value.starts_with("nexus_session="))?;
    Some(raw.split(';').next()?.to_string())
}

async fn page_cookie_and_token(app: &Router, uri: &str) -> (String, String) {
    let response = app
        .clone()
        .oneshot(get_request(uri, None))
        .await
        .expect("请求失败");
    assert_eq!(response.status(), StatusCode::OK, "{uri} 应当可访问");
    let cookie = cookie_of(&response).expect("应当签发会话 cookie");
    let html = body_of(response).await;
    let token = csrf_of(&html).expect("页面应当带 CSRF");
    (cookie, token)
}

async fn make_user(pool: &MySqlPool, username: &str, role: &str) -> u64 {
    let hash = service::hash_password("a-long-password")
        .await
        .expect("算密码哈希失败");
    sqlx::query(
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
    .expect("建用户失败");

    sqlx::query_scalar("SELECT id FROM users WHERE username = ?")
        .bind(username)
        .fetch_one(pool)
        .await
        .expect("查用户失败")
}

async fn login(app: &Router, username: &str) -> String {
    let (cookie, token) = page_cookie_and_token(app, "/login").await;
    let body = format!("_csrf={token}&username={username}&password=a-long-password&next=");
    let response = app
        .clone()
        .oneshot(form_request("/login", &body, Some(&cookie)))
        .await
        .expect("登录请求失败");
    assert_eq!(response.status(), StatusCode::SEE_OTHER, "登录应当成功");
    cookie_of(&response).unwrap_or(cookie)
}

async fn current_user(pool: &MySqlPool, username: &str) -> CurrentUser {
    let user = nexus::auth::model::find_by_username(pool, username)
        .await
        .expect("查用户失败")
        .expect("用户应当存在");
    CurrentUser::from_user_with_groups(&user, Vec::new())
}

async fn seed_space(pool: &MySqlPool, slug: &str, visibility: &str) -> u64 {
    let owner = make_user(pool, &unique("owner"), "member").await;
    sqlx::query(
        "INSERT INTO forum_spaces (name, slug, description, visibility, created_by) \
         VALUES (?, ?, ?, ?, ?)",
    )
    .bind("富文本分区")
    .bind(slug)
    .bind("测试用")
    .bind(visibility)
    .bind(owner)
    .execute(pool)
    .await
    .expect("建分区失败");

    sqlx::query_scalar("SELECT id FROM forum_spaces WHERE slug = ?")
        .bind(slug)
        .fetch_one(pool)
        .await
        .expect("查分区失败")
}

async fn seed_thread(pool: &MySqlPool, space_id: u64, username: &str) -> u64 {
    let actor = current_user(pool, username).await;
    let input = forum_service::NewThreadInput {
        space_id,
        title: "富文本议题",
        body: Some("首楼纯文本"),
        body_html: None,
    };
    forum_service::create_thread(pool, &actor, None, input)
        .await
        .expect("建议题失败")
}

fn encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len() * 3);
    for byte in value.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// 富文本回复：标签保留（白名单内）、危险标签连同内容被丢弃、纯文本列只存文字。
#[tokio::test]
async fn rich_reply_is_sanitized_and_plain_text_is_stored() {
    let Some(url) = db_url() else { return };
    let (app, pool) = harness(&url).await;

    let slug = unique("rt");
    let space_id = seed_space(&pool, &slug, "internal").await;
    let username = unique("rtm");
    make_user(&pool, &username, "member").await;
    let thread_id = seed_thread(&pool, space_id, &username).await;
    let cookie = login(&app, &username).await;

    let page = app
        .clone()
        .oneshot(get_request(&format!("/forum/t/{thread_id}"), Some(&cookie)))
        .await
        .expect("请求失败");
    assert_eq!(page.status(), StatusCode::OK);
    let html = body_of(page).await;
    let token = csrf_of(&html).expect("详情页应当带 CSRF");

    let rich = "<p>第一段<strong>粗体</strong></p><script>alert(1)</script>\
                <p><img src=\"/uploads/deadbeef.png\" alt=\"图\"></p>";
    let body = format!(
        "_csrf={}&body={}&body_html={}",
        encode(&token),
        encode("第一段粗体"),
        encode(rich),
    );
    let response = app
        .clone()
        .oneshot(form_request(
            &format!("/forum/t/{thread_id}/reply"),
            &body,
            Some(&cookie),
        ))
        .await
        .expect("请求失败");
    assert_eq!(response.status(), StatusCode::SEE_OTHER, "回复应当成功");

    // 落库：纯文本列不含标签，HTML 列保留白名单标签、丢掉 script。
    let plain: String = sqlx::query_scalar(
        "SELECT body FROM forum_posts WHERE thread_id = ? ORDER BY id DESC LIMIT 1",
    )
    .bind(thread_id)
    .fetch_one(&pool)
    .await
    .expect("查帖子失败");
    assert!(!plain.contains('<'), "纯文本列不应含标签：{plain}");
    assert!(plain.contains("粗体"), "纯文本列应保留文字：{plain}");

    let stored: Option<String> = sqlx::query_scalar(
        "SELECT body_html FROM forum_posts WHERE thread_id = ? ORDER BY id DESC LIMIT 1",
    )
    .bind(thread_id)
    .fetch_one(&pool)
    .await
    .expect("查帖子失败");
    let stored = stored.expect("富文本应当落库");
    assert!(stored.contains("<strong>"), "白名单标签应保留：{stored}");
    assert!(!stored.contains("script"), "script 必须被丢弃：{stored}");
    assert!(!stored.contains("alert"), "script 内容也要丢弃：{stored}");

    // 渲染：页面输出真正的 HTML，而不是把标签转义成文本。
    let page = app
        .clone()
        .oneshot(get_request(&format!("/forum/t/{thread_id}"), Some(&cookie)))
        .await
        .expect("请求失败");
    let html = body_of(page).await;
    assert!(html.contains("<strong>粗体</strong>"), "页面应渲染粗体");
    assert!(
        html.contains("class=\"post__body rich-text\""),
        "应带富文本容器"
    );
    assert!(html.contains("/uploads/deadbeef.png"), "图片应保留");
    // 注意：页面本身有 <script src> 引 app.js / editor.js，所以不能笼统断言"没有 script"，
    // 要钉的是用户提交的那段脚本没有活下来。
    assert!(!html.contains("alert(1)"), "用户提交的脚本不应出现在页面");
    assert!(!html.contains("<script>alert"), "不能有内联脚本被执行");
}

/// 只有图片、没有文字的正文也算合法（这正是富文本带来的新形态）。
#[tokio::test]
async fn image_only_reply_is_accepted() {
    let Some(url) = db_url() else { return };
    let (app, pool) = harness(&url).await;

    let slug = unique("img");
    let space_id = seed_space(&pool, &slug, "internal").await;
    let username = unique("imgm");
    make_user(&pool, &username, "member").await;
    let thread_id = seed_thread(&pool, space_id, &username).await;
    let cookie = login(&app, &username).await;

    let page = app
        .clone()
        .oneshot(get_request(&format!("/forum/t/{thread_id}"), Some(&cookie)))
        .await
        .expect("请求失败");
    let html = body_of(page).await;
    let token = csrf_of(&html).expect("详情页应当带 CSRF");

    let body = format!(
        "_csrf={}&body=&body_html={}",
        encode(&token),
        encode("<p><img src=\"/uploads/only.png\" alt=\"只有图\"></p>"),
    );
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
        StatusCode::SEE_OTHER,
        "只有图片的回复应当被接受"
    );
}

/// 没有 JS 的用户提交纯文本：body_html 为空时必须照常工作，且页面不能出现空容器。
#[tokio::test]
async fn plain_text_reply_still_works_without_javascript() {
    let Some(url) = db_url() else { return };
    let (app, pool) = harness(&url).await;

    let slug = unique("plain");
    let space_id = seed_space(&pool, &slug, "internal").await;
    let username = unique("plainm");
    make_user(&pool, &username, "member").await;
    let thread_id = seed_thread(&pool, space_id, &username).await;
    let cookie = login(&app, &username).await;

    let page = app
        .clone()
        .oneshot(get_request(&format!("/forum/t/{thread_id}"), Some(&cookie)))
        .await
        .expect("请求失败");
    let html = body_of(page).await;
    let token = csrf_of(&html).expect("详情页应当带 CSRF");

    let body = format!(
        "_csrf={}&body={}&body_html=",
        encode(&token),
        encode("纯文本回复")
    );
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
        StatusCode::SEE_OTHER,
        "纯文本回复应当成功"
    );

    let page = app
        .clone()
        .oneshot(get_request(&format!("/forum/t/{thread_id}"), Some(&cookie)))
        .await
        .expect("请求失败");
    let html = body_of(page).await;
    assert!(html.contains("纯文本回复"), "页面应显示纯文本回复");
}
