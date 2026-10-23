//! 管理区编排层（M7）。
//!
//! 这一层的存在理由是**跨聚合**：成员审批要同时动 `join_requests`（public 的表）
//! 与 `users`（auth 的表）；投稿通过要建 kb 草稿再回填投稿行。单个模块的 service
//! 没有义务替别的模块开事务，所以编排放在这里。
//!
//! 三条不变式（每条都在测试里被盯死）：
//! 1. 系统里永远至少有一个 **active** admin——降级/停用/软删最后一个都拒绝；
//! 2. admin 不能对自己做角色或状态变更——防手滑把自己锁在门外；
//! 3. 审计只读——本模块除了 `admin.audit_view` 那一行，不写任何审计。

use serde_json::json;
use sqlx::MySqlPool;

use crate::admin::model::{self as admin_model, AuditFilter, AuditRow, MemberRow};
use crate::auth::model::{self as auth_model, Role, UserStatus};
use crate::auth::policy::{self, CurrentUser};
use crate::error::AppError;
use crate::kb::service as kb_service;
use crate::kb::service::DocInput;
use crate::public::model::{self as public_model, SubmissionStatus};
use crate::shared::audit::{self, AuditCtx};
use crate::shared::pagination::{Page, PageQuery};
use crate::shared::visibility::Visibility;

/// 总览页要的四组数字 + 最近审计。
#[derive(Debug, Clone)]
pub struct Overview {
    pub pending_joins: u64,
    pub pending_submissions: u64,
    pub active_admins: u64,
    pub recent: Vec<AuditRow>,
}

fn require_admin(viewer: &CurrentUser) -> Result<(), AppError> {
    if policy::can_admin(Some(viewer)) {
        Ok(())
    } else {
        Err(AppError::Forbidden)
    }
}

fn audit_ctx<'a>(viewer: &'a CurrentUser, ip: Option<&'a str>) -> AuditCtx<'a> {
    AuditCtx::new(Some(viewer.id), ip)
}

pub async fn overview(pool: &MySqlPool, viewer: &CurrentUser) -> Result<Overview, AppError> {
    require_admin(viewer)?;

    let pending_joins = public_model::count_join_requests(pool, Some("pending")).await?;
    let pending_submissions =
        public_model::count_submissions(pool, None, Some(SubmissionStatus::Pending)).await?;
    let active_admins = auth_model::count_active_admins(pool).await?;
    let recent = admin_model::recent_audit(pool, 10).await?;

    Ok(Overview {
        pending_joins,
        pending_submissions,
        active_admins,
        recent,
    })
}

pub async fn members(
    pool: &MySqlPool,
    viewer: &CurrentUser,
    role: Option<&str>,
    status: Option<&str>,
    page: &PageQuery,
) -> Result<Page<MemberRow>, AppError> {
    require_admin(viewer)?;
    admin_model::list_members(pool, role, status, page).await
}

/// 改角色。
///
/// 两条防线都在这里，不在 handler：handler 只负责把字符串解析成枚举。
pub async fn change_role(
    pool: &MySqlPool,
    viewer: &CurrentUser,
    ip: Option<&str>,
    target_id: u64,
    role: Role,
) -> Result<(), AppError> {
    require_admin(viewer)?;
    if target_id == viewer.id {
        return Err(AppError::unprocessable(
            "不能修改自己的角色——这是为了防止把自己锁在管理区外面",
        ));
    }

    let Some(target) = auth_model::find_by_id(pool, target_id).await? else {
        return Err(AppError::NotFound);
    };
    if target.role == role {
        return Ok(());
    }

    // 最后一个 active admin 不许降级：降完就没人能进管理区了。
    if target.role == Role::Admin
        && target.status == UserStatus::Active
        && role != Role::Admin
        && auth_model::count_active_admins(pool).await? <= 1
    {
        return Err(AppError::unprocessable(
            "这是最后一个可用的管理员，不能降级——否则没人能再进管理区",
        ));
    }

    let mut tx = pool.begin().await?;
    let changed = auth_model::set_role(&mut tx, target_id, role).await?;
    if !changed {
        tx.rollback().await?;
        return Ok(());
    }
    audit::record(
        &mut tx,
        audit_ctx(viewer, ip),
        audit::action::USER_ROLE_CHANGE,
        ("user", Some(target_id)),
        Some(json!({ "from": target.role.as_str(), "to": role.as_str() })),
    )
    .await?;
    tx.commit().await?;
    Ok(())
}

/// 改状态（`active` / `suspended` / `pending`）。
///
/// `deleted` 走 `delete_user`：那是软删除，还要改名、清口令哈希，语义不同。
pub async fn change_status(
    pool: &MySqlPool,
    viewer: &CurrentUser,
    ip: Option<&str>,
    target_id: u64,
    status: UserStatus,
) -> Result<(), AppError> {
    require_admin(viewer)?;
    if target_id == viewer.id {
        return Err(AppError::unprocessable("不能修改自己的状态"));
    }
    if status == UserStatus::Deleted {
        return Err(AppError::bad_request(
            "删除账号请用软删除入口——它还要释放用户名并清空口令",
        ));
    }

    let Some(target) = auth_model::find_by_id(pool, target_id).await? else {
        return Err(AppError::NotFound);
    };
    if target.status == status {
        return Ok(());
    }

    if target.role == Role::Admin
        && target.status == UserStatus::Active
        && status != UserStatus::Active
        && auth_model::count_active_admins(pool).await? <= 1
    {
        return Err(AppError::unprocessable(
            "这是最后一个可用的管理员，不能停用——否则没人能再进管理区",
        ));
    }

    let mut tx = pool.begin().await?;
    let changed = auth_model::set_status(&mut tx, target_id, status).await?;
    if !changed {
        tx.rollback().await?;
        return Ok(());
    }
    audit::record(
        &mut tx,
        audit_ctx(viewer, ip),
        audit::action::USER_STATUS_CHANGE,
        ("user", Some(target_id)),
        Some(json!({ "from": target.status.as_str(), "to": status.as_str() })),
    )
    .await?;
    tx.commit().await?;
    Ok(())
}

/// 软删除用户：按 L74 改状态 + 释放唯一列 + 清空口令哈希，**行保留**。
pub async fn delete_user(
    pool: &MySqlPool,
    viewer: &CurrentUser,
    ip: Option<&str>,
    target_id: u64,
) -> Result<(), AppError> {
    require_admin(viewer)?;
    if target_id == viewer.id {
        return Err(AppError::unprocessable("不能删除自己"));
    }

    let Some(target) = auth_model::find_by_id(pool, target_id).await? else {
        return Err(AppError::NotFound);
    };
    if target.status == UserStatus::Deleted {
        return Ok(());
    }
    if target.role == Role::Admin
        && target.status == UserStatus::Active
        && auth_model::count_active_admins(pool).await? <= 1
    {
        return Err(AppError::unprocessable(
            "这是最后一个可用的管理员，不能删除",
        ));
    }

    let mut tx = pool.begin().await?;
    let changed = auth_model::soft_delete(&mut tx, target_id).await?;
    if !changed {
        tx.rollback().await?;
        return Ok(());
    }
    audit::record(
        &mut tx,
        audit_ctx(viewer, ip),
        audit::action::USER_STATUS_CHANGE,
        ("user", Some(target_id)),
        Some(json!({ "from": target.status.as_str(), "to": "deleted" })),
    )
    .await?;
    tx.commit().await?;
    Ok(())
}

/// 待审申请队列。
pub async fn join_queue(
    pool: &MySqlPool,
    viewer: &CurrentUser,
    page: &PageQuery,
) -> Result<Page<public_model::JoinRow>, AppError> {
    require_admin(viewer)?;
    public_model::list_join_requests(pool, Some(public_model::JoinStatus::Pending), page).await
}

/// 通过申请：申请置 approved + 用户升 member，**同一事务**。
///
/// 写两条审计（F1）：`join_request.approve` 记流程，`user.role_change` 记身份变化。
/// 只写一条的话，将来按 `user.role_change` 统计"谁被提拔过"会漏掉所有走申请的路径。
pub async fn approve_join(
    pool: &MySqlPool,
    viewer: &CurrentUser,
    ip: Option<&str>,
    request_id: u64,
) -> Result<(), AppError> {
    require_admin(viewer)?;

    let Some(request) = public_model::find_join_request(pool, request_id).await? else {
        return Err(AppError::NotFound);
    };
    if request.status != public_model::JoinStatus::Pending {
        return Err(AppError::conflict("这条申请已经处理过了"));
    }
    let Some(target) = auth_model::find_by_id(pool, request.user_id).await? else {
        return Err(AppError::NotFound);
    };

    let mut tx = pool.begin().await?;
    if !public_model::mark_join_reviewed(
        &mut tx,
        request_id,
        public_model::JoinStatus::Approved,
        viewer.id,
        "",
    )
    .await?
    {
        tx.rollback().await?;
        return Err(AppError::conflict("这条申请已经处理过了"));
    }
    let promoted = auth_model::set_role(&mut tx, request.user_id, Role::Member).await?;
    audit::record(
        &mut tx,
        audit_ctx(viewer, ip),
        audit::action::JOIN_REQUEST_APPROVE,
        ("join_request", Some(request_id)),
        Some(json!({ "user_id": request.user_id })),
    )
    .await?;
    if promoted {
        audit::record(
            &mut tx,
            audit_ctx(viewer, ip),
            audit::action::USER_ROLE_CHANGE,
            ("user", Some(request.user_id)),
            Some(json!({ "from": target.role.as_str(), "to": "member", "via": "join_request" })),
        )
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

pub async fn reject_join(
    pool: &MySqlPool,
    viewer: &CurrentUser,
    ip: Option<&str>,
    request_id: u64,
    note: &str,
) -> Result<(), AppError> {
    require_admin(viewer)?;
    let Some(request) = public_model::find_join_request(pool, request_id).await? else {
        return Err(AppError::NotFound);
    };
    if request.status != public_model::JoinStatus::Pending {
        return Err(AppError::conflict("这条申请已经处理过了"));
    }

    let mut tx = pool.begin().await?;
    if !public_model::mark_join_reviewed(
        &mut tx,
        request_id,
        public_model::JoinStatus::Rejected,
        viewer.id,
        note,
    )
    .await?
    {
        tx.rollback().await?;
        return Err(AppError::conflict("这条申请已经处理过了"));
    }
    audit::record(
        &mut tx,
        audit_ctx(viewer, ip),
        audit::action::JOIN_REQUEST_REJECT,
        ("join_request", Some(request_id)),
        Some(json!({ "user_id": request.user_id, "note": note })),
    )
    .await?;
    tx.commit().await?;
    Ok(())
}

pub async fn submission_queue(
    pool: &MySqlPool,
    viewer: &CurrentUser,
    page: &PageQuery,
) -> Result<Page<public_model::Submission>, AppError> {
    require_admin(viewer)?;
    public_model::list_submissions(pool, None, Some(SubmissionStatus::Pending), page).await
}

/// 通过投稿：先在档案区建一篇**草稿**，再回填投稿行。
///
/// 顺序与事务边界见 DESIGN §25.4：`kb_service::create_doc` 自己开事务并写自己的审计，
/// 硬塞进外层事务要把 kb 的全部写路径改造成"接受外部 tx"，为一个低频动作不值得。
/// 代价是极端情况下"草稿建了、回填失败"，此时投稿仍是 pending，重试会再建一篇草稿；
/// 失败分支会把 `doc_id` 打进错误日志，便于人工收拾。
pub async fn approve_submission(
    pool: &MySqlPool,
    viewer: &CurrentUser,
    ip: Option<&str>,
    submission_id: u64,
) -> Result<u64, AppError> {
    require_admin(viewer)?;

    let Some(submission) = public_model::find_submission(pool, submission_id).await? else {
        return Err(AppError::NotFound);
    };
    if submission.status != SubmissionStatus::Pending {
        return Err(AppError::conflict("这条投稿已经处理过了"));
    }

    // 草稿作者是操作的管理员：外人没有建档权限，而且草稿需要有人维护（F4）。
    let doc_id = kb_service::create_doc(
        pool,
        viewer,
        ip,
        DocInput {
            parent_id: None,
            title: &submission.title,
            body: &submission.body,
            // 投稿是纯文本表单，没有富文本层。
            body_html: None,
            visibility: Visibility::Internal,
            group_id: None,
            sort_order: 0,
        },
    )
    .await
    .map_err(|err| {
        tracing::error!(
            submission_id,
            "投稿通过时建草稿失败，投稿仍是 pending，可重试：{err:?}"
        );
        err
    })?;

    let mut tx = pool.begin().await?;
    if !public_model::mark_submission_reviewed(
        &mut tx,
        submission_id,
        SubmissionStatus::Approved,
        viewer.id,
        "",
        Some(("doc", doc_id)),
    )
    .await?
    {
        tx.rollback().await?;
        tracing::error!(
            submission_id,
            doc_id,
            "投稿回填失败：草稿已建但投稿仍是 pending，重试会再建一篇草稿"
        );
        return Err(AppError::conflict("这条投稿已经处理过了"));
    }
    audit::record(
        &mut tx,
        audit_ctx(viewer, ip),
        audit::action::PUBLIC_SUBMISSION_APPROVE,
        ("submission", Some(submission_id)),
        Some(json!({ "doc_id": doc_id, "title_len": submission.title.chars().count() })),
    )
    .await?;
    tx.commit().await?;

    Ok(doc_id)
}

pub async fn reject_submission(
    pool: &MySqlPool,
    viewer: &CurrentUser,
    ip: Option<&str>,
    submission_id: u64,
    note: &str,
) -> Result<(), AppError> {
    require_admin(viewer)?;
    let Some(submission) = public_model::find_submission(pool, submission_id).await? else {
        return Err(AppError::NotFound);
    };
    if submission.status != SubmissionStatus::Pending {
        return Err(AppError::conflict("这条投稿已经处理过了"));
    }

    let mut tx = pool.begin().await?;
    if !public_model::mark_submission_reviewed(
        &mut tx,
        submission_id,
        SubmissionStatus::Rejected,
        viewer.id,
        note,
        None,
    )
    .await?
    {
        tx.rollback().await?;
        return Err(AppError::conflict("这条投稿已经处理过了"));
    }
    audit::record(
        &mut tx,
        audit_ctx(viewer, ip),
        audit::action::PUBLIC_SUBMISSION_REJECT,
        ("submission", Some(submission_id)),
        Some(json!({ "note_len": note.chars().count() })),
    )
    .await?;
    tx.commit().await?;
    Ok(())
}

/// 审计页。**这是全站唯一"读操作也写审计"的地方**（需求 L179 明确要求）。
///
/// 写审计与读数据不同事务：审计写失败时，页面就不给看（返回 500）。
/// 反向做（先给人看再补审计）等于让"审计记录缺失"变成一个静默失败。
pub async fn audit_page(
    pool: &MySqlPool,
    viewer: &CurrentUser,
    ip: Option<&str>,
    filter: &AuditFilter,
    page: &PageQuery,
) -> Result<Page<AuditRow>, AppError> {
    require_admin(viewer)?;

    let mut tx = pool.begin().await?;
    audit::record(
        &mut tx,
        audit_ctx(viewer, ip),
        audit::action::ADMIN_AUDIT_VIEW,
        ("audit_logs", None),
        Some(json!({ "filtered": filter.is_filtered() })),
    )
    .await?;
    tx.commit().await?;

    admin_model::list_audit(pool, filter, page).await
}
