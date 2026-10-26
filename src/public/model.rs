//! 前厅（公开区）数据访问层。
//!
//! 三条实现纪律（与 forum/kb 一致，仍要重申一遍，因为这里是新的写域）：
//! 1. SQL 一律字面量：sqlx 0.9 的 `query()` 只接受 `&'static str`。
//! 2. 计数一律 `CAST(COUNT(*) AS UNSIGNED)`：MySQL 对 `COUNT(*)` 报有符号 BIGINT，
//!    sqlx 会拒绝把它解进 `u64`（M4 踩过，见 DESIGN §22.12.1）。
//! 3. 多态目标（`target_type` + `target_id`）没有外键，可见性判断在 service 层，
//!    **不在 SQL 里**——前厅只看公开内容，而 `public` 可见性本身是 policy 的语义。

use chrono::{DateTime, Utc};
use sqlx::mysql::MySqlRow;
use sqlx::{MySqlPool, Row, Transaction};

use crate::error::AppError;
use crate::shared::pagination::{PAGE_SIZE, Page, PageQuery};

/// 评论/点赞的目标类型。存进库的是 `as_str()` 的小写串。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommentTarget {
    Thread,
    Doc,
}

impl CommentTarget {
    /// 落库用的编码。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Thread => "thread",
            Self::Doc => "doc",
        }
    }

    /// 界面上给人看的名字。
    pub fn label(self) -> &'static str {
        match self {
            Self::Thread => "议题",
            Self::Doc => "档案",
        }
    }

    /// 从库里的值还原。**严格**：未知值直接报错，不静默兜底。
    ///
    /// 兜底成某个默认值的后果是：将来加了第三种目标类型，旧代码会把它当成议题处理，
    /// 于是评论挂到错误的对象上，而且没人会发现。
    pub fn from_db(raw: &str) -> Result<Self, AppError> {
        match raw {
            "thread" => Ok(Self::Thread),
            "doc" => Ok(Self::Doc),
            other => Err(AppError::internal(anyhow::anyhow!(
                "public_comments.target_type 出现未知取值：{other}"
            ))),
        }
    }

    /// 从用户输入（路径/表单）解析。宽一点：忽略大小写与首尾空白。
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "thread" => Some(Self::Thread),
            "doc" => Some(Self::Doc),
            _ => None,
        }
    }
}

/// 一条评论（已带上作者显示名，列表页不必再查一次）。
#[derive(Debug, Clone)]
pub struct Comment {
    pub id: u64,
    pub target_type: CommentTarget,
    pub target_id: u64,
    pub author_id: u64,
    pub author_name: String,
    pub body: String,
    pub created_at: DateTime<Utc>,
}

/// 投稿的审核状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubmissionStatus {
    Pending,
    Approved,
    Rejected,
}

impl SubmissionStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Approved => "approved",
            Self::Rejected => "rejected",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Pending => "待审核",
            Self::Approved => "已采纳",
            Self::Rejected => "未采纳",
        }
    }

    pub fn from_db(raw: &str) -> Result<Self, AppError> {
        match raw {
            "pending" => Ok(Self::Pending),
            "approved" => Ok(Self::Approved),
            "rejected" => Ok(Self::Rejected),
            other => Err(AppError::internal(anyhow::anyhow!(
                "public_submissions.status 出现未知取值：{other}"
            ))),
        }
    }
}

/// 一条投稿。
#[derive(Debug, Clone)]
pub struct Submission {
    pub id: u64,
    pub author_id: u64,
    pub author_name: String,
    pub title: String,
    pub body: String,
    pub status: SubmissionStatus,
    pub review_note: Option<String>,
    pub reviewed_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

/// 新建投稿的入参。
#[derive(Debug, Clone, Copy)]
pub struct NewSubmission<'a> {
    pub author_id: u64,
    pub title: &'a str,
    pub body: &'a str,
}

// ── 评论 ────────────────────────────────────────────────────────────────

/// 某个目标下的一页评论。顺序是「早的在上」，和读帖习惯一致。
pub async fn list_comments(
    pool: &MySqlPool,
    target: CommentTarget,
    target_id: u64,
    page: &PageQuery,
) -> Result<Page<Comment>, AppError> {
    let total = count_comments(pool, target, target_id).await?;
    let normalized = Page::<()>::new(Vec::new(), page.page, total, PAGE_SIZE);
    if total > 0 && normalized.page > normalized.total_pages() {
        return Ok(Page::new(Vec::new(), normalized.page, total, PAGE_SIZE));
    }

    let rows = sqlx::query(
        "SELECT c.id, c.target_type, c.target_id, c.author_id, u.display_name AS author_name, c.body, c.created_at \
           FROM public_comments c JOIN users u ON u.id = c.author_id \
          WHERE c.target_type = ? AND c.target_id = ? AND c.deleted_at IS NULL \
          ORDER BY c.created_at ASC, c.id ASC LIMIT ? OFFSET ?",
    )
    .bind(target.as_str())
    .bind(target_id)
    .bind(PAGE_SIZE as i64)
    .bind(normalized.offset() as i64)
    .fetch_all(pool)
    .await?;

    let items = rows
        .iter()
        .map(comment_from_row)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Page::new(items, normalized.page, total, PAGE_SIZE))
}

pub async fn count_comments(
    pool: &MySqlPool,
    target: CommentTarget,
    target_id: u64,
) -> Result<u64, AppError> {
    let total: u64 = sqlx::query_scalar(
        "SELECT CAST(COUNT(*) AS UNSIGNED) FROM public_comments \
          WHERE target_type = ? AND target_id = ? AND deleted_at IS NULL",
    )
    .bind(target.as_str())
    .bind(target_id)
    .fetch_one(pool)
    .await?;
    Ok(total)
}

pub async fn create_comment(
    tx: &mut Transaction<'_, sqlx::MySql>,
    target: CommentTarget,
    target_id: u64,
    author_id: u64,
    body: &str,
) -> Result<u64, AppError> {
    let result = sqlx::query(
        "INSERT INTO public_comments (target_type, target_id, author_id, body) VALUES (?, ?, ?, ?)",
    )
    .bind(target.as_str())
    .bind(target_id)
    .bind(author_id)
    .bind(body)
    .execute(&mut **tx)
    .await?;

    Ok(result.last_insert_id())
}

pub async fn find_comment(pool: &MySqlPool, id: u64) -> Result<Option<Comment>, AppError> {
    let row = sqlx::query(
        "SELECT c.id, c.target_type, c.target_id, c.author_id, u.display_name AS author_name, c.body, c.created_at \
           FROM public_comments c JOIN users u ON u.id = c.author_id \
          WHERE c.id = ? AND c.deleted_at IS NULL",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?;

    row.as_ref().map(comment_from_row).transpose()
}

/// 软删。返回 false 表示「本来就不存在或已删」，调用方自己决定是 404 还是当幂等。
pub async fn soft_delete_comment(
    tx: &mut Transaction<'_, sqlx::MySql>,
    id: u64,
) -> Result<bool, AppError> {
    let result = sqlx::query(
        "UPDATE public_comments SET deleted_at = UTC_TIMESTAMP() WHERE id = ? AND deleted_at IS NULL",
    )
    .bind(id)
    .execute(&mut **tx)
    .await?;

    Ok(result.rows_affected() == 1)
}

// ── 点赞 ────────────────────────────────────────────────────────────────

pub async fn like_count(
    pool: &MySqlPool,
    target: CommentTarget,
    target_id: u64,
) -> Result<u64, AppError> {
    let total: u64 = sqlx::query_scalar(
        "SELECT CAST(COUNT(*) AS UNSIGNED) FROM public_likes WHERE target_type = ? AND target_id = ?",
    )
    .bind(target.as_str())
    .bind(target_id)
    .fetch_one(pool)
    .await?;
    Ok(total)
}

pub async fn is_liked(
    pool: &MySqlPool,
    target: CommentTarget,
    target_id: u64,
    user_id: u64,
) -> Result<bool, AppError> {
    // 用 COUNT 而不是 EXISTS：EXISTS 在 MySQL 里是有符号 BIGINT，
    // 而 `CAST(COUNT(*) AS UNSIGNED)` 的类型是确定的（M4 的教训）。
    let hits: u64 = sqlx::query_scalar(
        "SELECT CAST(COUNT(*) AS UNSIGNED) FROM public_likes \
          WHERE target_type = ? AND target_id = ? AND user_id = ?",
    )
    .bind(target.as_str())
    .bind(target_id)
    .bind(user_id)
    .fetch_one(pool)
    .await?;
    Ok(hits > 0)
}

/// 点赞。返回 false 表示「已经点过了」（唯一索引 1062），不是错误。
///
/// 这里不用 `INSERT IGNORE`：它会把外键错误一起吞成 warning，
/// 于是「用户 id 不存在」会伪装成「已点过」（M3 在 add_member 上踩过同一个坑）。
pub async fn add_like(
    tx: &mut Transaction<'_, sqlx::MySql>,
    target: CommentTarget,
    target_id: u64,
    user_id: u64,
) -> Result<bool, AppError> {
    let result =
        sqlx::query("INSERT INTO public_likes (target_type, target_id, user_id) VALUES (?, ?, ?)")
            .bind(target.as_str())
            .bind(target_id)
            .bind(user_id)
            .execute(&mut **tx)
            .await;

    match result {
        Ok(_) => Ok(true),
        Err(sqlx::Error::Database(db)) => {
            let duplicate = db
                .try_downcast_ref::<sqlx::mysql::MySqlDatabaseError>()
                .map(|e| e.number() == 1062)
                .unwrap_or(false);
            if duplicate {
                Ok(false)
            } else {
                Err(AppError::from(sqlx::Error::Database(db)))
            }
        }
        Err(other) => Err(AppError::from(other)),
    }
}

pub async fn remove_like(
    tx: &mut Transaction<'_, sqlx::MySql>,
    target: CommentTarget,
    target_id: u64,
    user_id: u64,
) -> Result<bool, AppError> {
    let result = sqlx::query(
        "DELETE FROM public_likes WHERE target_type = ? AND target_id = ? AND user_id = ?",
    )
    .bind(target.as_str())
    .bind(target_id)
    .bind(user_id)
    .execute(&mut **tx)
    .await?;

    Ok(result.rows_affected() == 1)
}

// ── 投稿 ────────────────────────────────────────────────────────────────

pub async fn create_submission(
    tx: &mut Transaction<'_, sqlx::MySql>,
    input: NewSubmission<'_>,
) -> Result<u64, AppError> {
    let result =
        sqlx::query("INSERT INTO public_submissions (author_id, title, body) VALUES (?, ?, ?)")
            .bind(input.author_id)
            .bind(input.title)
            .bind(input.body)
            .execute(&mut **tx)
            .await?;

    Ok(result.last_insert_id())
}

pub async fn find_submission(pool: &MySqlPool, id: u64) -> Result<Option<Submission>, AppError> {
    let row = sqlx::query(
        "SELECT s.id, s.author_id, u.display_name AS author_name, s.title, s.body, s.status, \
                s.review_note, s.reviewed_at, s.created_at \
           FROM public_submissions s JOIN users u ON u.id = s.author_id \
          WHERE s.id = ? AND s.deleted_at IS NULL",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?;

    row.as_ref().map(submission_from_row).transpose()
}

/// 在事务里锁住一条投稿，用于"通过投稿"这种**先读后写、中间还要跨模块建文档**的流程。
///
/// `FOR UPDATE OF s` 只锁 `public_submissions` 那一行：不加 `OF s` 时连 `users` 的对应行
/// 也会被锁，管理员改这个用户的状态就会被一条投稿审批堵住。
///
/// 为什么需要它：通过投稿要"建草稿（kb 自己的事务）→ 回填投稿行"，中间有窗口。
/// 两个管理员同时点"通过"会各建一篇草稿，只有一次回填成功，库里就多出一篇没人认领的
/// 草稿。有了行锁，第二个请求会等第一个提交，然后看到状态已不是 pending 而直接拒绝。
pub async fn lock_submission(
    tx: &mut sqlx::Transaction<'_, sqlx::MySql>,
    id: u64,
) -> Result<Option<Submission>, AppError> {
    let row = sqlx::query(
        "SELECT s.id, s.author_id, u.display_name AS author_name, s.title, s.body, s.status, \
                s.review_note, s.reviewed_at, s.created_at \
           FROM public_submissions s JOIN users u ON u.id = s.author_id \
          WHERE s.id = ? AND s.deleted_at IS NULL \
          FOR UPDATE OF s",
    )
    .bind(id)
    .fetch_optional(&mut **tx)
    .await?;

    row.as_ref().map(submission_from_row).transpose()
}

/// 一页投稿。`author` / `status` 为 `None` 时不过滤——用同一条 SQL 加 `? IS NULL` 判断，
/// 避免为四种组合各写一份字面量 SQL（那四份里迟早有一份会漏掉 `deleted_at`）。
pub async fn list_submissions(
    pool: &MySqlPool,
    author: Option<u64>,
    status: Option<SubmissionStatus>,
    page: &PageQuery,
) -> Result<Page<Submission>, AppError> {
    let total = count_submissions(pool, author, status).await?;
    let normalized = Page::<()>::new(Vec::new(), page.page, total, PAGE_SIZE);
    if total > 0 && normalized.page > normalized.total_pages() {
        return Ok(Page::new(Vec::new(), normalized.page, total, PAGE_SIZE));
    }

    let status = status.map(|s| s.as_str());
    let rows = sqlx::query(
        "SELECT s.id, s.author_id, u.display_name AS author_name, s.title, s.body, s.status, \
                s.review_note, s.reviewed_at, s.created_at \
           FROM public_submissions s JOIN users u ON u.id = s.author_id \
          WHERE s.deleted_at IS NULL \
            AND (? IS NULL OR s.author_id = ?) \
            AND (? IS NULL OR s.status = ?) \
          ORDER BY s.created_at DESC, s.id DESC LIMIT ? OFFSET ?",
    )
    .bind(author)
    .bind(author)
    .bind(status)
    .bind(status)
    .bind(PAGE_SIZE as i64)
    .bind(normalized.offset() as i64)
    .fetch_all(pool)
    .await?;

    let items = rows
        .iter()
        .map(submission_from_row)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Page::new(items, normalized.page, total, PAGE_SIZE))
}

pub async fn count_submissions(
    pool: &MySqlPool,
    author: Option<u64>,
    status: Option<SubmissionStatus>,
) -> Result<u64, AppError> {
    let status = status.map(|s| s.as_str());
    let total: u64 = sqlx::query_scalar(
        "SELECT CAST(COUNT(*) AS UNSIGNED) FROM public_submissions s \
          WHERE s.deleted_at IS NULL \
            AND (? IS NULL OR s.author_id = ?) \
            AND (? IS NULL OR s.status = ?)",
    )
    .bind(author)
    .bind(author)
    .bind(status)
    .bind(status)
    .fetch_one(pool)
    .await?;
    Ok(total)
}

// ── 行映射 ──────────────────────────────────────────────────────────────

fn comment_from_row(row: &MySqlRow) -> Result<Comment, AppError> {
    Ok(Comment {
        id: row.try_get("id")?,
        target_type: CommentTarget::from_db(row.try_get::<String, _>("target_type")?.as_str())?,
        target_id: row.try_get("target_id")?,
        author_id: row.try_get("author_id")?,
        author_name: row.try_get("author_name")?,
        body: row.try_get("body")?,
        created_at: row.try_get("created_at")?,
    })
}

fn submission_from_row(row: &MySqlRow) -> Result<Submission, AppError> {
    Ok(Submission {
        id: row.try_get("id")?,
        author_id: row.try_get("author_id")?,
        author_name: row.try_get("author_name")?,
        title: row.try_get("title")?,
        body: row.try_get("body")?,
        status: SubmissionStatus::from_db(row.try_get::<String, _>("status")?.as_str())?,
        review_note: row.try_get("review_note")?,
        reviewed_at: row.try_get("reviewed_at")?,
        created_at: row.try_get("created_at")?,
    })
}

// ── 加入申请（M7） ──────────────────────────────────────────────────────
//
// `join_requests.status` 与 `users.status` 是两套完全不同的取值（L72）。
// 本文件里**每一条 SQL 都写 `j.status`**，不裸写 `status`——裸写在两张表之间
// 没有歧义的时候看起来很干净，但一旦有人 JOIN 起来就会变成猜谜。

/// 加入申请的状态。与 `users.status` 无关。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JoinStatus {
    Pending,
    Approved,
    Rejected,
}

impl JoinStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Approved => "approved",
            Self::Rejected => "rejected",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Pending => "待处理",
            Self::Approved => "已通过",
            Self::Rejected => "已驳回",
        }
    }

    pub fn from_db(raw: &str) -> Result<Self, AppError> {
        match raw {
            "pending" => Ok(Self::Pending),
            "approved" => Ok(Self::Approved),
            "rejected" => Ok(Self::Rejected),
            other => Err(AppError::internal(anyhow::anyhow!(
                "join_requests.status 出现未知取值：{other}"
            ))),
        }
    }
}

/// 一条加入申请（带申请人信息，列表页直接渲染）。
#[derive(Debug, Clone)]
pub struct JoinRow {
    pub id: u64,
    pub user_id: u64,
    pub username: String,
    pub display_name: String,
    pub email: String,
    pub message: String,
    pub status: JoinStatus,
    pub review_note: Option<String>,
    pub reviewed_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

/// 新建申请的入参。
#[derive(Debug, Clone, Copy)]
pub struct NewJoinRequest<'a> {
    pub user_id: u64,
    pub message: &'a str,
}

/// 这个用户当前有没有待处理的申请。有则返回 id。
///
/// 之所以查而不是靠唯一索引：唯一索引会让"被驳回后重新申请"永久失败，
/// 而那是完全正常的诉求。
pub async fn find_pending_join_request(
    pool: &MySqlPool,
    user_id: u64,
) -> Result<Option<u64>, AppError> {
    let id: Option<u64> = sqlx::query_scalar(
        "SELECT j.id FROM join_requests j \
          WHERE j.user_id = ? AND j.status = 'pending' \
          ORDER BY j.id DESC LIMIT 1",
    )
    .bind(user_id)
    .fetch_optional(pool)
    .await?;
    Ok(id)
}

/// 同一个人只能有一条待处理申请——这句话在库里由 `uk_join_pending` 保证（迁移 0015）。
/// service 层的"先查再插"只是把正常路径的提示说得更清楚，真正的护栏是唯一索引：
/// 两次并发点击都能通过那条检查，但只有一条插得进去。
pub const PENDING_EXISTS_MESSAGE: &str = "你已经有一条待处理的申请了，等管理员看完再说";

pub async fn create_join_request(
    tx: &mut Transaction<'_, sqlx::MySql>,
    input: NewJoinRequest<'_>,
) -> Result<u64, AppError> {
    let result = sqlx::query(
        "INSERT INTO join_requests (user_id, message, status) VALUES (?, ?, 'pending')",
    )
    .bind(input.user_id)
    .bind(input.message)
    .execute(&mut **tx)
    .await
    .map_err(|err| match &err {
        sqlx::Error::Database(db)
            if db
                .try_downcast_ref::<sqlx::mysql::MySqlDatabaseError>()
                .is_some_and(|e| e.number() == 1062) =>
        {
            AppError::conflict(PENDING_EXISTS_MESSAGE)
        }
        _ => AppError::from(err),
    })?;
    Ok(result.last_insert_id())
}

pub async fn list_join_requests(
    pool: &MySqlPool,
    status: Option<JoinStatus>,
    page: &PageQuery,
) -> Result<Page<JoinRow>, AppError> {
    let status = status.map(|s| s.as_str());
    let offset = page.page.saturating_sub(1).saturating_mul(PAGE_SIZE);
    let rows = sqlx::query(
        "SELECT j.id, j.user_id, u.username, u.display_name, u.email, j.message, \
                j.status, j.review_note, j.reviewed_at, j.created_at \
           FROM join_requests j JOIN users u ON u.id = j.user_id \
          WHERE (? IS NULL OR j.status = ?) \
          ORDER BY j.created_at DESC, j.id DESC \
          LIMIT ? OFFSET ?",
    )
    .bind(status)
    .bind(status)
    .bind(PAGE_SIZE)
    .bind(offset)
    .fetch_all(pool)
    .await?;

    let mut items = Vec::with_capacity(rows.len());
    for row in &rows {
        items.push(join_from_row(row)?);
    }

    let total = count_join_requests(pool, status).await?;
    Ok(Page::new(items, page.page, total, PAGE_SIZE))
}

pub async fn count_join_requests(pool: &MySqlPool, status: Option<&str>) -> Result<u64, AppError> {
    let total: u64 = sqlx::query_scalar(
        "SELECT CAST(COUNT(*) AS UNSIGNED) FROM join_requests j \
          WHERE (? IS NULL OR j.status = ?)",
    )
    .bind(status)
    .bind(status)
    .fetch_one(pool)
    .await?;
    Ok(total)
}

pub async fn find_join_request(pool: &MySqlPool, id: u64) -> Result<Option<JoinRow>, AppError> {
    let row = sqlx::query(
        "SELECT j.id, j.user_id, u.username, u.display_name, u.email, j.message, \
                j.status, j.review_note, j.reviewed_at, j.created_at \
           FROM join_requests j JOIN users u ON u.id = j.user_id \
          WHERE j.id = ?",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?;
    match row {
        Some(row) => Ok(Some(join_from_row(&row)?)),
        None => Ok(None),
    }
}

/// 只处理 `pending` 的行；返回是否真的改到了（false = 行不存在或已被处理）。
///
/// 这个 WHERE 条件是**并发防线**：两个 admin 同时点"通过"，
/// 第二个会拿到 false，从而不写第二条审计、不重复升角色。
pub async fn mark_join_reviewed(
    tx: &mut Transaction<'_, sqlx::MySql>,
    id: u64,
    status: JoinStatus,
    reviewed_by: u64,
    note: &str,
) -> Result<bool, AppError> {
    let result = sqlx::query(
        "UPDATE join_requests j \
            SET j.status = ?, j.review_note = ?, j.reviewed_by = ?, j.reviewed_at = UTC_TIMESTAMP() \
          WHERE j.id = ? AND j.status = 'pending'",
    )
    .bind(status.as_str())
    .bind(note)
    .bind(reviewed_by)
    .bind(id)
    .execute(&mut **tx)
    .await?;
    Ok(result.rows_affected() == 1)
}

/// 投稿审核落库。`target` 为 `None` 表示驳回（不挂目标）。
pub async fn mark_submission_reviewed(
    tx: &mut Transaction<'_, sqlx::MySql>,
    id: u64,
    status: SubmissionStatus,
    reviewed_by: u64,
    note: &str,
    target: Option<(&str, u64)>,
) -> Result<bool, AppError> {
    let (target_type, target_id) = match target {
        Some((kind, id)) => (Some(kind), Some(id)),
        None => (None, None),
    };
    let result = sqlx::query(
        "UPDATE public_submissions s \
            SET s.status = ?, s.review_note = ?, s.reviewed_by = ?, s.reviewed_at = UTC_TIMESTAMP(), \
                s.target_type = ?, s.target_id = ? \
          WHERE s.id = ? AND s.status = 'pending'",
    )
    .bind(status.as_str())
    .bind(note)
    .bind(reviewed_by)
    .bind(target_type)
    .bind(target_id)
    .bind(id)
    .execute(&mut **tx)
    .await?;
    Ok(result.rows_affected() == 1)
}

fn join_from_row(row: &MySqlRow) -> Result<JoinRow, AppError> {
    Ok(JoinRow {
        id: row.try_get("id")?,
        user_id: row.try_get("user_id")?,
        username: row.try_get("username")?,
        display_name: row.try_get("display_name")?,
        email: row.try_get("email")?,
        message: row.try_get("message")?,
        status: JoinStatus::from_db(row.try_get::<String, _>("status")?.as_str())?,
        review_note: row.try_get("review_note")?,
        reviewed_at: row.try_get("reviewed_at")?,
        created_at: row.try_get("created_at")?,
    })
}
