//! 「`created_at` 必须是 UTC」的集成测试。
//!
//! 这条验收项很容易被糊弄过去：DDL 写的是 `CURRENT_TIMESTAMP`，看着像 UTC，实际跟着
//! `@@session.time_zone` 走。本地实测过：MySQL 默认 `time_zone=SYSTEM` 时，
//! `CURRENT_TIMESTAMP` 比 `UTC_TIMESTAMP()` **快 28800 秒**。也就是说，只要连接忘了
//! `SET time_zone='+00:00'`，写进去的就是北京时间，而所有代码看起来都"没问题"。
//!
//! 所以这里不测 DDL 文本，测**真实连接上写出来的值**：走 `db::init_pool`（它带
//! `after_connect`），插入一行，再用 SQL 自己算 `created_at` 与 `UTC_TIMESTAMP()`、
//! `NOW()` 的秒差。断言的是三条：会话时区确实是 `+00:00`、与 UTC 的偏差在 2 秒内、
//! `NOW()` 与 `UTC_TIMESTAMP()` 在该会话里等价。

use nexus::config::Config;
use nexus::db;
use sqlx::Row;

fn config_for(url: String) -> Config {
    Config::from_source(move |key| match key {
        "DATABASE_URL" => Some(url.clone()),
        "APP_BASE_URL" => Some("http://127.0.0.1:8080".to_string()),
        _ => None,
    })
    .expect("配置应合法")
}

#[tokio::test]
async fn created_at_is_written_in_utc() {
    let Ok(url) = std::env::var("TEST_DATABASE_URL") else {
        println!("跳过 created_at_is_written_in_utc：未设置 TEST_DATABASE_URL");
        return;
    };

    let pool = db::init_pool(&config_for(url))
        .await
        .expect("建池失败（检查 TEST_DATABASE_URL 与 ssl-ca）");
    db::migrate(&pool).await.expect("迁移失败");

    // 用时间戳做后缀，避免与上一次失败残留的行冲突。
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("系统时钟")
        .as_nanos();
    let username = format!("utc_probe_{stamp}");
    let email = format!("utc_probe_{stamp}@example.invalid");

    let id = sqlx::query(
        "INSERT INTO users (username, email, password_hash, display_name) VALUES (?, ?, ?, ?)",
    )
    .bind(&username)
    .bind(&email)
    .bind("")
    .bind("UTC 探针")
    .execute(&pool)
    .await
    .expect("插入探针行失败")
    .last_insert_id();

    let row = sqlx::query(
        "SELECT @@session.time_zone AS tz, \
                TIMESTAMPDIFF(SECOND, created_at, UTC_TIMESTAMP()) AS drift_utc, \
                TIMESTAMPDIFF(SECOND, created_at, NOW()) AS drift_now \
         FROM users WHERE id = ?",
    )
    .bind(id)
    .fetch_one(&pool)
    .await
    .expect("读回探针行失败");

    let tz: String = row.get("tz");
    let drift_utc: i64 = row.get("drift_utc");
    let drift_now: i64 = row.get("drift_now");

    // 先删干净，断言失败也不留垃圾（断言在删除之后仍会 panic，信息不丢）。
    sqlx::query("DELETE FROM users WHERE id = ?")
        .bind(id)
        .execute(&pool)
        .await
        .expect("清理探针行失败");

    assert_eq!(
        tz, "+00:00",
        "连接的会话时区必须是 +00:00（after_connect 没生效？）"
    );
    assert!(
        drift_utc.abs() <= 120,
        "created_at 与 UTC_TIMESTAMP() 差 {drift_utc} 秒，说明写入的不是 UTC"
    );
    assert_eq!(
        drift_now, drift_utc,
        "会话时区是 +00:00 时 NOW() 必须等价于 UTC_TIMESTAMP()，实际差 {drift_now} vs {drift_utc} 秒"
    );
}
