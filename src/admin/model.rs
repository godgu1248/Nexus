//! 管理区的只读查询。
//!
//! 这里只做 `SELECT`：成员列表与审计列表。所有写操作都在 `service.rs`，
//! 因为本模块不拥有任何业务表（见 `mod.rs` 的说明）。
//!
//! 两条 sqlx 纪律（全项目一致，仍要重申）：
//! 1. SQL 一律字面量——sqlx 0.9 的 `query()` 只接受 `&'static str`；
//! 2. 计数一律 `CAST(COUNT(*) AS UNSIGNED)`——`COUNT(*)` 是有符号 BIGINT。

use chrono::{DateTime, NaiveDateTime, Utc};
use sqlx::mysql::MySqlRow;
use sqlx::{MySqlPool, Row};

use crate::auth::model::{Role, UserStatus};
use crate::error::AppError;
use crate::shared::pagination::{PAGE_SIZE, Page, PageQuery};
use crate::shared::sql::escape_like;

/// 成员列表的一行。
#[derive(Debug, Clone)]
pub struct MemberRow {
    pub id: u64,
    pub username: String,
    pub display_name: String,
    pub email: String,
    pub role: Role,
    pub status: UserStatus,
    pub created_at: DateTime<Utc>,
}

/// 成员列表：按角色/状态筛选，分页。
///
/// 筛选条件用 `(? IS NULL OR u.role = ?)` 表达，不用拼 SQL：
/// 拼 SQL 在 sqlx 0.9 里根本编译不过（`&'static str`），而这个模式恰好也够用。
pub async fn list_members(
    pool: &MySqlPool,
    role: Option<&str>,
    status: Option<&str>,
    page: &PageQuery,
) -> Result<Page<MemberRow>, AppError> {
    let offset = page.page.saturating_sub(1).saturating_mul(PAGE_SIZE);
    let rows = sqlx::query(
        "SELECT u.id, u.username, u.display_name, u.email, u.role, u.status, u.created_at \
           FROM users u \
          WHERE (? IS NULL OR u.role = ?) \
            AND (? IS NULL OR u.status = ?) \
          ORDER BY u.created_at DESC, u.id DESC \
          LIMIT ? OFFSET ?",
    )
    .bind(role)
    .bind(role)
    .bind(status)
    .bind(status)
    .bind(PAGE_SIZE)
    .bind(offset)
    .fetch_all(pool)
    .await?;

    let mut items = Vec::with_capacity(rows.len());
    for row in &rows {
        items.push(member_from_row(row)?);
    }

    let total: u64 = sqlx::query_scalar(
        "SELECT CAST(COUNT(*) AS UNSIGNED) FROM users u \
          WHERE (? IS NULL OR u.role = ?) \
            AND (? IS NULL OR u.status = ?)",
    )
    .bind(role)
    .bind(role)
    .bind(status)
    .bind(status)
    .fetch_one(pool)
    .await?;

    Ok(Page::new(items, page.page, total, PAGE_SIZE))
}

/// 审计列表的一行。
#[derive(Debug, Clone)]
pub struct AuditRow {
    pub id: u64,
    pub actor_id: Option<u64>,
    pub action: String,
    pub target_type: String,
    pub target_id: Option<u64>,
    pub ip: Option<String>,
    pub created_at: DateTime<Utc>,
}

/// 审计筛选条件。全部可选，`None` 表示不限制。
#[derive(Debug, Clone, Default)]
pub struct AuditFilter {
    pub actor_id: Option<u64>,
    /// 前缀匹配：填 `auth.` 能看全部登录相关动作。
    pub action: Option<String>,
    pub target_type: Option<String>,
    pub target_id: Option<u64>,
    /// 含当天：`created_at >= from 00:00:00`。
    pub from: Option<NaiveDateTime>,
    /// 含当天：`created_at < to 23:59:59` 的下一天零点。
    pub until: Option<NaiveDateTime>,
}

impl AuditFilter {
    /// 有没有真的用上筛选条件——审计页自己的 meta 要记这个，
    /// 用来区分"某人天天翻全量审计"和"某人只是查了一次自己的登录记录"。
    pub fn is_filtered(&self) -> bool {
        self.actor_id.is_some()
            || self.action.is_some()
            || self.target_type.is_some()
            || self.target_id.is_some()
            || self.from.is_some()
            || self.until.is_some()
    }
}

pub async fn list_audit(
    pool: &MySqlPool,
    filter: &AuditFilter,
    page: &PageQuery,
) -> Result<Page<AuditRow>, AppError> {
    let offset = page.page.saturating_sub(1).saturating_mul(PAGE_SIZE);
    let action = filter.action.as_deref();
    // 动作筛选是"前缀"语义（`user.` 能筛出全部用户相关动作）。用户输入里的
    // `%` 与 `_` 必须先转义，否则 `_` 会被 LIKE 当成"任意一个字符"，
    // 筛 `u_er` 会把 `user…` 也捞出来——筛选结果对不上，用户会以为审计缺了记录。
    let action_pattern = action.map(escape_like);
    let target_type = filter.target_type.as_deref();

    let rows = sqlx::query(
        "SELECT a.id, a.actor_id, a.action, a.target_type, a.target_id, a.ip, a.created_at \
           FROM audit_logs a \
          WHERE (? IS NULL OR a.actor_id = ?) \
            AND (? IS NULL OR a.action LIKE CONCAT(?, '%') ESCAPE '\\\\') \
            AND (? IS NULL OR a.target_type = ?) \
            AND (? IS NULL OR a.target_id = ?) \
            AND (? IS NULL OR a.created_at >= ?) \
            AND (? IS NULL OR a.created_at < ?) \
          ORDER BY a.created_at DESC, a.id DESC \
          LIMIT ? OFFSET ?",
    )
    .bind(filter.actor_id)
    .bind(filter.actor_id)
    .bind(action)
    .bind(action_pattern.as_deref())
    .bind(target_type)
    .bind(target_type)
    .bind(filter.target_id)
    .bind(filter.target_id)
    .bind(filter.from)
    .bind(filter.from)
    .bind(filter.until)
    .bind(filter.until)
    .bind(PAGE_SIZE)
    .bind(offset)
    .fetch_all(pool)
    .await?;

    let mut items = Vec::with_capacity(rows.len());
    for row in &rows {
        items.push(audit_from_row(row)?);
    }

    let total: u64 = sqlx::query_scalar(
        "SELECT CAST(COUNT(*) AS UNSIGNED) FROM audit_logs a \
          WHERE (? IS NULL OR a.actor_id = ?) \
            AND (? IS NULL OR a.action LIKE CONCAT(?, '%') ESCAPE '\\\\') \
            AND (? IS NULL OR a.target_type = ?) \
            AND (? IS NULL OR a.target_id = ?) \
            AND (? IS NULL OR a.created_at >= ?) \
            AND (? IS NULL OR a.created_at < ?)",
    )
    .bind(filter.actor_id)
    .bind(filter.actor_id)
    .bind(action)
    .bind(action_pattern.as_deref())
    .bind(target_type)
    .bind(target_type)
    .bind(filter.target_id)
    .bind(filter.target_id)
    .bind(filter.from)
    .bind(filter.from)
    .bind(filter.until)
    .bind(filter.until)
    .fetch_one(pool)
    .await?;

    Ok(Page::new(items, page.page, total, PAGE_SIZE))
}

/// 总览页用的"最近若干条审计"。不带筛选，按时间倒序。
pub async fn recent_audit(pool: &MySqlPool, limit: u32) -> Result<Vec<AuditRow>, AppError> {
    let rows = sqlx::query(
        "SELECT a.id, a.actor_id, a.action, a.target_type, a.target_id, a.ip, a.created_at \
           FROM audit_logs a \
          ORDER BY a.created_at DESC, a.id DESC \
          LIMIT ?",
    )
    .bind(limit)
    .fetch_all(pool)
    .await?;

    let mut items = Vec::with_capacity(rows.len());
    for row in &rows {
        items.push(audit_from_row(row)?);
    }
    Ok(items)
}

fn member_from_row(row: &MySqlRow) -> Result<MemberRow, AppError> {
    let role_raw: String = row.try_get("role")?;
    let status_raw: String = row.try_get("status")?;
    let role = Role::from_db(&role_raw).ok_or_else(|| {
        AppError::internal(anyhow::anyhow!("users.role 出现未知取值：{role_raw}"))
    })?;
    let status = UserStatus::from_db(&status_raw).ok_or_else(|| {
        AppError::internal(anyhow::anyhow!("users.status 出现未知取值：{status_raw}"))
    })?;

    Ok(MemberRow {
        id: row.try_get("id")?,
        username: row.try_get("username")?,
        display_name: row.try_get("display_name")?,
        email: row.try_get("email")?,
        role,
        status,
        created_at: row.try_get("created_at")?,
    })
}

fn audit_from_row(row: &MySqlRow) -> Result<AuditRow, AppError> {
    Ok(AuditRow {
        id: row.try_get("id")?,
        actor_id: row.try_get("actor_id")?,
        action: row.try_get("action")?,
        target_type: row.try_get("target_type")?,
        target_id: row.try_get("target_id")?,
        ip: row.try_get("ip")?,
        created_at: row.try_get("created_at")?,
    })
}
