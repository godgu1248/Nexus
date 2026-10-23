//! 数据库连接池与迁移。
//!
//! 两条硬要求（DESIGN.md §6.2 / §6.3）：
//! 1. **每个连接建立后立刻 `SET time_zone='+00:00'`**。实测服务器默认时区下
//!    `CURRENT_TIMESTAMP` 比 `UTC_TIMESTAMP()` 快 28800 秒；漏了这一步，`TIMESTAMP`
//!    列读回值会整体漂 8 小时，而且不报错——这是最贵的一类 bug，因为它静默。
//! 2. **迁移失败即拒绝启动**。表结构没对齐就对外服务，等于把 500 留给用户。

use crate::config::Config;
use crate::error::AppError;
use sqlx::Executor;
use sqlx::MySqlPool;
use sqlx::mysql::MySqlPoolOptions;

/// 建立连接池并跑完迁移。
///
/// 顺序是有意的：先建池（含 after_connect）→ 再迁移。迁移用的也是池里的连接，
/// 所以它同样带着 UTC 会话时区。
pub async fn init_pool(config: &Config) -> Result<MySqlPool, AppError> {
    // 首次连库允许重试：容器化数据库（或刚重启的实例）常在服务起来的头一两秒
    // 还没开始监听；一次失败就让整个进程退出，只会把重启次数堆成故障曲线。
    // 重试次数刻意很小——真正的配置错误（口令、ssl-mode、库名）重试也连不上，
    // 早失败早暴露，别把启动时间拖成黑箱。
    const CONNECT_ATTEMPTS: u32 = 3;
    const CONNECT_BACKOFF: std::time::Duration = std::time::Duration::from_millis(250);

    let mut last_error = None;
    let mut connected = None;
    for attempt in 1..=CONNECT_ATTEMPTS {
        let result = MySqlPoolOptions::new()
            .max_connections(config.db_max_connections)
            .min_connections(1)
            // 2核2G 上超时给足，但不无限等：拿不到连接就快速失败，别把请求堆在队列里。
            .acquire_timeout(std::time::Duration::from_secs(10))
            .idle_timeout(std::time::Duration::from_secs(600))
            .max_lifetime(std::time::Duration::from_secs(1800))
            .after_connect(|conn, _meta| {
                Box::pin(async move {
                    conn.execute("SET time_zone = '+00:00'").await?;
                    Ok(())
                })
            })
            .connect(&config.database_url)
            .await;

        match result {
            Ok(pool) => {
                connected = Some(pool);
                break;
            }
            Err(err) => {
                // 保留原始错误链：日志要看得到根因，响应侧由 AppError::Database 统一模糊化。
                // （口令在 DATABASE_URL 里，但 sqlx 的错误文本不含完整连接串，已实测。）
                tracing::warn!(
                    error = ?err,
                    attempt,
                    of = CONNECT_ATTEMPTS,
                    "数据库连接失败，准备重试"
                );
                last_error = Some(err);
                if attempt < CONNECT_ATTEMPTS {
                    tokio::time::sleep(CONNECT_BACKOFF).await;
                }
            }
        }
    }

    let pool = match connected {
        Some(pool) => pool,
        None => {
            let err = last_error.expect("重试循环结束时必然留下最后一次错误");
            tracing::error!(error = ?err, "数据库连接失败，放弃启动");
            return Err(AppError::Database(err));
        }
    };

    tracing::info!(
        max_connections = config.db_max_connections,
        "数据库连接池已建立"
    );

    migrate(&pool).await?;
    Ok(pool)
}

/// 跑 migrations 目录下的迁移。sqlx 自己维护 `_sqlx_migrations` 表，重复执行是幂等的。
pub async fn migrate(pool: &MySqlPool) -> Result<(), AppError> {
    sqlx::migrate!("./migrations")
        .run(pool)
        .await
        .map_err(|err| {
            tracing::error!(error = ?err, "数据库迁移失败，拒绝启动");
            AppError::internal(anyhow::Error::new(err).context("数据库迁移失败"))
        })?;
    tracing::info!("数据库迁移已完成");
    Ok(())
}

/// 健康检查用：确认连接池真的能拿到连接并执行语句。
///
/// 刻意不走缓存、不只看池状态——池里有一百个空闲连接但数据库已经不可写的情况是存在的。
pub async fn ping(pool: &MySqlPool) -> Result<(), AppError> {
    sqlx::query_scalar::<_, i32>("SELECT 1")
        .fetch_one(pool)
        .await
        .map(|_| ())
        .map_err(|err| {
            tracing::error!(error = %err, "健康检查失败");
            AppError::Database(err)
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    /// 测试配置：把 TEST_DATABASE_URL 当作 DATABASE_URL 喂进去，其余走默认。
    fn test_config() -> Option<Config> {
        let url = std::env::var("TEST_DATABASE_URL").ok()?;
        if url.trim().is_empty() {
            return None;
        }
        Config::from_source(|key| match key {
            "DATABASE_URL" => Some(url.clone()),
            "APP_BASE_URL" => Some("http://localhost:8080".to_string()),
            _ => None,
        })
        .ok()
    }

    /// 没有 TEST_DATABASE_URL 时跳过，不把集成测试变成"必须先手工建库"的门槛。
    macro_rules! require_db {
        () => {
            match test_config() {
                Some(cfg) => cfg,
                None => {
                    eprintln!("跳过：未设置 TEST_DATABASE_URL");
                    return;
                }
            }
        };
    }

    #[tokio::test]
    async fn after_connect_pins_session_timezone_to_utc() {
        let cfg = require_db!();
        let pool = init_pool(&cfg).await.expect("建池失败");

        let tz: String = sqlx::query_scalar("SELECT @@session.time_zone")
            .fetch_one(&pool)
            .await
            .expect("读会话时区失败");
        assert_eq!(tz, "+00:00", "after_connect 没有生效");

        // 行为验证：比读变量更硬。默认时区下这里会差 28800 秒。
        let skew: i64 =
            sqlx::query_scalar("SELECT TIMESTAMPDIFF(SECOND, CURRENT_TIMESTAMP, UTC_TIMESTAMP())")
                .fetch_one(&pool)
                .await
                .expect("读时区偏差失败");
        assert_eq!(
            skew, 0,
            "会话时区未钉到 UTC，CURRENT_TIMESTAMP 与 UTC 偏差 {skew} 秒"
        );
    }

    #[tokio::test]
    async fn migrations_create_the_expected_tables_and_are_idempotent() {
        let cfg = require_db!();
        let pool = init_pool(&cfg).await.expect("首次建池失败");

        let tables: Vec<String> = sqlx::query_scalar(
            "SELECT table_name FROM information_schema.tables \
             WHERE table_schema = DATABASE() ORDER BY table_name",
        )
        .fetch_all(&pool)
        .await
        .expect("读表清单失败");

        for expected in ["users", "audit_logs", "_sqlx_migrations"] {
            assert!(
                tables.iter().any(|t| t == expected),
                "缺少表 {expected}，实际有 {tables:?}"
            );
        }

        // 再跑一次：sqlx 的迁移表保证幂等，重复启动不应报错。
        init_pool(&cfg).await.expect("重复建池/迁移失败");
    }

    #[tokio::test]
    async fn ping_succeeds_on_live_database() {
        let cfg = require_db!();
        let pool = init_pool(&cfg).await.expect("建池失败");
        ping(&pool).await.expect("ping 失败");
    }

    #[tokio::test]
    async fn bad_database_url_fails_without_leaking_credentials() {
        // 端口 1 必然连不上，用来验证失败路径：错误里不能出现口令。
        let cfg = Config::from_source(|key| match key {
            "DATABASE_URL" => Some("mysql://nexus:sup3rsecret@127.0.0.1:1/nexus".to_string()),
            "APP_BASE_URL" => Some("http://localhost:8080".to_string()),
            _ => None,
        })
        .expect("配置应合法");

        let err = init_pool(&cfg).await.expect_err("应当连接失败");
        let shown = format!("{err}");
        assert!(
            !shown.contains("sup3rsecret"),
            "错误信息泄露了口令：{shown}"
        );
    }
}
