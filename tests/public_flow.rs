//! M6 公开区（前厅）的集成测试。
//!
//! 与 forum_flow / group_flow / kb_flow / task_flow 一致：辅助自备、用例造自己的数据、跑完不清理。
//! 这里覆盖 DESIGN §24.8 的十条验收，逐条对应一个 `#[tokio::test]`。
//!
//! 一条贯穿全篇的纪律：**前厅不区分「不存在」与「不可见」**，两者都必须 404。
//! 所以下面多处断言写的是 404 而不是 403，那不是宽松，而是刻意不泄漏"这里有东西"。

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
use nexus::public::model::CommentTarget;
use nexus::public::service as public_service;
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
    format!("pb_{prefix}_{nanos:x}_{n}")
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

/// 取一个页面的 HTML 与会话 cookie。前厅对游客也签发 CSRF，所以游客也能拿到。
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

async fn csrf_from(app: &Router, uri: &str, cookie: &str) -> String {
    let (html, _) = page_token(app, uri, Some(cookie)).await;
    csrf_of(&html).expect("页面应有 CSRF token")
}

/// 造一个分区。`created_by` 是 RESTRICT 外键，必须给真实用户 id。
async fn seed_space(pool: &MySqlPool, owner: u64, visibility: Visibility) -> (u64, String) {
    let slug = unique("sp").replace('_', "-");
    let result = sqlx::query(
        "INSERT INTO forum_spaces (name, slug, description, sort_order, visibility, created_by) \
         VALUES (?, ?, '集成测试分区', 0, ?, ?)",
    )
    .bind(format!("分区 {}", &slug[..slug.len().min(20)]))
    .bind(&slug)
    .bind(visibility.as_str())
    .bind(owner)
    .execute(pool)
    .await
    .expect("插入分区失败");
    (result.last_insert_id(), slug)
}

async fn seed_thread(
    pool: &MySqlPool,
    space_id: u64,
    author_id: u64,
    title: &str,
    visibility: Visibility,
) -> u64 {
    let result = sqlx::query(
        "INSERT INTO forum_threads (space_id, author_id, title, visibility, last_post_at) \
         VALUES (?, ?, ?, ?, UTC_TIMESTAMP())",
    )
    .bind(space_id)
    .bind(author_id)
    .bind(title)
    .bind(visibility.as_str())
    .execute(pool)
    .await
    .expect("插入议题失败");
    result.last_insert_id()
}

async fn seed_doc(
    pool: &MySqlPool,
    author_id: u64,
    title: &str,
    visibility: Visibility,
    published: bool,
) -> u64 {
    let result = sqlx::query(
        "INSERT INTO kb_docs (author_id, title, body, visibility, is_published, published_at) \
         VALUES (?, ?, '正文', ?, ?, IF(? = 1, UTC_TIMESTAMP(), NULL))",
    )
    .bind(author_id)
    .bind(title)
    .bind(visibility.as_str())
    .bind(i8::from(published))
    .bind(i8::from(published))
    .execute(pool)
    .await
    .expect("插入文档失败");
    result.last_insert_id()
}

/// `sql` 必须 `&'static str`：sqlx 0.9 的 `query_scalar` 只接受静态字符串，
/// 传 `&str` 会被拒绝（"borrowed data escapes outside of function"）。
async fn count_rows(pool: &MySqlPool, sql: &'static str, id: u64) -> i64 {
    sqlx::query_scalar(sql)
        .bind(id)
        .fetch_one(pool)
        .await
        .expect("计数失败")
}

async fn audit_count(pool: &MySqlPool, action: &str, target_type: &str, target_id: u64) -> i64 {
    sqlx::query_scalar(
        "SELECT COUNT(*) FROM audit_logs \
         WHERE action = ? AND target_type = ? AND target_id = ?",
    )
    .bind(action)
    .bind(target_type)
    .bind(target_id)
    .fetch_one(pool)
    .await
    .expect("审计计数失败")
}

// ---------------------------------------------------------------------------
// 十条验收
// ---------------------------------------------------------------------------

/// 1. 游客能看前厅与公开详情；`internal` 内容不进列表、直接访问 404。
#[tokio::test]
async fn guests_see_only_public_content_and_internal_is_not_found() {
    let Some(url) = db_url() else {
        println!("跳过：未设置 TEST_DATABASE_URL");
        return;
    };
    let (app, pool) = harness(&url).await;
    let owner = make_user(&pool, &unique("owner"), "member").await;

    let (public_space, _) = seed_space(&pool, owner, Visibility::Public).await;
    let (internal_space, _) = seed_space(&pool, owner, Visibility::Internal).await;

    let public_title = unique("公开议题");
    let internal_title = unique("内部议题");
    let public_thread = seed_thread(
        &pool,
        public_space,
        owner,
        &public_title,
        Visibility::Public,
    )
    .await;
    let internal_thread = seed_thread(
        &pool,
        internal_space,
        owner,
        &internal_title,
        Visibility::Internal,
    )
    .await;

    let public_doc_title = unique("公开档案");
    let internal_doc_title = unique("内部档案");
    let public_doc = seed_doc(&pool, owner, &public_doc_title, Visibility::Public, true).await;
    let internal_doc = seed_doc(
        &pool,
        owner,
        &internal_doc_title,
        Visibility::Internal,
        true,
    )
    .await;

    let (html, _) = page_token(&app, "/lobby", None).await;
    assert!(
        html.contains(&public_title),
        "公开议题应当出现在前厅：{html}"
    );
    assert!(html.contains(&public_doc_title), "公开档案应当出现在前厅");
    assert!(!html.contains(&internal_title), "内部议题不该出现在前厅");
    assert!(
        !html.contains(&internal_doc_title),
        "内部档案不该出现在前厅"
    );

    for uri in [
        format!("/lobby/forum/{internal_thread}"),
        format!("/lobby/kb/{internal_doc}"),
    ] {
        let response = app
            .clone()
            .oneshot(get_request(&uri, None))
            .await
            .expect("请求失败");
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "{uri} 应当 404");
    }

    for uri in [
        format!("/lobby/forum/{public_thread}"),
        format!("/lobby/kb/{public_doc}"),
    ] {
        let response = app
            .clone()
            .oneshot(get_request(&uri, None))
            .await
            .expect("请求失败");
        assert_eq!(response.status(), StatusCode::OK, "{uri} 应当 200");
    }
}

/// 2. 游客写操作一律 303 去登录页，不是 403——403 会让外人以为自己做错了什么。
#[tokio::test]
async fn guests_are_redirected_to_login_instead_of_forbidden() {
    let Some(url) = db_url() else {
        println!("跳过：未设置 TEST_DATABASE_URL");
        return;
    };
    let (app, pool) = harness(&url).await;
    let owner = make_user(&pool, &unique("owner"), "member").await;
    let (space, _) = seed_space(&pool, owner, Visibility::Public).await;
    let thread = seed_thread(&pool, space, owner, &unique("t"), Visibility::Public).await;

    let (html, cookie) = page_token(&app, "/lobby", None).await;
    let csrf = csrf_of(&html).expect("前厅应当给游客签发 CSRF");

    let cases = [
        (format!("/lobby/forum/{thread}/comments"), "body=hi"),
        (format!("/lobby/forum/{thread}/like"), ""),
        ("/lobby/submit".to_owned(), "title=t&body=b"),
    ];
    for (uri, payload) in cases {
        let body = format!("{payload}&_csrf={csrf}");
        let response = app
            .clone()
            .oneshot(form_request(&uri, &body, Some(&cookie)))
            .await
            .expect("请求失败");
        assert_eq!(response.status(), StatusCode::SEE_OTHER, "{uri} 应当 303");
        let location = location_of(&response);
        assert!(
            location.contains("/login"),
            "{uri} 应当跳到登录页，实际 {location}"
        );
    }
}

/// 3. 外部注册用户（role=user）能评论，详情页立刻出现，且审计一条。
#[tokio::test]
async fn registered_outsider_can_comment_and_the_audit_row_exists() {
    let Some(url) = db_url() else {
        println!("跳过：未设置 TEST_DATABASE_URL");
        return;
    };
    let (app, pool) = harness(&url).await;
    let owner = make_user(&pool, &unique("owner"), "member").await;
    let (space, _) = seed_space(&pool, owner, Visibility::Public).await;
    let thread = seed_thread(&pool, space, owner, &unique("t"), Visibility::Public).await;

    let outsider_name = unique("outsider");
    make_user(&pool, &outsider_name, "user").await;
    let cookie = login(&app, &outsider_name).await;
    let csrf = csrf_from(&app, &format!("/lobby/forum/{thread}"), &cookie).await;

    let text = unique("评论正文");
    let body = format!("body={text}&_csrf={csrf}");
    let response = app
        .clone()
        .oneshot(form_request(
            &format!("/lobby/forum/{thread}/comments"),
            &body,
            Some(&cookie),
        ))
        .await
        .expect("请求失败");
    assert_eq!(response.status(), StatusCode::SEE_OTHER, "评论应当成功");

    let (html, _) = page_token(&app, &format!("/lobby/forum/{thread}"), Some(&cookie)).await;
    assert!(html.contains(&text), "评论应当立刻出现在详情页：{html}");

    let audits = audit_count(&pool, "public.comment_create", "thread", thread).await;
    assert_eq!(audits, 1, "应当写入一条 comment_create 审计");
}

/// 4. 评论超长 → 422，且库里没有新行（校验必须在写库之前）。
#[tokio::test]
async fn comment_over_the_limit_is_rejected_with_422_and_no_row() {
    let Some(url) = db_url() else {
        println!("跳过：未设置 TEST_DATABASE_URL");
        return;
    };
    let (app, pool) = harness(&url).await;
    let owner = make_user(&pool, unique("owner").as_str(), "member").await;
    let (space, _) = seed_space(&pool, owner, Visibility::Public).await;
    let thread = seed_thread(&pool, space, owner, &unique("t"), Visibility::Public).await;

    let before = count_rows(
        &pool,
        "SELECT COUNT(*) FROM public_comments WHERE target_id = ? AND target_type = 'thread'",
        thread,
    )
    .await;

    let name = unique("chatty");
    make_user(&pool, &name, "user").await;
    let cookie = login(&app, &name).await;
    let csrf = csrf_from(&app, &format!("/lobby/forum/{thread}"), &cookie).await;

    let long = "字".repeat(public_service::COMMENT_MAX + 1);
    let body = format!("body={long}&_csrf={csrf}");
    let response = app
        .clone()
        .oneshot(form_request(
            &format!("/lobby/forum/{thread}/comments"),
            &body,
            Some(&cookie),
        ))
        .await
        .expect("请求失败");
    assert_eq!(
        response.status(),
        StatusCode::UNPROCESSABLE_ENTITY,
        "超长评论应当 422"
    );

    let after = count_rows(
        &pool,
        "SELECT COUNT(*) FROM public_comments WHERE target_id = ? AND target_type = 'thread'",
        thread,
    )
    .await;
    assert_eq!(before, after, "被拒的评论不该留下行");
}

/// 5. 删评论：作者与 admin 可以，别人 403；删后不显示但行仍在。
#[tokio::test]
async fn only_the_author_or_an_admin_may_delete_a_comment() {
    let Some(url) = db_url() else {
        println!("跳过：未设置 TEST_DATABASE_URL");
        return;
    };
    let (app, pool) = harness(&url).await;
    let owner = make_user(&pool, &unique("owner"), "member").await;
    let (space, _) = seed_space(&pool, owner, Visibility::Public).await;
    let thread = seed_thread(&pool, space, owner, &unique("t"), Visibility::Public).await;

    let author_name = unique("author");
    let author_id = make_user(&pool, &author_name, "user").await;
    let comment_text = unique("待删评论");
    let comment_id = public_service::comment(
        &pool,
        &CurrentUserFixture::new(author_id, "user").current(),
        None,
        CommentTarget::Thread,
        thread,
        &comment_text,
    )
    .await
    .expect("造评论失败");

    // 别人删 → 403
    let stranger_name = unique("stranger");
    make_user(&pool, &stranger_name, "user").await;
    let stranger_cookie = login(&app, &stranger_name).await;
    let csrf = csrf_from(&app, &format!("/lobby/forum/{thread}"), &stranger_cookie).await;
    let response = app
        .clone()
        .oneshot(form_request(
            &format!("/lobby/comments/{comment_id}/delete"),
            &format!("_csrf={csrf}"),
            Some(&stranger_cookie),
        ))
        .await
        .expect("请求失败");
    assert_eq!(
        response.status(),
        StatusCode::FORBIDDEN,
        "外人删评论应当 403"
    );

    // 作者删 → 303，页面消失，行仍在（deleted_at 非空）
    let author_cookie = login(&app, &author_name).await;
    let csrf = csrf_from(&app, &format!("/lobby/forum/{thread}"), &author_cookie).await;
    let response = app
        .clone()
        .oneshot(form_request(
            &format!("/lobby/comments/{comment_id}/delete"),
            &format!("_csrf={csrf}"),
            Some(&author_cookie),
        ))
        .await
        .expect("请求失败");
    assert_eq!(response.status(), StatusCode::SEE_OTHER, "作者应当能删");

    let (html, _) = page_token(&app, &format!("/lobby/forum/{thread}"), None).await;
    assert!(!html.contains(&comment_text), "删掉的评论不该还显示");

    let alive = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM public_comments WHERE id = ? AND deleted_at IS NULL",
    )
    .bind(comment_id)
    .fetch_one(&pool)
    .await
    .expect("查询失败");
    assert_eq!(alive, 0, "软删后不该还有未删行");

    let total = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM public_comments WHERE id = ?")
        .bind(comment_id)
        .fetch_one(&pool)
        .await
        .expect("查询失败");
    assert_eq!(total, 1, "软删必须保留行");
}

/// 6. 点赞可切换，计数随之变化，且同一人同一目标只有一行。
#[tokio::test]
async fn liking_toggles_and_leaves_exactly_one_row() {
    let Some(url) = db_url() else {
        println!("跳过：未设置 TEST_DATABASE_URL");
        return;
    };
    let (app, pool) = harness(&url).await;
    let owner = make_user(&pool, &unique("owner"), "member").await;
    let (space, _) = seed_space(&pool, owner, Visibility::Public).await;
    let thread = seed_thread(&pool, space, owner, &unique("t"), Visibility::Public).await;

    let name = unique("liker");
    let liker_id = make_user(&pool, &name, "user").await;
    let cookie = login(&app, &name).await;
    let csrf = csrf_from(&app, &format!("/lobby/forum/{thread}"), &cookie).await;

    let one = app
        .clone()
        .oneshot(form_request(
            &format!("/lobby/forum/{thread}/like"),
            &format!("_csrf={csrf}"),
            Some(&cookie),
        ))
        .await
        .expect("请求失败");
    assert_eq!(one.status(), StatusCode::SEE_OTHER);

    let liked = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM public_likes WHERE target_type = 'thread' AND target_id = ? AND user_id = ?",
    )
    .bind(thread)
    .bind(liker_id)
    .fetch_one(&pool)
    .await
    .expect("查询失败");
    assert_eq!(liked, 1, "第一次点赞应当留下 1 行");

    let count = public_service::toggle_like(
        &pool,
        &CurrentUserFixture::new(liker_id, "user").current(),
        CommentTarget::Thread,
        thread,
    )
    .await
    .expect("切换点赞失败");
    assert_eq!(count, (false, 0), "再点一次是取消，计数回到 0");

    let after = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM public_likes WHERE target_type = 'thread' AND target_id = ?",
    )
    .bind(thread)
    .fetch_one(&pool)
    .await
    .expect("查询失败");
    assert_eq!(after, 0, "取消赞必须物理删除，否则唯一索引会挡住下一次点赞");
}

/// 7. 投稿落 pending；`/lobby/mine` 可见；队列 user 403、admin 200。
#[tokio::test]
async fn submissions_land_pending_and_the_queue_is_admin_only() {
    let Some(url) = db_url() else {
        println!("跳过：未设置 TEST_DATABASE_URL");
        return;
    };
    let (app, pool) = harness(&url).await;

    let name = unique("writer");
    make_user(&pool, &name, "user").await;
    let cookie = login(&app, &name).await;
    let csrf = csrf_from(&app, "/lobby/submit", &cookie).await;

    let title = unique("投稿标题");
    let body = format!("title={title}&body=正文内容&_csrf={csrf}");
    let response = app
        .clone()
        .oneshot(form_request("/lobby/submit", &body, Some(&cookie)))
        .await
        .expect("请求失败");
    assert_eq!(response.status(), StatusCode::SEE_OTHER, "投稿应当成功");

    let status: String =
        sqlx::query_scalar("SELECT status FROM public_submissions WHERE title = ?")
            .bind(&title)
            .fetch_one(&pool)
            .await
            .expect("应当查到投稿行");
    assert_eq!(status, "pending", "投稿必须落 pending");

    let (mine, _) = page_token(&app, "/lobby/mine", Some(&cookie)).await;
    assert!(mine.contains(&title), "我的投稿应当能看到");

    let queue_user = app
        .clone()
        .oneshot(get_request("/lobby/queue", Some(&cookie)))
        .await
        .expect("请求失败");
    assert_eq!(
        queue_user.status(),
        StatusCode::FORBIDDEN,
        "队列对 user 403"
    );

    let admin_name = unique("boss");
    make_user(&pool, &admin_name, "admin").await;
    let admin_cookie = login(&app, &admin_name).await;
    let (queue, _) = page_token(&app, "/lobby/queue", Some(&admin_cookie)).await;
    assert!(queue.contains(&title), "admin 的队列应当能看到待审投稿");
}

/// 8. 投稿标题非法 → 422，且回填用户刚写的内容。
#[tokio::test]
async fn submission_title_is_validated_and_echoed_back() {
    let Some(url) = db_url() else {
        println!("跳过：未设置 TEST_DATABASE_URL");
        return;
    };
    let (app, pool) = harness(&url).await;
    let name = unique("writer");
    make_user(&pool, &name, "user").await;
    let cookie = login(&app, &name).await;
    let csrf = csrf_from(&app, "/lobby/submit", &cookie).await;

    let marker = unique("正文标记");
    let body = format!("title=&body={marker}&_csrf={csrf}");
    let response = app
        .clone()
        .oneshot(form_request("/lobby/submit", &body, Some(&cookie)))
        .await
        .expect("请求失败");
    assert_eq!(
        response.status(),
        StatusCode::UNPROCESSABLE_ENTITY,
        "空标题应当 422"
    );
    let html = body_of(response).await;
    assert!(
        html.contains(&marker),
        "422 页面必须回填正文，否则用户得重打一遍"
    );

    let rows =
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM public_submissions WHERE body = ?")
            .bind(&marker)
            .fetch_one(&pool)
            .await
            .expect("查询失败");
    assert_eq!(rows, 0, "被拒的投稿不该落库");
}

/// 9. 前厅分页两页不重叠、越界页空。
#[tokio::test]
async fn lobby_pagination_pages_do_not_overlap() {
    let Some(url) = db_url() else {
        println!("跳过：未设置 TEST_DATABASE_URL");
        return;
    };
    let (app, pool) = harness(&url).await;
    let owner = make_user(&pool, &unique("owner"), "member").await;
    let (space, _) = seed_space(&pool, owner, Visibility::Public).await;

    let marker = unique("page");
    let mut mine = Vec::new();
    for index in 0..21 {
        let title = format!("{marker}-{index:02}");
        let id = seed_thread(&pool, space, owner, &title, Visibility::Public).await;
        mine.push((id, title));
    }

    let collect = |html: &str| -> Vec<String> {
        mine.iter()
            .filter(|(_, title)| html.contains(title.as_str()))
            .map(|(_, title)| title.clone())
            .collect()
    };

    // 只在自己的分区里翻页，不去翻 /lobby：那是全站公开议题的合并列表，
    // 并行用例随时往里插新议题，排序一变，"两页不重叠"就会随机变红——
    // 这不是页面错了，是断言把"并发插入"误判成了"分页重叠"。
    // 分页机制（PAGE_SIZE、越界页、排序）在分区页与全站页是同一套代码。
    let slug = sqlx::query_scalar::<_, String>("SELECT slug FROM forum_spaces WHERE id = ?")
        .bind(space)
        .fetch_one(&pool)
        .await
        .expect("读分区 slug");
    let (page1, _) = page_token(&app, &format!("/forum/{slug}"), None).await;
    let (page2, _) = page_token(&app, &format!("/forum/{slug}?page=2"), None).await;
    let (page99, _) = page_token(&app, &format!("/forum/{slug}?page=99"), None).await;

    let first = collect(&page1);
    let second = collect(&page2);
    assert!(!first.is_empty(), "第一页应当有内容");
    assert!(
        first.iter().all(|title| !second.contains(title)),
        "两页不该出现同一条：{:?} / {:?}",
        first,
        second
    );
    assert!(collect(&page99).is_empty(), "越界页不该有内容");
}

/// 10. 目标不存在、或存在但不可见 → 404（绝不 403）。
#[tokio::test]
async fn missing_or_hidden_targets_are_not_found() {
    let Some(url) = db_url() else {
        println!("跳过：未设置 TEST_DATABASE_URL");
        return;
    };
    let (app, pool) = harness(&url).await;
    let owner = make_user(&pool, &unique("owner"), "member").await;

    let internal_doc = seed_doc(
        &pool,
        owner,
        &unique("internal"),
        Visibility::Internal,
        true,
    )
    .await;
    let draft_doc = seed_doc(&pool, owner, &unique("draft"), Visibility::Public, false).await;

    let missing = [
        "/lobby/forum/99999998".to_owned(),
        "/lobby/kb/99999997".to_owned(),
        format!("/lobby/kb/{internal_doc}"),
        format!("/lobby/kb/{draft_doc}"),
    ];
    for uri in missing {
        let response = app
            .clone()
            .oneshot(get_request(&uri, None))
            .await
            .expect("请求失败");
        assert_eq!(
            response.status(),
            StatusCode::NOT_FOUND,
            "{uri} 必须 404：前厅不能用 403 暴露「有东西但你看不到」"
        );
    }
}

// ---------------------------------------------------------------------------
// 小夹具
// ---------------------------------------------------------------------------

/// 造一个 `CurrentUser`。测试里经常只要"某个 id 的活跃用户"，角色不参与断言。
struct CurrentUserFixture {
    id: u64,
    role: nexus::auth::model::Role,
}

impl CurrentUserFixture {
    fn new(id: u64, role: &str) -> Self {
        let role = match role {
            "admin" => nexus::auth::model::Role::Admin,
            "member" => nexus::auth::model::Role::Member,
            _ => nexus::auth::model::Role::User,
        };
        Self { id, role }
    }

    fn current(&self) -> nexus::auth::policy::CurrentUser {
        nexus::auth::policy::CurrentUser {
            id: self.id,
            role: self.role,
            status: nexus::auth::model::UserStatus::Active,
            group_ids: Vec::new(),
        }
    }
}

/// 数据库层面的护栏：同一个用户插第二条 pending 必须被唯一索引挡住。
///
/// 这条测试故意**绕过 service**（它有一层"先查再插"的友好提示），直接调 model，
/// 因为要验的是"两个人同时点按钮"那条路径——服务层的检查在并发下会同时通过。
/// 迁移 0015 用生成列 + 唯一索引实现：pending 有值、approved/rejected 为 NULL，
/// NULL 不参与唯一性比较，所以历史记录可以有多条。
#[tokio::test]
async fn a_second_pending_join_request_is_rejected_by_the_database() {
    let Some(url) = db_url() else {
        println!("跳过：未设置 TEST_DATABASE_URL");
        return;
    };
    let (_app, pool) = harness(&url).await;

    let name = unique("joiner");
    let user_id = make_user(&pool, &name, "user").await;

    let mut tx = pool.begin().await.expect("开事务");
    nexus::public::model::create_join_request(
        &mut tx,
        nexus::public::model::NewJoinRequest {
            user_id,
            message: "我想加入",
        },
    )
    .await
    .expect("第一条应当成功");
    tx.commit().await.expect("提交");

    let mut tx = pool.begin().await.expect("开事务");
    let second = nexus::public::model::create_join_request(
        &mut tx,
        nexus::public::model::NewJoinRequest {
            user_id,
            message: "我又点了一次",
        },
    )
    .await;
    drop(tx);

    assert!(
        matches!(second, Err(nexus::error::AppError::Conflict(_))),
        "第二条 pending 必须被拒（唯一索引），实际：{second:?}"
    );

    let pending: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM join_requests WHERE user_id = ? AND status = 'pending'",
    )
    .bind(user_id)
    .fetch_one(&pool)
    .await
    .expect("计数");
    assert_eq!(pending, 1, "库里最终只能有一条 pending");
}

/// 页面里带着 CSRF token 和"当前是谁"，一律不许被缓存。
///
/// 少了这个头，浏览器后退就能看到上一个人登录时的页面；而缓存下来的 token
/// 在会话轮换后必然失效，用户只会看到"操作失败"，日志里什么都没有。
#[tokio::test]
async fn pages_are_marked_no_store() {
    let Some(url) = db_url() else {
        println!("跳过：未设置 TEST_DATABASE_URL");
        return;
    };
    let (app, _pool) = harness(&url).await;

    for uri in ["/lobby", "/", "/definitely-missing"] {
        let response = app
            .clone()
            .oneshot(get_request(uri, None))
            .await
            .expect("请求失败");
        let value = response
            .headers()
            .get("cache-control")
            .map(|v| v.to_str().unwrap_or("").to_owned());
        assert_eq!(
            value.as_deref(),
            Some("no-store"),
            "{uri} 必须带 Cache-Control: no-store"
        );
    }
}
