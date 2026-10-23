//! 请求体上限（`REQUEST_BODY_LIMIT`）的集成测试。
//!
//! 为什么不在主路由上测：M0 只有 `GET /healthz`、`GET /` 和 404 兜底，**没有任何会读请求体
//! 的路由**。而 `RequestBodyLimitLayer` 的工作方式是"限制读取"——没人读 body 时它永远不会
//! 报错，POST 到 `GET` 路由只会拿回 405。所以在主路由上发大 body 只能证明"405 很快"，
//! 证明不了 413。
//!
//! 因此这里用**与 `build_router` 完全相同的层与完全相同的上限值**（从 `Config` 读，不写死
//! 1024*1024）搭一个最小路由，让一个真正读 body 的 handler 去触发上限。这样测的是"这个层
//! 装在这个位置、用这个阈值时会发生什么"，而不是"我能不能凑出一个 413"。

use axum::Router;
use axum::body::{Body, Bytes};
use axum::http::{Request, StatusCode};
use axum::routing::post;
use http_body_util::BodyExt;
use nexus::config::Config;
use tower::ServiceExt;
use tower_http::limit::RequestBodyLimitLayer;

/// 合法配置的最小集合：生产必填项都给了，其余走默认值。
fn config() -> Config {
    Config::from_source(|key| match key {
        "DATABASE_URL" => Some(
            "mysql://nexus:pw@127.0.0.1:3306/nexus?ssl-mode=verify_ca&ssl-ca=/tmp/ca.pem"
                .to_string(),
        ),
        "APP_BASE_URL" => Some("http://127.0.0.1:8080".to_string()),
        _ => None,
    })
    .expect("配置应合法")
}

async fn echo_body(body: Bytes) -> String {
    format!("读了 {} 字节", body.len())
}

fn app(limit: usize) -> Router {
    Router::new()
        .route("/echo", post(echo_body))
        .layer(RequestBodyLimitLayer::new(limit))
}

fn post_with_len(len: usize) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/echo")
        .header("content-type", "application/octet-stream")
        .body(Body::from(vec![b'a'; len]))
        .expect("构造请求")
}

#[tokio::test]
async fn config_default_limit_is_one_mebibyte() {
    // 断言的是"测试用的阈值来自配置默认值"，不是另一个魔法数。
    assert_eq!(config().request_body_limit, 1024 * 1024);
}

#[tokio::test]
async fn body_at_limit_passes_and_is_fully_read() {
    let cfg = config();
    let response = app(cfg.request_body_limit)
        .oneshot(post_with_len(cfg.request_body_limit))
        .await
        .expect("请求应被处理");

    assert_eq!(response.status(), StatusCode::OK);
    let body = response
        .into_body()
        .collect()
        .await
        .expect("读取响应体")
        .to_bytes();
    let text = std::str::from_utf8(&body).expect("utf8");
    assert!(
        text.contains(&cfg.request_body_limit.to_string()),
        "handler 应该收到完整长度，实际：{text}"
    );
}

#[tokio::test]
async fn body_over_limit_is_rejected_with_413() {
    let cfg = config();
    let response = app(cfg.request_body_limit)
        .oneshot(post_with_len(cfg.request_body_limit + 1))
        .await
        .expect("请求应被处理"); // 上限是"拒绝"，不是"连接被掐断"

    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
}

#[tokio::test]
async fn oversize_body_does_not_reach_the_handler() {
    // 413 有可能来自 handler 之后（比如先读完再校验），这里把 handler 的输出做成可辨识的
    // 文本，确认它没被执行。
    let cfg = config();
    let response = app(cfg.request_body_limit)
        .oneshot(post_with_len(cfg.request_body_limit * 2))
        .await
        .expect("请求应被处理");

    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    let body = response
        .into_body()
        .collect()
        .await
        .expect("读取响应体")
        .to_bytes();
    let text = String::from_utf8_lossy(&body);
    assert!(
        !text.contains("读了"),
        "handler 不应被执行，实际响应体：{text}"
    );
}
