//! M3 小组可见性的集成测试。
//!
//! 这里刻意**不复用** `forum_flow.rs` 的辅助函数，而是自己写一份：
//! 集成测试 crate 之间不能互相 import，抽公共模块又会把"测试怎么搭环境"
//! 变成需要维护的第三个 crate。重复二十行换一个不会互相牵制的测试文件，值。
//!
//! 每个用例都自带数据（用户名、slug 都带纳秒后缀），跑完**不清理**：
//! 按前缀批量删会把并行用例的数据一起删掉，这个坑在 M2 已经踩过一次。

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};

use axum::Router;
use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt;
use nexus::app::build_router;
use nexus::auth::policy::CurrentUser;
use nexus::auth::service as auth_service;
use nexus::config::Config;
use nexus::forum::service as forum_service;
use nexus::group::service as group_service;
use nexus::shared::visibility::Visibility;
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
    format!("gf_{prefix}_{nanos:x}_{n}")
}

fn db_url() -> Option<String> {
    std::env::var("TEST_DATABASE_URL")
        .ok()
        .filter(|s| !s.is_empty())
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

/// 直接建一个可登录的用户。走 service 里的 hash，保证和线上同一套参数。
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
    let csrf = csrf_of(&html).expect("登录页应当有 csrf token");
    let body = format!("username={username}&password=a-long-password&_csrf={csrf}");
    let response = app
        .clone()
        .oneshot(form_request("/login", &body, Some(&cookie)))
        .await
        .expect("请求失败");
    assert_eq!(
        response.status(),
        StatusCode::SEE_OTHER,
        "登录应当成功并跳转"
    );
    cookie_of(&response).unwrap_or(cookie)
}

async fn seed_group(pool: &MySqlPool, slug: &str, name: &str, creator: u64) -> u64 {
    let result = sqlx::query(
        "INSERT INTO member_groups (name, slug, description, created_by) VALUES (?, ?, '', ?)",
    )
    .bind(name)
    .bind(slug)
    .bind(creator)
    .execute(pool)
    .await
    .expect("插入小组失败");
    result.last_insert_id()
}

async fn add_member_row(pool: &MySqlPool, group_id: u64, user_id: u64) {
    sqlx::query("INSERT INTO group_members (group_id, user_id) VALUES (?, ?)")
        .bind(group_id)
        .bind(user_id)
        .execute(pool)
        .await
        .expect("插入成员关系失败");
}

async fn seed_space(
    pool: &MySqlPool,
    slug: &str,
    name: &str,
    visibility: &str,
    group_id: Option<u64>,
    owner: u64,
) -> u64 {
    let result = sqlx::query(
        "INSERT INTO forum_spaces (name, slug, description, sort_order, visibility, group_id, created_by) \
         VALUES (?, ?, '', 0, ?, ?, ?)",
    )
    .bind(name)
    .bind(slug)
    .bind(visibility)
    .bind(group_id)
    .bind(owner)
    .execute(pool)
    .await
    .expect("插入分区失败");
    result.last_insert_id()
}

async fn current_user(pool: &MySqlPool, id: u64) -> CurrentUser {
    let user = nexus::auth::model::find_by_id(pool, id)
        .await
        .expect("查询用户失败")
        .expect("用户应当存在");
    CurrentUser::from_user(&user)
}

/// 库里没有任何用户时先造一个——`created_by` 是 RESTRICT 外键，写死 1 会被数据库拒绝。
async fn ensure_owner(pool: &MySqlPool) -> u64 {
    let existing: Option<u64> = sqlx::query_scalar("SELECT id FROM users ORDER BY id LIMIT 1")
        .fetch_optional(pool)
        .await
        .expect("查询用户失败");
    match existing {
        Some(id) => id,
        None => make_user(pool, &unique("owner"), "admin").await,
    }
}

#[tokio::test]
async fn group_space_is_hidden_from_outsiders_and_listed_for_members() {
    let Some(url) = db_url() else {
        eprintln!("跳过：没有设置 TEST_DATABASE_URL");
        return;
    };
    let (app, pool) = harness(&url).await;

    let owner = ensure_owner(&pool).await;
    let slug = unique("gs");
    let name = format!("小组分区{}", unique("n"));
    let gid = seed_group(&pool, &unique("g"), "核心小组", owner).await;
    let space_id = seed_space(&pool, &slug, &name, "group", Some(gid), owner).await;

    let member_name = unique("member");
    let member_id = make_user(&pool, &member_name, "member").await;
    add_member_row(&pool, gid, member_id).await;
    let member_cookie = login(&app, &member_name).await;

    // 组员：列表里看得到，详情页 200。
    let listed = app
        .clone()
        .oneshot(get_request("/forum", Some(&member_cookie)))
        .await
        .expect("请求失败");
    assert_eq!(listed.status(), StatusCode::OK);
    let html = body_of(listed).await;
    assert!(html.contains(&name), "组员应当能在论坛首页看到小组分区");

    let detail = app
        .clone()
        .oneshot(get_request(&format!("/forum/{slug}"), Some(&member_cookie)))
        .await
        .expect("请求失败");
    assert_eq!(detail.status(), StatusCode::OK, "组员应当能打开小组分区");

    // 游客：列表里没有，直接访问 404。
    let guest_list = app
        .clone()
        .oneshot(get_request("/forum", None))
        .await
        .expect("请求失败");
    let guest_html = body_of(guest_list).await;
    assert!(
        !guest_html.contains(&name),
        "游客不应当看到小组分区，连名字都不该出现"
    );

    let guest_detail = app
        .clone()
        .oneshot(get_request(&format!("/forum/{slug}"), None))
        .await
        .expect("请求失败");
    assert_eq!(guest_detail.status(), StatusCode::NOT_FOUND);

    // 已登录但不在组里的普通用户：同样 404，而不是 403。
    let outsider_name = unique("outsider");
    make_user(&pool, &outsider_name, "member").await;
    let outsider_cookie = login(&app, &outsider_name).await;
    let outsider = app
        .clone()
        .oneshot(get_request(
            &format!("/forum/{slug}"),
            Some(&outsider_cookie),
        ))
        .await
        .expect("请求失败");
    assert_eq!(
        outsider.status(),
        StatusCode::NOT_FOUND,
        "不在组里就是不存在，403 会暴露这个分区确实存在"
    );

    // 防止 space_id 未使用告警：它本就是给后续断言/清理留的句柄。
    let _ = space_id;
}

#[tokio::test]
async fn admin_who_is_not_a_member_gets_not_found() {
    let Some(url) = db_url() else {
        eprintln!("跳过：没有设置 TEST_DATABASE_URL");
        return;
    };
    let (app, pool) = harness(&url).await;

    let owner = ensure_owner(&pool).await;
    let slug = unique("ga");
    let gid = seed_group(&pool, &unique("g"), "只有组员能看", owner).await;
    seed_space(&pool, &slug, "管理员也别看", "group", Some(gid), owner).await;

    let admin_name = unique("admin");
    make_user(&pool, &admin_name, "admin").await;
    let cookie = login(&app, &admin_name).await;

    let response = app
        .clone()
        .oneshot(get_request(&format!("/forum/{slug}"), Some(&cookie)))
        .await
        .expect("请求失败");
    assert_eq!(
        response.status(),
        StatusCode::NOT_FOUND,
        "admin 不在组里就不该看见——可见性不是按角色开后门"
    );
}

#[tokio::test]
async fn removing_a_member_takes_effect_on_the_next_request() {
    let Some(url) = db_url() else {
        eprintln!("跳过：没有设置 TEST_DATABASE_URL");
        return;
    };
    let (app, pool) = harness(&url).await;

    let owner = ensure_owner(&pool).await;
    let slug = unique("gr");
    let gid = seed_group(&pool, &unique("g"), "临时小组", owner).await;
    seed_space(&pool, &slug, "临时分区", "group", Some(gid), owner).await;

    let name = unique("mover");
    let uid = make_user(&pool, &name, "member").await;
    add_member_row(&pool, gid, uid).await;
    let cookie = login(&app, &name).await;

    let before = app
        .clone()
        .oneshot(get_request(&format!("/forum/{slug}"), Some(&cookie)))
        .await
        .expect("请求失败");
    assert_eq!(before.status(), StatusCode::OK);

    sqlx::query("DELETE FROM group_members WHERE group_id = ? AND user_id = ?")
        .bind(gid)
        .bind(uid)
        .execute(&pool)
        .await
        .expect("移除成员失败");

    let after = app
        .clone()
        .oneshot(get_request(&format!("/forum/{slug}"), Some(&cookie)))
        .await
        .expect("请求失败");
    assert_eq!(
        after.status(),
        StatusCode::NOT_FOUND,
        "同一条 cookie 下一次请求就该失效——group_ids 每请求重查，不缓存在会话里"
    );
}

#[tokio::test]
async fn soft_deleted_group_hides_the_space_from_members() {
    let Some(url) = db_url() else {
        eprintln!("跳过：没有设置 TEST_DATABASE_URL");
        return;
    };
    let (app, pool) = harness(&url).await;

    let owner = ensure_owner(&pool).await;
    let slug = unique("gd");
    let gid = seed_group(&pool, &unique("g"), "解散小组", owner).await;
    seed_space(&pool, &slug, "解散分区", "group", Some(gid), owner).await;

    let name = unique("left");
    let uid = make_user(&pool, &name, "member").await;
    add_member_row(&pool, gid, uid).await;
    let cookie = login(&app, &name).await;

    // 软删小组：成员行保留（审计/历史要能追），但小组本身不再算数。
    sqlx::query("UPDATE member_groups SET deleted_at = UTC_TIMESTAMP() WHERE id = ?")
        .bind(gid)
        .execute(&pool)
        .await
        .expect("软删小组失败");

    let response = app
        .clone()
        .oneshot(get_request(&format!("/forum/{slug}"), Some(&cookie)))
        .await
        .expect("请求失败");
    assert_eq!(
        response.status(),
        StatusCode::NOT_FOUND,
        "小组软删后，成员身份不该继续放行"
    );

    let kept: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM group_members WHERE group_id = ?")
        .bind(gid)
        .fetch_one(&pool)
        .await
        .expect("统计成员失败");
    assert_eq!(kept, 1, "软删小组不该物理删除成员行");
}

#[tokio::test]
async fn group_visibility_requires_a_real_group_id() {
    let Some(url) = db_url() else {
        eprintln!("跳过：没有设置 TEST_DATABASE_URL");
        return;
    };
    let (_app, pool) = harness(&url).await;

    let owner = ensure_owner(&pool).await;
    let slug = unique("gv");
    let space_id = seed_space(&pool, &slug, "待收紧分区", "internal", None, owner).await;
    // 必须自己造一个 admin：ensure_owner 只保证"有个用户能当外键"，不保证角色。
    let admin_id = make_user(&pool, &unique("adm"), "admin").await;
    let admin = current_user(&pool, admin_id).await;

    // 不给 group_id：必须 422，而不是"设置成功但对谁都不可见"。
    let missing =
        forum_service::set_space_visibility(&pool, &admin, None, &slug, Visibility::Group, None)
            .await;
    assert!(
        matches!(missing, Err(nexus::error::AppError::Unprocessable(_))),
        "group 可见性缺 group_id 应当被拒"
    );

    // 给了不存在的组：同样 422。
    let bogus = forum_service::set_space_visibility(
        &pool,
        &admin,
        None,
        &slug,
        Visibility::Group,
        Some(9_999_999),
    )
    .await;
    assert!(matches!(
        bogus,
        Err(nexus::error::AppError::Unprocessable(_))
    ));

    // 两次失败都不能改动原值。
    let (visibility, group_id): (String, Option<u64>) =
        sqlx::query_as("SELECT visibility, group_id FROM forum_spaces WHERE id = ?")
            .bind(space_id)
            .fetch_one(&pool)
            .await
            .expect("查询分区失败");
    assert_eq!(visibility, "internal", "失败的调用不该改可见性");
    assert_eq!(group_id, None, "失败的调用不该写入 group_id");
}

#[tokio::test]
async fn visibility_change_writes_audit_and_cascades_to_threads() {
    let Some(url) = db_url() else {
        eprintln!("跳过：没有设置 TEST_DATABASE_URL");
        return;
    };
    let (_app, pool) = harness(&url).await;

    let owner = ensure_owner(&pool).await;
    let slug = unique("gc");
    let gid = seed_group(&pool, &unique("g"), "收紧到的组", owner).await;
    let space_id = seed_space(&pool, &slug, "要收紧的分区", "internal", None, owner).await;

    // 历史议题：改动前是 internal，改动后必须跟着变成 group。
    let thread = sqlx::query(
        "INSERT INTO forum_threads (space_id, author_id, title, visibility) VALUES (?, ?, '历史议题', 'internal')",
    )
    .bind(space_id)
    .bind(owner)
    .execute(&pool)
    .await
    .expect("插入议题失败");
    let thread_id = thread.last_insert_id();

    let admin_id = make_user(&pool, &unique("adm"), "admin").await;
    let admin = current_user(&pool, admin_id).await;
    forum_service::set_space_visibility(
        &pool,
        &admin,
        Some("127.0.0.1"),
        &slug,
        Visibility::Group,
        Some(gid),
    )
    .await
    .expect("改可见性应当成功");

    let (visibility, group_id): (String, Option<u64>) =
        sqlx::query_as("SELECT visibility, group_id FROM forum_spaces WHERE id = ?")
            .bind(space_id)
            .fetch_one(&pool)
            .await
            .expect("查询分区失败");
    assert_eq!(visibility, "group");
    assert_eq!(group_id, Some(gid));

    let (thread_visibility, thread_group): (String, Option<u64>) =
        sqlx::query_as("SELECT visibility, group_id FROM forum_threads WHERE id = ?")
            .bind(thread_id)
            .fetch_one(&pool)
            .await
            .expect("查询议题失败");
    assert_eq!(
        thread_visibility, "group",
        "历史议题必须跟着收紧——只改分区的话旧议题仍是 internal，所有 member 可见，等于假动作"
    );
    assert_eq!(thread_group, Some(gid));

    let audited: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM audit_logs \
         WHERE action = 'content.visibility_change' AND target_type = 'forum_space' AND target_id = ? \
           AND JSON_UNQUOTE(JSON_EXTRACT(meta, '$.from')) = 'internal' \
           AND JSON_UNQUOTE(JSON_EXTRACT(meta, '$.to')) = 'group' \
           AND JSON_EXTRACT(meta, '$.group_id') = ?",
    )
    .bind(space_id)
    .bind(gid)
    .fetch_one(&pool)
    .await
    .expect("查询审计失败");
    assert_eq!(audited, 1, "可见性变更必须有且只有一条审计");
}

#[tokio::test]
async fn adding_the_same_member_twice_is_idempotent() {
    let Some(url) = db_url() else {
        eprintln!("跳过：没有设置 TEST_DATABASE_URL");
        return;
    };
    let (_app, pool) = harness(&url).await;

    let owner = ensure_owner(&pool).await;
    let gid = seed_group(&pool, &unique("g"), "去重小组", owner).await;
    let name = unique("dup");
    let uid = make_user(&pool, &name, "member").await;
    let admin_id = make_user(&pool, &unique("adm"), "admin").await;
    let admin = current_user(&pool, admin_id).await;

    let first = group_service::add_member(&pool, &admin, None, gid, uid)
        .await
        .expect("第一次加人应当成功");
    assert!(first, "第一次应当真的插入了");

    let second = group_service::add_member(&pool, &admin, None, gid, uid)
        .await
        .expect("重复加人不该报错");
    assert!(!second, "第二次应当报告'本来就在组里'");

    let rows: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM group_members WHERE group_id = ? AND user_id = ?")
            .bind(gid)
            .bind(uid)
            .fetch_one(&pool)
            .await
            .expect("统计成员失败");
    assert_eq!(rows, 1, "复合主键应当只留一行");
}

#[tokio::test]
async fn non_admin_cannot_change_visibility() {
    let Some(url) = db_url() else {
        eprintln!("跳过：没有设置 TEST_DATABASE_URL");
        return;
    };
    let (app, pool) = harness(&url).await;

    let owner = ensure_owner(&pool).await;
    let slug = unique("gn");
    seed_space(&pool, &slug, "别动我", "internal", None, owner).await;

    let name = unique("plain");
    make_user(&pool, &name, "member").await;
    let cookie = login(&app, &name).await;

    let (html, cookie) = page_token(&app, "/forum", Some(&cookie)).await;
    let csrf = csrf_of(&html).expect("已登录页面应当有 csrf token");
    let body = format!("visibility=public&group_id=&_csrf={csrf}");

    let response = app
        .clone()
        .oneshot(form_request(
            &format!("/forum/{slug}/visibility"),
            &body,
            Some(&cookie),
        ))
        .await
        .expect("请求失败");
    assert_eq!(
        response.status(),
        StatusCode::FORBIDDEN,
        "普通成员不能改可见性"
    );
}
