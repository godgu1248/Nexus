//! 前厅业务层。
//!
//! 本模块的所有写入只碰 `public_comments` / `public_likes` / `public_submissions`。
//! 对 forum / kb 的访问一律是"读一条记录来判断可见性与取标题"，符合需求 L123 的只读依赖。

use sqlx::MySqlPool;

use crate::auth::policy::{self, ContentCtx, CurrentUser};
use crate::error::AppError;
use crate::forum::model as forum_model;
use crate::kb::model as kb_model;
use crate::public::model::{
    self as public_model, Comment, CommentTarget, NewSubmission, Submission, SubmissionStatus,
};
use crate::shared::Visibility;
use crate::shared::audit::{self, AuditCtx, action};
use crate::shared::pagination::{Page, PageQuery};
use crate::shared::validate;

/// 评论正文上限（与 `public_comments.body VARCHAR(4000)` 的**字符**预算一致）。
///
/// 列宽是 4000 个字符，不是 4000 字节：utf8mb4 下中文占 3 字节，但 MySQL 的
/// `VARCHAR(n)` 计数按字符算。这里卡 2000 而不是 4000，是为了留出「引用原文」之类的
/// 后续扩展空间——现在就把列宽用满，将来加字段只能改表。
pub const COMMENT_MAX: usize = 2_000;

pub const SUBMISSION_TITLE_MAX: usize = 200;
pub const SUBMISSION_BODY_MAX: usize = 40_000;

/// 入队申请理由上限，与 `join_requests.message VARCHAR(500)` 的字符预算一致。
pub const JOIN_MESSAGE_MAX: usize = 500;

/// 一个已解析、且已确认「对当前访客公开可见」的目标。
///
/// 为什么要带 `title`：审计 meta 里要记「评论挂在哪条内容上」，
/// 只记 id 的话，M7 的审计查看页面得回查 forum/kb 才能显示人话。
pub struct PublicTarget {
    pub kind: CommentTarget,
    pub id: u64,
    pub title: String,
}

impl PublicTarget {
    /// 审计里用的 `target_type`（沿用 `thread` / `doc` 这套编码，不另造词）。
    fn target_type(&self) -> &'static str {
        self.kind.as_str()
    }
}

/// 解析目标并校验「对外可见」。
///
/// 三层判断缺一不可：
/// 1. 记录存在且未软删；
/// 2. **必须是 `public`**——前厅只谈公开内容，`internal` 的议题即便当前访客是 member 也不该在前厅被评论；
/// 3. `policy::can_view` 放行（这层现在是冗余的，因为 public 对所有 active 访问者都放行；
///    留着是因为将来若引入「公开但有条件」的可见性，缺了这层会静默放宽）。
///
/// 任何一层不过都返回 `NotFound`，**不回 403**：前厅是对外页面，
/// 403 等于告诉外人「这里有东西但你不能看」，而那恰好是必须隐藏的信息。
pub async fn resolve_target(
    pool: &MySqlPool,
    viewer: Option<&CurrentUser>,
    kind: CommentTarget,
    id: u64,
) -> Result<PublicTarget, AppError> {
    match kind {
        CommentTarget::Thread => {
            let thread = forum_model::find_thread(pool, id).await?;
            let Some(thread) = thread else {
                return Err(AppError::NotFound);
            };
            let ctx = ContentCtx::new(thread.visibility, Some(thread.author_id), thread.group_id);
            if thread.visibility != Visibility::Public || !policy::can_view(viewer, &ctx) {
                return Err(AppError::NotFound);
            }
            Ok(PublicTarget {
                kind,
                id,
                title: thread.title,
            })
        }
        CommentTarget::Doc => {
            let doc = kb_model::find_doc(pool, id).await?;
            let Some(doc) = doc else {
                return Err(AppError::NotFound);
            };
            let ctx = ContentCtx::new(doc.visibility, Some(doc.author_id), doc.group_id);
            if doc.visibility != Visibility::Public
                || !doc.is_published
                || !policy::can_view(viewer, &ctx)
            {
                return Err(AppError::NotFound);
            }
            Ok(PublicTarget {
                kind,
                id,
                title: doc.title,
            })
        }
    }
}

/// 把表单校验错误翻成 `Unprocessable`（422）。
///
/// 为什么不给 `AppError` 加 `From<FieldError>`：`AppError` 是整个应用的错误类型，
/// 让它认识"表单字段"这层概念，等于把表单层泄漏进错误层——`shared::validate` 之外
/// 就没有人该知道 `FieldError` 长什么样。这个转换只发生在 service 的边界上。
fn checked(result: Result<(), validate::FieldError>) -> Result<(), AppError> {
    result.map_err(|err| AppError::unprocessable(err.message))
}

/// 写操作的最小前提：真的登录了、且账号可用。
///
/// 游客的 `CurrentUser` 是 `id == 0 / status == Pending`，两个条件都会把他拦下；
/// 这里仍显式检查 `id == 0`，因为"靠 status 顺带挡住"是隐式的，
/// 将来有人给游客换一个 active 的 status 就会静默放开写权限。
fn signed_in(viewer: &CurrentUser) -> Result<(), AppError> {
    if viewer.id == 0 || !viewer.status.is_active() {
        return Err(AppError::LoginRequired {
            next: "/lobby".to_owned(),
        });
    }
    Ok(())
}

fn audit_ctx<'a>(viewer: &'a CurrentUser, ip: Option<&'a str>) -> AuditCtx<'a> {
    AuditCtx::new(Some(viewer.id), ip)
}

/// 发一条评论。返回评论 id。
pub async fn comment(
    pool: &MySqlPool,
    viewer: &CurrentUser,
    ip: Option<&str>,
    kind: CommentTarget,
    target_id: u64,
    body: &str,
) -> Result<u64, AppError> {
    signed_in(viewer)?;
    checked(validate::non_empty("body", body, COMMENT_MAX))?;

    let target = resolve_target(pool, Some(viewer), kind, target_id).await?;

    let mut tx = pool.begin().await?;
    let id = public_model::create_comment(&mut tx, target.kind, target.id, viewer.id, body.trim())
        .await?;
    audit::record(
        &mut tx,
        audit_ctx(viewer, ip),
        action::PUBLIC_COMMENT_CREATE,
        (target.target_type(), Some(target.id)),
        Some(serde_json::json!({
            "comment_id": id,
            "target_title": target.title,
            "chars": body.chars().count(),
        })),
    )
    .await?;
    tx.commit().await?;

    Ok(id)
}

/// 删一条评论：作者本人或 admin。
///
/// 已删除的评论按「不存在」处理（`find_comment` 已经过滤 `deleted_at`），
/// 于是重复提交删除表单得到的是 404 而不是静默成功——这在多人协作里能暴露"我以为我删了"。
pub async fn delete_comment(
    pool: &MySqlPool,
    viewer: &CurrentUser,
    ip: Option<&str>,
    comment_id: u64,
) -> Result<(), AppError> {
    signed_in(viewer)?;

    let Some(existing) = public_model::find_comment(pool, comment_id).await? else {
        return Err(AppError::NotFound);
    };

    let is_author = existing.author_id == viewer.id;
    if !is_author && !policy::can_admin(Some(viewer)) {
        return Err(AppError::Forbidden);
    }

    let mut tx = pool.begin().await?;
    if !public_model::soft_delete_comment(&mut tx, comment_id).await? {
        tx.rollback().await?;
        return Err(AppError::NotFound);
    }
    audit::record(
        &mut tx,
        audit_ctx(viewer, ip),
        action::PUBLIC_COMMENT_DELETE,
        (existing.target_type.as_str(), Some(existing.target_id)),
        Some(serde_json::json!({
            "comment_id": comment_id,
            "by_author": is_author,
        })),
    )
    .await?;
    tx.commit().await?;

    Ok(())
}

/// 点赞 / 取消赞。返回 `(现在是否已赞, 最新计数)`。
///
/// **不写审计**（DESIGN §24.7 的显式取舍）：它是全站最高频写入，且当前态就是行本身，
/// 写进审计只会把表撑爆并淹没真正的管理动作。
pub async fn toggle_like(
    pool: &MySqlPool,
    viewer: &CurrentUser,
    kind: CommentTarget,
    target_id: u64,
) -> Result<(bool, u64), AppError> {
    signed_in(viewer)?;
    let target = resolve_target(pool, Some(viewer), kind, target_id).await?;

    let already = public_model::is_liked(pool, target.kind, target.id, viewer.id).await?;

    let mut tx = pool.begin().await?;
    if already {
        public_model::remove_like(&mut tx, target.kind, target.id, viewer.id).await?;
    } else {
        public_model::add_like(&mut tx, target.kind, target.id, viewer.id).await?;
    }
    tx.commit().await?;

    let count = public_model::like_count(pool, target.kind, target.id).await?;
    Ok((!already, count))
}

/// 提交一条投稿，落 `pending` 队列。
///
/// 投稿正文允许换行、允许长（40k 字符），因为它是"外人写给我们的一段东西"，
/// 不该因为格式被挡在门外；但标题必须短且非空，否则队列页面无法辨认。
pub async fn submit(
    pool: &MySqlPool,
    viewer: &CurrentUser,
    ip: Option<&str>,
    title: &str,
    body: &str,
) -> Result<u64, AppError> {
    signed_in(viewer)?;
    checked(validate::non_empty("title", title, SUBMISSION_TITLE_MAX))?;
    checked(validate::non_empty("body", body, SUBMISSION_BODY_MAX))?;

    let mut tx = pool.begin().await?;
    let id = public_model::create_submission(
        &mut tx,
        NewSubmission {
            author_id: viewer.id,
            title: title.trim(),
            body: body.trim(),
        },
    )
    .await?;
    audit::record(
        &mut tx,
        audit_ctx(viewer, ip),
        action::PUBLIC_SUBMISSION_SUBMIT,
        ("submission", Some(id)),
        Some(serde_json::json!({
            "title": title.trim(),
            "chars": body.chars().count(),
        })),
    )
    .await?;
    tx.commit().await?;

    Ok(id)
}

/// 我投过的稿。
pub async fn my_submissions(
    pool: &MySqlPool,
    viewer: &CurrentUser,
    page: &PageQuery,
) -> Result<Page<Submission>, AppError> {
    signed_in(viewer)?;
    public_model::list_submissions(pool, Some(viewer.id), None, page).await
}

/// 待审队列（仅 admin，M6 只读；审核动作在 M7）。
pub async fn queue(
    pool: &MySqlPool,
    viewer: &CurrentUser,
    page: &PageQuery,
) -> Result<Page<Submission>, AppError> {
    signed_in(viewer)?;
    if !policy::can_admin(Some(viewer)) {
        return Err(AppError::Forbidden);
    }
    public_model::list_submissions(pool, None, Some(SubmissionStatus::Pending), page).await
}

/// 某个目标下的评论分页（handler 直接用，不需要额外权限判断——目标可见性已在 `resolve_target` 里判过）。
pub async fn comments_of(
    pool: &MySqlPool,
    target: CommentTarget,
    target_id: u64,
    page: &PageQuery,
) -> Result<Page<Comment>, AppError> {
    public_model::list_comments(pool, target, target_id, page).await
}

/// 当前访客能否删这条评论（模板用它决定要不要画删除按钮）。
pub fn may_delete(viewer: Option<&CurrentUser>, comment: &Comment) -> bool {
    match viewer {
        Some(user) => {
            user.id != 0 && (user.id == comment.author_id || policy::can_admin(Some(user)))
        }
        None => false,
    }
}

/// 申请加入团队（M7）。
///
/// 只有 `user`（注册外人）需要申请：已经是 member/admin 的人再点一次没有意义，
/// 但那不该是错误，所以这里只拦"已有待审申请"这一种重复。
///
/// 申请落 `join_requests`，**不动 `users.status`**：审批前他仍是 active 的注册外人，
/// 能评论点赞，但看不到内厅。这正是需求 L72「两种状态互相独立」的落地。
pub async fn submit_join(
    pool: &MySqlPool,
    viewer: &CurrentUser,
    ip: Option<&str>,
    message: &str,
) -> Result<u64, AppError> {
    signed_in(viewer)?;

    let message = message.trim();
    if message.chars().count() > JOIN_MESSAGE_MAX {
        return Err(AppError::unprocessable(format!(
            "申请理由最多 {JOIN_MESSAGE_MAX} 个字"
        )));
    }

    if public_model::find_pending_join_request(pool, viewer.id)
        .await?
        .is_some()
    {
        return Err(AppError::conflict(public_model::PENDING_EXISTS_MESSAGE));
    }

    let mut tx = pool.begin().await?;
    let id = public_model::create_join_request(
        &mut tx,
        public_model::NewJoinRequest {
            user_id: viewer.id,
            message,
        },
    )
    .await?;
    audit::record(
        &mut tx,
        AuditCtx::new(Some(viewer.id), ip),
        action::JOIN_REQUEST_SUBMIT,
        ("join_request", Some(id)),
        Some(serde_json::json!({ "message_len": message.chars().count() })),
    )
    .await?;
    tx.commit().await?;

    Ok(id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::model::{Role, UserStatus};

    fn viewer(id: u64, role: Role, status: UserStatus) -> CurrentUser {
        CurrentUser {
            id,
            role,
            status,
            group_ids: Vec::new(),
        }
    }

    #[test]
    fn guests_cannot_write_anything() {
        let guest = CurrentUser::guest();
        assert!(signed_in(&guest).is_err());
    }

    #[test]
    fn suspended_members_cannot_write_even_though_they_have_an_id() {
        let user = viewer(7, Role::Member, UserStatus::Suspended);
        assert!(signed_in(&user).is_err());
    }

    #[test]
    fn plain_registered_outside_user_can_write() {
        // 需求 L10：「外部人可注册后评论点赞」——user 角色就是"注册外人"，
        // 他能写前厅，但看不到 internal 内容。两件事必须一致。
        let user = viewer(9, Role::User, UserStatus::Active);
        assert!(signed_in(&user).is_ok());
    }

    #[test]
    fn only_the_author_or_an_admin_may_delete_a_comment() {
        let comment = Comment {
            id: 1,
            target_type: CommentTarget::Thread,
            target_id: 3,
            author_id: 42,
            author_name: "作者".to_owned(),
            body: "hi".to_owned(),
            created_at: chrono::Utc::now(),
        };

        let stranger = viewer(9, Role::Member, UserStatus::Active);
        assert!(!may_delete(Some(&stranger), &comment));

        let author = viewer(42, Role::User, UserStatus::Active);
        assert!(may_delete(Some(&author), &comment));

        let admin = viewer(1, Role::Admin, UserStatus::Active);
        assert!(may_delete(Some(&admin), &comment));

        assert!(!may_delete(None, &comment));
    }
}
