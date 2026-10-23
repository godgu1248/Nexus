//! 论坛的数据访问层：空间 → 议题 → 回复。
//!
//! 三条贯穿本模块的规矩：
//!
//! 1. **SQL 一律字面量**。sqlx 0.9 的 `query()` 只接受 `&'static str`（`SqlSafeStr`），
//!    `format!` 拼出来的字符串编译期就过不去——这是好事，它把注入面直接焊死了。
//!    代价是"按角色切换 WHERE 条件"不能拼串，只能写成两条字面量语句再二选一。
//! 2. **可见性在 SQL 里过滤，不在 Rust 里过滤**。取回来再 `.filter()` 会让分页总数和
//!    实际条目对不上，而且一旦某条查询忘了过滤，泄漏是静默的。
//! 3. **没有 `reply_count` 列**。计数器会漂移（删回复、删议题都可能漏减），
//!    10 人团队的数据量用相关子查询 + 索引完全够。用查询换正确性。
//!
//! `visibility = 'group'` 的行自 M3 起会出现在成员可见的列表里，但**不靠 SQL 过滤**：
//! `IN (?,?,…)` 的占位符个数是运行期才知道的，而 sqlx 只收字面量 SQL。
//! 因此小组行的最终裁决交给 `visible_spaces()`（白名单过滤，见该函数注释），
//! 单条记录的读取则一律先取回再交给 `policy::can_view` 判定。

use serde_json::Value;
use sqlx::mysql::MySqlRow;
use sqlx::{MySqlPool, Row, Transaction};

use crate::auth::policy::{self, CurrentUser};
use crate::error::AppError;
use crate::shared::pagination::{PAGE_SIZE, Page, PageQuery};
use crate::shared::visibility::Visibility;

/// 一条空间。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Space {
    pub id: u64,
    pub name: String,
    pub slug: String,
    pub description: String,
    /// 热度：未删议题数 + 未删回复数。分区列表按它倒序（DESIGN §31）。
    /// `forum_spaces.sort_order` 列还在，只为回滚；界面与排序都不再读它。
    pub thread_count: u64,
    pub post_count: u64,
    pub visibility: Visibility,
    /// `visibility = group` 时指向 `member_groups.id`；其它可见性下为 `None`。
    pub group_id: Option<u64>,
    pub created_by: u64,
}

/// 新建空间。
#[derive(Debug, Clone)]
pub struct NewSpace<'a> {
    pub name: &'a str,
    pub slug: &'a str,
    pub description: &'a str,
    pub visibility: Visibility,
    pub created_by: u64,
}

/// 议题列表/详情共用的一行。带上空间信息，避免每个议题再查一次空间（N+1）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadRow {
    pub id: u64,
    pub space_id: u64,
    pub space_slug: String,
    pub space_name: String,
    pub author_id: u64,
    pub author_name: String,
    pub title: String,
    pub visibility: Visibility,
    pub group_id: Option<u64>,
    pub is_pinned: bool,
    pub is_resolved: bool,
    pub is_archived: bool,
    pub reply_count: u64,
    pub last_post_at: chrono::DateTime<chrono::Utc>,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

/// 新建议题。
#[derive(Debug, Clone)]
pub struct NewThread<'a> {
    pub space_id: u64,
    pub author_id: u64,
    pub title: &'a str,
    pub visibility: Visibility,
    pub group_id: Option<u64>,
}

/// 一条回复。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Post {
    pub id: u64,
    pub thread_id: u64,
    pub author_id: u64,
    pub author_name: String,
    pub body: String,
    /// 净化后的富文本 HTML；`None` 表示这条帖子只有纯文本（老数据或纯文本提交）。
    pub body_html: Option<String>,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Clone)]
pub struct NewPost<'a> {
    pub thread_id: u64,
    pub author_id: u64,
    pub body: &'a str,
    /// 已经过 `shared::richtext::sanitize` 的 HTML；`None` 表示只有纯文本。
    pub body_html: Option<&'a str>,
}

/// 调用者能看到哪些可见性。用来决定走哪条字面量 SQL。
///
/// 这里的 `can_view` 调用是刻意的：策略只写在一个地方，SQL 只负责执行它的结论。
fn include_internal(viewer: &CurrentUser) -> bool {
    let ctx = crate::auth::policy::ContentCtx::new(Visibility::Internal, None, None);
    crate::auth::policy::can_view(Some(viewer), &ctx)
}

// 议题查询的列清单在下面被重复了三遍（两条列表语句 + 一条详情语句）。
// 这不是手滑：sqlx 0.9 的 `query()` 只接受 `&'static str`，任何 `format!` 拼接都会
// 在编译期被 `SqlSafeStr` 拦下。用常量拼串省下的这几行，换来的是一整类注入风险，
// 不划算。要改列就三处一起改——重复在这里是特性，不是缺陷。

pub async fn list_spaces(pool: &MySqlPool, viewer: &CurrentUser) -> Result<Vec<Space>, AppError> {
    // 排序按热度（议题 + 回复），不按人工 sort_order：分区谁活跃谁靠前，
    // 团队新开的分区不用靠管理员记得去改数字才能被看见。
    // 热度相同时按 id 升序——稳定顺序，避免同热度下每次刷新换位。
    let rows = if include_internal(viewer) {
        sqlx::query(
            "SELECT s.id, s.name, s.slug, s.description, s.visibility, s.group_id, s.created_by, \
                    CAST((SELECT COUNT(*) FROM forum_threads t \
                          WHERE t.space_id = s.id AND t.deleted_at IS NULL) AS UNSIGNED) AS thread_count, \
                    CAST((SELECT COUNT(*) FROM forum_posts p \
                          JOIN forum_threads t2 ON t2.id = p.thread_id \
                          WHERE t2.space_id = s.id AND t2.deleted_at IS NULL \
                            AND p.deleted_at IS NULL) AS UNSIGNED) AS post_count \
             FROM forum_spaces s \
             WHERE s.deleted_at IS NULL AND s.visibility IN ('public', 'internal', 'group') \
             ORDER BY (thread_count + post_count) DESC, s.id ASC",
        )
        .fetch_all(pool)
        .await?
    } else {
        sqlx::query(
            "SELECT s.id, s.name, s.slug, s.description, s.visibility, s.group_id, s.created_by, \
                    CAST((SELECT COUNT(*) FROM forum_threads t \
                          WHERE t.space_id = s.id AND t.deleted_at IS NULL) AS UNSIGNED) AS thread_count, \
                    CAST((SELECT COUNT(*) FROM forum_posts p \
                          JOIN forum_threads t2 ON t2.id = p.thread_id \
                          WHERE t2.space_id = s.id AND t2.deleted_at IS NULL \
                            AND p.deleted_at IS NULL) AS UNSIGNED) AS post_count \
             FROM forum_spaces s \
             WHERE s.deleted_at IS NULL AND s.visibility = 'public' \
             ORDER BY (thread_count + post_count) DESC, s.id ASC",
        )
        .fetch_all(pool)
        .await?
    };

    let spaces: Vec<Space> = rows.iter().map(row_to_space).collect::<Result<_, _>>()?;
    Ok(visible_spaces(viewer, spaces))
}

/// 小组分区在 Rust 侧按成员身份过滤，而不是拼进 SQL。
///
/// 为什么不在 SQL 里过滤：sqlx 0.9 的 `query()` 只收 `&'static str`，
/// `IN (?,?,?)` 的占位符个数是运行期才知道的，拼串会被编译器直接拦下。
/// 为什么这里可以接受：① 这个查询不分页（分区数量是团队规模级，几十条）；
/// ② 过滤是**白名单**——只有 group_id 明确落在 viewer.group_ids 里才保留，
/// 漏判一律不可见，不会因为某个分支忘了写而泄漏。
fn visible_spaces(viewer: &CurrentUser, spaces: Vec<Space>) -> Vec<Space> {
    spaces
        .into_iter()
        .filter(|space| {
            policy::can_view(
                Some(viewer),
                &policy::ContentCtx::new(space.visibility, None, space.group_id),
            )
        })
        .collect()
}

pub async fn find_space_by_slug(pool: &MySqlPool, slug: &str) -> Result<Option<Space>, AppError> {
    // 注意：这里**不按可见性过滤**。调用方要先拿到空间才能判断"该给 404 还是 403"，
    // 而且无权时统一按"不存在"处理——过滤掉就分不清"没这个 slug"和"你没权限"。
    let row = sqlx::query(
        "SELECT s.id, s.name, s.slug, s.description, s.visibility, s.group_id, s.created_by, \
                CAST((SELECT COUNT(*) FROM forum_threads t \
                      WHERE t.space_id = s.id AND t.deleted_at IS NULL) AS UNSIGNED) AS thread_count, \
                CAST((SELECT COUNT(*) FROM forum_posts p \
                      JOIN forum_threads t2 ON t2.id = p.thread_id \
                      WHERE t2.space_id = s.id AND t2.deleted_at IS NULL \
                        AND p.deleted_at IS NULL) AS UNSIGNED) AS post_count \
         FROM forum_spaces s WHERE s.slug = ? AND s.deleted_at IS NULL",
    )
    .bind(slug)
    .fetch_optional(pool)
    .await?;

    row.as_ref().map(row_to_space).transpose()
}

pub async fn find_space_by_id(pool: &MySqlPool, id: u64) -> Result<Option<Space>, AppError> {
    let row = sqlx::query(
        "SELECT s.id, s.name, s.slug, s.description, s.visibility, s.group_id, s.created_by, \
                CAST((SELECT COUNT(*) FROM forum_threads t \
                      WHERE t.space_id = s.id AND t.deleted_at IS NULL) AS UNSIGNED) AS thread_count, \
                CAST((SELECT COUNT(*) FROM forum_posts p \
                      JOIN forum_threads t2 ON t2.id = p.thread_id \
                      WHERE t2.space_id = s.id AND t2.deleted_at IS NULL \
                        AND p.deleted_at IS NULL) AS UNSIGNED) AS post_count \
         FROM forum_spaces s WHERE s.id = ? AND s.deleted_at IS NULL",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?;

    row.as_ref().map(row_to_space).transpose()
}

pub async fn create_space(
    tx: &mut Transaction<'_, sqlx::MySql>,
    input: NewSpace<'_>,
) -> Result<u64, AppError> {
    // INSERT 必须用 execute + last_insert_id；query_scalar 会因为"没有返回行"直接 RowNotFound。
    // sort_order 列还在（回滚用），但新建一律落默认 0：它的语义已经被"按热度排序"取代。
    let result = sqlx::query(
        "INSERT INTO forum_spaces (name, slug, description, visibility, created_by) \
         VALUES (?, ?, ?, ?, ?)",
    )
    .bind(input.name)
    .bind(input.slug)
    .bind(input.description)
    .bind(input.visibility.as_str())
    .bind(input.created_by)
    .execute(&mut **tx)
    .await?;

    Ok(result.last_insert_id())
}

pub async fn count_threads(pool: &MySqlPool, space_id: u64) -> Result<u64, AppError> {
    let total: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM forum_threads WHERE space_id = ? AND deleted_at IS NULL",
    )
    .bind(space_id)
    .fetch_one(pool)
    .await?;
    Ok(total as u64)
}

pub async fn list_threads(
    pool: &MySqlPool,
    space_id: u64,
    viewer: &CurrentUser,
    page: &PageQuery,
) -> Result<Page<ThreadRow>, AppError> {
    let total = count_threads(pool, space_id).await?;
    let normalized = Page::<()>::new(Vec::new(), page.page, total, PAGE_SIZE);
    if normalized.items.is_empty() && total > 0 && normalized.page > normalized.total_pages() {
        // 越界页：返回空列表而不是让 LIMIT/OFFSET 越界报错，前端看到的是"这一页没有内容"。
        return Ok(Page::new(Vec::new(), normalized.page, total, PAGE_SIZE));
    }

    let limit = PAGE_SIZE as i64;
    let offset = normalized.offset() as i64;

    let rows = if include_internal(viewer) {
        sqlx::query(
            "SELECT t.id, t.space_id, s.slug AS space_slug, s.name AS space_name, \
             t.author_id, u.display_name AS author_name, t.title, t.visibility, t.group_id, t.is_pinned, \
             t.is_resolved, t.is_archived, t.last_post_at, t.created_at, \
             (SELECT COUNT(*) FROM forum_posts p WHERE p.thread_id = t.id AND p.deleted_at IS NULL) AS reply_count \
             FROM forum_threads t \
             JOIN forum_spaces s ON s.id = t.space_id \
             JOIN users u ON u.id = t.author_id \
             WHERE t.space_id = ? AND t.deleted_at IS NULL AND t.visibility IN ('public', 'internal', 'group') \
             ORDER BY t.is_pinned DESC, t.last_post_at DESC, t.id DESC LIMIT ? OFFSET ?",
        )
        .bind(space_id)
        .bind(limit)
        .bind(offset)
        .fetch_all(pool)
        .await?
    } else {
        sqlx::query(
            "SELECT t.id, t.space_id, s.slug AS space_slug, s.name AS space_name, \
             t.author_id, u.display_name AS author_name, t.title, t.visibility, t.group_id, t.is_pinned, \
             t.is_resolved, t.is_archived, t.last_post_at, t.created_at, \
             (SELECT COUNT(*) FROM forum_posts p WHERE p.thread_id = t.id AND p.deleted_at IS NULL) AS reply_count \
             FROM forum_threads t \
             JOIN forum_spaces s ON s.id = t.space_id \
             JOIN users u ON u.id = t.author_id \
             WHERE t.space_id = ? AND t.deleted_at IS NULL AND t.visibility = 'public' \
             ORDER BY t.is_pinned DESC, t.last_post_at DESC, t.id DESC LIMIT ? OFFSET ?",
        )
        .bind(space_id)
        .bind(limit)
        .bind(offset)
        .fetch_all(pool)
        .await?
    };

    let items = rows
        .iter()
        .map(row_to_thread)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Page::new(items, normalized.page, total, PAGE_SIZE))
}

pub async fn find_thread(pool: &MySqlPool, id: u64) -> Result<Option<ThreadRow>, AppError> {
    // 详情页不按可见性过滤：调用方要先拿到议题才知道该按哪条规则判，而且判失败了
    // 也要统一回 404（而不是让"查不到"和"没权限"在数据库层面就分不出来）。
    let row = sqlx::query(
        "SELECT t.id, t.space_id, s.slug AS space_slug, s.name AS space_name, \
         t.author_id, u.display_name AS author_name, t.title, t.visibility, t.group_id, t.is_pinned, \
         t.is_resolved, t.is_archived, t.last_post_at, t.created_at, \
         (SELECT COUNT(*) FROM forum_posts p WHERE p.thread_id = t.id AND p.deleted_at IS NULL) AS reply_count \
         FROM forum_threads t \
         JOIN forum_spaces s ON s.id = t.space_id \
         JOIN users u ON u.id = t.author_id \
         WHERE t.id = ? AND t.deleted_at IS NULL",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?;

    row.as_ref().map(row_to_thread).transpose()
}

pub async fn create_thread(
    tx: &mut Transaction<'_, sqlx::MySql>,
    input: NewThread<'_>,
) -> Result<u64, AppError> {
    let result = sqlx::query(
        "INSERT INTO forum_threads (space_id, author_id, title, visibility, group_id) VALUES (?, ?, ?, ?, ?)",
    )
    .bind(input.space_id)
    .bind(input.author_id)
    .bind(input.title)
    .bind(input.visibility.as_str())
    .bind(input.group_id)
    .execute(&mut **tx)
    .await?;

    Ok(result.last_insert_id())
}

/// 回复之后把议题顶上来。
///
/// 用 `CURRENT_TIMESTAMP` 而不是 `NOW()`：前者在会话时区里也是同一时刻，
/// 但语义更明确是"数据库此刻"；本项目的会话时区已经钉死 UTC（`after_connect`）。
pub async fn touch_thread(
    tx: &mut Transaction<'_, sqlx::MySql>,
    thread_id: u64,
) -> Result<(), AppError> {
    sqlx::query("UPDATE forum_threads SET last_post_at = CURRENT_TIMESTAMP WHERE id = ?")
        .bind(thread_id)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

pub async fn set_resolved(
    tx: &mut Transaction<'_, sqlx::MySql>,
    thread_id: u64,
    resolved: bool,
) -> Result<(), AppError> {
    sqlx::query("UPDATE forum_threads SET is_resolved = ? WHERE id = ?")
        .bind(i8::from(resolved))
        .bind(thread_id)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

pub async fn set_archived(
    tx: &mut Transaction<'_, sqlx::MySql>,
    thread_id: u64,
    archived: bool,
) -> Result<(), AppError> {
    sqlx::query("UPDATE forum_threads SET is_archived = ? WHERE id = ?")
        .bind(i8::from(archived))
        .bind(thread_id)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

pub async fn count_posts(pool: &MySqlPool, thread_id: u64) -> Result<u64, AppError> {
    let total: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM forum_posts WHERE thread_id = ? AND deleted_at IS NULL",
    )
    .bind(thread_id)
    .fetch_one(pool)
    .await?;
    Ok(total as u64)
}

pub async fn list_posts(
    pool: &MySqlPool,
    thread_id: u64,
    page: &PageQuery,
) -> Result<Page<Post>, AppError> {
    let total = count_posts(pool, thread_id).await?;
    let normalized = Page::<()>::new(Vec::new(), page.page, total, PAGE_SIZE);
    if total > 0 && normalized.page > normalized.total_pages() {
        return Ok(Page::new(Vec::new(), normalized.page, total, PAGE_SIZE));
    }

    let rows = sqlx::query(
        "SELECT p.id, p.thread_id, p.author_id, u.display_name AS author_name, p.body, p.body_html, p.created_at \
         FROM forum_posts p JOIN users u ON u.id = p.author_id \
         WHERE p.thread_id = ? AND p.deleted_at IS NULL \
         ORDER BY p.created_at ASC, p.id ASC LIMIT ? OFFSET ?",
    )
    .bind(thread_id)
    .bind(PAGE_SIZE as i64)
    .bind(normalized.offset() as i64)
    .fetch_all(pool)
    .await?;

    let items = rows
        .iter()
        .map(row_to_post)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Page::new(items, normalized.page, total, PAGE_SIZE))
}

pub async fn create_post(
    tx: &mut Transaction<'_, sqlx::MySql>,
    input: NewPost<'_>,
) -> Result<u64, AppError> {
    let result = sqlx::query(
        "INSERT INTO forum_posts (thread_id, author_id, body, body_html) VALUES (?, ?, ?, ?)",
    )
    .bind(input.thread_id)
    .bind(input.author_id)
    .bind(input.body)
    .bind(input.body_html)
    .execute(&mut **tx)
    .await?;

    Ok(result.last_insert_id())
}

pub async fn find_post(pool: &MySqlPool, id: u64) -> Result<Option<Post>, AppError> {
    let row = sqlx::query(
        "SELECT p.id, p.thread_id, p.author_id, u.display_name AS author_name, p.body, p.body_html, p.created_at \
         FROM forum_posts p JOIN users u ON u.id = p.author_id \
         WHERE p.id = ? AND p.deleted_at IS NULL",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?;

    row.as_ref().map(row_to_post).transpose()
}

/// 软删除一条回复。权限由 service 判定——这里有意识不做 `author_id = ?` 的二次校验，
/// 因为 admin 也要能删别人的回复，把条件写进 SQL 就得再传一个"是否是 admin"的布尔。
pub async fn soft_delete_post(
    tx: &mut Transaction<'_, sqlx::MySql>,
    post_id: u64,
) -> Result<(), AppError> {
    sqlx::query(
        "UPDATE forum_posts SET deleted_at = CURRENT_TIMESTAMP WHERE id = ? AND deleted_at IS NULL",
    )
    .bind(post_id)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

fn visibility_of(row: &MySqlRow, column: &str) -> Result<Visibility, AppError> {
    let raw: String = row.try_get(column)?;
    Visibility::from_db(&raw)
        .ok_or_else(|| AppError::internal(anyhow::anyhow!("{column} 出现未知取值：{raw}")))
}

fn flag(row: &MySqlRow, column: &str) -> Result<bool, AppError> {
    // MySQL 的 TINYINT(1) 在 sqlx 里读 i8 最稳；直接读 bool 依赖驱动对 tinyint 的解释。
    let raw: i8 = row.try_get(column)?;
    Ok(raw != 0)
}

fn row_to_space(row: &MySqlRow) -> Result<Space, AppError> {
    // 热度两列是 `CAST(... AS UNSIGNED)`：MySQL 会把它们报成 BIGINT UNSIGNED，
    // 所以这里必须读 u64。读 i64 会得到
    // `Rust type i64 (as SQL type BIGINT) is not compatible with SQL type BIGINT UNSIGNED`——
    // 这正是 kb 那次"标量子查询计数"踩过的同一类坑（DESIGN §22.12.1）。
    let thread_count: u64 = row.try_get("thread_count")?;
    let post_count: u64 = row.try_get("post_count")?;
    Ok(Space {
        id: row.try_get("id")?,
        name: row.try_get("name")?,
        slug: row.try_get("slug")?,
        description: row.try_get("description")?,
        thread_count,
        post_count,
        visibility: visibility_of(row, "visibility")?,
        group_id: row.try_get("group_id")?,
        created_by: row.try_get("created_by")?,
    })
}

fn row_to_thread(row: &MySqlRow) -> Result<ThreadRow, AppError> {
    let reply_count: i64 = row.try_get("reply_count")?;
    Ok(ThreadRow {
        id: row.try_get("id")?,
        space_id: row.try_get("space_id")?,
        space_slug: row.try_get("space_slug")?,
        space_name: row.try_get("space_name")?,
        author_id: row.try_get("author_id")?,
        author_name: row.try_get("author_name")?,
        title: row.try_get("title")?,
        visibility: visibility_of(row, "visibility")?,
        group_id: row.try_get("group_id")?,
        is_pinned: flag(row, "is_pinned")?,
        is_resolved: flag(row, "is_resolved")?,
        is_archived: flag(row, "is_archived")?,
        reply_count: reply_count.max(0) as u64,
        last_post_at: row.try_get("last_post_at")?,
        created_at: row.try_get("created_at")?,
    })
}

fn row_to_post(row: &MySqlRow) -> Result<Post, AppError> {
    Ok(Post {
        id: row.try_get("id")?,
        thread_id: row.try_get("thread_id")?,
        author_id: row.try_get("author_id")?,
        author_name: row.try_get("author_name")?,
        body: row.try_get("body")?,
        body_html: row.try_get("body_html")?,
        created_at: row.try_get("created_at")?,
    })
}

/// 改分区的可见性与组归属。
///
/// **同时**把该分区下所有未删议题的可见性一起改写。理由：议题的可见性在创建时
/// 继承空间，若只改空间，那么"内部 → 小组"的收紧对历史议题不生效——旧议题仍是
/// internal，而 internal 对所有 member 可见，等于这次收紧是个假动作。
/// 放宽方向同理，否则会出现"分区公开、议题内部"的谜之状态。
///
/// 返回 `false` 表示分区不存在或已软删。
pub async fn set_space_visibility(
    tx: &mut sqlx::Transaction<'_, sqlx::MySql>,
    space_id: u64,
    visibility: Visibility,
    group_id: Option<u64>,
) -> Result<bool, AppError> {
    let updated = sqlx::query(
        "UPDATE forum_spaces SET visibility = ?, group_id = ? \
         WHERE id = ? AND deleted_at IS NULL",
    )
    .bind(visibility.as_str())
    .bind(group_id)
    .bind(space_id)
    .execute(&mut **tx)
    .await?
    .rows_affected()
        > 0;

    if !updated {
        return Ok(false);
    }

    sqlx::query(
        "UPDATE forum_threads SET visibility = ?, group_id = ? \
         WHERE space_id = ? AND deleted_at IS NULL",
    )
    .bind(visibility.as_str())
    .bind(group_id)
    .bind(space_id)
    .execute(&mut **tx)
    .await?;

    Ok(true)
}

/// 审计 meta 里允许出现的最小上下文。单独抽出来是为了让"meta 里有什么"只有一个定义处。
pub fn thread_meta(thread: &ThreadRow) -> Value {
    serde_json::json!({
        "thread_id": thread.id,
        "space_id": thread.space_id,
        "title": thread.title,
    })
}

/// 前厅用的公开议题列表（M6）。
///
/// 与 [`list_threads`] 的区别：那个是"某个分区里的议题"，这个是"全站对外可见的议题"。
/// 判定要**同时**满足三件事：议题是 `public`、它所在的分区也是 `public`、两者都没被删。
/// 只看议题可见性是不够的——分区被整体收回时，里面的议题不该继续在门厅挂着。
///
/// 这是 public 模块唯一需要 forum 新增的查询：前厅要一份跨分区的公开列表，
/// 用 `list_threads` 逐个分区拉会变成 N+1。它只读、不写，符合需求 L123 的只读依赖。
pub async fn list_public_threads(
    pool: &MySqlPool,
    page: &PageQuery,
) -> Result<Page<ThreadRow>, AppError> {
    let total: u64 = sqlx::query_scalar(
        "SELECT CAST(COUNT(*) AS UNSIGNED) \
           FROM forum_threads t JOIN forum_spaces s ON s.id = t.space_id \
          WHERE t.deleted_at IS NULL AND s.deleted_at IS NULL \
            AND t.visibility = 'public' AND s.visibility = 'public'",
    )
    .fetch_one(pool)
    .await?;

    let normalized = Page::<()>::new(Vec::new(), page.page, total, PAGE_SIZE);
    if total > 0 && normalized.page > normalized.total_pages() {
        return Ok(Page::new(Vec::new(), normalized.page, total, PAGE_SIZE));
    }

    let rows = sqlx::query(
        "SELECT t.id, t.space_id, s.slug AS space_slug, s.name AS space_name, \
         t.author_id, u.display_name AS author_name, t.title, t.visibility, t.group_id, t.is_pinned, \
         t.is_resolved, t.is_archived, t.last_post_at, t.created_at, \
         (SELECT COUNT(*) FROM forum_posts p WHERE p.thread_id = t.id AND p.deleted_at IS NULL) AS reply_count \
         FROM forum_threads t \
         JOIN forum_spaces s ON s.id = t.space_id \
         JOIN users u ON u.id = t.author_id \
         WHERE t.deleted_at IS NULL AND s.deleted_at IS NULL \
           AND t.visibility = 'public' AND s.visibility = 'public' \
         ORDER BY t.last_post_at DESC, t.id DESC LIMIT ? OFFSET ?",
    )
    .bind(PAGE_SIZE as i64)
    .bind(normalized.offset() as i64)
    .fetch_all(pool)
    .await?;

    let items = rows
        .iter()
        .map(row_to_thread)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Page::new(items, normalized.page, total, PAGE_SIZE))
}
