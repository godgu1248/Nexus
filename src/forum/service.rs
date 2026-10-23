//! 论坛的业务层：把"谁能做什么"和"做完要留什么痕"编排在一起。
//!
//! 这一层是唯一允许开事务、写审计的地方。`model.rs` 只管 SQL，谁也不许在那里判权限；
//! `handler.rs` 只管取表单和渲染，不许自己写库——不然同一条规则会在两个地方各写一遍，
//! 早晚分叉。
//!
//! 三条贯穿规则：
//!
//! 1. **看不见就当不存在**。读不到的空间/议题一律 `NotFound`，不回 403。
//!    403 等于告诉对方"这个东西存在，只是不给你看"——对内部议题来说这就是泄漏。
//! 2. **写不了才回 403**。空间看得见、但你的角色不能发议题，这时 403 是诚实的，
//!    也不会泄漏任何东西。
//! 3. **审计与业务同事务**。发帖成功但审计没写进去，等于这次操作没发生过——
//!    回滚比补写便宜得多。

use sqlx::MySqlPool;

use crate::auth::policy::{self, ContentCtx, CurrentUser};
use crate::error::AppError;
use crate::forum::model::{self, NewPost, NewSpace, NewThread, Post, Space, ThreadRow};
use crate::shared::audit::{self, AuditCtx};
use crate::shared::pagination::{Page, PageQuery};
use crate::shared::richtext;
use crate::shared::validate;
use crate::shared::visibility::Visibility;

/// 议题标题上限。与 `forum_threads.title VARCHAR(200)` 对齐。
pub const TITLE_MAX: usize = 200;
/// 正文上限。列是 MEDIUMTEXT（16 MB），但 2C2G 的机器上不该让一条回复吃掉几十 MB 内存。
pub const BODY_MAX: usize = 20_000;
pub const SPACE_NAME_MAX: usize = 64;
pub const SPACE_DESC_MAX: usize = 500;
pub const SPACE_SLUG_MAX: usize = 64;

const SLUG_CONFLICT: &str = "这个英文标识已经被占用了，换一个";

/// 新建空间的入参。**没有排序值**：分区的先后由热度（议题 + 回复）决定，
/// 不再让管理员手填一个没人维护的数字（DESIGN §31）。
#[derive(Debug, Clone)]
pub struct NewSpaceInput<'a> {
    pub name: &'a str,
    pub slug: &'a str,
    pub description: &'a str,
    pub visibility: Visibility,
}

#[derive(Debug, Clone)]
pub struct NewThreadInput<'a> {
    pub space_id: u64,
    pub title: &'a str,
    /// 首楼正文。允许为空——有人就想开个标题让大家讨论，
    /// 强行要求正文只会逼出 "asdf" 这种占位内容。
    pub body: Option<&'a str>,
    /// 富文本 HTML（净化前的原始值，由 service 走 `richtext::prepare`）。
    pub body_html: Option<&'a str>,
}

fn ctx_of(visibility: Visibility, owner_id: Option<u64>, group_id: Option<u64>) -> ContentCtx {
    ContentCtx::new(visibility, owner_id, group_id)
}

fn audit_ctx<'a>(actor: &CurrentUser, ip: Option<&'a str>) -> AuditCtx<'a> {
    AuditCtx::new(Some(actor.id), ip)
}

/// `validate` 返回的是 `FieldError`（带字段名，给表单回填用），service 只需要一句能返回的错。
/// 在这里做一次翻译，好过让 `error.rs` 去依赖 `shared::validate`——错误类型不该知道表单长什么样。
fn checked(result: Result<(), validate::FieldError>) -> Result<(), AppError> {
    result.map_err(|err| AppError::unprocessable(err.message))
}

/// `can_moderate` / `can_admin` 与内容无关，但签名要一个 `ContentCtx`。
/// 统一在这里造一个"中立上下文"，避免每个调用点各写一遍 `ContentCtx::new(..., None, None)`。
fn any_ctx() -> ContentCtx {
    ContentCtx::new(Visibility::Internal, None, None)
}

fn slug_is_valid(slug: &str) -> bool {
    !slug.is_empty()
        && slug.len() <= SPACE_SLUG_MAX
        && slug
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && !slug.starts_with('-')
        && !slug.ends_with('-')
}

/// 建空间。只有 admin 能做——空间的划分是整个团队的骨架，
/// 让任何成员随手加一个"闲聊"分区，三个月后就会有十七个空分区。
pub async fn create_space(
    pool: &MySqlPool,
    actor: &CurrentUser,
    ip: Option<&str>,
    input: NewSpaceInput<'_>,
) -> Result<u64, AppError> {
    if !policy::can_admin(Some(actor)) {
        return Err(AppError::Forbidden);
    }

    checked(validate::non_empty("name", input.name, SPACE_NAME_MAX))?;
    checked(validate::non_empty(
        "description",
        input.description,
        SPACE_DESC_MAX,
    ))?;

    if !slug_is_valid(input.slug) {
        return Err(AppError::unprocessable(
            "英文标识只能用小写字母、数字和短横线，且不能以短横线开头或结尾",
        ));
    }

    let mut tx = pool.begin().await?;

    let space_id = model::create_space(
        &mut tx,
        NewSpace {
            name: input.name.trim(),
            slug: input.slug,
            description: input.description.trim(),
            visibility: input.visibility,
            created_by: actor.id,
        },
    )
    .await
    .map_err(|err| match err {
        // 唯一索引撞车在 SQL 层是 Conflict，但用户看到的是"表单填错了"，
        // 所以这里翻译成 422，而不是 409——409 会让人以为是并发冲突，想去重试。
        AppError::Conflict(_) => AppError::unprocessable(SLUG_CONFLICT),
        other => other,
    })?;

    audit::record(
        &mut tx,
        audit_ctx(actor, ip),
        audit::action::FORUM_SPACE_CREATE,
        ("space", Some(space_id)),
        Some(serde_json::json!({
            "slug": input.slug,
            "visibility": input.visibility.as_str(),
        })),
    )
    .await?;

    tx.commit().await?;
    Ok(space_id)
}

/// 看得见的所有空间。看不见的由 `model` 在 SQL 层就滤掉了。
pub async fn list_spaces(pool: &MySqlPool, actor: &CurrentUser) -> Result<Vec<Space>, AppError> {
    model::list_spaces(pool, actor).await
}

/// 某个空间下的议题列表。空间不可见时返回 `NotFound`——不给"存在但你没权限"的信号。
pub async fn threads_of_space(
    pool: &MySqlPool,
    actor: &CurrentUser,
    slug: &str,
    page: &PageQuery,
) -> Result<(Space, Page<ThreadRow>), AppError> {
    let space = model::find_space_by_slug(pool, slug)
        .await?
        .ok_or_else(|| AppError::NotFound)?;

    if !policy::can_view(Some(actor), &ctx_of(space.visibility, None, space.group_id)) {
        return Err(AppError::NotFound);
    }

    let threads = model::list_threads(pool, space.id, actor, page).await?;
    Ok((space, threads))
}

/// 议题详情页的数据：议题本身 + 这一页的回复。
pub async fn thread_detail(
    pool: &MySqlPool,
    actor: &CurrentUser,
    thread_id: u64,
    page: &PageQuery,
) -> Result<(ThreadRow, Page<Post>), AppError> {
    let thread = visible_thread(pool, actor, thread_id).await?;
    let posts = model::list_posts(pool, thread.id, page).await?;
    Ok((thread, posts))
}

async fn visible_thread(
    pool: &MySqlPool,
    actor: &CurrentUser,
    thread_id: u64,
) -> Result<ThreadRow, AppError> {
    let thread = model::find_thread(pool, thread_id)
        .await?
        .ok_or_else(|| AppError::NotFound)?;

    if !policy::can_view(
        Some(actor),
        &ctx_of(thread.visibility, Some(thread.author_id), thread.group_id),
    ) {
        return Err(AppError::NotFound);
    }

    Ok(thread)
}

/// 发议题。首楼正文（若有）与议题本身在同一个事务里：只落议题不落正文，
/// 用户点进来会看到一个空议题，比发帖失败更让人困惑。
pub async fn create_thread(
    pool: &MySqlPool,
    actor: &CurrentUser,
    ip: Option<&str>,
    input: NewThreadInput<'_>,
) -> Result<u64, AppError> {
    let space = model::find_space_by_id(pool, input.space_id)
        .await?
        .ok_or_else(|| AppError::NotFound)?;

    if !policy::can_view(Some(actor), &ctx_of(space.visibility, None, space.group_id)) {
        return Err(AppError::NotFound);
    }

    if !policy::can_write(Some(actor), &ctx_of(space.visibility, None, space.group_id)) {
        return Err(AppError::Forbidden);
    }

    checked(validate::non_empty("title", input.title, TITLE_MAX))?;

    let body = richtext::prepare(input.body_html, input.body, BODY_MAX, "正文")?;
    let has_body = !body.text.is_empty() || body.html.is_some();

    let mut tx = pool.begin().await?;

    let thread_id = model::create_thread(
        &mut tx,
        NewThread {
            space_id: space.id,
            author_id: actor.id,
            title: input.title.trim(),
            // 议题继承空间的可见性**和组归属**。M3 只让分区能被改可见性，
            // 议题随分区走：允许单个议题比分区更宽，就会出现"内部议题里挂着一条公开回复"的矛盾态。
            visibility: space.visibility,
            group_id: space.group_id,
        },
    )
    .await?;

    if has_body {
        model::create_post(
            &mut tx,
            NewPost {
                thread_id,
                author_id: actor.id,
                body: &body.text,
                body_html: body.html.as_deref(),
            },
        )
        .await?;
    }

    audit::record(
        &mut tx,
        audit_ctx(actor, ip),
        audit::action::FORUM_THREAD_CREATE,
        ("thread", Some(thread_id)),
        Some(serde_json::json!({
            "space_id": space.id,
            "title": input.title.trim(),
            "has_body": has_body,
        })),
    )
    .await?;

    tx.commit().await?;
    Ok(thread_id)
}

/// 回帖。归档议题不许再回复——归档的含义就是"这件事结束了"。
pub async fn reply(
    pool: &MySqlPool,
    actor: &CurrentUser,
    ip: Option<&str>,
    thread_id: u64,
    plain: &str,
    html: Option<&str>,
) -> Result<u64, AppError> {
    let thread = visible_thread(pool, actor, thread_id).await?;

    if thread.is_archived {
        return Err(AppError::unprocessable("这个议题已经归档，不能再回复了"));
    }

    if !policy::can_write(
        Some(actor),
        &ctx_of(thread.visibility, Some(thread.author_id), thread.group_id),
    ) {
        return Err(AppError::Forbidden);
    }

    let body = richtext::prepare(html, Some(plain), BODY_MAX, "回复")?;
    if body.text.is_empty() && body.html.is_none() {
        return Err(AppError::unprocessable("回复不能为空"));
    }

    let mut tx = pool.begin().await?;

    let post_id = model::create_post(
        &mut tx,
        NewPost {
            thread_id,
            author_id: actor.id,
            body: &body.text,
            body_html: body.html.as_deref(),
        },
    )
    .await?;

    model::touch_thread(&mut tx, thread_id).await?;

    audit::record(
        &mut tx,
        audit_ctx(actor, ip),
        audit::action::FORUM_POST_CREATE,
        ("post", Some(post_id)),
        Some(serde_json::json!({ "thread_id": thread_id })),
    )
    .await?;

    tx.commit().await?;
    Ok(post_id)
}

/// 定论/取消定论。作者自己可以定（"我这问题解决了"），admin 也可以。
pub async fn set_resolved(
    pool: &MySqlPool,
    actor: &CurrentUser,
    ip: Option<&str>,
    thread_id: u64,
    resolved: bool,
) -> Result<(), AppError> {
    let thread = visible_thread(pool, actor, thread_id).await?;

    if actor.id != thread.author_id && !policy::can_moderate(Some(actor), &any_ctx()) {
        return Err(AppError::Forbidden);
    }

    let mut tx = pool.begin().await?;
    model::set_resolved(&mut tx, thread_id, resolved).await?;

    audit::record(
        &mut tx,
        audit_ctx(actor, ip),
        audit::action::FORUM_THREAD_RESOLVE,
        ("thread", Some(thread_id)),
        Some(serde_json::json!({ "resolved": resolved })),
    )
    .await?;

    tx.commit().await?;
    Ok(())
}

/// 归档/取消归档。只有 admin——归档会把一个议题从所有人的视野里"冻住"。
pub async fn set_archived(
    pool: &MySqlPool,
    actor: &CurrentUser,
    ip: Option<&str>,
    thread_id: u64,
    archived: bool,
) -> Result<(), AppError> {
    let thread = visible_thread(pool, actor, thread_id).await?;

    if !policy::can_moderate(Some(actor), &any_ctx()) {
        return Err(AppError::Forbidden);
    }

    let mut tx = pool.begin().await?;
    model::set_archived(&mut tx, thread_id, archived).await?;

    audit::record(
        &mut tx,
        audit_ctx(actor, ip),
        audit::action::FORUM_THREAD_ARCHIVE,
        ("thread", Some(thread_id)),
        Some(serde_json::json!({ "archived": archived })),
    )
    .await?;

    tx.commit().await?;

    let _ = thread; // 保留读取，确保"看不见的议题不能归档"这条先被校验
    Ok(())
}

/// 软删一条回复。作者能删自己的，admin 能删任何人的。
///
/// 返回所属议题 id，方便 handler 把用户送回原帖——删完跳首页是常见但很烦的体验。
pub async fn delete_post(
    pool: &MySqlPool,
    actor: &CurrentUser,
    ip: Option<&str>,
    post_id: u64,
) -> Result<u64, AppError> {
    let post = model::find_post(pool, post_id)
        .await?
        .ok_or_else(|| AppError::NotFound)?;

    let thread = visible_thread(pool, actor, post.thread_id).await?;

    if post.author_id != actor.id && !policy::can_moderate(Some(actor), &any_ctx()) {
        return Err(AppError::Forbidden);
    }

    let mut tx = pool.begin().await?;
    model::soft_delete_post(&mut tx, post_id).await?;

    audit::record(
        &mut tx,
        audit_ctx(actor, ip),
        audit::action::FORUM_POST_DELETE,
        ("post", Some(post_id)),
        Some(serde_json::json!({ "thread_id": thread.id })),
    )
    .await?;

    tx.commit().await?;
    Ok(thread.id)
}

/// 改分区可见性（仅 admin）。
///
/// 三个裁决：
/// 1. `group` 必须绑一个**真实存在**的小组。不校验的话这个分区对所有人（含组员）都不可见，
///    界面显示"设置成功"，实际是把内容锁进了没有钥匙的房间。
/// 2. 非 group 一律把 `group_id` 清成 NULL。留着旧值会在下次改成 group 时悄悄指向上次的组，
///    那是一次没人授权的越权。
/// 3. 审计与改写同事务：审计失败则可见性不变，不存在"改了但没记录"。
pub async fn set_space_visibility(
    pool: &MySqlPool,
    actor: &CurrentUser,
    ip: Option<&str>,
    slug: &str,
    visibility: Visibility,
    group_id: Option<u64>,
) -> Result<(), AppError> {
    if !policy::can_admin(Some(actor)) {
        return Err(AppError::Forbidden);
    }

    let space = model::find_space_by_slug(pool, slug)
        .await?
        .ok_or(AppError::NotFound)?;

    let group_id = match visibility {
        Visibility::Group => {
            let id =
                group_id.ok_or_else(|| AppError::unprocessable("小组可见性必须指定一个小组"))?;
            if crate::group::model::find_group_by_id(pool, id)
                .await?
                .is_none()
            {
                return Err(AppError::unprocessable("指定的小组不存在"));
            }
            Some(id)
        }
        _ => None,
    };

    let mut tx = pool.begin().await?;

    if !model::set_space_visibility(&mut tx, space.id, visibility, group_id).await? {
        tx.rollback().await?;
        return Err(AppError::NotFound);
    }

    audit::record(
        &mut tx,
        audit_ctx(actor, ip),
        audit::action::CONTENT_VISIBILITY_CHANGE,
        ("forum_space", Some(space.id)),
        Some(serde_json::json!({
            "slug": space.slug,
            "from": space.visibility.as_str(),
            "to": visibility.as_str(),
            "group_id": group_id,
        })),
    )
    .await?;

    tx.commit().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slug_accepts_only_lowercase_digits_and_inner_dashes() {
        assert!(slug_is_valid("video-editing"));
        assert!(slug_is_valid("a1"));
        assert!(slug_is_valid("2024"));

        assert!(!slug_is_valid(""));
        assert!(
            !slug_is_valid("Video"),
            "大写不能进 URL，转小写是调用方的事"
        );
        assert!(!slug_is_valid("-leading"));
        assert!(!slug_is_valid("trailing-"));
        assert!(!slug_is_valid("中文"));
        assert!(!slug_is_valid("with space"));
        assert!(!slug_is_valid("under_score"));
    }

    #[test]
    fn slug_rejects_oversized_input() {
        let long = "a".repeat(SPACE_SLUG_MAX + 1);
        assert!(!slug_is_valid(&long));
        let edge = "a".repeat(SPACE_SLUG_MAX);
        assert!(slug_is_valid(&edge));
    }
}
