//! 档案业务层：树形规则、发布语义、从议题沉淀、搜索。
//!
//! 这一层是唯一允许写审计的地方（`shared::audit::record`），handler 不许直连 audit。
//! 所有写操作 = `begin` → model 写 → `audit::record` → `commit`，审计失败业务必须失败。

use sqlx::MySqlPool;

use crate::auth::policy::{self, ContentCtx, CurrentUser};
use crate::error::AppError;
use crate::forum::model as forum_model;
use crate::shared::audit::{self, AuditCtx, action};
use crate::shared::pagination::{PAGE_SIZE, Page, PageQuery};
use crate::shared::richtext;
use crate::shared::validate;
use crate::shared::visibility::Visibility;

use super::model::{self, DocCard, KbDoc, NewDoc};

pub const TITLE_MAX: usize = 200;
pub const BODY_MAX: usize = 40_000;
/// 树的最大层数（根为第 1 层）。
pub const MAX_DEPTH: u32 = 5;
pub const SEARCH_MIN: usize = 1;
pub const SEARCH_MAX: usize = 64;

/// 新建 / 编辑文档的入参。
pub struct DocInput<'a> {
    pub parent_id: Option<u64>,
    pub title: &'a str,
    pub body: &'a str,
    /// 富文本 HTML（净化前），落库前由 `richtext::prepare` 归一。
    pub body_html: Option<&'a str>,
    pub visibility: Visibility,
    pub group_id: Option<u64>,
    pub sort_order: i32,
}

/// 字段校验错误 → 422。
fn checked(result: Result<(), validate::FieldError>) -> Result<(), AppError> {
    result.map_err(|err| AppError::unprocessable(err.message))
}

fn audit_ctx<'a>(actor: &CurrentUser, ip: Option<&'a str>) -> AuditCtx<'a> {
    AuditCtx::new(Some(actor.id), ip)
}

fn ctx_of(visibility: Visibility, author_id: Option<u64>, group_id: Option<u64>) -> ContentCtx {
    ContentCtx::new(visibility, author_id, group_id)
}

/// 写权限门槛：档案库是内部知识，`can_write` 只对 member / admin 为真。
fn can_write_kb(actor: &CurrentUser) -> bool {
    policy::can_write(Some(actor), &ctx_of(Visibility::Internal, None, None))
}

/// 「能不能看这份文档」= 可见性通过 **且** （已发布 或 我是作者 或 我是 admin）。
///
/// 两个条件缺一不可，而且不能把草稿条件塞进 `policy::can_view`——
/// policy 回答的是"这个可见性级别他能不能看"，不回答"这份内容发没发"。
pub fn can_see(viewer: Option<&CurrentUser>, doc: &KbDoc) -> bool {
    if !policy::can_view(
        viewer,
        &ctx_of(doc.visibility, Some(doc.author_id), doc.group_id),
    ) {
        return false;
    }

    if doc.is_published {
        return true;
    }

    match viewer {
        None => false,
        Some(user) => user.id == doc.author_id || policy::can_admin(Some(user)),
    }
}

/// 作者本人或 admin 才能改 / 发 / 删。
fn can_manage(actor: &CurrentUser, doc: &KbDoc) -> bool {
    actor.id == doc.author_id || policy::can_admin(Some(actor))
}

fn validate_doc(input: &DocInput<'_>) -> Result<(), AppError> {
    checked(validate::non_empty("title", input.title, TITLE_MAX))?;
    // 正文的"非空"判定要看富文本（只有图片没有文字也是合法正文），交给 prepare。
    Ok(())
}

/// 正文归一：净化 / 抽纯文本 / 校验长度，并保证不是一张空文档。
fn prepare_body(input: &DocInput<'_>) -> Result<richtext::RichBody, AppError> {
    let body = richtext::prepare(input.body_html, Some(input.body), BODY_MAX, "正文")?;
    if body.text.is_empty() && body.html.is_none() {
        return Err(AppError::unprocessable("正文不能为空"));
    }
    Ok(body)
}

/// 解析并校验可见性；`group` 必须绑一个真实存在、且**自己也在里面**的小组。
///
/// 最后那条检查看起来多余，其实是最省事的防呆：不检查的话，member 可以把文档
/// 设成自己不在的小组，建完立刻变成"自己也看不见"的孤儿文档，还得找 admin 救。
async fn resolve_visibility(
    pool: &MySqlPool,
    actor: &CurrentUser,
    visibility: Visibility,
    group_id: Option<u64>,
) -> Result<(Visibility, Option<u64>), AppError> {
    if visibility != Visibility::Group {
        return Ok((visibility, None));
    }

    let gid = group_id.ok_or_else(|| AppError::unprocessable("小组可见的文档必须指定小组"))?;
    crate::group::model::find_group_by_id(pool, gid)
        .await?
        .ok_or_else(|| AppError::unprocessable("指定的小组不存在或已解散"))?;

    if !policy::can_view(Some(actor), &ctx_of(Visibility::Group, None, Some(gid))) {
        return Err(AppError::unprocessable(
            "你不在这个小组里，建完自己也看不到它",
        ));
    }

    Ok((Visibility::Group, Some(gid)))
}

/// 新建文档。返回新 id。
pub async fn create_doc(
    pool: &MySqlPool,
    actor: &CurrentUser,
    ip: Option<&str>,
    input: DocInput<'_>,
) -> Result<u64, AppError> {
    if !can_write_kb(actor) {
        return Err(AppError::Forbidden);
    }
    validate_doc(&input)?;

    let (visibility, group_id) =
        resolve_visibility(pool, actor, input.visibility, input.group_id).await?;

    if let Some(parent_id) = input.parent_id {
        let parent = model::find_doc(pool, parent_id)
            .await?
            .ok_or(AppError::NotFound)?;

        // 父节点看不见 → 404：不区分"不存在"与"无权"，否则可以拿它探测结构。
        if !can_see(Some(actor), &parent) {
            return Err(AppError::NotFound);
        }

        let parent_depth = model::ancestor_depth(pool, parent_id).await?.unwrap_or(1);
        if parent_depth + 1 > MAX_DEPTH {
            return Err(AppError::unprocessable(format!("文档最多 {MAX_DEPTH} 层")));
        }

        if visibility.rank() > parent.visibility.rank() {
            return Err(AppError::unprocessable("子文档的可见范围不能比上级更宽"));
        }
    }

    let body = prepare_body(&input)?;

    let mut tx = pool.begin().await?;
    let id = model::create_doc(
        &mut tx,
        NewDoc {
            parent_id: input.parent_id,
            author_id: actor.id,
            title: input.title,
            body: &body.text,
            body_html: body.html.as_deref(),
            visibility,
            group_id,
            sort_order: input.sort_order,
            is_published: false,
        },
    )
    .await?;

    audit::record(
        &mut tx,
        audit_ctx(actor, ip),
        action::KB_DOC_CREATE,
        ("kb_doc", Some(id)),
        Some(serde_json::json!({
            "title": input.title,
            "visibility": visibility.as_str(),
            "parent_id": input.parent_id,
            "source": "manual",
        })),
    )
    .await?;
    tx.commit().await?;

    Ok(id)
}

/// 编辑文档。**不支持改父节点**（M4 不做移动），父节点只用来校验"不得更宽"。
pub async fn update_doc(
    pool: &MySqlPool,
    actor: &CurrentUser,
    ip: Option<&str>,
    id: u64,
    input: DocInput<'_>,
) -> Result<(), AppError> {
    let doc = model::find_doc(pool, id).await?.ok_or(AppError::NotFound)?;
    if !can_manage(actor, &doc) {
        return Err(AppError::Forbidden);
    }
    validate_doc(&input)?;

    let (visibility, group_id) =
        resolve_visibility(pool, actor, input.visibility, input.group_id).await?;

    if let Some(parent_id) = doc.parent_id {
        let parent = model::find_doc(pool, parent_id).await?.ok_or_else(|| {
            AppError::internal(anyhow::anyhow!("文档 {id} 的父节点 {parent_id} 丢失"))
        })?;
        if visibility.rank() > parent.visibility.rank() {
            return Err(AppError::unprocessable("子文档的可见范围不能比上级更宽"));
        }
    }

    let body = prepare_body(&input)?;

    let mut tx = pool.begin().await?;
    let hit = model::update_doc(
        &mut tx,
        id,
        &model::DocRevision {
            title: input.title,
            body: &body.text,
            body_html: body.html.as_deref(),
            visibility,
            group_id,
            sort_order: input.sort_order,
        },
    )
    .await?;
    if !hit {
        tx.rollback().await?;
        return Err(AppError::NotFound);
    }

    audit::record(
        &mut tx,
        audit_ctx(actor, ip),
        action::KB_DOC_UPDATE,
        ("kb_doc", Some(id)),
        Some(serde_json::json!({
            "title": input.title,
            "visibility": visibility.as_str(),
        })),
    )
    .await?;
    tx.commit().await?;

    Ok(())
}

/// 发布 / 撤回。撤回后对他人立即不可见（列表与详情同一判定，不靠缓存）。
pub async fn publish(
    pool: &MySqlPool,
    actor: &CurrentUser,
    ip: Option<&str>,
    id: u64,
    published: bool,
) -> Result<(), AppError> {
    let doc = model::find_doc(pool, id).await?.ok_or(AppError::NotFound)?;
    if !can_manage(actor, &doc) {
        return Err(AppError::Forbidden);
    }

    let mut tx = pool.begin().await?;
    if !model::set_published(&mut tx, id, published).await? {
        tx.rollback().await?;
        return Err(AppError::NotFound);
    }

    audit::record(
        &mut tx,
        audit_ctx(actor, ip),
        action::KB_DOC_PUBLISH,
        ("kb_doc", Some(id)),
        Some(serde_json::json!({
            "title": doc.title,
            "to": if published { "published" } else { "draft" },
        })),
    )
    .await?;
    tx.commit().await?;

    Ok(())
}

/// 软删。有未删子文档时拒绝——级联软删会让"删一个父节点"变成一次不可见的批量删除。
pub async fn delete_doc(
    pool: &MySqlPool,
    actor: &CurrentUser,
    ip: Option<&str>,
    id: u64,
) -> Result<(), AppError> {
    let doc = model::find_doc(pool, id).await?.ok_or(AppError::NotFound)?;
    if !can_manage(actor, &doc) {
        return Err(AppError::Forbidden);
    }
    if model::has_live_children(pool, id).await? {
        return Err(AppError::unprocessable("请先处理子文档，再删除这一篇"));
    }

    let mut tx = pool.begin().await?;
    if !model::soft_delete(&mut tx, id).await? {
        tx.rollback().await?;
        return Err(AppError::NotFound);
    }

    audit::record(
        &mut tx,
        audit_ctx(actor, ip),
        action::KB_DOC_DELETE,
        ("kb_doc", Some(id)),
        Some(serde_json::json!({ "title": doc.title })),
    )
    .await?;
    tx.commit().await?;

    Ok(())
}

/// 从议题沉淀成新文档（草稿）。
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
        &ctx_of(thread.visibility, Some(thread.author_id), thread.group_id),
    ) {
        return Err(AppError::NotFound);
    }
    if !can_write_kb(actor) {
        return Err(AppError::Forbidden);
    }

    // 首楼正文：复用 forum 的读取路径，不在 kb 里另写一份 JOIN。
    let first_page = forum_model::list_posts(pool, thread_id, &PageQuery { page: 1 }).await?;
    let first_post = first_page.items.first();
    let body = first_post.map(|post| post.body.clone()).unwrap_or_default();
    // 议题首楼若带富文本，一并搬过来；净化交给读的时候做（`sanitize` 是幂等的）。
    let body_html = first_post.and_then(|post| post.body_html.clone());

    let mut tx = pool.begin().await?;
    let id = model::create_doc(
        &mut tx,
        NewDoc {
            parent_id: None,
            author_id: actor.id,
            title: &thread.title,
            body: &body,
            body_html: body_html.as_deref(),
            visibility: thread.visibility,
            group_id: thread.group_id,
            sort_order: 0,
            // 沉淀是草稿：刚搬过来的内容通常还要整理，直接发布等于把原样贴出去。
            is_published: false,
        },
    )
    .await?;

    audit::record(
        &mut tx,
        audit_ctx(actor, ip),
        action::KB_DOC_CREATE,
        ("kb_doc", Some(id)),
        Some(serde_json::json!({
            "title": thread.title,
            "source": "thread",
            "thread_id": thread_id,
            "visibility": thread.visibility.as_str(),
        })),
    )
    .await?;
    tx.commit().await?;

    Ok(id)
}

/// 详情：不可见与不存在一律 `NotFound`。
pub async fn visible_detail(
    pool: &MySqlPool,
    viewer: Option<&CurrentUser>,
    id: u64,
) -> Result<KbDoc, AppError> {
    let doc = model::find_doc(pool, id).await?.ok_or(AppError::NotFound)?;
    if !can_see(viewer, &doc) {
        return Err(AppError::NotFound);
    }
    Ok(doc)
}

/// 搜索（标题 + 正文），分页。
pub async fn search_page(
    pool: &MySqlPool,
    viewer: Option<&CurrentUser>,
    q: &str,
    page: &PageQuery,
) -> Result<Page<DocCard>, AppError> {
    let keyword = q.trim();
    let len = keyword.chars().count();

    if len > SEARCH_MAX {
        return Err(AppError::unprocessable(format!(
            "搜索词最多 {SEARCH_MAX} 个字符"
        )));
    }
    if len < SEARCH_MIN {
        return Ok(Page::new(Vec::new(), 1, 0, PAGE_SIZE));
    }

    // 先算 offset 再查：越界页直接给空列表，而不是去数据库里白跑一趟。
    let probe = Page::<()>::new(Vec::new(), page.page, 0, PAGE_SIZE);
    let offset = probe.offset();

    // Page::offset() is u64 and the page is clamped, so this always fits u32.
    let offset = u32::try_from(offset).unwrap_or(u32::MAX);
    let (items, total) = model::search_docs(pool, viewer, keyword, offset, PAGE_SIZE).await?;
    Ok(Page::new(items, probe.page, total, PAGE_SIZE))
}

/// 置顶 / 取消置顶。只有 admin 能做——它决定所有人打开档案先看到什么。
///
/// 普通作者不能给自己置顶：否则第一个写完文档的人就能长期霸占首页，
/// 而"按互动排序"的初衷正好被绕过去了。
pub async fn set_pinned(
    pool: &MySqlPool,
    actor: &CurrentUser,
    ip: Option<&str>,
    id: u64,
    pinned: bool,
    order: i32,
) -> Result<(), AppError> {
    if !policy::can_admin(Some(actor)) {
        return Err(AppError::Forbidden);
    }
    let doc = model::find_doc(pool, id).await?.ok_or(AppError::NotFound)?;

    let mut tx = pool.begin().await?;
    // 没改动就不写审计：`set_pinned` 的 WHERE 带 `<>` 守卫，重复点同一个按钮会返回 false。
    // 审计的意义是"谁在什么时候改变了什么"，把无变化也记进去只会让真正的那条更难找。
    if !model::set_pinned(&mut tx, id, pinned, order).await? {
        tx.rollback().await?;
        return Ok(());
    }
    audit::record(
        &mut tx,
        AuditCtx::new(Some(actor.id), ip),
        action::KB_DOC_PIN,
        ("doc", Some(id)),
        Some(serde_json::json!({
            "title": doc.title,
            "pinned": pinned,
            "order": if pinned { order } else { 0 },
        })),
    )
    .await?;
    tx.commit().await?;
    Ok(())
}

/// 记一次浏览。作者自己打开不算——否则作者改一遍稿子就给自己刷了十个浏览量。
pub async fn count_view(
    pool: &MySqlPool,
    viewer: Option<&CurrentUser>,
    id: u64,
) -> Result<(), AppError> {
    let doc = match model::find_doc(pool, id).await? {
        Some(doc) => doc,
        None => return Ok(()),
    };
    if Some(doc.author_id) == viewer.map(|user| user.id) {
        return Ok(());
    }
    model::bump_view(pool, id).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::model::{Role, UserStatus};
    use crate::auth::policy::CurrentUser;
    use chrono::Utc;

    fn doc(
        visibility: Visibility,
        author_id: u64,
        group_id: Option<u64>,
        published: bool,
    ) -> KbDoc {
        KbDoc {
            id: 1,
            parent_id: None,
            author_id,
            author_name: "作者".to_string(),
            title: "标题".to_string(),
            body: "正文".to_string(),
            body_html: None,
            visibility,
            group_id,
            sort_order: 0,
            view_count: 0,
            is_pinned: false,
            pin_order: 0,
            is_published: published,
            published_at: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    fn user(id: u64, role: Role, status: UserStatus, groups: Vec<u64>) -> CurrentUser {
        CurrentUser {
            id,
            role,
            status,
            group_ids: groups,
        }
    }

    #[test]
    fn drafts_are_only_visible_to_their_author_and_admins() {
        let draft = doc(Visibility::Internal, 7, None, false);
        let author = user(7, Role::Member, UserStatus::Active, vec![]);
        let other_member = user(8, Role::Member, UserStatus::Active, vec![]);
        let admin = user(9, Role::Admin, UserStatus::Active, vec![]);

        assert!(can_see(Some(&author), &draft), "作者必须能看到自己的草稿");
        assert!(can_see(Some(&admin), &draft), "admin 要能审阅草稿");
        assert!(
            !can_see(Some(&other_member), &draft),
            "其他人不能看到草稿——发布的意义就在这里"
        );
        assert!(!can_see(None, &draft), "游客更不能看到草稿");

        // 作者身份不等于永久授权：被降级成普通 user 之后，连自己写的 internal 草稿
        // 也看不到了。这是有意的——权限跟着角色走，不跟着"我写的"走。
        let demoted = user(7, Role::User, UserStatus::Active, vec![]);
        assert!(
            !can_see(Some(&demoted), &draft),
            "降级后的作者不能再看到 internal 内容"
        );
    }

    #[test]
    fn published_documents_follow_the_visibility_matrix() {
        let public_doc = doc(Visibility::Public, 7, None, true);
        let internal_doc = doc(Visibility::Internal, 7, None, true);
        let group_doc = doc(Visibility::Group, 7, Some(3), true);

        let guest = None;
        let outsider = user(8, Role::User, UserStatus::Active, vec![]);
        let member = user(9, Role::Member, UserStatus::Active, vec![]);
        let insider = user(10, Role::Member, UserStatus::Active, vec![3]);

        assert!(can_see(guest, &public_doc));
        assert!(!can_see(guest, &internal_doc));
        assert!(!can_see(guest, &group_doc));

        assert!(can_see(Some(&outsider), &public_doc));
        assert!(
            !can_see(Some(&outsider), &internal_doc),
            "普通 user 不在内厅，看不到 internal"
        );

        assert!(can_see(Some(&member), &internal_doc));
        assert!(
            !can_see(Some(&member), &group_doc),
            "member 不是这个小组的成员"
        );
        assert!(can_see(Some(&insider), &group_doc), "组员能看到组文档");
    }

    #[test]
    fn managing_requires_author_or_admin() {
        let internal_doc = doc(Visibility::Internal, 7, None, true);
        let author = user(7, Role::User, UserStatus::Active, vec![]);
        let admin = user(9, Role::Admin, UserStatus::Active, vec![]);
        let member = user(8, Role::Member, UserStatus::Active, vec![]);

        assert!(can_manage(&author, &internal_doc));
        assert!(can_manage(&admin, &internal_doc));
        assert!(
            !can_manage(&member, &internal_doc),
            "member 也不能改别人的文档"
        );
    }
}
