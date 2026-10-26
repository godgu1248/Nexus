//! 写入口限速的集成测试（DESIGN §39）。
//!
//! 这一批用例只验一件事：**四处写入口真的会挡住脚本刷量**，而且挡的时候：
//! 1. 返回 429 而不是 500/403（前者是"稍后再试"，后者会被当成"我没权限"）；
//! 2. 带 `Retry-After`，客户端知道要等多久；
//! 3. 键是"谁/IP"而不是全局——一个人的额度用完不能连累别人。
//!
//! 为了让"打三次就撞墙"，每个用例都用 `Rules::uniform(2)` 建 router：
//! 生产额度是给人用的（注册 20/小时），拿生产值跑测试既要打二十次注册，
//! 又会让断言依赖 argon2 的速度。

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};

use axum::Router;
use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt;
use nexus::app::build_router;
use nexus::auth::service as auth_service;
use nexus::config::Config;
use nexus::shared::ratelimit::Rules;
use nexus::shared::visibility::Visibility;
use nexus::state::AppState;
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
    format!("rl_{prefix}_{nanos:x}_{n}")
}

fn config_for(database_url: &str) -> Config {
    // 上传目录指到系统临时目录：默认的 `./uploads` 会在仓库里堆测试图片。
    let uploads = std::env::temp_dir().join("nexus-test-uploads");
    Config::from_source(|key| match key {
        "DATABASE_URL" => Some(database_url.to_owned()),
        "APP_BASE_URL" => Some("http://127.0.0.1:8080".to_owned()),
        "APP_ENV" => Some("dev".to_owned()),
        "UPLOAD_DIR" => Some(uploads.display().to_string()),
        // 让 X-Real-IP 生效，才能在一个进程里模拟"两个不同来源 IP"。
        "TRUST_PROXY" => Some("true".to_owned()),
        _ => None,
    })
    .expect("测试配置应当合法")
}

async fn harness(url: &str, rules: Rules) -> (Router, MySqlPool) {
    let config = config_for(url);
    let pool = nexus::db::init_pool(&config).await.expect("测试库连接失败");
    let state = AppState::new(pool.clone(), config).with_rate_rules(rules);
    (build_router(state), pool)
}

fn request(method: &str, uri: &str) -> axum::http::request::Builder {
    Request::builder()
        .method(method)
        .uri(uri)
        .extension(ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 45_678))))
}

fn get_request(uri: &str, cookie: Option<&str>, ip: Option<&str>) -> Request<Body> {
    let mut builder = request("GET", uri);
    if let Some(value) = cookie {
        builder = builder.header(header::COOKIE, value);
    }
    if let Some(value) = ip {
        builder = builder.header("x-real-ip", value);
    }
    builder.body(Body::empty()).expect("请求构造失败")
}

fn form_request(uri: &str, body: &str, cookie: Option<&str>, ip: Option<&str>) -> Request<Body> {
    let mut builder =
        request("POST", uri).header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
    if let Some(value) = cookie {
        builder = builder.header(header::COOKIE, value);
    }
    if let Some(value) = ip {
        builder = builder.header("x-real-ip", value);
    }
    builder
        .body(Body::from(body.to_owned()))
        .expect("请求构造失败")
}

fn multipart_request(uri: &str, bytes: &[u8], cookie: &str, csrf: &str) -> Request<Body> {
    const BOUNDARY: &str = "----nexustestboundary";
    let mut body = Vec::new();
    body.extend_from_slice(
        format!(
            "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"a.png\"\r\n\
             Content-Type: image/png\r\n\r\n"
        )
        .as_bytes(),
    );
    body.extend_from_slice(bytes);
    body.extend_from_slice(format!("\r\n--{BOUNDARY}--\r\n").as_bytes());

    request("POST", uri)
        .header(
            header::CONTENT_TYPE,
            format!("multipart/form-data; boundary={BOUNDARY}"),
        )
        .header(header::COOKIE, cookie)
        .header("x-csrf-token", csrf)
        .body(Body::from(body))
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

fn retry_after_of(response: &axum::http::Response<axum::body::Body>) -> Option<u32> {
    response
        .headers()
        .get(header::RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse().ok())
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

async fn page_token(
    app: &Router,
    uri: &str,
    cookie: Option<&str>,
    ip: Option<&str>,
) -> (String, String) {
    let response = app
        .clone()
        .oneshot(get_request(uri, cookie, ip))
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
    let (html, cookie) = page_token(app, "/login", None, None).await;
    let csrf = csrf_of(&html).expect("登录页应有 CSRF token");
    let body = format!("username={username}&password=a-long-password&_csrf={csrf}");
    let response = app
        .clone()
        .oneshot(form_request("/login", &body, Some(&cookie), None))
        .await
        .expect("请求失败");
    assert_eq!(response.status(), StatusCode::SEE_OTHER, "登录应当成功");
    cookie_of(&response).unwrap_or(cookie)
}

/// 一次注册尝试：自己开一个会话（拿自己的 CSRF），再提交表单。
async fn register_once(app: &Router, ip: Option<&str>) -> axum::http::Response<Body> {
    let username = unique("reg");
    let (html, cookie) = page_token(app, "/register", None, ip).await;
    let csrf = csrf_of(&html).expect("注册页应有 CSRF token");
    let body = format!(
        "username={username}&email={username}%40example.test&password=a-long-password&_csrf={csrf}"
    );
    app.clone()
        .oneshot(form_request("/register", &body, Some(&cookie), ip))
        .await
        .expect("请求失败")
}

/// 一张 1×1 的 PNG。选它是因为只需要过"魔数判定"，不涉及解码。
const PNG_1PX: &[u8] = &[
    0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F, 0x15, 0xC4,
    0x89, 0x00, 0x00, 0x00, 0x0A, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9C, 0x63, 0x00, 0x01, 0x00, 0x00,
    0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE,
    0x42, 0x60, 0x82,
];

// ---------------------------------------------------------------------------
// 用例
// ---------------------------------------------------------------------------

#[tokio::test]
async fn registering_too_fast_returns_429_with_retry_after() {
    let Some(url) = std::env::var("TEST_DATABASE_URL")
        .ok()
        .filter(|v| !v.is_empty())
    else {
        println!("跳过：未设置 TEST_DATABASE_URL");
        return;
    };
    let (app, _pool) = harness(&url, Rules::uniform(2)).await;
    let ip = Some("203.0.113.10");

    assert_eq!(
        register_once(&app, ip).await.status(),
        StatusCode::SEE_OTHER,
        "第一次注册应当成功"
    );
    assert_eq!(
        register_once(&app, ip).await.status(),
        StatusCode::SEE_OTHER,
        "额度内的第二次注册应当成功"
    );

    let blocked = register_once(&app, ip).await;
    assert_eq!(
        blocked.status(),
        StatusCode::TOO_MANY_REQUESTS,
        "超出额度应当 429，而不是让 argon2 继续跑"
    );
    let retry = retry_after_of(&blocked).expect("429 必须带 Retry-After");
    assert!(
        (1..=3600).contains(&retry),
        "Retry-After 应当落在窗口内，实际 {retry}"
    );
    let html = body_of(blocked).await;
    assert!(
        html.contains("操作太频繁"),
        "错误页应当告诉用户稍后再试，实际：{html}"
    );
}

#[tokio::test]
async fn another_source_ip_keeps_its_own_quota() {
    let Some(url) = std::env::var("TEST_DATABASE_URL")
        .ok()
        .filter(|v| !v.is_empty())
    else {
        println!("跳过：未设置 TEST_DATABASE_URL");
        return;
    };
    let (app, _pool) = harness(&url, Rules::uniform(1)).await;

    assert_eq!(
        register_once(&app, Some("203.0.113.21")).await.status(),
        StatusCode::SEE_OTHER
    );
    assert_eq!(
        register_once(&app, Some("203.0.113.21")).await.status(),
        StatusCode::TOO_MANY_REQUESTS,
        "同一 IP 第二次应被拒"
    );
    assert_eq!(
        register_once(&app, Some("203.0.113.22")).await.status(),
        StatusCode::SEE_OTHER,
        "另一个 IP 不该被连累——键必须分得开"
    );
}

#[tokio::test]
async fn comments_are_limited_per_user_not_globally() {
    let Some(url) = std::env::var("TEST_DATABASE_URL")
        .ok()
        .filter(|v| !v.is_empty())
    else {
        println!("跳过：未设置 TEST_DATABASE_URL");
        return;
    };
    let (app, pool) = harness(&url, Rules::uniform(2)).await;

    let owner = make_user(&pool, &unique("own"), "member").await;
    let slug = unique("sp").replace('_', "-");
    let space_id = sqlx::query(
        "INSERT INTO forum_spaces (name, slug, description, sort_order, visibility, created_by) \
         VALUES ('限速分区', ?, '限速用例', 0, ?, ?)",
    )
    .bind(&slug)
    .bind(Visibility::Public.as_str())
    .bind(owner)
    .execute(&pool)
    .await
    .expect("插入分区失败")
    .last_insert_id();
    let thread_id = sqlx::query(
        "INSERT INTO forum_threads (space_id, author_id, title, visibility, last_post_at) \
         VALUES (?, ?, '限速议题', 'public', UTC_TIMESTAMP())",
    )
    .bind(space_id)
    .bind(owner)
    .execute(&pool)
    .await
    .expect("插入议题失败")
    .last_insert_id();

    let alice = unique("alice");
    let alice_id = make_user(&pool, &alice, "user").await;
    let _ = alice_id;
    let alice_cookie = login(&app, &alice).await;
    let uri = format!("/lobby/forum/{thread_id}");
    let (html, _) = page_token(&app, &uri, Some(&alice_cookie), None).await;
    let csrf = csrf_of(&html).expect("议题页应有 CSRF token");

    for attempt in 1..=2 {
        let body = format!("body=第{attempt}条&_csrf={csrf}");
        let response = app
            .clone()
            .oneshot(form_request(
                &format!("{uri}/comments"),
                &body,
                Some(&alice_cookie),
                None,
            ))
            .await
            .expect("请求失败");
        assert_eq!(
            response.status(),
            StatusCode::SEE_OTHER,
            "额度内第 {attempt} 条评论应当成功"
        );
    }

    let blocked = app
        .clone()
        .oneshot(form_request(
            &format!("{uri}/comments"),
            &format!("body=第3条&_csrf={csrf}"),
            Some(&alice_cookie),
            None,
        ))
        .await
        .expect("请求失败");
    assert_eq!(blocked.status(), StatusCode::TOO_MANY_REQUESTS);

    // 另一个人评论同一条议题必须照常成功。
    let bob = unique("bob");
    make_user(&pool, &bob, "user").await;
    let bob_cookie = login(&app, &bob).await;
    let (bob_html, _) = page_token(&app, &uri, Some(&bob_cookie), None).await;
    let bob_csrf = csrf_of(&bob_html).expect("议题页应有 CSRF token");
    let response = app
        .clone()
        .oneshot(form_request(
            &format!("{uri}/comments"),
            &format!("body=我还没用完&_csrf={bob_csrf}"),
            Some(&bob_cookie),
            None,
        ))
        .await
        .expect("请求失败");
    assert_eq!(
        response.status(),
        StatusCode::SEE_OTHER,
        "别人的额度不该被占用"
    );
}

#[tokio::test]
async fn uploads_are_limited_per_user() {
    let Some(url) = std::env::var("TEST_DATABASE_URL")
        .ok()
        .filter(|v| !v.is_empty())
    else {
        println!("跳过：未设置 TEST_DATABASE_URL");
        return;
    };
    let (app, pool) = harness(&url, Rules::uniform(2)).await;

    let username = unique("upl");
    make_user(&pool, &username, "member").await;
    let cookie = login(&app, &username).await;
    let (html, _) = page_token(&app, "/kb/new", Some(&cookie), None).await;
    let csrf = csrf_of(&html).expect("新建档案页应有 CSRF token");

    for attempt in 1..=2 {
        let response = app
            .clone()
            .oneshot(multipart_request("/media/upload", PNG_1PX, &cookie, &csrf))
            .await
            .expect("请求失败");
        assert_eq!(
            response.status(),
            StatusCode::OK,
            "额度内第 {attempt} 次上传应当成功"
        );
    }

    let blocked = app
        .clone()
        .oneshot(multipart_request("/media/upload", PNG_1PX, &cookie, &csrf))
        .await
        .expect("请求失败");
    assert_eq!(
        blocked.status(),
        StatusCode::TOO_MANY_REQUESTS,
        "上传也要限速，否则 4 MiB × 无限次能填满磁盘"
    );
    assert!(retry_after_of(&blocked).is_some(), "429 必须带 Retry-After");
}
