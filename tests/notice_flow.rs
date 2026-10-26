//! 公告的集成测试。
//!
//! 三件事必须钉住：
//! 1. 公告发出来之后，未确认的人页面上的红点真的出现；
//! 2. 点过「我知道了」之后红点消失，而且重复点是幂等的；
//! 3. 非 admin 发不了公告（403），不是只有界面上没按钮。

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
    format!("nt_{prefix}_{nanos:x}_{n}")
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
        .extension(ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 45_680))))
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
    let raw = response.headers().get(header::SET_COOKIE)?.to_str().ok()?;
    Some(raw.split(';').next()?.to_owned())
}

async fn body_of(response: axum::http::Response<axum::body::Body>) -> String {
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("读响应体失败")
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
        .expect("算哈希失败");
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
        .expect("打开登录页失败");
    let cookie = cookie_of(&response).expect("登录页应当发会话 cookie");
    let html = body_of(response).await;
    let token = csrf_of(&html).expect("登录页应当有 csrf");

    let body = format!("username={username}&password=a-long-password&_csrf={token}");
    let response = app
        .clone()
        .oneshot(form_request("/login", &body, Some(&cookie)))
        .await
        .expect("登录请求失败");
    assert_eq!(response.status(), StatusCode::SEE_OTHER, "登录失败");
    cookie_of(&response).unwrap_or(cookie)
}

async fn page_token(app: &Router, uri: &str, cookie: &str) -> (String, String) {
    let response = app
        .clone()
        .oneshot(get_request(uri, Some(cookie)))
        .await
        .expect("请求失败");
    let html = body_of(response).await;
    let token = csrf_of(&html).expect("页面应当带 csrf 元信息");
    (html, token)
}

#[tokio::test]
async fn red_dot_appears_until_the_member_acknowledges() {
    let Some(url) = db_url() else {
        println!("跳过：没有 TEST_DATABASE_URL");
        return;
    };
    let (app, pool) = harness(&url).await;
    let admin_name = unique("admin");
    make_user(&pool, &admin_name, "admin").await;
    let admin_cookie = login(&app, &admin_name).await;

    let member_name = unique("member");
    let member_id = make_user(&pool, &member_name, "member").await;
    let member_cookie = login(&app, &member_name).await;

    let title = unique("notice");
    let notice_id = publish_using(&app, &pool, &admin_cookie, &title, "all").await;

    // 未确认时红点必须在，且红点上的数字必须等于库里此刻的未读数。
    let dot = body_of(
        app.clone()
            .oneshot(get_request("/notice/dot", Some(&member_cookie)))
            .await
            .expect("取红点失败"),
    )
    .await;
    assert!(
        dot.contains("class=\"nav-dot\""),
        "未确认时红点必须出现，实际：{dot}"
    );
    let before = unread_count(&pool, member_id, true).await;
    let shown = rendered_unread(&dot);
    // 共享测试库里同文件的其它用例随时会发新公告，所以"红点数字"与"库里未读数"
    // 不存在严格相等的瞬间——两次查询之间就可能有新插入。
    // 这里钉的是"两者相符（允许并发带来的小幅漂移）"，精确的那条断言在下面：
    // 本用例发的这条公告，ack 之后在库里必须是已读。
    assert!(
        (shown - before).abs() <= 2,
        "红点上的数字应接近库里的未读数：渲染 {shown}，库里 {before}"
    );

    let (_, token) = page_token(&app, "/notice", &member_cookie).await;
    let body = format!("_csrf={token}");
    let response = app
        .clone()
        .oneshot(form_request(
            &format!("/notice/{notice_id}/ack"),
            &body,
            Some(&member_cookie),
        ))
        .await
        .expect("确认失败");
    assert_eq!(response.status(), StatusCode::SEE_OTHER);

    // 共享测试库里还有并发用例发的公告，所以不能断言"红点彻底消失"；
    // 能钉死的是：这条公告已记为已读、未读数少一条、红点数字与库里一致。
    let acked: i8 = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM announcement_acks WHERE announcement_id = ? AND user_id = ?)",
    )
    .bind(notice_id)
    .bind(member_id)
    .fetch_one(&pool)
    .await
    .expect("查确认记录失败");
    assert_eq!(acked, 1, "确认之后库里必须有这一行");

    let after = unread_count(&pool, member_id, true).await;
    let dot = body_of(
        app.clone()
            .oneshot(get_request("/notice/dot", Some(&member_cookie)))
            .await
            .expect("取红点失败"),
    )
    .await;
    let shown_after = rendered_unread(&dot);
    assert!(
        (shown_after - after).abs() <= 2,
        "确认一条之后，红点数字仍要与库里相符：渲染 {shown_after}，库里 {after}"
    );
    // 这里**不能**断言"确认之后红点数字变小"：公告是全局的，并行用例随时会发新公告，
    // 这个人的未读数完全可能从 3 涨到 5。曾经写过 `shown_after <= shown`，
    // 在满负载跑全量时就会红——那不是产品缺陷，是断言把"并发新增"当成了"确认无效"。
    // 能钉死且不依赖全局计数的是：这条公告已经不在他的未读集合里了。
    let still_unread: i8 = sqlx::query_scalar(
        "SELECT EXISTS( \
             SELECT 1 FROM announcements n \
              WHERE n.id = ? \
                AND n.is_active = 1 \
                AND n.deleted_at IS NULL \
                AND NOT EXISTS ( \
                    SELECT 1 FROM announcement_acks a \
                     WHERE a.announcement_id = n.id AND a.user_id = ? \
                ) \
         )",
    )
    .bind(notice_id)
    .bind(member_id)
    .fetch_one(&pool)
    .await
    .expect("查未读集合失败");
    assert_eq!(still_unread, 0, "确认过的公告不该再算作他的未读");

    // 把这个人剩下的公告也确认掉，红点必须彻底消失。
    // 轮次给到 6：并行用例每隔几毫秒就可能发一条新公告，ack 的速度要追得上它。
    let mut cleared = false;
    for _ in 0..6 {
        ack_everything(&pool, member_id).await;
        let dot = body_of(
            app.clone()
                .oneshot(get_request("/notice/dot", Some(&member_cookie)))
                .await
                .expect("取红点失败"),
        )
        .await;
        if !dot.contains("class=\"nav-dot\"") {
            cleared = true;
            break;
        }
    }
    assert!(cleared, "全部确认之后红点必须消失");
}

#[tokio::test]
async fn acknowledging_twice_is_idempotent() {
    let Some(url) = db_url() else {
        println!("跳过：没有 TEST_DATABASE_URL");
        return;
    };
    let (app, pool) = harness(&url).await;
    let admin_name = unique("admin2");
    make_user(&pool, &admin_name, "admin").await;
    let admin_cookie = login(&app, &admin_name).await;

    let member_name = unique("member2");
    let member_id = make_user(&pool, &member_name, "member").await;
    let member_cookie = login(&app, &member_name).await;

    let title = unique("idem");
    let notice_id = publish_using(&app, &pool, &admin_cookie, &title, "all").await;

    let (_, token) = page_token(&app, "/notice", &member_cookie).await;
    for _ in 0..2 {
        let body = format!("_csrf={token}");
        let response = app
            .clone()
            .oneshot(form_request(
                &format!("/notice/{notice_id}/ack"),
                &body,
                Some(&member_cookie),
            ))
            .await
            .expect("确认失败");
        assert_eq!(response.status(), StatusCode::SEE_OTHER, "重复确认也应 303");
    }

    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM announcement_acks WHERE announcement_id = ? AND user_id = ?",
    )
    .bind(notice_id)
    .bind(member_id)
    .fetch_one(&pool)
    .await
    .expect("查确认记录失败");
    assert_eq!(count, 1, "重复确认不能留下两行");
}

#[tokio::test]
async fn non_admin_cannot_publish() {
    let Some(url) = db_url() else {
        println!("跳过：没有 TEST_DATABASE_URL");
        return;
    };
    let (app, pool) = harness(&url).await;
    let name = unique("plain");
    make_user(&pool, &name, "user").await;
    let cookie = login(&app, &name).await;

    let (_, token) = page_token(&app, "/notice", &cookie).await;
    let body = format!("title=越权公告&body=x&_csrf={token}");
    let response = app
        .oneshot(form_request("/notice/new", &body, Some(&cookie)))
        .await
        .expect("请求失败");
    assert_eq!(
        response.status(),
        StatusCode::FORBIDDEN,
        "普通用户发公告必须 403"
    );
}

/// 用给定的池子发公告并返回 id（测试里不能靠全局池）。
///
/// `audience` 是 `all` 或 `team`：受众必须走表单提交而不是直接写库，
/// 否则"表单能不能提交受众"这件事根本没被验证过。
async fn publish_using(
    app: &Router,
    pool: &MySqlPool,
    cookie: &str,
    title: &str,
    audience: &str,
) -> u64 {
    let (_, token) = page_token(app, "/notice/new", cookie).await;
    let body = format!("title={title}&body=测试公告正文&audience={audience}&_csrf={token}");
    let response = app
        .clone()
        .oneshot(form_request("/notice/new", &body, Some(cookie)))
        .await
        .expect("发布公告失败");
    assert_eq!(response.status(), StatusCode::SEE_OTHER, "发布公告应 303");
    sqlx::query_scalar("SELECT id FROM announcements WHERE title = ? ORDER BY id DESC LIMIT 1")
        .bind(title)
        .fetch_one(pool)
        .await
        .expect("取公告 id 失败")
}

/// 从红点片段里抠出渲染的未读数（没有红点时是 0）。
fn rendered_unread(html: &str) -> i64 {
    let marker = " 条未读公告";
    match html.find(marker) {
        None => 0,
        Some(end) => {
            let head = &html[..end];
            let start = head.rfind('"').map(|i| i + 1).unwrap_or(0);
            head[start..].parse::<i64>().unwrap_or(-1)
        }
    }
}

/// 这个人在库里还有几条未确认的公告。
///
/// `sees_team` 必须和应用里的受众语义一致：来宾只数 `audience='all'` 的公告，
/// 团队成员数全部。助手和被测代码用两套口径，是这类断言最容易假红/假绿的地方。
async fn unread_count(pool: &MySqlPool, user_id: u64, sees_team: bool) -> i64 {
    sqlx::query_scalar(
        "SELECT COUNT(*) FROM announcements n \
          WHERE n.deleted_at IS NULL AND n.is_active = 1 \
            AND (? = 1 OR n.audience = 'all') \
            AND NOT EXISTS (SELECT 1 FROM announcement_acks a \
                             WHERE a.announcement_id = n.id AND a.user_id = ?)",
    )
    .bind(u8::from(sees_team))
    .bind(user_id)
    .fetch_one(pool)
    .await
    .expect("查未读数失败")
}

/// 把某人所有未确认的公告都写上确认行（模拟"都点过了"）。
async fn ack_everything(pool: &MySqlPool, user_id: u64) {
    sqlx::query(
        "INSERT IGNORE INTO announcement_acks (announcement_id, user_id, acked_at) \
         SELECT n.id, ?, UTC_TIMESTAMP() FROM announcements n \
          WHERE n.deleted_at IS NULL AND n.is_active = 1",
    )
    .bind(user_id)
    .execute(pool)
    .await
    .expect("批量确认失败");
}

/// 发一条「仅团队」公告：来宾从列表、横幅、红点三处都不该看到它，成员要看得到。
///
/// 这条用例是"屏幕里看得见"的证据，不是"库里字段对"的证据——字段对但模板忘了
/// 过滤，是这类功能最常见的一种坏法。
#[tokio::test]
async fn team_only_notice_is_hidden_from_guests_but_visible_to_members() {
    let Some(url) = db_url() else {
        println!("跳过：没有 TEST_DATABASE_URL");
        return;
    };
    let (app, pool) = harness(&url).await;

    let admin_name = unique("admin_team");
    make_user(&pool, &admin_name, "admin").await;
    let admin_cookie = login(&app, &admin_name).await;

    let guest_name = unique("guest_team");
    make_user(&pool, &guest_name, "user").await;
    let guest_cookie = login(&app, &guest_name).await;

    let member_name = unique("member_team");
    make_user(&pool, &member_name, "member").await;
    let member_cookie = login(&app, &member_name).await;

    let title = unique("teamonly");
    let notice_id = publish_using(&app, &pool, &admin_cookie, &title, "team").await;

    // 1) 库里的受众确实落成了 team（表单 → service → SQL 三段都通）。
    let audience: String = sqlx::query_scalar("SELECT audience FROM announcements WHERE id = ?")
        .bind(notice_id)
        .fetch_one(&pool)
        .await
        .expect("查受众失败");
    assert_eq!(audience, "team", "受众必须按表单提交的值落库");

    // 2) 来宾的公告页看不到标题。
    let (guest_html, _) = page_token(&app, "/notice", &guest_cookie).await;
    assert!(
        !guest_html.contains(&title),
        "来宾不该在公告列表里看到仅团队公告：{guest_html}"
    );

    // 3) 来宾的横幅片段也看不到。
    let guest_banner = body_of(
        app.clone()
            .oneshot(get_request("/notice/banner", Some(&guest_cookie)))
            .await
            .expect("取横幅失败"),
    )
    .await;
    assert!(
        !guest_banner.contains(&title),
        "来宾不该在横幅里看到仅团队公告：{guest_banner}"
    );

    // 4) 成员看得到，而且列表上标了「仅团队」。
    let (member_html, _) = page_token(&app, "/notice", &member_cookie).await;
    assert!(
        member_html.contains(&title),
        "成员必须看到仅团队公告：{member_html}"
    );
    assert!(
        member_html.contains("仅团队"),
        "仅团队公告在列表里必须带标记：{member_html}"
    );
}

/// 来宾不能借猜 id 去确认一条自己看不到的公告。
#[tokio::test]
async fn guests_cannot_acknowledge_a_team_only_notice() {
    let Some(url) = db_url() else {
        println!("跳过：没有 TEST_DATABASE_URL");
        return;
    };
    let (app, pool) = harness(&url).await;

    let admin_name = unique("admin_ack");
    make_user(&pool, &admin_name, "admin").await;
    let admin_cookie = login(&app, &admin_name).await;

    let guest_name = unique("guest_ack");
    let guest_id = make_user(&pool, &guest_name, "user").await;
    let guest_cookie = login(&app, &guest_name).await;

    let member_name = unique("member_ack");
    let member_id = make_user(&pool, &member_name, "member").await;
    let member_cookie = login(&app, &member_name).await;

    let title = unique("ackteam");
    let notice_id = publish_using(&app, &pool, &admin_cookie, &title, "team").await;

    // 来宾确认 → 404（不是 403：403 等于告诉他"这条存在，只是你不能看"）。
    let (_, guest_token) = page_token(&app, "/notice", &guest_cookie).await;
    let body = format!("_csrf={guest_token}");
    let response = app
        .clone()
        .oneshot(form_request(
            &format!("/notice/{notice_id}/ack"),
            &body,
            Some(&guest_cookie),
        ))
        .await
        .expect("确认请求失败");
    assert_eq!(
        response.status(),
        StatusCode::NOT_FOUND,
        "来宾确认仅团队公告必须 404"
    );

    let guest_rows: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM announcement_acks WHERE announcement_id = ? AND user_id = ?",
    )
    .bind(notice_id)
    .bind(guest_id)
    .fetch_one(&pool)
    .await
    .expect("查确认记录失败");
    assert_eq!(guest_rows, 0, "看不到的公告不能留下确认凭据");

    // 正向对照：同一条公告，成员确认成功（否则上面的 404 也可能只是"全都不让确认"）。
    let (_, member_token) = page_token(&app, "/notice", &member_cookie).await;
    let body = format!("_csrf={member_token}");
    let response = app
        .oneshot(form_request(
            &format!("/notice/{notice_id}/ack"),
            &body,
            Some(&member_cookie),
        ))
        .await
        .expect("确认请求失败");
    assert_eq!(
        response.status(),
        StatusCode::SEE_OTHER,
        "成员确认同一条公告应当成功"
    );

    let member_rows: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM announcement_acks WHERE announcement_id = ? AND user_id = ?",
    )
    .bind(notice_id)
    .bind(member_id)
    .fetch_one(&pool)
    .await
    .expect("查确认记录失败");
    assert_eq!(member_rows, 1, "成员确认后应当有一行");
}
