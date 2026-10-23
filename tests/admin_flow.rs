//! M7 管理区的集成测试。
//!
//! 全部走线上的 `build_router`，不另建一套路由：测试里能构造出来的请求，
//! 生产里也一定走得通；反过来，测试自建路由就只能证明"我那段代码自洽"。
//!
//! 数据库是 `TEST_DATABASE_URL` 指向的库，用例并行跑，所以每个用例都自己造
//! 带唯一后缀的用户与小组，**不做清理**：残留行不影响断言，而按前缀批量删
//! 会把别的用例的数据删掉（M2 时踩过，七个用例连环红）。

use axum::Router;
use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{Request, StatusCode};
use sqlx::MySqlPool;
use tower::ServiceExt;

use nexus::app::build_router;
use nexus::auth::model::{Role, UserStatus};
use nexus::config::Config;
use nexus::state::AppState;

const PASSWORD: &str = "a-long-password";

fn db_url() -> String {
    std::env::var("TEST_DATABASE_URL").expect("需要 TEST_DATABASE_URL")
}

fn unique(prefix: &str) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("ad{prefix}{nanos:x}")
}

fn config_for(url: String) -> Config {
    Config::from_source(|key| match key {
        "DATABASE_URL" => Some(url.clone()),
        "APP_BASE_URL" => Some("http://localhost:8080".to_string()),
        "SESSION_SECURE" => Some("false".to_string()),
        _ => None,
    })
    .expect("测试配置应合法")
}

async fn harness(url: &str) -> (Router, MySqlPool) {
    let config = config_for(url.to_string());
    let pool = nexus::db::init_pool(&config).await.expect("建池失败");
    nexus::db::migrate(&pool).await.expect("迁移失败");
    let state = AppState::new(pool.clone(), config);
    (build_router(state), pool)
}

fn with_peer(mut request: Request<Body>) -> Request<Body> {
    request
        .extensions_mut()
        .insert(ConnectInfo(std::net::SocketAddr::from((
            [127, 0, 0, 1],
            45_678,
        ))));
    request
}

async fn send(app: &Router, request: Request<Body>) -> axum::response::Response {
    app.clone()
        .oneshot(with_peer(request))
        .await
        .expect("请求失败")
}

async fn get(app: &Router, uri: &str, cookie: Option<&str>) -> axum::response::Response {
    let mut builder = Request::builder().method("GET").uri(uri);
    if let Some(cookie) = cookie {
        builder = builder.header("cookie", cookie);
    }
    send(app, builder.body(Body::empty()).expect("构造请求失败")).await
}

async fn form(
    app: &Router,
    uri: &str,
    cookie: Option<&str>,
    fields: &[(&str, &str)],
) -> axum::response::Response {
    let body = fields
        .iter()
        .map(|(key, value)| format!("{}={}", encode(key), encode(value)))
        .collect::<Vec<_>>()
        .join("&");
    let mut builder = Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/x-www-form-urlencoded");
    if let Some(cookie) = cookie {
        builder = builder.header("cookie", cookie);
    }
    send(app, builder.body(Body::from(body)).expect("构造请求失败")).await
}

/// 极简 percent-encoding：只处理会破坏表单的字符，够测试用。
fn encode(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for byte in raw.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            b' ' => out.push('+'),
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

fn status(response: &axum::response::Response) -> StatusCode {
    response.status()
}

fn location(response: &axum::response::Response) -> String {
    response
        .headers()
        .get("location")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_string()
}

fn cookie_of(response: &axum::response::Response) -> Option<String> {
    response
        .headers()
        .get_all("set-cookie")
        .iter()
        .filter_map(|value| value.to_str().ok())
        .find_map(|value| {
            let first = value.split(';').next()?.trim();
            if first.starts_with("nexus_session=") {
                Some(first.to_string())
            } else {
                None
            }
        })
}

async fn body_of(response: axum::response::Response) -> String {
    let bytes = axum::body::to_bytes(response.into_body(), 4 * 1024 * 1024)
        .await
        .expect("读响应体失败");
    String::from_utf8_lossy(&bytes).into_owned()
}

/// 从页面里取 64 位 CSRF token（layout 注入的 meta）。
fn csrf_of(html: &str) -> String {
    let marker = "name=\"csrf-token\" content=\"";
    let start = html.find(marker).expect("页面里没有 csrf meta") + marker.len();
    let rest = &html[start..];
    let end = rest.find('"').expect("csrf meta 没有闭合");
    rest[..end].to_string()
}

async fn make_user(pool: &MySqlPool, username: &str, role: Role, status: UserStatus) -> u64 {
    let hash = nexus::auth::service::hash_password(PASSWORD)
        .await
        .expect("算哈希失败");
    let mut tx = pool.begin().await.expect("开事务失败");
    let id = nexus::auth::model::insert_user(
        &mut tx,
        nexus::auth::model::NewUser {
            username,
            email: &format!("{username}@example.com"),
            password_hash: &hash,
            display_name: username,
            role,
            status,
        },
    )
    .await
    .expect("插入用户失败");
    tx.commit().await.expect("提交失败");
    id
}

async fn login(app: &Router, username: &str) -> String {
    // 必须先拿到 login 页面下发的会话 cookie，再用同一个会话去 POST：
    // CSRF token 存在 session 里，两次请求换了会话就必然 403。
    let page_response = get(app, "/login", None).await;
    let session_cookie = cookie_of(&page_response).expect("登录页应下发会话 cookie");
    let page = body_of(page_response).await;
    let csrf = csrf_of(&page);
    let response = form(
        app,
        "/login",
        Some(&session_cookie),
        &[
            ("username", username),
            ("password", PASSWORD),
            ("_csrf", &csrf),
        ],
    )
    .await;
    assert_eq!(status(&response), StatusCode::SEE_OTHER, "登录应 303");
    // 登录成功会换新会话 id（防固定攻击），优先用响应里的新 cookie。
    cookie_of(&response).unwrap_or(session_cookie)
}

async fn token_for(app: &Router, uri: &str, cookie: &str) -> String {
    let html = body_of(get(app, uri, Some(cookie)).await).await;
    csrf_of(&html)
}

async fn audit_count(pool: &MySqlPool, action: &str) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM audit_logs WHERE action = ?")
        .bind(action)
        .fetch_one(pool)
        .await
        .expect("查审计失败")
}

/// 只数某个操作者自己的审计。用例并行跑，全表计数会被别的用例干扰。
async fn audit_count_for(pool: &MySqlPool, action: &str, actor_id: u64) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM audit_logs WHERE action = ? AND actor_id = ?")
        .bind(action)
        .bind(actor_id)
        .fetch_one(pool)
        .await
        .expect("查审计失败")
}

async fn audit_meta(pool: &MySqlPool, action: &str, max_id: u64) -> Vec<String> {
    sqlx::query_scalar(
        "SELECT CAST(meta AS CHAR) FROM audit_logs \
          WHERE action = ? AND id <= ? ORDER BY id DESC LIMIT 5",
    )
    .bind(action)
    .bind(max_id)
    .fetch_all(pool)
    .await
    .expect("查审计 meta 失败")
}

async fn max_audit_id(pool: &MySqlPool) -> u64 {
    sqlx::query_scalar("SELECT CAST(COALESCE(MAX(id), 0) AS UNSIGNED) FROM audit_logs")
        .fetch_one(pool)
        .await
        .expect("查最大审计 id 失败")
}

// ── 1. 非管理员进不来 ──────────────────────────────────────────────────

#[tokio::test]
async fn non_admin_cannot_open_admin_pages() {
    let url = db_url();
    let (app, pool) = harness(&url).await;
    let name = unique("plain");
    make_user(&pool, &name, Role::User, UserStatus::Active).await;
    let cookie = login(&app, &name).await;

    for uri in ["/admin", "/admin/members", "/admin/audit"] {
        let response = get(&app, uri, Some(&cookie)).await;
        assert_eq!(status(&response), StatusCode::FORBIDDEN, "{uri} 应 403");
    }

    // 游客拿到的不是 403 而是"去登录"——403 会让人以为自己被判定为坏人。
    let guest = get(&app, "/admin", None).await;
    assert_eq!(status(&guest), StatusCode::SEE_OTHER);
    assert!(location(&guest).contains("/login"));
}

// ── 2. 通过申请：升成员 + 两条审计 ──────────────────────────────────────

#[tokio::test]
async fn approving_a_join_promotes_the_user_and_writes_two_audits() {
    let url = db_url();
    let (app, pool) = harness(&url).await;
    let admin_name = unique("adm");
    make_user(&pool, &admin_name, Role::Admin, UserStatus::Active).await;
    let admin_cookie = login(&app, &admin_name).await;

    let joiner = unique("join");
    let joiner_id = make_user(&pool, &joiner, Role::User, UserStatus::Active).await;
    let joiner_cookie = login(&app, &joiner).await;

    // 先真的从前厅提交申请，走一遍用户的路径。
    let join_csrf = token_for(&app, "/join", &joiner_cookie).await;
    let submitted = form(
        &app,
        "/join",
        Some(&joiner_cookie),
        &[("message", "想参与美术"), ("_csrf", &join_csrf)],
    )
    .await;
    assert_eq!(status(&submitted), StatusCode::SEE_OTHER, "提交申请应 303");

    let request_id: u64 =
        sqlx::query_scalar("SELECT CAST(MAX(id) AS UNSIGNED) FROM join_requests WHERE user_id = ?")
            .bind(joiner_id)
            .fetch_one(&pool)
            .await
            .expect("查申请失败");

    let mark = max_audit_id(&pool).await;
    let csrf = token_for(&app, "/admin/joins", &admin_cookie).await;
    let approved = form(
        &app,
        &format!("/admin/joins/{request_id}/approve"),
        Some(&admin_cookie),
        &[("_csrf", &csrf)],
    )
    .await;
    assert_eq!(status(&approved), StatusCode::SEE_OTHER);

    let role: String = sqlx::query_scalar("SELECT role FROM users WHERE id = ?")
        .bind(joiner_id)
        .fetch_one(&pool)
        .await
        .expect("查角色失败");
    assert_eq!(role, "member", "通过申请后角色必须是 member");

    let status_raw: String = sqlx::query_scalar("SELECT status FROM join_requests WHERE id = ?")
        .bind(request_id)
        .fetch_one(&pool)
        .await
        .expect("查申请状态失败");
    assert_eq!(status_raw, "approved");

    assert!(audit_count(&pool, "join_request.approve").await >= 1);
    assert!(
        audit_count(&pool, "user.role_change").await >= 1,
        "升角色必须另记一条 user.role_change，否则按角色变更统计会漏掉这条路径"
    );
    let metas = audit_meta(&pool, "user.role_change", mark + 1000).await;
    assert!(
        metas.iter().any(|meta| meta.contains("join_request")),
        "meta 里要能看出是走申请提的权：{metas:?}"
    );
}

// ── 3. 拒绝申请：角色不变 ───────────────────────────────────────────────

#[tokio::test]
async fn rejecting_a_join_keeps_the_user_a_plain_user() {
    let url = db_url();
    let (app, pool) = harness(&url).await;
    let admin_name = unique("adm");
    make_user(&pool, &admin_name, Role::Admin, UserStatus::Active).await;
    let admin_cookie = login(&app, &admin_name).await;

    let joiner = unique("rej");
    let joiner_id = make_user(&pool, &joiner, Role::User, UserStatus::Active).await;

    let mut tx = pool.begin().await.expect("开事务失败");
    let request_id = nexus::public::model::create_join_request(
        &mut tx,
        nexus::public::model::NewJoinRequest {
            user_id: joiner_id,
            message: "试试",
        },
    )
    .await
    .expect("建申请失败");
    tx.commit().await.expect("提交失败");

    let csrf = token_for(&app, "/admin/joins", &admin_cookie).await;
    let rejected = form(
        &app,
        &format!("/admin/joins/{request_id}/reject"),
        Some(&admin_cookie),
        &[("note", "暂时不需要"), ("_csrf", &csrf)],
    )
    .await;
    assert_eq!(status(&rejected), StatusCode::SEE_OTHER);

    let role: String = sqlx::query_scalar("SELECT role FROM users WHERE id = ?")
        .bind(joiner_id)
        .fetch_one(&pool)
        .await
        .expect("查角色失败");
    assert_eq!(role, "user", "拒绝申请不该改角色");

    let note: Option<String> =
        sqlx::query_scalar("SELECT review_note FROM join_requests WHERE id = ?")
            .bind(request_id)
            .fetch_one(&pool)
            .await
            .expect("查备注失败");
    assert_eq!(note.as_deref(), Some("暂时不需要"));
}

// ── 4. 不能动自己 ───────────────────────────────────────────────────────

#[tokio::test]
async fn admin_cannot_change_own_role_or_status_or_delete_self() {
    let url = db_url();
    let (app, pool) = harness(&url).await;
    let admin_name = unique("self");
    let admin_id = make_user(&pool, &admin_name, Role::Admin, UserStatus::Active).await;
    let cookie = login(&app, &admin_name).await;
    let csrf = token_for(&app, "/admin/members", &cookie).await;

    let demote = form(
        &app,
        &format!("/admin/members/{admin_id}/role"),
        Some(&cookie),
        &[("role", "user"), ("_csrf", &csrf)],
    )
    .await;
    assert_eq!(
        status(&demote),
        StatusCode::UNPROCESSABLE_ENTITY,
        "改自己必须被拒"
    );

    let suspend = form(
        &app,
        &format!("/admin/members/{admin_id}/status"),
        Some(&cookie),
        &[("status", "suspended"), ("_csrf", &csrf)],
    )
    .await;
    assert_eq!(status(&suspend), StatusCode::UNPROCESSABLE_ENTITY);

    let delete = form(
        &app,
        &format!("/admin/members/{admin_id}/delete"),
        Some(&cookie),
        &[("_csrf", &csrf)],
    )
    .await;
    assert_eq!(status(&delete), StatusCode::UNPROCESSABLE_ENTITY);

    let role: String = sqlx::query_scalar("SELECT role FROM users WHERE id = ?")
        .bind(admin_id)
        .fetch_one(&pool)
        .await
        .expect("查角色失败");
    assert_eq!(role, "admin", "自己那一行必须原封不动");
}

// ── 5. 改角色：写审计 + from/to ─────────────────────────────────────────

#[tokio::test]
async fn role_change_writes_audit_with_from_and_to() {
    let url = db_url();
    let (app, pool) = harness(&url).await;
    let admin_name = unique("adm");
    make_user(&pool, &admin_name, Role::Admin, UserStatus::Active).await;
    let admin_cookie = login(&app, &admin_name).await;

    let target_name = unique("tar");
    let target_id = make_user(&pool, &target_name, Role::User, UserStatus::Active).await;

    let csrf = token_for(&app, "/admin/members", &admin_cookie).await;
    let mark = max_audit_id(&pool).await;
    let promoted = form(
        &app,
        &format!("/admin/members/{target_id}/role"),
        Some(&admin_cookie),
        &[("role", "member"), ("_csrf", &csrf)],
    )
    .await;
    assert_eq!(status(&promoted), StatusCode::SEE_OTHER);

    let metas = audit_meta(&pool, "user.role_change", mark + 1000).await;
    assert!(
        metas
            .iter()
            .any(|meta| meta.contains("\"from\"") && meta.contains("\"to\"")),
        "审计 meta 必须带 from/to：{metas:?}"
    );
}

// ── 6. 软删除：行还在，用户名被释放 ────────────────────────────────────

#[tokio::test]
async fn soft_delete_keeps_the_row_and_frees_the_username() {
    let url = db_url();
    let (app, pool) = harness(&url).await;
    let admin_name = unique("adm");
    make_user(&pool, &admin_name, Role::Admin, UserStatus::Active).await;
    let admin_cookie = login(&app, &admin_name).await;

    let victim = unique("vic");
    let victim_id = make_user(&pool, &victim, Role::User, UserStatus::Active).await;

    let csrf = token_for(&app, "/admin/members", &admin_cookie).await;
    let deleted = form(
        &app,
        &format!("/admin/members/{victim_id}/delete"),
        Some(&admin_cookie),
        &[("_csrf", &csrf)],
    )
    .await;
    assert_eq!(status(&deleted), StatusCode::SEE_OTHER);

    let row: (String, String, String) =
        sqlx::query_as("SELECT status, username, password_hash FROM users WHERE id = ?")
            .bind(victim_id)
            .fetch_one(&pool)
            .await
            .expect("行必须还在——软删除不是物理删除");
    assert_eq!(row.0, "deleted");
    assert!(row.1.starts_with(&victim), "用户名应保留原文并追加后缀");
    assert!(row.1.contains("__deleted__"));
    assert_eq!(row.2, "", "口令哈希必须清空，否则旧口令还能验");
}

// ── 7. 投稿通过：建草稿 + 回填 + 审计 ──────────────────────────────────

#[tokio::test]
async fn approving_a_submission_creates_an_unpublished_doc() {
    let url = db_url();
    let (app, pool) = harness(&url).await;
    let admin_name = unique("adm");
    make_user(&pool, &admin_name, Role::Admin, UserStatus::Active).await;
    let admin_cookie = login(&app, &admin_name).await;

    let author = unique("sub");
    let author_id = make_user(&pool, &author, Role::User, UserStatus::Active).await;
    let mut tx = pool.begin().await.expect("开事务失败");
    let submission_id = nexus::public::model::create_submission(
        &mut tx,
        nexus::public::model::NewSubmission {
            author_id,
            title: "外部投稿的设定",
            body: "正文内容",
        },
    )
    .await
    .expect("建投稿失败");
    tx.commit().await.expect("提交失败");

    let csrf = token_for(&app, "/admin/submissions", &admin_cookie).await;
    let approved = form(
        &app,
        &format!("/admin/submissions/{submission_id}/approve"),
        Some(&admin_cookie),
        &[("_csrf", &csrf)],
    )
    .await;
    assert_eq!(status(&approved), StatusCode::SEE_OTHER);

    let submission_status: String =
        sqlx::query_scalar("SELECT status FROM public_submissions WHERE id = ?")
            .bind(submission_id)
            .fetch_one(&pool)
            .await
            .expect("查投稿失败");
    assert_eq!(submission_status, "approved");

    let target: Option<String> =
        sqlx::query_scalar("SELECT target_type FROM public_submissions WHERE id = ?")
            .bind(submission_id)
            .fetch_one(&pool)
            .await
            .expect("查回填失败");
    assert_eq!(target.as_deref(), Some("doc"));

    // 建出来的必须是一篇未发布的草稿：采纳不等于替团队拍板发布。
    let doc: (String, i8) =
        sqlx::query_as("SELECT title, is_published FROM kb_docs ORDER BY id DESC LIMIT 1")
            .fetch_one(&pool)
            .await
            .expect("应建出档案");
    assert_eq!(doc.0, "外部投稿的设定");
    assert_eq!(doc.1, 0, "通过的投稿应先落成草稿");
}

// ── 8. 审计页：每看一次写一条 ──────────────────────────────────────────

#[tokio::test]
async fn opening_the_audit_page_writes_an_audit_view_every_time() {
    let url = db_url();
    let (app, pool) = harness(&url).await;
    let admin_name = unique("aud");
    let admin_id = make_user(&pool, &admin_name, Role::Admin, UserStatus::Active).await;
    let cookie = login(&app, &admin_name).await;

    let before = audit_count_for(&pool, "admin.audit_view", admin_id).await;
    let first = get(&app, "/admin/audit", Some(&cookie)).await;
    assert_eq!(status(&first), StatusCode::OK);
    let second = get(&app, "/admin/audit", Some(&cookie)).await;
    assert_eq!(status(&second), StatusCode::OK);
    let after = audit_count_for(&pool, "admin.audit_view", admin_id).await;

    assert_eq!(after - before, 2, "看两次必须记两条（需求 L179）");
}

// ── 9. 审计筛选：真的收窄了结果 ────────────────────────────────────────

#[tokio::test]
async fn audit_filters_narrow_the_rows() {
    let url = db_url();
    let (app, pool) = harness(&url).await;
    let admin_name = unique("aud");
    let admin_id = make_user(&pool, &admin_name, Role::Admin, UserStatus::Active).await;
    let cookie = login(&app, &admin_name).await;

    let all = body_of(get(&app, "/admin/audit", Some(&cookie)).await).await;
    let filtered = body_of(
        get(
            &app,
            &format!("/admin/audit?action=admin.audit_view&actor={admin_id}"),
            Some(&cookie),
        )
        .await,
    )
    .await;

    assert!(all.contains("admin.audit_view"), "全量里应能看到审计动作");
    assert!(
        filtered.contains("admin.audit_view"),
        "按动作前缀筛也应留下审计动作本身"
    );
    assert!(
        !filtered.contains("auth.login"),
        "按 actor+action 筛过之后不该还混进登录记录"
    );
}

// ── 10. 待审队列只列 pending ───────────────────────────────────────────

#[tokio::test]
async fn the_join_queue_only_lists_pending_requests() {
    let url = db_url();
    let (app, pool) = harness(&url).await;
    let admin_name = unique("adm");
    make_user(&pool, &admin_name, Role::Admin, UserStatus::Active).await;
    let admin_cookie = login(&app, &admin_name).await;

    let waiting = unique("wait");
    let waiting_id = make_user(&pool, &waiting, Role::User, UserStatus::Active).await;
    let mut tx = pool.begin().await.expect("开事务失败");
    nexus::public::model::create_join_request(
        &mut tx,
        nexus::public::model::NewJoinRequest {
            user_id: waiting_id,
            message: "排队中",
        },
    )
    .await
    .expect("建申请失败");
    tx.commit().await.expect("提交失败");

    let html = body_of(get(&app, "/admin/joins", Some(&admin_cookie)).await).await;
    assert!(html.contains(&waiting), "待审申请应出现在队列里：{html}");
}
