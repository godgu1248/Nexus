//! 公告的业务层：谁能发、谁能看、谁要确认、确认怎么落库。
//!
//! 不做"删除公告时顺手清 ack"：ack 是"谁确认过"的凭据，
//! 公告软删之后它仍然有意义（复盘时能回答"当时通知到了谁"）。

use sqlx::MySqlPool;

use crate::auth::policy::{self, ContentCtx, CurrentUser};
use crate::error::AppError;
use crate::notice::model::{self, Audience, Notice};
use crate::shared::audit::{self, AuditCtx};
use crate::shared::richtext;
use crate::shared::validate;
use crate::shared::visibility::Visibility;

pub const TITLE_MAX: usize = 120;
pub const BODY_MAX: usize = 2_000;
/// 导航红点与首页横幅都只需要"还没确认的那几条"，不需要全量历史。
pub const PANEL_LIMIT: u32 = 20;

/// 这个人算不算"团队"。
///
/// 判据只有一条：`policy::can_view(Internal)` 放不放行，也就是 member / admin。
/// 不另写 `matches!(role, Member | Admin)`，否则"什么算团队"会有两份定义，
/// 而权限矩阵改动时只会改其中一份——来宾能不能看团队公告，会和能不能看内部议题脱节。
pub fn sees_team(viewer: &CurrentUser) -> bool {
    policy::can_view(
        Some(viewer),
        &ContentCtx::new(Visibility::Internal, None, None),
    )
}

fn require_admin(viewer: &CurrentUser) -> Result<(), AppError> {
    if policy::can_admin(Some(viewer)) {
        Ok(())
    } else {
        Err(AppError::Forbidden)
    }
}

fn checked(result: Result<(), validate::FieldError>) -> Result<(), AppError> {
    result.map_err(|err| AppError::unprocessable(err.message))
}

/// 发布公告。只有 admin 能发——公告会出现在每个人首页的最上方。
pub async fn create(
    pool: &MySqlPool,
    actor: &CurrentUser,
    ip: Option<&str>,
    title: &str,
    body: &str,
    body_html: Option<&str>,
    audience: Audience,
) -> Result<u64, AppError> {
    require_admin(actor)?;
    checked(validate::non_empty("title", title, TITLE_MAX))?;

    let rich = richtext::prepare(body_html, Some(body), BODY_MAX, "正文")?;
    if rich.text.is_empty() && rich.html.is_none() {
        return Err(AppError::unprocessable("正文不能为空"));
    }

    let mut tx = pool.begin().await?;
    let id = model::create(
        &mut tx,
        title.trim(),
        &rich.text,
        rich.html.as_deref(),
        audience,
        actor.id,
    )
    .await?;
    audit::record(
        &mut tx,
        AuditCtx::new(Some(actor.id), ip),
        audit::action::NOTICE_CREATE,
        ("announcement", Some(id)),
        Some(serde_json::json!({
            "title": title.trim(),
            "audience": audience.as_str(),
        })),
    )
    .await?;
    tx.commit().await?;
    Ok(id)
}

/// 撤下公告（软删）。
///
/// 这里用 `find(pool, id, true)`：撤下只可能由 admin 发起，而 admin 一定是团队，
/// 传 `true` 表示"不受受众过滤影响"，否则撤一条只发团队的公告还得绕一圈。
pub async fn delete(
    pool: &MySqlPool,
    actor: &CurrentUser,
    ip: Option<&str>,
    id: u64,
) -> Result<(), AppError> {
    require_admin(actor)?;
    let notice = model::find(pool, id, true)
        .await?
        .ok_or(AppError::NotFound)?;

    let mut tx = pool.begin().await?;
    if !model::soft_delete(&mut tx, id).await? {
        tx.rollback().await?;
        return Err(AppError::NotFound);
    }
    audit::record(
        &mut tx,
        AuditCtx::new(Some(actor.id), ip),
        audit::action::NOTICE_DELETE,
        ("announcement", Some(id)),
        Some(serde_json::json!({ "title": notice.title })),
    )
    .await?;
    tx.commit().await?;
    Ok(())
}

/// 确认"我读过了"。
///
/// 这里**不写审计**：一条公告 × 十个人 = 十条流水，会把真正需要追溯的
/// 权限与内容变更淹掉。确认与否的状态本身在 `announcement_acks` 里查得到。
///
/// 看不到的公告（来宾对"仅团队"那条）当成不存在：否则他能靠 id 猜出
/// "这一条存在但我不该看"，还会留下一条自己根本没读过的确认记录。
pub async fn acknowledge(pool: &MySqlPool, actor: &CurrentUser, id: u64) -> Result<bool, AppError> {
    model::find(pool, id, sees_team(actor))
        .await?
        .ok_or(AppError::NotFound)?;
    let mut tx = pool.begin().await?;
    let fresh = model::ack(&mut tx, id, actor.id).await?;
    tx.commit().await?;
    Ok(fresh)
}

/// 页面要的两份清单：还没确认的、全部能看到的。
pub async fn panel(
    pool: &MySqlPool,
    actor: &CurrentUser,
) -> Result<(Vec<Notice>, Vec<Notice>), AppError> {
    let team = sees_team(actor);
    let unread = model::list_unacked(pool, actor.id, PANEL_LIMIT, team).await?;
    let all = model::list_active(pool, PANEL_LIMIT, team).await?;
    Ok((unread, all))
}

pub async fn unread_count(pool: &MySqlPool, actor: &CurrentUser) -> Result<u64, AppError> {
    model::unread_count(pool, actor.id, sees_team(actor)).await
}
