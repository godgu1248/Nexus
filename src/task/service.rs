//! 任务的业务规则。
//!
//! 这一层是唯一同时知道"权限""事务""审计"三件事的地方：
//! 权限判定、`pool.begin()`、`audit::record`、`commit` 全部在这里闭环，
//! handler 只负责把 HTTP 入参翻成这里的入参、把错误翻成状态码。

use chrono::NaiveDate;
use sqlx::MySqlPool;

use crate::auth::model::{UserStatus, find_by_id};
use crate::auth::policy::{self, ContentCtx, CurrentUser};
use crate::error::AppError;
use crate::forum::model as forum_model;
use crate::shared::audit::{self, AuditCtx, action};
use crate::shared::pagination::{PAGE_SIZE, Page, PageQuery};
use crate::shared::validate;
use crate::shared::visibility::Visibility;
use crate::task::model::{self, NewTask, Task, TaskCard, TaskStatus};

pub const TITLE_MAX: usize = 200;
pub const BODY_MAX: usize = 20_000;

/// 新建 / 从议题生成共用的入参。
#[derive(Debug, Clone, Copy)]
pub struct TaskInput<'a> {
    pub title: &'a str,
    pub body: Option<&'a str>,
    pub assignee_id: Option<u64>,
    pub due_on: Option<NaiveDate>,
}

fn ctx_of(visibility: Visibility, group_id: Option<u64>) -> ContentCtx {
    // 任务的所有者是"创建者"，但任务的可见性不按所有者放行（M5 全是 internal），
    // 所以 owner_id 传 None：多传一个用不上的参数，将来就会有人以为它有用。
    ContentCtx::new(visibility, None, group_id)
}

fn audit_ctx<'a>(actor: &'a CurrentUser, ip: Option<&'a str>) -> AuditCtx<'a> {
    AuditCtx::new(Some(actor.id), ip)
}

fn checked(result: Result<(), validate::FieldError>) -> Result<(), AppError> {
    result.map_err(|error| AppError::unprocessable(error.message))
}

/// 任务板对谁可见：M5 的任务全是 internal，普通 user 看不到。
fn require_reader(viewer: &CurrentUser) -> Result<(), AppError> {
    if policy::can_view(Some(viewer), &ctx_of(Visibility::Internal, None)) {
        Ok(())
    } else {
        Err(AppError::Forbidden)
    }
}

fn require_writer(actor: &CurrentUser) -> Result<(), AppError> {
    if policy::can_write(Some(actor), &ctx_of(Visibility::Internal, None)) {
        Ok(())
    } else {
        Err(AppError::Forbidden)
    }
}

/// 取一条对当前用户可见的任务。不可见与不存在都给 404——
/// 403 会变成"这里有一个你看不到的任务"的探测器。
pub async fn visible_task(
    pool: &MySqlPool,
    viewer: &CurrentUser,
    task_id: u64,
) -> Result<Task, AppError> {
    require_reader(viewer)?;
    model::find_task(pool, task_id)
        .await?
        .ok_or(AppError::NotFound)
}

/// 谁能改派：创建者或 admin。
///
/// 被指派人**不能**把任务转给别人——否则"谁该干这件事"会变成转发链，
/// 而这件事本来就是创建者的判断。
fn can_assign(actor: &CurrentUser, task: &Task) -> bool {
    policy::can_admin(Some(actor)) || task.created_by == actor.id
}

/// 谁能改状态：负责人 / 创建者 / admin。
fn can_change_status(actor: &CurrentUser, task: &Task) -> bool {
    policy::can_admin(Some(actor))
        || task.created_by == actor.id
        || task.assignee_id == Some(actor.id)
}

/// 谁能删：创建者或 admin。
fn can_delete(actor: &CurrentUser, task: &Task) -> bool {
    policy::can_admin(Some(actor)) || task.created_by == actor.id
}

/// 指派目标必须是"能干活"的人：不存在或非 active 都给 422。
///
/// 这里不能只看 id 存在——把一个已停用的人指派成负责人，任务会永远停在
/// "已指派给某人"，而那个人其实已经进不来了。
async fn ensure_assignable(pool: &MySqlPool, user_id: u64) -> Result<(), AppError> {
    let user = find_by_id(pool, user_id).await?;
    match user {
        Some(user) if user.status == UserStatus::Active => Ok(()),
        _ => Err(AppError::unprocessable("只能指派给正常状态的成员")),
    }
}

async fn validate_input(input: TaskInput<'_>) -> Result<(), AppError> {
    checked(validate::non_empty("title", input.title, TITLE_MAX))?;
    if let Some(body) = input.body {
        checked(validate::non_empty("body", body, BODY_MAX))?;
    }
    Ok(())
}

/// 新建任务。
pub async fn create(
    pool: &MySqlPool,
    actor: &CurrentUser,
    ip: Option<&str>,
    input: TaskInput<'_>,
) -> Result<u64, AppError> {
    require_writer(actor)?;
    validate_input(input).await?;
    if let Some(assignee) = input.assignee_id {
        ensure_assignable(pool, assignee).await?;
    }

    let mut tx = pool.begin().await?;
    let id = model::create_task(
        &mut tx,
        NewTask {
            title: input.title,
            body: input.body,
            status: TaskStatus::Todo,
            assignee_id: input.assignee_id,
            created_by: actor.id,
            due_on: input.due_on,
            source_thread_id: None,
        },
    )
    .await?;

    audit::record(
        &mut tx,
        audit_ctx(actor, ip),
        action::TASK_CREATE,
        ("task", Some(id)),
        Some(serde_json::json!({
            "title": input.title,
            "status": TaskStatus::Todo.as_str(),
            "assignee_id": input.assignee_id,
        })),
    )
    .await?;

    tx.commit().await?;
    Ok(id)
}

/// 从议题生成任务。
///
/// 唯一的 `task → forum` 依赖（需求 §122 允许的那一条）。标题取议题标题、
/// 正文取首楼，与 M4 沉淀文档同源，避免同一件事在两处显示成两种说法。
pub async fn create_from_thread(
    pool: &MySqlPool,
    actor: &CurrentUser,
    ip: Option<&str>,
    thread_id: u64,
) -> Result<u64, AppError> {
    let thread = forum_model::find_thread(pool, thread_id)
        .await?
        .ok_or(AppError::NotFound)?;

    if !policy::can_view(
        Some(actor),
        &ContentCtx::new(thread.visibility, Some(thread.author_id), thread.group_id),
    ) {
        return Err(AppError::NotFound);
    }
    require_writer(actor)?;

    let first_page = forum_model::list_posts(pool, thread_id, &PageQuery { page: 1 }).await?;
    let body = first_page
        .items
        .first()
        .map(|post| post.body.clone())
        .unwrap_or_default();

    let mut tx = pool.begin().await?;
    let id = model::create_task(
        &mut tx,
        NewTask {
            title: &thread.title,
            body: Some(&body),
            status: TaskStatus::Todo,
            assignee_id: None,
            created_by: actor.id,
            due_on: None,
            source_thread_id: Some(thread_id),
        },
    )
    .await?;

    audit::record(
        &mut tx,
        audit_ctx(actor, ip),
        action::TASK_CREATE,
        ("task", Some(id)),
        Some(serde_json::json!({
            "title": thread.title,
            "status": TaskStatus::Todo.as_str(),
            "source": "thread",
            "thread_id": thread_id,
        })),
    )
    .await?;

    tx.commit().await?;
    Ok(id)
}

/// 改状态。同状态不写审计（没有变化就没有事件）。
pub async fn change_status(
    pool: &MySqlPool,
    actor: &CurrentUser,
    ip: Option<&str>,
    task_id: u64,
    status: TaskStatus,
) -> Result<(), AppError> {
    let task = visible_task(pool, actor, task_id).await?;
    if !can_change_status(actor, &task) {
        return Err(AppError::Forbidden);
    }
    if task.status == status {
        return Ok(());
    }

    let mut tx = pool.begin().await?;
    if !model::set_status(&mut tx, task_id, status).await? {
        // 上面刚读到过，这里失败只可能是并发软删；回滚后按不存在处理。
        tx.rollback().await?;
        return Err(AppError::NotFound);
    }
    audit::record(
        &mut tx,
        audit_ctx(actor, ip),
        action::TASK_STATUS_CHANGE,
        ("task", Some(task_id)),
        Some(serde_json::json!({
            "from": task.status.as_str(),
            "to": status.as_str(),
        })),
    )
    .await?;
    tx.commit().await?;

    Ok(())
}

/// 指派 / 改派 / 取消指派。`assignee = None` 表示取消指派。
pub async fn assign(
    pool: &MySqlPool,
    actor: &CurrentUser,
    ip: Option<&str>,
    task_id: u64,
    assignee: Option<u64>,
) -> Result<(), AppError> {
    let task = visible_task(pool, actor, task_id).await?;
    if !can_assign(actor, &task) {
        return Err(AppError::Forbidden);
    }
    if let Some(user_id) = assignee {
        ensure_assignable(pool, user_id).await?;
    }
    if task.assignee_id == assignee {
        return Ok(());
    }

    let mut tx = pool.begin().await?;
    if !model::set_assignee(&mut tx, task_id, assignee).await? {
        tx.rollback().await?;
        return Err(AppError::NotFound);
    }
    audit::record(
        &mut tx,
        audit_ctx(actor, ip),
        action::TASK_ASSIGN,
        ("task", Some(task_id)),
        Some(serde_json::json!({
            "from": task.assignee_id,
            "to": assignee,
        })),
    )
    .await?;
    tx.commit().await?;

    Ok(())
}

/// 软删。
pub async fn delete(
    pool: &MySqlPool,
    actor: &CurrentUser,
    ip: Option<&str>,
    task_id: u64,
) -> Result<(), AppError> {
    let task = visible_task(pool, actor, task_id).await?;
    if !can_delete(actor, &task) {
        return Err(AppError::Forbidden);
    }

    let mut tx = pool.begin().await?;
    if !model::soft_delete(&mut tx, task_id).await? {
        tx.rollback().await?;
        return Err(AppError::NotFound);
    }
    audit::record(
        &mut tx,
        audit_ctx(actor, ip),
        action::TASK_DELETE,
        ("task", Some(task_id)),
        Some(serde_json::json!({ "title": task.title })),
    )
    .await?;
    tx.commit().await?;

    Ok(())
}

/// 列表分页。`mine = true` 时只看指派给自己的。
pub async fn list_page(
    pool: &MySqlPool,
    viewer: &CurrentUser,
    mine: bool,
    page: &PageQuery,
) -> Result<Page<TaskCard>, AppError> {
    require_reader(viewer)?;

    let assignee = mine.then_some(viewer.id);
    let total = model::count_tasks(pool, assignee).await?;
    let normalized = Page::<()>::new(Vec::new(), page.page, total, PAGE_SIZE);
    if total > 0 && normalized.page > normalized.total_pages() {
        return Ok(Page::new(Vec::new(), normalized.page, total, PAGE_SIZE));
    }

    let offset = u32::try_from(normalized.offset()).unwrap_or(u32::MAX);
    let items = model::list_tasks(pool, assignee, offset, PAGE_SIZE).await?;
    Ok(Page::new(items, normalized.page, total, PAGE_SIZE))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::model::Role;

    fn task(created_by: u64, assignee: Option<u64>) -> Task {
        Task {
            id: 1,
            title: "任务".to_owned(),
            body: None,
            status: TaskStatus::Todo,
            assignee_id: assignee,
            assignee_name: None,
            created_by,
            created_by_name: "创建者".to_owned(),
            due_on: None,
            source_thread_id: None,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        }
    }

    fn member(id: u64) -> CurrentUser {
        CurrentUser {
            id,
            role: Role::Member,
            status: UserStatus::Active,
            group_ids: Vec::new(),
        }
    }

    fn admin(id: u64) -> CurrentUser {
        CurrentUser {
            id,
            role: Role::Admin,
            status: UserStatus::Active,
            group_ids: Vec::new(),
        }
    }

    #[test]
    fn assignee_can_move_status_but_cannot_reassign() {
        let t = task(1, Some(2));

        assert!(can_change_status(&member(2), &t), "负责人能推进状态");
        assert!(!can_assign(&member(2), &t), "负责人不能把任务转给别人");
    }

    #[test]
    fn outsider_can_neither_move_nor_reassign() {
        let t = task(1, Some(2));
        let stranger = member(3);

        assert!(!can_change_status(&stranger, &t));
        assert!(!can_assign(&stranger, &t));
        assert!(!can_delete(&stranger, &t));
    }

    #[test]
    fn creator_and_admin_keep_full_control() {
        let t = task(1, Some(2));

        assert!(can_change_status(&member(1), &t), "创建者能兜底改状态");
        assert!(can_assign(&member(1), &t), "创建者能改派");
        assert!(can_delete(&member(1), &t));

        assert!(can_change_status(&admin(9), &t), "admin 能纠错");
        assert!(can_assign(&admin(9), &t));
        assert!(can_delete(&admin(9), &t));
    }
}
