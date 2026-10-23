//! 进程入口。**只做装配**：读配置 → 起日志 → 建池 → 迁移 → 装配路由 → 起服务。
//!
//! 路由装配本身在 `nexus::app` 里，不在这里——`tests/` 下的集成测试只能链接库 crate，
//! 装配逻辑留在 bin 里，测试就只能自己再拼一遍路由，那份"测试专用路由"迟早和线上分叉。

use std::process::ExitCode;

use tracing_subscriber::{EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};

use nexus::{app::build_router, config::Config, db, error::AppError, state::AppState};

#[tokio::main]
async fn main() -> ExitCode {
    // .env 只服务于本地开发；生产由 systemd 注入环境变量，文件不存在不算错误。
    let _ = dotenvy::dotenv();

    let config = match Config::from_env() {
        Ok(config) => config,
        Err(err) => {
            // 此刻日志系统还没初始化，配置错误只能走 stderr。
            eprintln!("启动失败：配置不合法 —— {err}");
            return ExitCode::FAILURE;
        }
    };

    init_tracing(&config.log_level);

    match run(config).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            tracing::error!(error = ?err, "启动失败");
            eprintln!("启动失败：{err}");
            ExitCode::FAILURE
        }
    }
}

/// 日志：结构化输出到 stdout（systemd 用 journald 收），级别由 `LOG_LEVEL` 决定。
///
/// `RUST_LOG` 存在时优先——排障时想临时开一个模块的 debug，不必改配置重启服务。
fn init_tracing(level: &str) {
    let filter = EnvFilter::try_from_default_env()
        .or_else(|_| EnvFilter::try_new(level))
        .unwrap_or_else(|_| EnvFilter::new("info"));

    tracing_subscriber::registry()
        .with(filter)
        .with(tracing_subscriber::fmt::layer().with_target(true))
        .init();
}

async fn run(config: Config) -> Result<(), AppError> {
    let bind_addr = config.bind_addr;

    // 建池与迁移失败就拒绝启动。带病启动比启动失败更糟：用户会看到 500，
    // 而我们得到的是一个"服务活着但全坏"的模糊信号。
    //
    // 迁移在 init_pool 里跑完了（见 src/db.rs 与 DESIGN §27.13）：早先这里还多调一次
    // db::migrate，结果每次启动都打两遍"数据库迁移已完成"，日志里像有两条链路。
    // 现在只留 init_pool 一条，测试里 `harness()` 也因此天然带上 schema。
    let pool = db::init_pool(&config).await?;

    // 首个管理员引导。只在库里一个 admin 都没有时生效——这是 M1 之后唯一的建 admin 途径，
    // 没有它 M7 的审批界面永远没人能打开（见 DESIGN §19 A13）。
    if let (Some(username), Some(password)) = (
        config.bootstrap_admin_username.as_deref(),
        config.bootstrap_admin_password.as_deref(),
    ) {
        match nexus::auth::service::ensure_bootstrap_admin(&pool, username, password).await? {
            nexus::auth::service::BootstrapOutcome::Created { user_id } => {
                tracing::info!(user_id, "已按环境变量创建首个管理员");
            }
            nexus::auth::service::BootstrapOutcome::Skipped => {
                tracing::debug!("库中已存在管理员，跳过引导");
            }
        }
    }

    let state = AppState::new(pool, config);
    let app = build_router(state);

    let listener = tokio::net::TcpListener::bind(bind_addr)
        .await
        .map_err(|err| {
            AppError::internal(anyhow::Error::new(err).context(format!("绑定 {bind_addr} 失败")))
        })?;

    tracing::info!(%bind_addr, "Nexus 已启动");

    // `into_make_service_with_connect_info` 不是可选项：登录失败计数要按 IP 记，
    // 不带连接信息时 `ConnectInfo<SocketAddr>` 提取器拿不到对端地址，
    // 限速就会退化成"只按用户名"，攻击者换个用户名就是一格新的计数。
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal())
    .await
    .map_err(|err| AppError::internal(anyhow::Error::new(err).context("HTTP 服务异常退出")))?;

    tracing::info!("已优雅退出");
    Ok(())
}

/// 优雅关闭：Ctrl-C 或（Linux 上）systemd 发的 SIGTERM。
async fn shutdown_signal() {
    let ctrl_c = async {
        if let Err(err) = tokio::signal::ctrl_c().await {
            tracing::error!(error = %err, "监听 Ctrl-C 失败");
            // 监听失败就让它永远挂着，避免这里立刻返回导致服务被误停。
            std::future::pending::<()>().await;
        }
    };

    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut stream) => {
                stream.recv().await;
            }
            Err(err) => {
                tracing::error!(error = %err, "监听 SIGTERM 失败");
                std::future::pending::<()>().await;
            }
        }
    };

    // Windows 上没有 SIGTERM；systemd 的生产环境才需要它。
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = ctrl_c => {},
        () = terminate => {},
    }

    tracing::info!("收到退出信号，开始优雅关闭");
}
