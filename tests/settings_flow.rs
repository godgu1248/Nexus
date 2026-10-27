//! 界面设置（主题）的集成测试。
//!
//! 覆盖四件事：游客被带去登录、色卡齐全且标出当前项、保存后落库并回显、
//! 非法主题名回 422 且什么都不改。

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
use nexus::state::AppState;
use sqlx::MySqlPool;
use tower::ServiceExt;

fn unique(prefix: &str) -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or_default();
    format!("st_{prefix}_{nanos:x}_{n}")
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
        .extension(ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 45_679))))
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

fn location_of(response: &axum::http::Response<axum::body::Body>) -> String {
    response
        .headers()
        .get(header::LOCATION)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_owned()
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
    let response = app
        .clone()
        .oneshot(get_request("/login", None))
        .await
        .expect("请求失败");
    let cookie = cookie_of(&response).expect("登录页应带会话 cookie");
    let html = body_of(response).await;
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

async fn theme_in_db(pool: &MySqlPool, user_id: u64) -> String {
    sqlx::query_scalar("SELECT theme FROM users WHERE id = ?")
        .bind(user_id)
        .fetch_one(pool)
        .await
        .expect("读取主题失败")
}

async fn require_db() -> Option<(Router, MySqlPool)> {
    let url = db_url()?;
    Some(harness(&url).await)
}

#[tokio::test]
async fn guests_are_sent_to_login_with_a_next_parameter() {
    let Some((app, _pool)) = require_db().await else {
        println!("跳过：没有设置 TEST_DATABASE_URL");
        return;
    };

    let response = app
        .oneshot(get_request("/settings", None))
        .await
        .expect("请求失败");
    assert_eq!(response.status(), StatusCode::SEE_OTHER, "游客应被带去登录");
    let location = location_of(&response);
    assert!(
        location.starts_with("/login"),
        "应去登录页，实际 {location}"
    );
    assert!(
        location.contains("next=%2Fsettings") || location.contains("next=/settings"),
        "登录后要能回到设置页，实际 {location}"
    );
}

#[tokio::test]
async fn every_theme_is_listed_and_the_current_one_is_marked() {
    let Some((app, pool)) = require_db().await else {
        println!("跳过：没有设置 TEST_DATABASE_URL");
        return;
    };

    let name = unique("list");
    make_user(&pool, &name, "member").await;
    let cookie = login(&app, &name).await;

    let response = app
        .clone()
        .oneshot(get_request("/settings", Some(&cookie)))
        .await
        .expect("请求失败");
    assert_eq!(response.status(), StatusCode::OK, "登录后应能打开设置页");
    let html = body_of(response).await;

    // 主题清单以目录为准：写死数字会变成"加一套配色就得改一次测试"，
    // 而这条用例真正要守的是"目录里的每一套都渲染出来，不多也不少"。
    for choice in nexus::shared::appearance::THEMES {
        let key = choice.key;
        assert!(
            html.contains(&format!("name=\"theme\" value=\"{key}\"")),
            "色卡缺少主题 {key}"
        );
    }
    assert_eq!(
        html.matches("name=\"theme\" value=\"").count(),
        nexus::shared::appearance::THEMES.len(),
        "色卡数量应与目录一致"
    );
    // 默认 ink，必须标成当前项且已勾选。
    assert!(html.contains("theme-ink"), "body 应带当前主题 class");
    assert!(html.contains("is-selected"), "当前主题的卡片要有选中态");
    // 按钮在最后：保存按钮必须出现在色卡区之后。
    let grid_end = html.find("</fieldset>").expect("应有 fieldset");
    let save_at = html.find("保存外观").expect("应有保存按钮");
    assert!(save_at > grid_end, "保存按钮要放在色卡下面");
}

#[tokio::test]
async fn saving_a_theme_persists_it_and_reports_back() {
    let Some((app, pool)) = require_db().await else {
        println!("跳过：没有设置 TEST_DATABASE_URL");
        return;
    };

    let name = unique("save");
    let user_id = make_user(&pool, &name, "member").await;
    let cookie = login(&app, &name).await;

    let page = app
        .clone()
        .oneshot(get_request("/settings", Some(&cookie)))
        .await
        .expect("请求失败");
    let html = body_of(page).await;
    let csrf = csrf_of(&html).expect("设置页应有 CSRF token");

    let response = app
        .clone()
        .oneshot(form_request(
            "/settings",
            &format!("theme=ocean&_csrf={csrf}"),
            Some(&cookie),
        ))
        .await
        .expect("请求失败");
    assert_eq!(response.status(), StatusCode::SEE_OTHER, "保存应重定向");
    assert_eq!(
        location_of(&response),
        "/settings?saved=1",
        "保存后应带提示参数"
    );
    assert_eq!(theme_in_db(&pool, user_id).await, "ocean", "主题应落库");

    let response = app
        .oneshot(get_request("/settings?saved=1", Some(&cookie)))
        .await
        .expect("请求失败");
    let html = body_of(response).await;
    assert!(html.contains("notice--ok"), "保存后应有成功提示");
    assert!(html.contains("theme-ocean"), "body 应换成新主题");
    assert!(
        html.contains("theme-ocean") && html.contains("is-selected"),
        "新主题的卡片应标成当前项"
    );
}

#[tokio::test]
async fn an_unknown_theme_is_rejected_and_changes_nothing() {
    let Some((app, pool)) = require_db().await else {
        println!("跳过：没有设置 TEST_DATABASE_URL");
        return;
    };

    let name = unique("bad");
    let user_id = make_user(&pool, &name, "member").await;
    let cookie = login(&app, &name).await;

    let page = app
        .clone()
        .oneshot(get_request("/settings", Some(&cookie)))
        .await
        .expect("请求失败");
    let html = body_of(page).await;
    let csrf = csrf_of(&html).expect("设置页应有 CSRF token");

    let response = app
        .oneshot(form_request(
            "/settings",
            &format!("theme=plasma&_csrf={csrf}"),
            Some(&cookie),
        ))
        .await
        .expect("请求失败");
    assert_eq!(
        response.status(),
        StatusCode::UNPROCESSABLE_ENTITY,
        "未知主题应当被拒"
    );
    let html = body_of(response).await;
    assert!(html.contains("取值不认识"), "应说明取值不认识");
    assert_eq!(theme_in_db(&pool, user_id).await, "ink", "被拒后主题不能变");
}

async fn appearance_in_db(pool: &MySqlPool, user_id: u64) -> (String, String, String) {
    sqlx::query_as::<_, (String, String, String)>(
        "SELECT theme, background, font FROM users WHERE id = ?",
    )
    .bind(user_id)
    .fetch_one(pool)
    .await
    .expect("查外观失败")
}

#[tokio::test]
async fn saving_backdrop_and_font_persists_all_three() {
    let Some((app, pool)) = require_db().await else {
        println!("跳过：没有设置 TEST_DATABASE_URL");
        return;
    };

    let name = unique("look");
    let user_id = make_user(&pool, &name, "member").await;
    let cookie = login(&app, &name).await;

    let page = app
        .clone()
        .oneshot(get_request("/settings", Some(&cookie)))
        .await
        .expect("请求失败");
    let html = body_of(page).await;
    let csrf = csrf_of(&html).expect("设置页应有 CSRF token");

    // 三组一起提交：这正是表单的真实行为。
    let response = app
        .clone()
        .oneshot(form_request(
            "/settings",
            &format!("theme=forest&background=aurora&font=serif&_csrf={csrf}"),
            Some(&cookie),
        ))
        .await
        .expect("请求失败");
    assert_eq!(response.status(), StatusCode::SEE_OTHER, "保存应重定向");

    assert_eq!(
        appearance_in_db(&pool, user_id).await,
        ("forest".to_owned(), "aurora".to_owned(), "serif".to_owned()),
        "三列都应落库"
    );

    let response = app
        .oneshot(get_request("/settings", Some(&cookie)))
        .await
        .expect("请求失败");
    let html = body_of(response).await;
    assert!(
        html.contains("class=\"theme-forest bg-aurora font-serif\""),
        "body 应带三组 class"
    );
    assert!(html.contains("bg-preview--aurora"), "背板卡要有预览块");
    assert!(html.contains("font-preview--serif"), "字体卡要有预览块");
}

#[tokio::test]
async fn an_unknown_backdrop_is_rejected_and_changes_nothing() {
    let Some((app, pool)) = require_db().await else {
        println!("跳过：没有设置 TEST_DATABASE_URL");
        return;
    };

    let name = unique("badbg");
    let user_id = make_user(&pool, &name, "member").await;
    let cookie = login(&app, &name).await;

    let page = app
        .clone()
        .oneshot(get_request("/settings", Some(&cookie)))
        .await
        .expect("请求失败");
    let html = body_of(page).await;
    let csrf = csrf_of(&html).expect("设置页应有 CSRF token");

    let response = app
        .oneshot(form_request(
            "/settings",
            &format!("theme=ink&background=nebula&font=sans&_csrf={csrf}"),
            Some(&cookie),
        ))
        .await
        .expect("请求失败");
    assert_eq!(
        response.status(),
        StatusCode::UNPROCESSABLE_ENTITY,
        "未知背板应当被拒"
    );
    let html = body_of(response).await;
    assert!(html.contains("背板"), "错误提示要点名背板");
    assert_eq!(
        appearance_in_db(&pool, user_id).await,
        ("ink".to_owned(), "plain".to_owned(), "sans".to_owned()),
        "被拒后三列都不能变"
    );
}

#[tokio::test]
async fn nav_highlights_the_current_section_only() {
    let Some((app, pool)) = require_db().await else {
        println!("跳过：没有设置 TEST_DATABASE_URL");
        return;
    };

    let name = unique("nav");
    make_user(&pool, &name, "member").await;
    let cookie = login(&app, &name).await;

    // 论坛页：论坛是当前项，内厅不是。
    let html = body_of(
        app.clone()
            .oneshot(get_request("/forum", Some(&cookie)))
            .await
            .expect("请求失败"),
    )
    .await;
    assert!(
        html.contains("class=\"nav-link is-current\" href=\"/forum\""),
        "论坛应是当前项"
    );
    assert!(
        !html.contains("class=\"nav-link is-current\" href=\"/\""),
        "在内厅之外，内厅不能是当前项"
    );

    // 档案页：换成档案当前。
    let html = body_of(
        app.clone()
            .oneshot(get_request("/kb", Some(&cookie)))
            .await
            .expect("请求失败"),
    )
    .await;
    assert!(
        html.contains("class=\"nav-link is-current\" href=\"/kb\""),
        "档案应是当前项"
    );

    // 设置页：设置当前。
    let html = body_of(
        app.oneshot(get_request("/settings", Some(&cookie)))
            .await
            .expect("请求失败"),
    )
    .await;
    assert!(
        html.contains("class=\"nav-link is-current\" href=\"/settings\""),
        "设置应是当前项"
    );
}
