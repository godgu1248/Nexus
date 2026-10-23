//! 公告的数据访问层。
//!
//! 两条"读"要分清楚：
//! - `list_active` 是"所有人能看到哪些公告"；
//! - `list_unacked` 是"这个人还有哪几条没确认"。
//!
//! 后者用 `NOT EXISTS` 反连接而不是在 Rust 里做差集：差集要先把全表拉回来，
//! 而公告表会随年份增长，这里本该由索引回答。

use chrono::{DateTime, Utc};
use sqlx::MySqlPool;

use crate::error::AppError;

#[derive(Debug, Clone)]
pub struct Notice {
    pub id: u64,
    pub title: String,
    pub body: String,
    /// 净化后的富文本；`None` 表示只有纯文本。
    pub body_html: Option<String>,
    pub author_name: String,
    pub created_at: DateTime<Utc>,
}

fn from_row(row: &sqlx::mysql::MySqlRow) -> Result<Notice, AppError> {
    use sqlx::Row;
    Ok(Notice {
        id: row.try_get("id")?,
        title: row.try_get("title")?,
        body: row.try_get("body")?,
        body_html: row.try_get("body_html")?,
        author_name: row.try_get("author_name")?,
        created_at: row.try_get("created_at")?,
    })
}

/// 全部有效公告（新的在上面）。
pub async fn list_active(pool: &MySqlPool, limit: u32) -> Result<Vec<Notice>, AppError> {
    let rows = sqlx::query(
        "SELECT n.id, n.title, n.body, n.body_html, u.display_name AS author_name, n.created_at \
           FROM announcements n JOIN users u ON u.id = n.author_id \
          WHERE n.deleted_at IS NULL AND n.is_active = 1 \
          ORDER BY n.created_at DESC, n.id DESC LIMIT ?",
    )
    .bind(limit)
    .fetch_all(pool)
    .await?;
    rows.iter().map(from_row).collect()
}

/// 这个人还没确认的公告（新的在上面）。
pub async fn list_unacked(
    pool: &MySqlPool,
    user_id: u64,
    limit: u32,
) -> Result<Vec<Notice>, AppError> {
    let rows = sqlx::query(
        "SELECT n.id, n.title, n.body, n.body_html, u.display_name AS author_name, n.created_at \
           FROM announcements n JOIN users u ON u.id = n.author_id \
          WHERE n.deleted_at IS NULL AND n.is_active = 1 \
            AND NOT EXISTS (SELECT 1 FROM announcement_acks a \
                             WHERE a.announcement_id = n.id AND a.user_id = ?) \
          ORDER BY n.created_at DESC, n.id DESC LIMIT ?",
    )
    .bind(user_id)
    .bind(limit)
    .fetch_all(pool)
    .await?;
    rows.iter().map(from_row).collect()
}

pub async fn unread_count(pool: &MySqlPool, user_id: u64) -> Result<u64, AppError> {
    let count: u64 = sqlx::query_scalar(
        "SELECT CAST(COUNT(*) AS UNSIGNED) FROM announcements n \
          WHERE n.deleted_at IS NULL AND n.is_active = 1 \
            AND NOT EXISTS (SELECT 1 FROM announcement_acks a \
                             WHERE a.announcement_id = n.id AND a.user_id = ?)",
    )
    .bind(user_id)
    .fetch_one(pool)
    .await?;
    Ok(count)
}

pub async fn find(pool: &MySqlPool, id: u64) -> Result<Option<Notice>, AppError> {
    // 字面量 SQL：sqlx 0.9 的 `query()` 只收 `&'static str`，
    // 想拼串会在编译期被 SqlSafeStr 拦下来（这条约束在 M4 已经踩过一次）。
    let row = sqlx::query(
        "SELECT n.id, n.title, n.body, n.body_html, u.display_name AS author_name, n.created_at \
           FROM announcements n JOIN users u ON u.id = n.author_id \
          WHERE n.deleted_at IS NULL AND n.is_active = 1 AND n.id = ?",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?;
    match row {
        Some(row) => Ok(Some(from_row(&row)?)),
        None => Ok(None),
    }
}

pub async fn create(
    tx: &mut sqlx::Transaction<'_, sqlx::MySql>,
    title: &str,
    body: &str,
    body_html: Option<&str>,
    author_id: u64,
) -> Result<u64, AppError> {
    let result = sqlx::query(
        "INSERT INTO announcements (title, body, body_html, author_id, is_active) VALUES (?, ?, ?, ?, 1)",
    )
    .bind(title)
    .bind(body)
    .bind(body_html)
    .bind(author_id)
    .execute(&mut **tx)
    .await?;
    Ok(result.last_insert_id())
}

pub async fn soft_delete(
    tx: &mut sqlx::Transaction<'_, sqlx::MySql>,
    id: u64,
) -> Result<bool, AppError> {
    let result = sqlx::query(
        "UPDATE announcements SET deleted_at = UTC_TIMESTAMP() \
          WHERE id = ? AND deleted_at IS NULL",
    )
    .bind(id)
    .execute(&mut **tx)
    .await?;
    Ok(result.rows_affected() == 1)
}

/// 记一条"已读"。重复确认是幂等的——返回 false 表示本来就有这条记录。
pub async fn ack(
    tx: &mut sqlx::Transaction<'_, sqlx::MySql>,
    notice_id: u64,
    user_id: u64,
) -> Result<bool, AppError> {
    let result =
        sqlx::query("INSERT INTO announcement_acks (announcement_id, user_id) VALUES (?, ?)")
            .bind(notice_id)
            .bind(user_id)
            .execute(&mut **tx)
            .await;
    match result {
        Ok(done) => Ok(done.rows_affected() == 1),
        // 1062 = 已经确认过了。这不是错误：用户连点两次、或两个标签页同时确认都会走到这里。
        Err(sqlx::Error::Database(db))
            if db
                .try_downcast_ref::<sqlx::mysql::MySqlDatabaseError>()
                .map(|err| err.number() == 1062)
                .unwrap_or(false) =>
        {
            Ok(false)
        }
        Err(err) => Err(AppError::from(err)),
    }
}
