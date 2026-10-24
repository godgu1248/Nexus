//! M4 档案（知识库）的集成测试。
//!
//! 与 forum_flow / group_flow 一样：辅助函数自备一份，不跨测试 crate import；
//! 用例自带数据、跑完不清理（按前缀批量删会误删并行用例的数据，M2 踩过）。

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
use nexus::error::AppError;
use nexus::forum::service as forum_service;
use nexus::kb::model as kb_model;
use nexus::kb::service::{self as kb_service, DocInput};
use nexus::shared::pagination::PageQuery;
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
    format!("kb_{prefix}_{nanos:x}_{n}")
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

fn member(id: u64, groups: Vec<u64>) -> CurrentUser {
    CurrentUser {
        id,
        role: Role::Member,
        status: UserStatus::Active,
        group_ids: groups,
    }
}

fn plain_user(id: u64) -> CurrentUser {
    CurrentUser {
        id,
        role: Role::User,
        status: UserStatus::Active,
        group_ids: Vec::new(),
    }
}

/// 直接插一行文档：测试要精确控制已发布 / 父节点 / 可见性，走 service 反而绕。
///
/// 这里显式豁免 `too_many_arguments`：它是测试夹具，八个参数各自独立且全部由调用点
/// 字面量给出；包成结构体只会把"哪个字段是哪个"藏进构造函数，阅读成本更高、安全性不变。
#[allow(clippy::too_many_arguments)]
async fn seed_doc(
    pool: &MySqlPool,
    author_id: u64,
    parent_id: Option<u64>,
    title: &str,
    body: &str,
    visibility: Visibility,
    group_id: Option<u64>,
    published: bool,
) -> u64 {
    let result = sqlx::query(
        "INSERT INTO kb_docs (parent_id, author_id, title, body, visibility, group_id, is_published, published_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, IF(? = 1, UTC_TIMESTAMP(), NULL))",
    )
    .bind(parent_id)
    .bind(author_id)
    .bind(title)
    .bind(body)
    .bind(visibility.as_str())
    .bind(group_id)
    .bind(i8::from(published))
    .bind(i8::from(published))
    .execute(pool)
    .await
    .expect("插入文档失败");
    result.last_insert_id()
}

async fn seed_group(pool: &MySqlPool, owner_id: u64, slug: &str) -> u64 {
    let result = sqlx::query(
        "INSERT INTO member_groups (name, slug, description, created_by) VALUES (?, ?, '', ?)",
    )
    .bind(slug)
    .bind(slug)
    .bind(owner_id)
    .execute(pool)
    .await
    .expect("插入小组失败");
    result.last_insert_id()
}

async fn add_group_member(pool: &MySqlPool, group_id: u64, user_id: u64) {
    sqlx::query("INSERT INTO group_members (group_id, user_id) VALUES (?, ?)")
        .bind(group_id)
        .bind(user_id)
        .execute(pool)
        .await
        .expect("插入小组成员失败");
}

async fn audit_count(pool: &MySqlPool, action: &str, target_id: u64) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM audit_logs WHERE action = ? AND target_id = ?")
        .bind(action)
        .bind(target_id)
        .fetch_one(pool)
        .await
        .expect("查询审计失败")
}

#[tokio::test]
async fn draft_is_listed_for_its_author_and_hidden_from_other_members() {
    let Some(url) = db_url() else {
        println!("跳过：未设置 TEST_DATABASE_URL");
        return;
    };
    let (app, pool) = harness(&url).await;

    let suffix = unique("draft");
    let author_name = format!("a_{suffix}");
    let other_name = format!("b_{suffix}");
    let author_id = make_user(&pool, &author_name, "member").await;
    make_user(&pool, &other_name, "member").await;

    let title = format!("草稿 {suffix}");
    let doc_id = seed_doc(
        &pool,
        author_id,
        None,
        &title,
        "还没发布",
        Visibility::Internal,
        None,
        false,
    )
    .await;

    let author_cookie = login(&app, &author_name).await;
    let (author_html, _) = page_token(&app, "/kb", Some(&author_cookie)).await;
    assert!(
        author_html.contains(&title),
        "作者应当在自己的档案首页看到草稿；页面开头是：{}",
        author_html.chars().take(400).collect::<String>()
    );

    let other_cookie = login(&app, &other_name).await;
    let (other_html, _) = page_token(&app, "/kb", Some(&other_cookie)).await;
    assert!(
        !other_html.contains(&title),
        "其他成员不该看到别人的草稿：{other_html}"
    );

    // 详情页：作者 200，其他人 404（不是 403——403 等于承认"这里有一篇你看不到的文档"）。
    let response = app
        .clone()
        .oneshot(get_request(&format!("/kb/{doc_id}"), Some(&author_cookie)))
        .await
        .expect("请求失败");
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "作者应当能打开自己的草稿"
    );

    let response = app
        .clone()
        .oneshot(get_request(&format!("/kb/{doc_id}"), Some(&other_cookie)))
        .await
        .expect("请求失败");
    assert_eq!(
        response.status(),
        StatusCode::NOT_FOUND,
        "别人的草稿必须是 404"
    );
}

#[tokio::test]
async fn child_cannot_be_wider_than_its_parent() {
    let Some(url) = db_url() else {
        println!("跳过：未设置 TEST_DATABASE_URL");
        return;
    };
    let (_, pool) = harness(&url).await;

    let suffix = unique("wider");
    let author_name = format!("m_{suffix}");
    let author_id = make_user(&pool, &author_name, "member").await;
    let actor = member(author_id, vec![]);

    let parent_id = seed_doc(
        &pool,
        author_id,
        None,
        "内部父文档",
        "父",
        Visibility::Internal,
        None,
        true,
    )
    .await;

    let err = kb_service::create_doc(
        &pool,
        &actor,
        None,
        DocInput {
            parent_id: Some(parent_id),
            title: "想公开的子文档",
            body: "正文",
            body_html: None,
            visibility: Visibility::Public,
            group_id: None,
            sort_order: 0,
        },
    )
    .await
    .expect_err("子文档比父更宽时必须拒绝");

    assert!(
        matches!(err, AppError::Unprocessable(_)),
        "应当是 422 而不是 500：{err:?}"
    );

    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM kb_docs WHERE parent_id = ?")
        .bind(parent_id)
        .fetch_one(&pool)
        .await
        .expect("计数失败");
    assert_eq!(count, 0, "被拒绝的创建不能留下半截数据");
}

#[tokio::test]
async fn tree_depth_is_capped_at_five() {
    let Some(url) = db_url() else {
        println!("跳过：未设置 TEST_DATABASE_URL");
        return;
    };
    let (_, pool) = harness(&url).await;

    let suffix = unique("depth");
    let author_name = format!("d_{suffix}");
    let author_id = make_user(&pool, &author_name, "member").await;
    let actor = member(author_id, vec![]);

    let mut parent: Option<u64> = None;
    for level in 1..=5 {
        let id = kb_service::create_doc(
            &pool,
            &actor,
            None,
            DocInput {
                parent_id: parent,
                title: &format!("第 {level} 层"),
                body: "内容",
                body_html: None,
                visibility: Visibility::Internal,
                group_id: None,
                sort_order: 0,
            },
        )
        .await
        .unwrap_or_else(|err| panic!("第 {level} 层应当创建成功：{err:?}"));
        parent = Some(id);
    }

    let err = kb_service::create_doc(
        &pool,
        &actor,
        None,
        DocInput {
            parent_id: parent,
            title: "第 6 层",
            body: "内容",
            body_html: None,
            visibility: Visibility::Internal,
            group_id: None,
            sort_order: 0,
        },
    )
    .await
    .expect_err("第 6 层必须被拒绝");

    assert!(
        matches!(err, AppError::Unprocessable(_)),
        "应当是 422：{err:?}"
    );
}

#[tokio::test]
async fn deleting_a_parent_requires_clearing_children_first() {
    let Some(url) = db_url() else {
        println!("跳过：未设置 TEST_DATABASE_URL");
        return;
    };
    let (_, pool) = harness(&url).await;

    let suffix = unique("del");
    let author_name = format!("x_{suffix}");
    let author_id = make_user(&pool, &author_name, "member").await;
    let actor = member(author_id, vec![]);

    let parent_id = seed_doc(
        &pool,
        author_id,
        None,
        "父",
        "父正文",
        Visibility::Internal,
        None,
        true,
    )
    .await;
    let child_id = seed_doc(
        &pool,
        author_id,
        Some(parent_id),
        "子",
        "子正文",
        Visibility::Internal,
        None,
        true,
    )
    .await;

    let err = kb_service::delete_doc(&pool, &actor, None, parent_id)
        .await
        .expect_err("有子文档时不能删父");
    assert!(
        matches!(err, AppError::Unprocessable(_)),
        "应当是 422：{err:?}"
    );

    kb_service::delete_doc(&pool, &actor, None, child_id)
        .await
        .expect("先删子文档");
    kb_service::delete_doc(&pool, &actor, None, parent_id)
        .await
        .expect("子文档清空后可以删父");

    let live: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM kb_docs WHERE id = ? AND deleted_at IS NULL")
            .bind(parent_id)
            .fetch_one(&pool)
            .await
            .expect("计数失败");
    assert_eq!(live, 0, "软删后行还在，但 deleted_at 必须非空");
}

#[tokio::test]
async fn publishing_writes_audit_and_unpublishing_hides_it_again() {
    let Some(url) = db_url() else {
        println!("跳过：未设置 TEST_DATABASE_URL");
        return;
    };
    let (app, pool) = harness(&url).await;

    let suffix = unique("pub");
    let author_name = format!("p_{suffix}");
    let other_name = format!("q_{suffix}");
    let author_id = make_user(&pool, &author_name, "member").await;
    make_user(&pool, &other_name, "member").await;
    let actor = member(author_id, vec![]);

    let title = format!("发布测试 {suffix}");
    let doc_id = seed_doc(
        &pool,
        author_id,
        None,
        &title,
        "正文",
        Visibility::Internal,
        None,
        false,
    )
    .await;

    kb_service::publish(&pool, &actor, None, doc_id, true)
        .await
        .expect("发布应当成功");
    assert_eq!(
        audit_count(&pool, "kb.doc_publish", doc_id).await,
        1,
        "发布要写审计"
    );

    let other_cookie = login(&app, &other_name).await;
    let response = app
        .clone()
        .oneshot(get_request(&format!("/kb/{doc_id}"), Some(&other_cookie)))
        .await
        .expect("请求失败");
    assert_eq!(response.status(), StatusCode::OK, "发布后其他成员应当能看");

    kb_service::publish(&pool, &actor, None, doc_id, false)
        .await
        .expect("撤回应当成功");
    assert_eq!(
        audit_count(&pool, "kb.doc_publish", doc_id).await,
        2,
        "撤回也要写审计"
    );

    let response = app
        .clone()
        .oneshot(get_request(&format!("/kb/{doc_id}"), Some(&other_cookie)))
        .await
        .expect("请求失败");
    assert_eq!(
        response.status(),
        StatusCode::NOT_FOUND,
        "撤回后对他人立即不可见"
    );

    let meta: String = sqlx::query_scalar(
        "SELECT CAST(meta AS CHAR) FROM audit_logs WHERE action = 'kb.doc_publish' AND target_id = ? ORDER BY id DESC LIMIT 1",
    )
    .bind(doc_id)
    .fetch_one(&pool)
    .await
    .expect("读审计失败");
    assert!(
        meta.contains("\"draft\""),
        "撤回的审计要记 to=draft：{meta}"
    );
}

#[tokio::test]
async fn seeding_from_a_thread_creates_a_draft_that_inherits_visibility() {
    let Some(url) = db_url() else {
        println!("跳过：未设置 TEST_DATABASE_URL");
        return;
    };
    let (_, pool) = harness(&url).await;

    let suffix = unique("thread");
    let author_name = format!("t_{suffix}");
    let author_id = make_user(&pool, &author_name, "member").await;
    let actor = member(author_id, vec![]);

    let group_id = seed_group(&pool, author_id, &unique("g")).await;
    let space_slug = unique("space").replace('_', "-");
    let space_id = forum_service::create_space(
        &pool,
        &CurrentUser {
            id: author_id,
            role: Role::Admin,
            status: UserStatus::Active,
            group_ids: vec![group_id],
        },
        None,
        forum_service::NewSpaceInput {
            name: &format!("空间 {suffix}"),
            slug: &space_slug,
            description: "测试分区说明",
            visibility: Visibility::Internal,
        },
    )
    .await
    .expect("建分区失败");

    let thread_title = format!("议题 {suffix}");
    let thread_id = forum_service::create_thread(
        &pool,
        &actor,
        None,
        forum_service::NewThreadInput {
            space_id,
            title: &thread_title,
            body: Some("结论：先出脚本再剪辑。"),
            body_html: None,
        },
    )
    .await
    .expect("建议题失败");

    let doc_id = kb_service::create_from_thread(&pool, &actor, None, thread_id)
        .await
        .expect("从议题沉淀应当成功");

    let doc = kb_model::find_doc(&pool, doc_id)
        .await
        .expect("查询失败")
        .expect("文档应当存在");
    assert_eq!(doc.title, thread_title, "标题应当继承议题");
    assert_eq!(doc.visibility, Visibility::Internal, "可见性应当继承议题");
    assert_eq!(doc.group_id, None, "议题没有小组，文档也不该有");
    assert!(!doc.is_published, "沉淀出来的是草稿");
    assert!(doc.body.contains("先出脚本"), "正文应当来自首楼");

    let meta: String = sqlx::query_scalar(
        "SELECT CAST(meta AS CHAR) FROM audit_logs WHERE action = 'kb.doc_create' AND target_id = ? ORDER BY id DESC LIMIT 1",
    )
    .bind(doc_id)
    .fetch_one(&pool)
    .await
    .expect("读审计失败");
    // MySQL 会把 JSON 列重新序列化（冒号后带空格），断言前先去掉空白，
    // 否则同一份内容因为排版不同就判失败。
    let meta_compact = meta.replace(' ', "");
    assert!(
        meta_compact.contains("\"source\":\"thread\""),
        "来源要记在审计里：{meta}"
    );
    assert!(
        meta_compact.contains(&format!("\"thread_id\":{thread_id}")),
        "审计要能指回议题：{meta}"
    );
}

#[tokio::test]
async fn plain_users_cannot_seed_documents_from_threads() {
    let Some(url) = db_url() else {
        println!("跳过：未设置 TEST_DATABASE_URL");
        return;
    };
    let (_, pool) = harness(&url).await;

    let suffix = unique("noauth");
    let member_name = format!("u1_{suffix}");
    let user_name = format!("u2_{suffix}");
    let member_id = make_user(&pool, &member_name, "member").await;
    let user_id = make_user(&pool, &user_name, "user").await;

    let space_slug = unique("space").replace('_', "-");
    let space_id = forum_service::create_space(
        &pool,
        &CurrentUser {
            id: member_id,
            role: Role::Admin,
            status: UserStatus::Active,
            group_ids: vec![],
        },
        None,
        forum_service::NewSpaceInput {
            name: &format!("公开空间 {suffix}"),
            slug: &space_slug,
            description: "测试分区说明",
            visibility: Visibility::Public,
        },
    )
    .await
    .expect("建分区失败");

    let thread_id = forum_service::create_thread(
        &pool,
        &member(member_id, vec![]),
        None,
        forum_service::NewThreadInput {
            space_id,
            title: "公开议题",
            body: Some("正文"),
            body_html: None,
        },
    )
    .await
    .expect("建议题失败");

    // 只数这个 outsider 名下的文档：全表 COUNT(*) 会被同文件并发用例干扰，随机变红。
    let before: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM kb_docs WHERE author_id = ?")
        .bind(user_id)
        .fetch_one(&pool)
        .await
        .expect("计数失败");

    let err = kb_service::create_from_thread(&pool, &plain_user(user_id), None, thread_id)
        .await
        .expect_err("普通 user 不能沉淀文档");
    assert!(matches!(err, AppError::Forbidden), "应当是 403：{err:?}");

    let after: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM kb_docs WHERE author_id = ?")
        .bind(user_id)
        .fetch_one(&pool)
        .await
        .expect("计数失败");
    assert_eq!(before, after, "被拒绝的沉淀不能留下文档");
}

#[tokio::test]
async fn search_finds_titles_and_bodies_but_never_leaks_group_documents() {
    let Some(url) = db_url() else {
        println!("跳过：未设置 TEST_DATABASE_URL");
        return;
    };
    let (_, pool) = harness(&url).await;

    let suffix = unique("search");
    let author_name = format!("s_{suffix}");
    let author_id = make_user(&pool, &author_name, "member").await;
    let group_id = seed_group(&pool, author_id, &unique("sg")).await;

    let keyword = format!("onlyword{}", suffix.replace('_', ""));
    let public_title = format!("公开 {suffix}");
    let public_id = seed_doc(
        &pool,
        author_id,
        None,
        &public_title,
        &format!("正文里包含 {keyword}"),
        Visibility::Public,
        None,
        true,
    )
    .await;
    let group_title = format!("小组内部 {suffix}");
    let group_doc_id = seed_doc(
        &pool,
        author_id,
        None,
        &group_title,
        &format!("组内也提到 {keyword}"),
        Visibility::Group,
        Some(group_id),
        true,
    )
    .await;

    let outsider = plain_user(author_id + 1000);
    let page = kb_service::search_page(&pool, Some(&outsider), &keyword, &PageQuery { page: 1 })
        .await
        .expect("搜索失败");

    let ids: Vec<u64> = page.items.iter().map(|card| card.id).collect();
    assert!(ids.contains(&public_id), "公开文档应当被搜到");
    assert!(
        !ids.contains(&group_doc_id),
        "小组文档不该出现在局外人的搜索结果里"
    );

    // 作者本人能搜到自己的组文档——前提是成员关系真的落在库里。
    // 搜索 SQL 判的是 `group_members` 里的行，不是内存里的 group_ids：
    // 两条路径（policy 与 model）都指向同一个事实来源，测试也必须照这个事实造数据。
    add_group_member(&pool, group_id, author_id).await;
    let insider = member(author_id, vec![group_id]);
    let page = kb_service::search_page(&pool, Some(&insider), &keyword, &PageQuery { page: 1 })
        .await
        .expect("搜索失败");
    let ids: Vec<u64> = page.items.iter().map(|card| card.id).collect();
    assert!(ids.contains(&group_doc_id), "组员应当能搜到组文档");

    // 正文命中也要算：只有关键词出现在 body 里的那条。
    let body_only = format!("bodyonly{}", suffix.replace('_', ""));
    let body_doc = seed_doc(
        &pool,
        author_id,
        None,
        &format!("标题里没有关键词 {suffix}"),
        &format!("这里才出现 {body_only}"),
        Visibility::Public,
        None,
        true,
    )
    .await;
    let page = kb_service::search_page(&pool, None, &body_only, &PageQuery { page: 1 })
        .await
        .expect("搜索失败");
    assert_eq!(page.total, 1, "正文命中应当算作结果");
    assert_eq!(page.items[0].id, body_doc);
}

#[tokio::test]
async fn only_the_author_or_an_admin_can_edit_publish_or_delete() {
    let Some(url) = db_url() else {
        println!("跳过：未设置 TEST_DATABASE_URL");
        return;
    };
    let (app, pool) = harness(&url).await;

    let suffix = unique("owner");
    let author_name = format!("o1_{suffix}");
    let other_name = format!("o2_{suffix}");
    let author_id = make_user(&pool, &author_name, "member").await;
    make_user(&pool, &other_name, "member").await;

    let doc_id = seed_doc(
        &pool,
        author_id,
        None,
        &format!("归属 {suffix}"),
        "正文",
        Visibility::Internal,
        None,
        true,
    )
    .await;

    let other_cookie = login(&app, &other_name).await;
    let (html, cookie) = page_token(&app, &format!("/kb/{doc_id}"), Some(&other_cookie)).await;
    let csrf = csrf_of(&html).expect("详情页应当带 CSRF token");

    for (uri, body) in [
        (
            format!("/kb/{doc_id}/edit"),
            format!("title=改掉&body=改掉&visibility=internal&_csrf={csrf}"),
        ),
        (
            format!("/kb/{doc_id}/publish"),
            format!("value=0&_csrf={csrf}"),
        ),
        (format!("/kb/{doc_id}/delete"), format!("_csrf={csrf}")),
    ] {
        let response = app
            .clone()
            .oneshot(form_request(&uri, &body, Some(&cookie)))
            .await
            .expect("请求失败");
        assert_eq!(response.status(), StatusCode::FORBIDDEN, "{uri} 应当被拒绝");
    }

    let row: (String, i8) = sqlx::query_as("SELECT title, is_published FROM kb_docs WHERE id = ?")
        .bind(doc_id)
        .fetch_one(&pool)
        .await
        .expect("读回文档失败");
    assert!(row.0.starts_with("归属"), "标题不该被改动：{}", row.0);
    assert_eq!(row.1, 1, "发布状态不该被改动");
}

/// 置顶优先、置顶之间按 pin_order、其余按浏览/点赞/时间。
/// 这里只钉住"置顶一定排在没置顶的前面，且 pin_order 小的在前"——
/// 热度那部分涉及浏览数与点赞，混进来会让这条测试变成计时器测试。
#[tokio::test]
async fn pinned_documents_come_first_in_pin_order() {
    let Some(url) = db_url() else {
        println!("跳过：没有 TEST_DATABASE_URL");
        return;
    };
    let (app, pool) = harness(&url).await;
    let owner = make_user(&pool, &unique("pin_owner"), "member").await;

    let doc_a = seed_doc(
        &pool,
        owner,
        None,
        &unique("pin_a"),
        "a",
        Visibility::Internal,
        None,
        true,
    )
    .await;
    let doc_b = seed_doc(
        &pool,
        owner,
        None,
        &unique("pin_b"),
        "b",
        Visibility::Internal,
        None,
        true,
    )
    .await;
    let doc_c = seed_doc(
        &pool,
        owner,
        None,
        &unique("pin_c"),
        "c",
        Visibility::Internal,
        None,
        true,
    )
    .await;

    let admin_name = unique("pin_admin");
    make_user(&pool, &admin_name, "admin").await;
    let admin_cookie = login(&app, &admin_name).await;

    // c 置顶到 0，b 置顶到 1：期望顺序 c、b，a 落回普通区。
    for (id, order) in [(doc_c, 0), (doc_b, 1)] {
        let (html, cookie) = page_token(&app, &format!("/kb/{id}"), Some(&admin_cookie)).await;
        let token = csrf_of(&html).expect("详情页应当带 csrf");
        let body = format!("value=on&order={order}&_csrf={token}");
        let response = app
            .clone()
            .oneshot(form_request(&format!("/kb/{id}/pin"), &body, Some(&cookie)))
            .await
            .expect("置顶请求失败");
        assert_eq!(response.status(), StatusCode::SEE_OTHER, "置顶应 303");
    }

    let (html, _) = page_token(&app, "/kb", Some(&admin_cookie)).await;
    let pos_c = html.find(&format!("/kb/{doc_c}")).expect("找不到 c");
    let pos_b = html.find(&format!("/kb/{doc_b}")).expect("找不到 b");
    let pos_a = html.find(&format!("/kb/{doc_a}")).expect("找不到 a");
    assert!(pos_c < pos_b, "pin_order 0 必须排在 1 前面");
    assert!(pos_b < pos_a, "置顶必须排在未置顶前面");

    // 取消置顶后回到普通区（顺序不再保证，但徽章与排序标记要消失）。
    let (html, cookie) = page_token(&app, &format!("/kb/{doc_c}"), Some(&admin_cookie)).await;
    let token = csrf_of(&html).expect("详情页应当带 csrf");
    let body = format!("value=off&order=0&_csrf={token}");
    let response = app
        .clone()
        .oneshot(form_request(
            &format!("/kb/{doc_c}/pin"),
            &body,
            Some(&cookie),
        ))
        .await
        .expect("取消置顶失败");
    assert_eq!(response.status(), StatusCode::SEE_OTHER);

    let pinned: i8 = sqlx::query_scalar("SELECT is_pinned FROM kb_docs WHERE id = ?")
        .bind(doc_c)
        .fetch_one(&pool)
        .await
        .expect("查置顶标记失败");
    assert_eq!(pinned, 0, "取消置顶后 is_pinned 必须归零");
    // 列表上必须能看出这条是置顶的，否则"排在最前"和"碰巧刚更新"无法区分。
    let list_html = body_of(
        app.clone()
            .oneshot(get_request("/kb", Some(&admin_cookie)))
            .await
            .expect("列表失败"),
    )
    .await;
    assert!(
        list_html.contains("badge--pinned"),
        "置顶文档在列表里必须有置顶徽章"
    );
}

/// 置顶是 admin 的内容治理动作，普通成员连试都不该试成。
#[tokio::test]
async fn non_admin_cannot_pin() {
    let Some(url) = db_url() else {
        println!("跳过：没有 TEST_DATABASE_URL");
        return;
    };
    let (app, pool) = harness(&url).await;
    let author = make_user(&pool, &unique("pin_denied"), "member").await;
    let doc = seed_doc(
        &pool,
        author,
        None,
        &unique("pin_denied_doc"),
        "b",
        Visibility::Internal,
        None,
        true,
    )
    .await;

    let name = unique("pin_denied2");
    let user_id = make_user(&pool, &name, "member").await;
    let _ = user_id;
    let cookie = login(&app, &name).await;

    let (html, cookie) = page_token(&app, &format!("/kb/{doc}"), Some(&cookie)).await;
    let token = csrf_of(&html).expect("详情页应当带 csrf");
    let body = format!("value=on&order=0&_csrf={token}");
    let response = app
        .oneshot(form_request(
            &format!("/kb/{doc}/pin"),
            &body,
            Some(&cookie),
        ))
        .await
        .expect("置顶请求失败");
    assert_eq!(
        response.status(),
        StatusCode::FORBIDDEN,
        "非 admin 置顶必须 403"
    );
}

/// 来宾（自助注册的 `role=user`）不该从列表、最近、搜索这三条"侧门"看到 internal 档案。
///
/// 这条测试钉的是 `kb::model::Viewer::internal_ok`：修之前这三条 SQL 只判
/// "已登录"，于是详情页 404、列表里却躺着标题——用户看到的是"看得见、点不进"。
/// 详情页那条路径本来就是对的（`visible_detail` → `can_see` → policy），
/// 所以这里同时断言三个入口，防止下次只修一处。
#[tokio::test]
async fn plain_users_never_see_internal_docs_in_list_recent_or_search() {
    let Some(url) = db_url() else {
        println!("跳过：未设置 TEST_DATABASE_URL");
        return;
    };
    let (app, pool) = harness(&url).await;

    let suffix = unique("leak");
    let author_name = format!("ml_{suffix}");
    let guest_name = format!("gl_{suffix}");
    let author_id = make_user(&pool, &author_name, "member").await;
    make_user(&pool, &guest_name, "user").await;

    let keyword = format!("inner{}", suffix.replace('_', ""));
    let title = format!("内部档案 {suffix}");
    let doc_id = seed_doc(
        &pool,
        author_id,
        None,
        &title,
        &format!("只有成员能看 {keyword}"),
        Visibility::Internal,
        None,
        true,
    )
    .await;

    // 正向对照：成员看得见。少了这一半，红色可能来自"根本没查出来"而不是"过滤对了"。
    let author_cookie = login(&app, &author_name).await;
    let (author_html, _) = page_token(&app, "/kb", Some(&author_cookie)).await;
    assert!(
        author_html.contains(&title),
        "成员应当能在档案列表里看到内部档案"
    );
    let (author_search, _) = page_token(
        &app,
        &format!("/kb/search?q={keyword}"),
        Some(&author_cookie),
    )
    .await;
    assert!(
        author_search.contains(&title),
        "成员应当能搜到内部档案；页面开头是：{}",
        author_search.chars().take(600).collect::<String>()
    );

    // 来宾：列表看不见、搜索搜不到、详情 404（三者必须一致）。
    let guest_cookie = login(&app, &guest_name).await;
    let (guest_html, _) = page_token(&app, "/kb", Some(&guest_cookie)).await;
    assert!(
        !guest_html.contains(&title),
        "来宾不该在档案列表里看到内部档案"
    );
    let (guest_search, _) = page_token(
        &app,
        &format!("/kb/search?q={keyword}"),
        Some(&guest_cookie),
    )
    .await;
    assert!(
        !guest_search.contains(&title),
        "来宾不该搜到内部档案：{guest_search}"
    );

    let response = app
        .clone()
        .oneshot(get_request(&format!("/kb/{doc_id}"), Some(&guest_cookie)))
        .await
        .expect("请求失败");
    assert_eq!(
        response.status(),
        StatusCode::NOT_FOUND,
        "来宾打开内部档案必须是 404"
    );
}
