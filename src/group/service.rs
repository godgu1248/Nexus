//! 小组的业务层：事务、审计、权限判定都在这里，`model.rs` 只管 SQL。
//!
//! 写操作一律 `begin → 写入 → audit::record → commit`：
//! 审计与业务同事务，审计失败则业务失败——这条在 M1 已经用测试钉死，
//! 小组的建组/加人/移人也必须沿用，否则"谁把人加进了组"会成为查不到的事。

use sqlx::MySqlPool;

use crate::auth::model::{self as user_model, UserStatus};
use crate::auth::policy::{self, CurrentUser};
use crate::error::AppError;
use crate::group::model::{self, NewGroup};
use crate::shared::audit::{self, AuditCtx, action};
use crate::shared::validate;
use crate::shared::visibility::Visibility;

pub const GROUP_NAME_MAX: usize = 64;
pub const GROUP_SLUG_MAX: usize = 64;
pub const GROUP_DESC_MAX: usize = 500;

/// slug 撞车的统一提示。跟命名空间冲突分开讲不清，用户只需要知道"换一个"。
pub const SLUG_CONFLICT: &str = "这个地址已经被占用了，换一个 slug";

#[derive(Debug, Clone, Copy)]
pub struct NewGroupInput<'a> {
    pub name: &'a str,
    pub slug: &'a str,
    pub description: &'a str,
}

/// slug 只允许小写字母、数字、短横线，且不能以短横线开头或结尾。
/// 与 `forum::service::slug_is_valid` 保持同一套规则——同一个站点的 URL 不该有两种风格。
pub fn slug_is_valid(slug: &str) -> bool {
    let bytes = slug.as_bytes();
    if bytes.is_empty() || bytes.len() > GROUP_SLUG_MAX {
        return false;
    }
    if bytes.first() == Some(&b'-') || bytes.last() == Some(&b'-') {
        return false;
    }
    bytes
        .iter()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-')
}

fn checked(result: Result<(), validate::FieldError>) -> Result<(), AppError> {
    result.map_err(|err| AppError::unprocessable(err.message))
}

fn require_admin(viewer: &CurrentUser) -> Result<(), AppError> {
    if policy::can_admin(Some(viewer)) {
        Ok(())
    } else {
        Err(AppError::Forbidden)
    }
}

/// 建组。仅管理员。
///
/// 为什么不让普通成员建组：小组是可见性的授予单位，一个人能建组就意味着
/// 他能造出一个"只有自己看得见"的空间，接着就能把内容塞进去，对团队来说是信息黑洞。
pub async fn create_group(
    pool: &MySqlPool,
    viewer: &CurrentUser,
    ip: Option<&str>,
    input: NewGroupInput<'_>,
) -> Result<u64, AppError> {
    require_admin(viewer)?;

    checked(validate::non_empty("name", input.name, GROUP_NAME_MAX))?;
    // 简介允许留空——新建小组的表单里它也没标 required。
    // 代码和表单必须对同一件事说同一句话，否则用户会拿到一个"表单说可以、服务端说不行"的 422。
    // 这里只卡上限；空串照落空串（列是 NOT NULL DEFAULT ''）。
    let description = input.description.trim();
    if description.chars().count() > GROUP_DESC_MAX {
        return Err(AppError::unprocessable(format!(
            "简介最多 {GROUP_DESC_MAX} 个字"
        )));
    }
    if !input.slug.is_empty() && !slug_is_valid(input.slug) {
        return Err(AppError::unprocessable(
            "slug 只能用小写字母、数字和短横线，且不能以短横线开头或结尾",
        ));
    }
    if input.slug.is_empty() {
        return Err(AppError::unprocessable("slug 不能为空"));
    }

    let mut tx = pool.begin().await?;
    let group_id = model::create_group(
        &mut tx,
        NewGroup {
            name: input.name.trim(),
            slug: input.slug,
            description,
            created_by: viewer.id,
        },
    )
    .await
    .map_err(|err| match err {
        // slug 的唯一索引撞了：对用户来说这是"换个地址"，不是"服务器出错"。
        AppError::Conflict(_) => AppError::unprocessable(SLUG_CONFLICT),
        other => other,
    })?;

    audit::record(
        &mut tx,
        AuditCtx::new(Some(viewer.id), ip),
        action::GROUP_CREATE,
        ("group", Some(group_id)),
        Some(serde_json::json!({
            "name": input.name.trim(),
            "slug": input.slug,
            "visibility": Visibility::Group.as_str(),
        })),
    )
    .await?;
    tx.commit().await?;
    Ok(group_id)
}

/// 软删组。成员行保留（见 `model::soft_delete_group` 的说明）。
pub async fn delete_group(
    pool: &MySqlPool,
    viewer: &CurrentUser,
    ip: Option<&str>,
    group_id: u64,
) -> Result<(), AppError> {
    require_admin(viewer)?;
    let mut tx = pool.begin().await?;
    let existed = model::soft_delete_group(&mut tx, group_id).await?;
    if !existed {
        // 事务没有别的写入，直接回滚，别留一个空事务占着连接。
        tx.rollback().await?;
        return Err(AppError::NotFound);
    }
    audit::record(
        &mut tx,
        AuditCtx::new(Some(viewer.id), ip),
        action::GROUP_DELETE,
        ("group", Some(group_id)),
        None,
    )
    .await?;
    tx.commit().await?;
    Ok(())
}

/// 加成员。返回 `true` 表示这次真的加进去了，`false` 表示本来就在组里。
pub async fn add_member(
    pool: &MySqlPool,
    viewer: &CurrentUser,
    ip: Option<&str>,
    group_id: u64,
    user_id: u64,
) -> Result<bool, AppError> {
    require_admin(viewer)?;
    // 组必须存在且未软删，否则外键会把错误推迟到 insert 那一刻，报出来的是数据库错误。
    if model::find_group_by_id(pool, group_id).await?.is_none() {
        return Err(AppError::NotFound);
    }
    // 目标用户必须存在且 active：把 suspended / deleted 的人拉进组没有意义，
    // 而且他们本来也过不了 `effective_role()` 那一关，加进去只会让成员列表骗人。
    let target = user_model::find_by_id(pool, user_id)
        .await?
        .ok_or(AppError::NotFound)?;
    if target.status != UserStatus::Active {
        return Err(AppError::unprocessable("只能把正常状态的成员加入小组"));
    }

    let mut tx = pool.begin().await?;
    let inserted = model::add_member(&mut tx, group_id, user_id).await?;
    if inserted {
        audit::record(
            &mut tx,
            AuditCtx::new(Some(viewer.id), ip),
            action::GROUP_MEMBER_ADD,
            ("group", Some(group_id)),
            Some(serde_json::json!({ "user_id": user_id })),
        )
        .await?;
    }
    tx.commit().await?;
    Ok(inserted)
}

/// 移出成员。
pub async fn remove_member(
    pool: &MySqlPool,
    viewer: &CurrentUser,
    ip: Option<&str>,
    group_id: u64,
    user_id: u64,
) -> Result<bool, AppError> {
    require_admin(viewer)?;
    if model::find_group_by_id(pool, group_id).await?.is_none() {
        return Err(AppError::NotFound);
    }

    let mut tx = pool.begin().await?;
    let removed = model::remove_member(&mut tx, group_id, user_id).await?;
    if removed {
        audit::record(
            &mut tx,
            AuditCtx::new(Some(viewer.id), ip),
            action::GROUP_MEMBER_REMOVE,
            ("group", Some(group_id)),
            Some(serde_json::json!({ "user_id": user_id })),
        )
        .await?;
    }
    tx.commit().await?;
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slug_accepts_lowercase_digits_and_inner_hyphens() {
        assert!(slug_is_valid("core-team"));
        assert!(slug_is_valid("g2"));
        assert!(slug_is_valid("a-b-c-1"));
    }

    #[test]
    fn slug_rejects_empty_uppercase_underscore_and_edge_hyphens() {
        assert!(!slug_is_valid(""));
        assert!(!slug_is_valid("Core"));
        assert!(!slug_is_valid("core_team"));
        assert!(!slug_is_valid("-core"));
        assert!(!slug_is_valid("core-"));
        assert!(!slug_is_valid("核心"));
    }

    #[test]
    fn slug_rejects_overlong_input() {
        assert!(!slug_is_valid(&"a".repeat(GROUP_SLUG_MAX + 1)));
        assert!(slug_is_valid(&"a".repeat(GROUP_SLUG_MAX)));
    }
}
