//! 公告的数据访问层。
//!
//! 三条"读"要分清楚：
//! - `list_active` 是"这个人能看到哪些公告"；
//! - `list_unacked` 是"这个人还有哪几条没确认"；
//! - `unread_count` 是"红点该显示几"。
//!
//! 后两者用 `NOT EXISTS` 反连接而不是在 Rust 里做差集：差集要先把全表拉回来，
//! 而公告表会随年份增长，这里本该由索引回答。
//!
//! **受众过滤（`audience`）必须在 SQL 里做，不能取回来再筛**：一旦在 Rust 里筛，
//! 分页与未读计数就与列表条数对不上，红点会显示"3 条未读"但列表只有 2 条；
//! 而且漏筛等于把"仅团队"的公告标题直接推给来宾。

use chrono::{DateTime, Utc};
use sqlx::MySqlPool;

use crate::error::AppError;

/// 公告的受众。
///
/// `All`  = 全体可见：团队成员、来宾、游客都能看到（历史数据的默认值）；
/// `Team` = 仅团队：只有 member / admin 看得到，来宾既看不到也不算未读。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Audience {
    All,
    Team,
}

impl Audience {
    /// 发布表单里要列出来的选项，顺序就是界面顺序。
    pub const ALL: &'static [Audience] = &[Audience::All, Audience::Team];

    pub fn as_str(self) -> &'static str {
        match self {
            Audience::All => "all",
            Audience::Team => "team",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Audience::All => "全体可见",
            Audience::Team => "仅团队",
        }
    }

    pub fn hint(self) -> &'static str {
        match self {
            Audience::All => "团队成员和来宾都能看到",
            Audience::Team => "只有成员与管理员能看到",
        }
    }

    /// 从数据库读出来的值必须严格匹配：出现未知值说明有人绕过应用写了库，
    /// 这时候默认成 `All` 会把本该保密的公告公开出去，是最坏的一种"容错"。
    pub fn from_db(value: &str) -> Result<Self, AppError> {
        match value {
            "all" => Ok(Audience::All),
            "team" => Ok(Audience::Team),
            other => Err(AppError::internal(anyhow::anyhow!(
                "announcements.audience 出现未知取值：{other}"
            ))),
        }
    }

    /// 表单里的值用宽解析（去空白、大小写不敏感），不合法返回 `None` 交给校验层报 422。
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "all" => Some(Audience::All),
            "team" => Some(Audience::Team),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Notice {
    pub id: u64,
    pub title: String,
    pub body: String,
    /// 净化后的富文本；`None` 表示只有纯文本。
    pub body_html: Option<String>,
    pub audience: Audience,
    pub author_name: String,
    pub created_at: DateTime<Utc>,
}

/// 受众过滤的 SQL 片段：`? = 1` 表示"这个人属于团队"。
///
/// 不用 `if` 拼两条 SQL 是因为片段只有一处、且参数顺序固定在前面，
/// 拼串会踩到 sqlx 0.9 的 `SqlSafeStr`（只收 `&'static str`）。
fn audience_allows(sees_team: bool) -> u8 {
    u8::from(sees_team)
}

fn from_row(row: &sqlx::mysql::MySqlRow) -> Result<Notice, AppError> {
    use sqlx::Row;
    let audience: String = row.try_get("audience")?;
    Ok(Notice {
        id: row.try_get("id")?,
        title: row.try_get("title")?,
        body: row.try_get("body")?,
        body_html: row.try_get("body_html")?,
        audience: Audience::from_db(&audience)?,
        author_name: row.try_get("author_name")?,
        created_at: row.try_get("created_at")?,
    })
}

/// 这个人能看到的全部有效公告（新的在上面）。
pub async fn list_active(
    pool: &MySqlPool,
    limit: u32,
    sees_team: bool,
) -> Result<Vec<Notice>, AppError> {
    let rows = sqlx::query(
        "SELECT n.id, n.title, n.body, n.body_html, n.audience, \
                u.display_name AS author_name, n.created_at \
           FROM announcements n JOIN users u ON u.id = n.author_id \
          WHERE n.deleted_at IS NULL AND n.is_active = 1 \
            AND (? = 1 OR n.audience = 'all') \
          ORDER BY n.created_at DESC, n.id DESC LIMIT ?",
    )
    .bind(audience_allows(sees_team))
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
    sees_team: bool,
) -> Result<Vec<Notice>, AppError> {
    let rows = sqlx::query(
        "SELECT n.id, n.title, n.body, n.body_html, n.audience, \
                u.display_name AS author_name, n.created_at \
           FROM announcements n JOIN users u ON u.id = n.author_id \
          WHERE n.deleted_at IS NULL AND n.is_active = 1 \
            AND (? = 1 OR n.audience = 'all') \
            AND NOT EXISTS (SELECT 1 FROM announcement_acks a \
                             WHERE a.announcement_id = n.id AND a.user_id = ?) \
          ORDER BY n.created_at DESC, n.id DESC LIMIT ?",
    )
    .bind(audience_allows(sees_team))
    .bind(user_id)
    .bind(limit)
    .fetch_all(pool)
    .await?;
    rows.iter().map(from_row).collect()
}

pub async fn unread_count(
    pool: &MySqlPool,
    user_id: u64,
    sees_team: bool,
) -> Result<u64, AppError> {
    let count: u64 = sqlx::query_scalar(
        "SELECT CAST(COUNT(*) AS UNSIGNED) FROM announcements n \
          WHERE n.deleted_at IS NULL AND n.is_active = 1 \
            AND (? = 1 OR n.audience = 'all') \
            AND NOT EXISTS (SELECT 1 FROM announcement_acks a \
                             WHERE a.announcement_id = n.id AND a.user_id = ?)",
    )
    .bind(audience_allows(sees_team))
    .bind(user_id)
    .fetch_one(pool)
    .await?;
    Ok(count)
}

/// 按 id 取公告；`sees_team = false` 的人取不到"仅团队"的公告（表现为 404）。
///
/// 确认公告走这条路径，所以来宾不可能凭猜 id 去确认一条自己看不到的公告——
/// 那会在 `announcement_acks` 里留下"他确认过"的假凭据。
pub async fn find(pool: &MySqlPool, id: u64, sees_team: bool) -> Result<Option<Notice>, AppError> {
    // 字面量 SQL：sqlx 0.9 的 `query()` 只收 `&'static str`，
    // 想拼串会在编译期被 SqlSafeStr 拦下来（这条约束在 M4 已经踩过一次）。
    let row = sqlx::query(
        "SELECT n.id, n.title, n.body, n.body_html, n.audience, \
                u.display_name AS author_name, n.created_at \
           FROM announcements n JOIN users u ON u.id = n.author_id \
          WHERE n.deleted_at IS NULL AND n.is_active = 1 AND n.id = ? \
            AND (? = 1 OR n.audience = 'all')",
    )
    .bind(id)
    .bind(audience_allows(sees_team))
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
    audience: Audience,
    author_id: u64,
) -> Result<u64, AppError> {
    let result = sqlx::query(
        "INSERT INTO announcements (title, body, body_html, audience, author_id, is_active) \
         VALUES (?, ?, ?, ?, ?, 1)",
    )
    .bind(title)
    .bind(body)
    .bind(body_html)
    .bind(audience.as_str())
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
