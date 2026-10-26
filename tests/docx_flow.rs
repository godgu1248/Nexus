//! Word 文档上传与内嵌查看器的集成测试（DESIGN §41）。
//!
//! 这一批只验四件事，都是"上传 .docx"这条链路上真正会出事的地方：
//! 1. 上传能拿到一个 `/doc/<sha256>` 地址，正文文字进得去；
//! 2. docx 里写成转义尖括号的脚本片段在查看器里仍然是**文字**，不是标签；
//! 3. 改了扩展名的非 docx 被挡住（422，不是 500）；
//! 4. 不存在的 sha 是 404（不是 500、也不是把空页面渲染成 200）。
//!
//! 为什么要自己造 .docx 而不是放个二进制夹具：一个 6 KB 的 zip 在仓库里既看不出内容、
//! 也不能一眼确认它测的是什么。这里用 zip 现场拼最小结构，XML 就在测试里，读得懂。

use std::io::{Cursor, Write};
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
use zip::write::SimpleFileOptions;

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
    format!("dx_{prefix}_{nanos:x}_{n}")
}

fn config_for(database_url: &str) -> Config {
    let uploads = std::env::temp_dir().join("nexus-test-docx");
    Config::from_source(|key| match key {
        "DATABASE_URL" => Some(database_url.to_owned()),
        "APP_BASE_URL" => Some("http://127.0.0.1:8080".to_owned()),
        "APP_ENV" => Some("dev".to_owned()),
        "UPLOAD_DIR" => Some(uploads.display().to_string()),
        _ => None,
    })
    .expect("测试配置应当合法")
}

async fn harness(url: &str) -> (Router, MySqlPool) {
    let config = config_for(url);
    let pool = nexus::db::init_pool(&config).await.expect("测试库连接失败");
    let state = AppState::new(pool.clone(), config);
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

fn docx_request(uri: &str, bytes: &[u8], cookie: &str, csrf: &str) -> Request<Body> {
    const BOUNDARY: &str = "----nexusdocxboundary";
    let mut body = Vec::new();
    body.extend_from_slice(
        format!(
            "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"测试.docx\"\r\n\
             Content-Type: application/vnd.openxmlformats-officedocument.wordprocessingml.document\r\n\r\n"
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

/// 现场拼一个最小可用 .docx：只写解析器真正需要的 `word/document.xml`，
/// 外加两个 Word 一定会有的元数据部件（缺了也不影响解析，但缺了就不像真文件）。
fn build_docx(body_xml: &str) -> Vec<u8> {
    let document = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
<w:body>{body_xml}</w:body>
</w:document>"#
    );
    let content_types = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
<Default Extension="xml" ContentType="application/xml"/>
<Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/>
</Types>"#;

    let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
    let options = SimpleFileOptions::default();
    for (name, data) in [
        ("[Content_Types].xml", content_types),
        ("word/document.xml", document.as_str()),
    ] {
        writer
            .start_file(name, options)
            .expect("写入 docx 条目失败");
        writer
            .write_all(data.as_bytes())
            .expect("写入 docx 内容失败");
    }
    writer.finish().expect("收尾 docx 失败").into_inner()
}

fn paragraph(text: &str) -> String {
    format!(r#"<w:p><w:r><w:t xml:space="preserve">{text}</w:t></w:r></w:p>"#)
}

fn require_db() -> Option<String> {
    std::env::var("TEST_DATABASE_URL")
        .ok()
        .filter(|value| !value.is_empty())
}

// ---------------------------------------------------------------------------
// 用例
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_uploaded_docx_is_stored_and_rendered_by_the_viewer() {
    let Some(url) = require_db() else {
        println!("跳过：未设置 TEST_DATABASE_URL");
        return;
    };
    let (app, pool) = harness(&url).await;
    let username = unique("up");
    make_user(&pool, &username, "member").await;
    let cookie = login(&app, &username).await;
    let (html, cookie) = page_token(&app, "/", Some(&cookie)).await;
    let csrf = csrf_of(&html).expect("首页应有 CSRF token");

    let docx = build_docx(&format!(
        "{}{}",
        paragraph("第一段正文"),
        paragraph("第二段正文")
    ));
    let response = app
        .clone()
        .oneshot(docx_request("/media/docx", &docx, &cookie, &csrf))
        .await
        .expect("请求失败");
    // 先取状态与正文再断言：非 200 时把服务端返回的原因带进断言消息，
    // 否则失败信息只有 "left: 500 right: 200"，还得再跑一次才知道为什么。
    let status = response.status();
    let body = body_of(response).await;
    assert_eq!(status, StatusCode::OK, "上传应当成功，实际响应：{body}");

    let json: serde_json::Value = serde_json::from_str(&body).expect("响应应当是 JSON");
    let viewer = json["url"].as_str().expect("响应里应有 url");
    assert!(
        viewer.starts_with("/doc/") && viewer.len() == 5 + 64,
        "查看器地址应当形如 /doc/<sha256>，实际 {viewer}"
    );

    let page = app
        .clone()
        .oneshot(get_request(viewer, None))
        .await
        .expect("请求失败");
    assert_eq!(page.status(), StatusCode::OK, "查看器应当能打开");
    let html = body_of(page).await;
    assert!(html.contains("第一段正文"), "查看器里应当有正文");
    assert!(html.contains("第二段正文"), "查看器里应当有全部段落");
}

#[tokio::test]
async fn escaped_script_text_in_a_docx_stays_text() {
    let Some(url) = require_db() else {
        println!("跳过：未设置 TEST_DATABASE_URL");
        return;
    };
    let (app, pool) = harness(&url).await;
    let username = unique("hostile");
    make_user(&pool, &username, "member").await;
    let cookie = login(&app, &username).await;
    let (html, cookie) = page_token(&app, "/", Some(&cookie)).await;
    let csrf = csrf_of(&html).expect("首页应有 CSRF token");

    // Word 里写 `<script>alert(1)</script>` 这段字面文字时，document.xml 里是 &lt;script&gt;…
    // 解析器必须把它当文字还原并重新转义，而不是还原成标签。
    let docx = build_docx(&paragraph("&lt;script&gt;alert(1)&lt;/script&gt;"));
    let response = app
        .clone()
        .oneshot(docx_request("/media/docx", &docx, &cookie, &csrf))
        .await
        .expect("请求失败");
    assert_eq!(response.status(), StatusCode::OK);
    let json: serde_json::Value =
        serde_json::from_str(&body_of(response).await).expect("响应应当是 JSON");
    let viewer = json["url"].as_str().expect("响应里应有 url");

    let html = body_of(
        app.clone()
            .oneshot(get_request(viewer, None))
            .await
            .expect("请求失败"),
    )
    .await;
    assert!(
        !html.contains("<script>alert(1)</script>"),
        "docx 里的脚本文字不能被当成标签输出"
    );
    assert!(
        html.contains("&lt;script&gt;") || html.contains("script&gt;alert"),
        "原文应当以转义形式留在页面上"
    );
}

#[tokio::test]
async fn a_file_that_is_not_a_docx_is_rejected_with_422() {
    let Some(url) = require_db() else {
        println!("跳过：未设置 TEST_DATABASE_URL");
        return;
    };
    let (app, pool) = harness(&url).await;
    let username = unique("fake");
    make_user(&pool, &username, "member").await;
    let cookie = login(&app, &username).await;
    let (html, cookie) = page_token(&app, "/", Some(&cookie)).await;
    let csrf = csrf_of(&html).expect("首页应有 CSRF token");

    // 一张 PNG 改名叫 .docx：魔数不是 zip，必须在解析前挡住。
    let png = b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR".to_vec();
    let response = app
        .clone()
        .oneshot(docx_request("/media/docx", &png, &cookie, &csrf))
        .await
        .expect("请求失败");
    assert_eq!(
        response.status(),
        StatusCode::UNPROCESSABLE_ENTITY,
        "不是 docx 的文件应当被拒绝"
    );
}

#[tokio::test]
async fn an_unknown_sha_in_the_viewer_is_404() {
    let Some(url) = require_db() else {
        println!("跳过：未设置 TEST_DATABASE_URL");
        return;
    };
    let (app, _pool) = harness(&url).await;
    let response = app
        .clone()
        .oneshot(get_request(&format!("/doc/{}", "a".repeat(64)), None))
        .await
        .expect("请求失败");
    assert_eq!(response.status(), StatusCode::NOT_FOUND);

    // 形状不对的 sha 同样 404：不能让 `/doc/..%2f..%2fetc%2fpasswd` 这类输入进到查库。
    let malformed = app
        .clone()
        .oneshot(get_request("/doc/not-a-sha", None))
        .await
        .expect("请求失败");
    assert_eq!(malformed.status(), StatusCode::NOT_FOUND);
}
