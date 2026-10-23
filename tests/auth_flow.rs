//! 身份流程的端到端集成测试：注册、登录、登出、会话失效。
//!
//! 这些测试走的是**线上同一条路由**（`nexus::app::build_router`），不是测试专用装配。
//! 手工 curl 验过的那些结论（303、403、统一提示、停用即失效）必须由机器守住，
//! 否则下次谁改一个 layer 顺序，就只能靠人肉复测。
//!
//! 需要 `TEST_DATABASE_URL`；没设时全部打印跳过并返回——没库的机器上 `cargo test` 仍然绿，
//! 但"绿"不代表这些断言跑过，DESIGN §16 已经写明。

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};

use axum::Router;
use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt;
use nexus::app::build_router;
use nexus::auth::service::{self, BootstrapOutcome, RegisterInput};
use nexus::config::Config;
use nexus::shared::audit::AuditCtx;
use nexus::state::AppState;
use sqlx::MySqlPool;
use tower::ServiceExt;

/// 每个测试用独立的用户名，避免并行执行时互相踩。
fn unique(prefix: &str) -> String {
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("it_{prefix}_{nanos:x}_{n}")
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
        // 测试走 http，Secure cookie 不会被回传。
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
        // 对端地址：不经过真 TCP 时 `ConnectInfo` 也得有值，否则 handler 提取失败。
        .extension(ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 45_678))))
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

fn get_request(uri: &str, cookie: Option<&str>) -> Request<Body> {
    let mut builder = request("GET", uri);
    if let Some(cookie) = cookie {
        builder = builder.header(header::COOKIE, cookie);
    }
    builder.body(Body::empty()).expect("构造请求失败")
}

/// 从 `set-cookie` 里取出 `name=value`，丢掉属性——回传时只该带这一截。
fn cookie_of(response: &axum::http::Response<axum::body::Body>) -> Option<String> {
    let raw = response.headers().get(header::SET_COOKIE)?.to_str().ok()?;
    let pair = raw.split(';').next()?.trim();
    if pair.is_empty() {
        None
    } else {
        Some(pair.to_string())
    }
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

/// 从页面里抠出 CSRF token。真实浏览器是读隐藏字段，测试也读同一个地方，
/// 这样"模板没渲染 token"会被测试直接抓住，而不是靠人眼看页面。
fn csrf_of(html: &str) -> Option<String> {
    const MARK: &str = "name=\"_csrf\" value=\"";
    let start = html.find(MARK)? + MARK.len();
    let rest = &html[start..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

/// 拿一个"已登录"的会话：GET 表单页 → 抠 token → POST。
async fn page_token(app: &Router, uri: &str) -> (String, String) {
    let response = app
        .clone()
        .oneshot(get_request(uri, None))
        .await
        .expect("请求失败");
    assert_eq!(response.status(), StatusCode::OK, "{uri} 应可访问");
    let cookie =
        cookie_of(&response).expect("页面必须下发会话 cookie（游客也要有，CSRF 存在会话里）");
    let html = body_of(response).await;
    let token = csrf_of(&html).unwrap_or_else(|| panic!("{uri} 页面里没有 CSRF 字段：{html}"));
    (cookie, token)
}

async fn cleanup(pool: &MySqlPool, prefix: &str) {
    let like = format!("{prefix}%");
    let _ = sqlx::query("DELETE FROM users WHERE username LIKE ?")
        .bind(like)
        .execute(pool)
        .await;
}

#[tokio::test]
async fn register_creates_a_regular_active_user_and_redirects_to_login() {
    let Some(url) = db_url() else { return };
    let (app, pool) = harness(&url).await;

    let username = unique("reg");
    let (cookie, token) = page_token(&app, "/register").await;

    let body = format!(
        "_csrf={token}&username={username}&email={username}%40example.com&password=a-long-password&display_name=%E6%B5%8B%E8%AF%95"
    );
    let response = app
        .clone()
        .oneshot(form_request("/register", &body, Some(&cookie)))
        .await
        .expect("请求失败");

    assert_eq!(response.status(), StatusCode::SEE_OTHER, "注册成功应是 303");
    let location = response
        .headers()
        .get(header::LOCATION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    assert!(
        location.starts_with("/login"),
        "注册后应跳登录页而不是自动登录：{location}"
    );

    // 库里的状态才是事实：自助注册必须是 user + active。
    let (role, status): (String, String) =
        sqlx::query_as("SELECT role, status FROM users WHERE username = ?")
            .bind(&username)
            .fetch_one(&pool)
            .await
            .expect("注册后库里必须有这行");
    assert_eq!(role, "user", "自助注册不能直接给 member/admin");
    assert_eq!(status, "active");

    cleanup(&pool, &username).await;
}

#[tokio::test]
async fn register_without_csrf_token_is_forbidden() {
    let Some(url) = db_url() else { return };
    let (app, pool) = harness(&url).await;

    let username = unique("nocsrf");
    let (cookie, _token) = page_token(&app, "/register").await;

    // 故意带一个错的 token：真实攻击场景里攻击者拿不到会话里的那个值。
    let body = format!(
        "_csrf=deadbeef&username={username}&email={username}%40example.com&password=a-long-password"
    );
    let response = app
        .clone()
        .oneshot(form_request("/register", &body, Some(&cookie)))
        .await
        .expect("请求失败");

    assert_eq!(response.status(), StatusCode::FORBIDDEN);

    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM users WHERE username = ?")
        .bind(&username)
        .fetch_one(&pool)
        .await
        .expect("查询失败");
    assert_eq!(count, 0, "被 CSRF 挡下的请求不能留下任何用户行");
}

#[tokio::test]
async fn login_failure_and_unknown_user_share_one_message() {
    let Some(url) = db_url() else { return };
    let (app, pool) = harness(&url).await;

    let username = unique("msg");
    service::register(
        &pool,
        AuditCtx::anonymous(None),
        RegisterInput {
            username: &username,
            email: &format!("{username}@example.com"),
            password: "a-long-password",
            display_name: None,
        },
    )
    .await
    .expect("准备用户失败");

    // 密码错
    let (cookie, token) = page_token(&app, "/login").await;
    let wrong = format!("_csrf={token}&username={username}&password=wrong-password");
    let response = app
        .clone()
        .oneshot(form_request("/login", &wrong, Some(&cookie)))
        .await
        .expect("请求失败");
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "失败应回表单页而不是错误页"
    );
    let wrong_html = body_of(response).await;

    // 用户不存在
    let (cookie2, token2) = page_token(&app, "/login").await;
    let ghost = format!(
        "_csrf={token2}&username={}_ghost&password=wrong-password",
        unique("ghost")
    );
    let response = app
        .clone()
        .oneshot(form_request("/login", &ghost, Some(&cookie2)))
        .await
        .expect("请求失败");
    let ghost_html = body_of(response).await;

    assert!(
        wrong_html.contains("用户名或密码不正确"),
        "密码错时提示不对"
    );
    assert!(
        ghost_html.contains("用户名或密码不正确"),
        "用户不存在时提示必须一模一样"
    );
    assert!(!ghost_html.contains("不存在"), "不能透露账号是否存在");

    cleanup(&pool, &username).await;
}

#[tokio::test]
async fn login_sets_a_session_and_logout_destroys_it() {
    let Some(url) = db_url() else { return };
    let (app, pool) = harness(&url).await;

    let username = unique("flow");
    let password = "a-long-password";
    service::register(
        &pool,
        AuditCtx::anonymous(None),
        RegisterInput {
            username: &username,
            email: &format!("{username}@example.com"),
            password,
            display_name: None,
        },
    )
    .await
    .expect("准备用户失败");

    let (cookie, token) = page_token(&app, "/login").await;
    let body = format!("_csrf={token}&username={username}&password={password}");
    let response = app
        .clone()
        .oneshot(form_request("/login", &body, Some(&cookie)))
        .await
        .expect("请求失败");
    assert_eq!(response.status(), StatusCode::SEE_OTHER, "登录成功应 303");

    let session = cookie_of(&response).expect("登录成功必须下发会话 cookie");
    assert!(
        session.contains("HttpOnly") || !session.is_empty(),
        "会话 cookie 必须存在"
    );
    // cookie 属性单独从响应头里核，避免把属性回传成 cookie 值。
    let raw = response
        .headers()
        .get(header::SET_COOKIE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    assert!(raw.contains("HttpOnly"), "会话 cookie 必须 HttpOnly：{raw}");

    // 带着会话访问首页，应是登录态。
    let home = app
        .clone()
        .oneshot(get_request("/", Some(&session)))
        .await
        .expect("请求失败");
    let html = body_of(home).await;
    assert!(html.contains(&username), "登录后首页应显示身份：{html}");
    assert!(
        html.contains("action=\"/logout\""),
        "登录后导航应有退出表单"
    );

    // 登出：拿首页的 token。
    let token = csrf_of(&html).expect("登录态页面必须有 CSRF 字段");
    let response = app
        .clone()
        .oneshot(form_request(
            "/logout",
            &format!("_csrf={token}"),
            Some(&session),
        ))
        .await
        .expect("请求失败");
    assert_eq!(response.status(), StatusCode::SEE_OTHER);

    // 用同一个 cookie 再访问：必须已经是游客。
    let after = app
        .clone()
        .oneshot(get_request("/", Some(&session)))
        .await
        .expect("请求失败");
    let html = body_of(after).await;
    assert!(
        !html.contains("action=\"/logout\""),
        "登出后旧 cookie 不能还有效"
    );

    cleanup(&pool, &username).await;
}

#[tokio::test]
async fn suspending_a_user_kills_an_existing_session_on_the_next_request() {
    let Some(url) = db_url() else { return };
    let (app, pool) = harness(&url).await;

    let username = unique("susp");
    let password = "a-long-password";
    service::register(
        &pool,
        AuditCtx::anonymous(None),
        RegisterInput {
            username: &username,
            email: &format!("{username}@example.com"),
            password,
            display_name: None,
        },
    )
    .await
    .expect("准备用户失败");

    let (cookie, token) = page_token(&app, "/login").await;
    let body = format!("_csrf={token}&username={username}&password={password}");
    let response = app
        .clone()
        .oneshot(form_request("/login", &body, Some(&cookie)))
        .await
        .expect("请求失败");
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    let session = cookie_of(&response).expect("必须有会话 cookie");

    // 管理员在另一边把账号停用。
    sqlx::query("UPDATE users SET status = 'suspended' WHERE username = ?")
        .bind(&username)
        .execute(&pool)
        .await
        .expect("停用失败");

    // 旧 cookie 的下一次请求就必须是游客——这条断言是"每请求查库"的唯一硬证据。
    let response = app
        .clone()
        .oneshot(get_request("/", Some(&session)))
        .await
        .expect("请求失败");
    let html = body_of(response).await;
    assert!(
        !html.contains("action=\"/logout\""),
        "账号被停用后，旧会话必须立刻失效"
    );

    cleanup(&pool, &username).await;
}

#[tokio::test]
async fn bootstrap_admin_is_created_at_most_once() {
    let Some(url) = db_url() else { return };
    let (_app, pool) = harness(&url).await;

    if nexus::auth::model::has_any_admin(&pool)
        .await
        .expect("查询管理员失败")
    {
        eprintln!("跳过：测试库里已经有管理员，无法验证引导逻辑");
        return;
    }

    let username = unique("boot");
    let first = service::ensure_bootstrap_admin(&pool, &username, "a-long-enough-password")
        .await
        .expect("首次引导应成功");
    assert!(matches!(first, BootstrapOutcome::Created { .. }));

    let role: String = sqlx::query_scalar("SELECT role FROM users WHERE username = ?")
        .bind(&username)
        .fetch_one(&pool)
        .await
        .expect("引导出的账号必须存在");
    assert_eq!(role, "admin");

    // 第二次必须什么都不做——否则能改环境变量的人随时能种一个新管理员。
    let again = service::ensure_bootstrap_admin(&pool, &unique("boot2"), "a-long-enough-password")
        .await
        .expect("第二次引导不应报错");
    assert_eq!(again, BootstrapOutcome::Skipped);

    cleanup(&pool, "it_boot").await;
}
