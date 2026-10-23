//! 小组的数据访问层。
//!
//! 与 `forum::model` 一样的三条纪律：
//! 1. SQL 一律写成字面量——sqlx 0.9 的 `query()` 收 `&'static str`，拼串在编译期就过不去。
//! 2. 已软删的组在**每条**查询里都排除，不靠调用方记得过滤。
//! 3. 不做 N+1：成员列表一次 join 拿全。

use chrono::{DateTime, Utc};
use sqlx::mysql::MySqlRow;
use sqlx::{MySqlPool, Row};

use crate::error::AppError;

/// 一个小组。`deleted_at` 有值即视为不存在（下面每条查询都已过滤）。
#[derive(Debug, Clone)]
pub struct Group {
    pub id: u64,
    pub name: String,
    pub slug: String,
    pub description: String,
    pub created_by: u64,
    pub created_at: DateTime<Utc>,
    pub deleted_at: Option<DateTime<Utc>>,
}

/// 建组时需要的字段。`created_by` 从调用方的 `CurrentUser` 来，不由表单提交。
#[derive(Debug, Clone, Copy)]
pub struct NewGroup<'a> {
    pub name: &'a str,
    pub slug: &'a str,
    pub description: &'a str,
    pub created_by: u64,
}

/// 成员视图：把 `users` 里展示需要的两列一起带出来，
/// 免得页面为每个成员再查一次用户名（那就是 N+1）。
#[derive(Debug, Clone)]
pub struct Member {
    pub user_id: u64,
    pub username: String,
    pub display_name: String,
    pub joined_at: DateTime<Utc>,
}

pub async fn list_groups(pool: &MySqlPool) -> Result<Vec<Group>, AppError> {
    let rows = sqlx::query(
        "SELECT id, name, slug, description, created_by, created_at, deleted_at \
         FROM member_groups WHERE deleted_at IS NULL ORDER BY name ASC, id ASC",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows.iter().map(row_to_group).collect())
}

pub async fn find_group_by_slug(pool: &MySqlPool, slug: &str) -> Result<Option<Group>, AppError> {
    let row = sqlx::query(
        "SELECT id, name, slug, description, created_by, created_at, deleted_at \
         FROM member_groups WHERE slug = ? AND deleted_at IS NULL",
    )
    .bind(slug)
    .fetch_optional(pool)
    .await?;
    Ok(row.as_ref().map(row_to_group))
}

pub async fn find_group_by_id(pool: &MySqlPool, id: u64) -> Result<Option<Group>, AppError> {
    let row = sqlx::query(
        "SELECT id, name, slug, description, created_by, created_at, deleted_at \
         FROM member_groups WHERE id = ? AND deleted_at IS NULL",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?;
    Ok(row.as_ref().map(row_to_group))
}

pub async fn create_group(
    tx: &mut sqlx::Transaction<'_, sqlx::MySql>,
    new: NewGroup<'_>,
) -> Result<u64, AppError> {
    let result = sqlx::query(
        "INSERT INTO member_groups (name, slug, description, created_by) VALUES (?, ?, ?, ?)",
    )
    .bind(new.name)
    .bind(new.slug)
    .bind(new.description)
    .bind(new.created_by)
    .execute(&mut **tx)
    .await?;
    Ok(result.last_insert_id())
}

/// 软删。返回 `false` 表示本来就不存在（或已经删过），调用方据此决定要不要报 404。
pub async fn soft_delete_group(
    tx: &mut sqlx::Transaction<'_, sqlx::MySql>,
    group_id: u64,
) -> Result<bool, AppError> {
    let result = sqlx::query(
        "UPDATE member_groups SET deleted_at = UTC_TIMESTAMP() WHERE id = ? AND deleted_at IS NULL",
    )
    .bind(group_id)
    .execute(&mut **tx)
    .await?;
    Ok(result.rows_affected() > 0)
}

/// 加成员。返回 `false` 表示本来就在组里。
///
/// 这里用 `ON DUPLICATE KEY UPDATE joined_at = joined_at` 而**不是** `INSERT IGNORE`：
/// 后者会把外键错误、类型错误一起吞成 warning，然后被误报成"已经是成员"——
/// 一个本该报错的情况被伪装成正常情况，比直接报错难查得多。
/// `ON DUPLICATE KEY` 只吸收重复主键，别的错误照常冒出来。
pub async fn add_member(
    tx: &mut sqlx::Transaction<'_, sqlx::MySql>,
    group_id: u64,
    user_id: u64,
) -> Result<bool, AppError> {
    // 用普通 INSERT + 精确识别 1062，而不是 `ON DUPLICATE KEY UPDATE joined_at = joined_at`。
    //
    // 原因：那条语句的 `rows_affected()` 语义取决于连接是否带 CLIENT_FOUND_ROWS——
    // 带上时"行已存在"也会返回 1，于是"重复加人"会被报告成"新插入"，
    // 连审计都会多写一条"某人加入了小组"。用 1062 判定没有这种歧义。
    //
    // 外键错误（组或用户不存在）会原样上抛：service 已在事务外校验过存在性，
    // 若仍撞到外键，那是真异常，不该被吞成"他本来就在组里"。
    let result = sqlx::query("INSERT INTO group_members (group_id, user_id) VALUES (?, ?)")
        .bind(group_id)
        .bind(user_id)
        .execute(&mut **tx)
        .await;

    match result {
        Ok(_) => Ok(true),
        Err(sqlx::Error::Database(db)) => {
            let duplicate = db
                .try_downcast_ref::<sqlx::mysql::MySqlDatabaseError>()
                .map(|error| error.number() == 1062)
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

/// 移出成员。返回 `false` 表示本来就不在组里。
pub async fn remove_member(
    tx: &mut sqlx::Transaction<'_, sqlx::MySql>,
    group_id: u64,
    user_id: u64,
) -> Result<bool, AppError> {
    let result = sqlx::query("DELETE FROM group_members WHERE group_id = ? AND user_id = ?")
        .bind(group_id)
        .bind(user_id)
        .execute(&mut **tx)
        .await?;
    Ok(result.rows_affected() > 0)
}

pub async fn members_of(pool: &MySqlPool, group_id: u64) -> Result<Vec<Member>, AppError> {
    let rows = sqlx::query(
        "SELECT gm.user_id, u.username, u.display_name, gm.joined_at \
         FROM group_members gm JOIN users u ON u.id = gm.user_id \
         WHERE gm.group_id = ? ORDER BY u.display_name ASC, gm.user_id ASC",
    )
    .bind(group_id)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .iter()
        .map(|row| Member {
            user_id: row.get("user_id"),
            username: row.get("username"),
            display_name: row.get("display_name"),
            joined_at: row.get("joined_at"),
        })
        .collect())
}

/// 中间件每请求调用一次。已软删的组不算（否则删了组还能看见组内容）。
/// 排序是为了让 `CurrentUser` 可比较、可在测试里稳定断言。
pub async fn groups_of_user(pool: &MySqlPool, user_id: u64) -> Result<Vec<u64>, AppError> {
    let rows = sqlx::query(
        "SELECT gm.group_id FROM group_members gm \
         JOIN member_groups g ON g.id = gm.group_id \
         WHERE gm.user_id = ? AND g.deleted_at IS NULL ORDER BY gm.group_id ASC",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .iter()
        .map(|row| row.get::<u64, _>("group_id"))
        .collect())
}

fn row_to_group(row: &MySqlRow) -> Group {
    Group {
        id: row.get("id"),
        name: row.get("name"),
        slug: row.get("slug"),
        description: row.get("description"),
        created_by: row.get("created_by"),
        created_at: row.get("created_at"),
        deleted_at: row.get("deleted_at"),
    }
}

/// 组 + 成员数。成员数用相关子查询一次取回，避免"列 N 个组再各查一次人数"。
#[derive(Debug, Clone)]
pub struct GroupCard {
    pub group: Group,
    pub member_count: u64,
}

#[derive(Debug, Clone)]
pub struct GroupId {
    pub group_id: u64,
}

/// 管理员视角：所有未软删的组 + 各自成员数。
pub async fn list_groups_with_counts(pool: &MySqlPool) -> Result<Vec<GroupCard>, AppError> {
    let rows = sqlx::query(
        "SELECT g.id, g.name, g.slug, g.description, g.created_by, g.created_at, g.deleted_at, \
         (SELECT COUNT(*) FROM group_members gm WHERE gm.group_id = g.id) AS member_count \
         FROM member_groups g WHERE g.deleted_at IS NULL ORDER BY g.name ASC, g.id ASC",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .iter()
        .map(|row| GroupCard {
            group: row_to_group(row),
            member_count: row.get("member_count"),
        })
        .collect())
}

/// 普通成员视角：只有自己所在的组。
///
/// 不用先查 `groups_of_user` 再过滤——那样要么两次往返，要么在应用层过滤已取回的行；
/// 一条 `IN (子查询)` 让数据库用 `idx_group_members_user` 一次搞定。
pub async fn list_groups_for_user(
    pool: &MySqlPool,
    user_id: u64,
) -> Result<Vec<GroupCard>, AppError> {
    let rows = sqlx::query(
        "SELECT g.id, g.name, g.slug, g.description, g.created_by, g.created_at, g.deleted_at, \
         (SELECT COUNT(*) FROM group_members gm2 WHERE gm2.group_id = g.id) AS member_count \
         FROM member_groups g \
         WHERE g.deleted_at IS NULL \
           AND g.id IN (SELECT gm.group_id FROM group_members gm WHERE gm.user_id = ?) \
         ORDER BY g.name ASC, g.id ASC",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .iter()
        .map(|row| GroupCard {
            group: row_to_group(row),
            member_count: row.get("member_count"),
        })
        .collect())
}

/// 可被加进组的候选人：只列 active 账号。
///
/// 与 `add_member` 的服务端校验保持一致——如果这里把 suspended 的人也列出来，
/// 管理员点了就会收到 422，那属于界面在骗人。
#[derive(Debug, Clone)]
pub struct Candidate {
    pub user_id: u64,
    pub username: String,
    pub display_name: String,
}

pub async fn list_active_users(pool: &MySqlPool) -> Result<Vec<Candidate>, AppError> {
    let rows = sqlx::query(
        "SELECT id, username, display_name FROM users \
         WHERE status = 'active' ORDER BY display_name ASC, id ASC",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .iter()
        .map(|row| Candidate {
            user_id: row.get("id"),
            username: row.get("username"),
            display_name: row.get("display_name"),
        })
        .collect())
}
