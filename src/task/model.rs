//! 任务的 SQL 层。
//!
//! 纪律（与 forum / kb 一致，踩过的坑都在这里）：
//! - SQL 一律**字面量**：sqlx 0.9 的 `query()` 只接受 `&'static str`，`format!` 拼出来的
//!   字符串编译期就被拒（`dynamic SQL strings should be audited for possible injections`）。
//! - `COUNT(*)` 在 MySQL 里是**有符号** BIGINT，直接解码进 `u64` 会在运行期报
//!   `ColumnDecode`；这里一律 `CAST(... AS UNSIGNED)`。
//! - 列表查询不在这里做可见性过滤：M5 的任务全是 `internal`，过滤在 service 用
//!   `policy::can_view` 一次性判掉；等 M6 真出现 public 任务时再改这条 SQL。

use chrono::{DateTime, NaiveDate, Utc};
use sqlx::mysql::MySqlRow;
use sqlx::{MySql, MySqlPool, Row, Transaction};

use crate::error::AppError;

/// 任务状态。三个值就是三个值——多一个状态就多一组转移规则，
/// 十人团队还没到需要 `blocked` / `cancelled` 的地步（DESIGN §23.9 E1）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskStatus {
    Todo,
    Doing,
    Done,
}

impl TaskStatus {
    pub const ALL: [TaskStatus; 3] = [TaskStatus::Todo, TaskStatus::Doing, TaskStatus::Done];

    pub fn as_str(self) -> &'static str {
        match self {
            TaskStatus::Todo => "todo",
            TaskStatus::Doing => "doing",
            TaskStatus::Done => "done",
        }
    }

    /// 中文标签只给模板用，不参与任何判断——展示文案不是协议。
    pub fn label(self) -> &'static str {
        match self {
            TaskStatus::Todo => "待开始",
            TaskStatus::Doing => "进行中",
            TaskStatus::Done => "已完成",
        }
    }

    /// 严格解析：只认数据库里的三个小写值。
    /// 未知取值说明有人绕过代码写了库，属于数据损坏，必须炸出来而不是当 todo。
    pub fn from_db(raw: &str) -> Result<Self, AppError> {
        match raw {
            "todo" => Ok(TaskStatus::Todo),
            "doing" => Ok(TaskStatus::Doing),
            "done" => Ok(TaskStatus::Done),
            other => Err(AppError::internal(anyhow::anyhow!(
                "tasks.status 出现未知取值：{other}"
            ))),
        }
    }

    /// 宽解析：给表单用（大小写与空白容错），非法值返回 None 由调用方给 422。
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "todo" => Some(TaskStatus::Todo),
            "doing" => Some(TaskStatus::Doing),
            "done" => Some(TaskStatus::Done),
            _ => None,
        }
    }
}

impl std::fmt::Display for TaskStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 详情页用：把负责人和创建者的显示名一起取回来，避免详情页再查两次。
#[derive(Debug, Clone)]
pub struct Task {
    pub id: u64,
    pub title: String,
    pub body: Option<String>,
    pub status: TaskStatus,
    pub assignee_id: Option<u64>,
    pub assignee_name: Option<String>,
    pub created_by: u64,
    pub created_by_name: String,
    pub due_on: Option<NaiveDate>,
    pub source_thread_id: Option<u64>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// 列表页用：只取渲染需要的列。
#[derive(Debug, Clone)]
pub struct TaskCard {
    pub id: u64,
    pub title: String,
    pub status: TaskStatus,
    pub assignee_name: Option<String>,
    pub due_on: Option<NaiveDate>,
    pub updated_at: DateTime<Utc>,
}

/// 新建任务的入参（落库用，不含权限与校验结果）。
#[derive(Debug, Clone)]
pub struct NewTask<'a> {
    pub title: &'a str,
    pub body: Option<&'a str>,
    pub status: TaskStatus,
    pub assignee_id: Option<u64>,
    pub created_by: u64,
    pub due_on: Option<NaiveDate>,
    pub source_thread_id: Option<u64>,
}

fn status_of(row: &MySqlRow) -> Result<TaskStatus, AppError> {
    let raw: String = row.try_get("status")?;
    TaskStatus::from_db(&raw)
}

fn card_from_row(row: &MySqlRow) -> Result<TaskCard, AppError> {
    Ok(TaskCard {
        id: row.try_get("id")?,
        title: row.try_get("title")?,
        status: status_of(row)?,
        assignee_name: row.try_get("assignee_name")?,
        due_on: row.try_get("due_on")?,
        updated_at: row.try_get("updated_at")?,
    })
}

fn task_from_row(row: &MySqlRow) -> Result<Task, AppError> {
    Ok(Task {
        id: row.try_get("id")?,
        title: row.try_get("title")?,
        body: row.try_get("body")?,
        status: status_of(row)?,
        assignee_id: row.try_get("assignee_id")?,
        assignee_name: row.try_get("assignee_name")?,
        created_by: row.try_get("created_by")?,
        created_by_name: row.try_get("created_by_name")?,
        due_on: row.try_get("due_on")?,
        source_thread_id: row.try_get("source_thread_id")?,
        created_at: row.try_get("created_at")?,
        updated_at: row.try_get("updated_at")?,
    })
}

/// 单条任务。不做可见性过滤（管理员与作者都要能取到），由调用方判。
pub async fn find_task(pool: &MySqlPool, id: u64) -> Result<Option<Task>, AppError> {
    let row = sqlx::query(
        "SELECT t.id, t.title, t.body, t.status, t.assignee_id, \
                a.display_name AS assignee_name, \
                t.created_by, c.display_name AS created_by_name, \
                t.due_on, t.source_thread_id, t.created_at, t.updated_at \
           FROM tasks t \
           JOIN users c ON c.id = t.created_by \
           LEFT JOIN users a ON a.id = t.assignee_id \
          WHERE t.id = ? AND t.deleted_at IS NULL",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?;

    row.as_ref().map(task_from_row).transpose()
}

/// 列表。`assignee = Some(id)` 表示"只看指派给我的"。
///
/// 排序把 `done` 沉底：已完成的任务不该占着第一屏——这是"不漏事"的反面，
/// 已办完的事还在眼前晃，就等于噪声。
pub async fn list_tasks(
    pool: &MySqlPool,
    assignee: Option<u64>,
    offset: u32,
    limit: u32,
) -> Result<Vec<TaskCard>, AppError> {
    let rows = sqlx::query(
        "SELECT t.id, t.title, t.status, a.display_name AS assignee_name, t.due_on, t.updated_at \
           FROM tasks t LEFT JOIN users a ON a.id = t.assignee_id \
          WHERE t.deleted_at IS NULL \
            AND (? IS NULL OR t.assignee_id = ?) \
          ORDER BY (t.status = 'done'), t.updated_at DESC, t.id DESC \
          LIMIT ? OFFSET ?",
    )
    .bind(assignee)
    .bind(assignee)
    .bind(limit)
    .bind(offset)
    .fetch_all(pool)
    .await?;

    rows.iter().map(card_from_row).collect()
}

/// 列表总数。`CAST` 不是装饰：`COUNT(*)` 是有符号 BIGINT，解进 u64 会在运行期炸。
pub async fn count_tasks(pool: &MySqlPool, assignee: Option<u64>) -> Result<u64, AppError> {
    let total: u64 = sqlx::query_scalar(
        "SELECT CAST(COUNT(*) AS UNSIGNED) FROM tasks t \
          WHERE t.deleted_at IS NULL AND (? IS NULL OR t.assignee_id = ?)",
    )
    .bind(assignee)
    .bind(assignee)
    .fetch_one(pool)
    .await?;

    Ok(total)
}

/// 插入一条任务，返回新 id。必须在事务里调用——审计要和它同生共死。
pub async fn create_task(
    tx: &mut Transaction<'_, MySql>,
    input: NewTask<'_>,
) -> Result<u64, AppError> {
    let result = sqlx::query(
        "INSERT INTO tasks (title, body, status, assignee_id, created_by, due_on, source_thread_id) \
         VALUES (?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(input.title)
    .bind(input.body)
    .bind(input.status.as_str())
    .bind(input.assignee_id)
    .bind(input.created_by)
    .bind(input.due_on)
    .bind(input.source_thread_id)
    .execute(&mut **tx)
    .await?;

    Ok(result.last_insert_id())
}

/// 改状态。返回 false 表示任务不存在或已软删——调用方据此给 404。
pub async fn set_status(
    tx: &mut Transaction<'_, MySql>,
    id: u64,
    status: TaskStatus,
) -> Result<bool, AppError> {
    let result = sqlx::query("UPDATE tasks SET status = ? WHERE id = ? AND deleted_at IS NULL")
        .bind(status.as_str())
        .bind(id)
        .execute(&mut **tx)
        .await?;

    Ok(result.rows_affected() > 0)
}

/// 指派 / 改派 / 取消指派（`assignee = None`）。
pub async fn set_assignee(
    tx: &mut Transaction<'_, MySql>,
    id: u64,
    assignee: Option<u64>,
) -> Result<bool, AppError> {
    let result =
        sqlx::query("UPDATE tasks SET assignee_id = ? WHERE id = ? AND deleted_at IS NULL")
            .bind(assignee)
            .bind(id)
            .execute(&mut **tx)
            .await?;

    Ok(result.rows_affected() > 0)
}

/// 软删。行保留，历史审计仍然能指回它。
pub async fn soft_delete(tx: &mut Transaction<'_, MySql>, id: u64) -> Result<bool, AppError> {
    let result = sqlx::query(
        "UPDATE tasks SET deleted_at = UTC_TIMESTAMP() WHERE id = ? AND deleted_at IS NULL",
    )
    .bind(id)
    .execute(&mut **tx)
    .await?;

    Ok(result.rows_affected() > 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_round_trips_through_its_wire_form() {
        for status in TaskStatus::ALL {
            assert_eq!(TaskStatus::from_db(status.as_str()).unwrap(), status);
            assert_eq!(TaskStatus::parse(status.as_str()), Some(status));
        }
    }

    #[test]
    fn unknown_status_is_an_error_not_a_default() {
        // 库里出现 'blocked' 说明有人绕过代码写了数据；静默当 todo 会让它永远查不出来。
        assert!(TaskStatus::from_db("blocked").is_err());
        // 表单输入宽松：大小写与空白容错，非法值给 None 由 handler 回 422。
        assert_eq!(TaskStatus::parse("  DONE "), Some(TaskStatus::Done));
        assert_eq!(TaskStatus::parse("finished"), None);
    }
}
