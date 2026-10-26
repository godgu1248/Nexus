//! 用户的持久化模型与查询。
//!
//! 三条与 DDL 强绑定的约定（DESIGN.md §6.2）：
//!
//! 1. **枚举存字符串**（`VARCHAR(16)`），不用 MySQL ENUM——加一个角色只该改代码与迁移，
//!    不该 `ALTER TABLE` 重定义列类型。
//! 2. **读库严、读输入宽**：`from_db` 只认小写、不 trim；`FromStr` 容忍空格与大小写。
//!    库里出现 `ADMIN` 说明写入口漏了约束，这时报错让人发现，比悄悄兜住安全——
//!    兜住的结果是"两个看起来一样的值走两条不同的权限分支"。
//! 3. **软删除不改行数**：`status='deleted'` 的用户**照常能查出来**。查询函数不按 status 过滤，
//!    因为中间件必须看见 `suspended`/`deleted` 才能把会话归一化成"未登录"；
//!    过滤掉的话，调用方只能看到 `None`，无法区分"没这个人"和"被封了"。

use std::fmt;
use std::str::FromStr;

use chrono::{DateTime, Utc};
use sqlx::MySql;
use sqlx::MySqlPool;
use sqlx::Row;
use sqlx::Transaction;
use sqlx::mysql::MySqlRow;

use crate::error::AppError;

/// 角色。权限判定的**唯一**依据是 `auth::policy`，这里只负责值的往返。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Role {
    /// 外人（自助注册的默认值）。只看得到 public。
    User,
    /// 内部成员。public + internal。
    Member,
    /// 管理员。额外拿到 moderate/admin。
    Admin,
}

impl Role {
    pub const ALL: [Self; 3] = [Self::User, Self::Member, Self::Admin];

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Member => "member",
            Self::Admin => "admin",
        }
    }

    /// 从数据库读出来时用。**严格**：只认小写，不 trim、不归一。
    pub fn from_db(value: &str) -> Option<Self> {
        match value {
            "user" => Some(Self::User),
            "member" => Some(Self::Member),
            "admin" => Some(Self::Admin),
            _ => None,
        }
    }

    pub fn is_admin(self) -> bool {
        matches!(self, Self::Admin)
    }
}

impl fmt::Display for Role {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 未知的角色取值（来自数据库，或来自未来某个漏改的写入口）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidRole(pub String);

impl fmt::Display for InvalidRole {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "未知的角色：{}（可选 user/member/admin）", self.0)
    }
}

impl std::error::Error for InvalidRole {}

/// 从**用户输入**解析角色。比 [`Role::from_db`] 宽：容忍首尾空格与大小写。
///
/// 注意：这个入口不意味着"用户能给自己改角色"。角色变更只走 admin 的 service，
/// 且必须写审计（`user.role_change`）。
impl FromStr for Role {
    type Err = InvalidRole;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let normalized = s.trim().to_ascii_lowercase();
        Self::from_db(&normalized).ok_or_else(|| InvalidRole(s.to_string()))
    }
}

/// 账号状态。与 `join_requests.status`（审批流）是**两件独立的事**，别混用。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum UserStatus {
    Active,
    Pending,
    /// 被封禁：保留内容与身份，禁止登录。
    Suspended,
    /// 软删除：行仍在，`username`/`email` 已被改成 `原名__deleted__{id}`，`password_hash` 为空串。
    Deleted,
}

impl UserStatus {
    pub const ALL: [Self; 4] = [Self::Active, Self::Pending, Self::Suspended, Self::Deleted];

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Pending => "pending",
            Self::Suspended => "suspended",
            Self::Deleted => "deleted",
        }
    }

    /// 从数据库读出来时用。**严格**：只认小写。
    pub fn from_db(value: &str) -> Option<Self> {
        match value {
            "active" => Some(Self::Active),
            "pending" => Some(Self::Pending),
            "suspended" => Some(Self::Suspended),
            "deleted" => Some(Self::Deleted),
            _ => None,
        }
    }

    /// 只有 `active` 能拥有有效会话。其余三态一律"视为未登录"（DESIGN §18.2）。
    pub fn is_active(self) -> bool {
        matches!(self, Self::Active)
    }
}

impl fmt::Display for UserStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidStatus(pub String);

impl fmt::Display for InvalidStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "未知的账号状态：{}（可选 active/pending/suspended/deleted）",
            self.0
        )
    }
}

impl std::error::Error for InvalidStatus {}

impl FromStr for UserStatus {
    type Err = InvalidStatus;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let normalized = s.trim().to_ascii_lowercase();
        Self::from_db(&normalized).ok_or_else(|| InvalidStatus(s.to_string()))
    }
}

/// 一行用户。字段顺序与 `SELECT` 的列顺序无关，映射按列名取。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct User {
    pub id: u64,
    pub username: String,
    pub email: String,
    /// argon2id 的 PHC 串；软删除用户为空串（**不是** NULL，DDL 里也没给 NULL）。
    pub password_hash: String,
    pub display_name: String,
    /// 外观三项（强调色/背板/字体），白名单在 `shared::appearance`；渲染前还要 `normalize` 一次，
    /// 这样即使库里被手工塞了个不存在的 key，页面也只会回落到默认主题而不是白屏。
    pub theme: String,
    pub background: String,
    pub font: String,
    pub role: Role,
    pub status: UserStatus,
    /// `TIMESTAMP` 列在 sqlx 里映射成 `DateTime<Utc>`（**不是** `NaiveDateTime`——
    /// 那是 `DATETIME` 的映射，混用会在读取时抛 `ColumnDecode`）。
    /// 会话时区已被 `after_connect` 钉在 `+00:00`，所以这里的 Utc 语义是真的。
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl User {
    /// 有没有可用的口令。软删除用户是空串，`verify_password("")` 必然失败，
    /// 但显式判断能让"这是软删除"和"密码错了"在代码里可分辨（响应仍然一致）。
    pub fn has_password(&self) -> bool {
        !self.password_hash.is_empty()
    }

    /// 能不能持有有效会话。中间件用它做状态归一化。
    pub fn can_hold_session(&self) -> bool {
        self.status.is_active() && self.has_password()
    }
}

/// 新建用户要写的列。用借用而不是 `String`，避免注册路径上多三次分配。
#[derive(Debug, Clone, Copy)]
pub struct NewUser<'a> {
    pub username: &'a str,
    pub email: &'a str,
    pub password_hash: &'a str,
    pub display_name: &'a str,
    pub role: Role,
    pub status: UserStatus,
}

/// 按主键取用户。
///
/// **不按 status 过滤**（见模块注释第 3 条）：中间件需要看到 `suspended`/`deleted`
/// 才能把会话归一化成"未登录"。
///
/// SQL 一律写成字面量，不拼字符串：sqlx 0.9 用 `SqlSafeStr` 把 `query()` 约束成只收
/// `&'static str`，拼接必须显式 `AssertSqlSafe` 才能通过——那道关卡就是要让"动态 SQL"
/// 在 review 时被看见，这里没有任何动态成分，所以直接写字面量。
pub async fn find_by_id(pool: &MySqlPool, id: u64) -> Result<Option<User>, AppError> {
    let row = sqlx::query(
        "SELECT id, username, email, password_hash, display_name, theme, background, font, role, status, created_at, \
         updated_at FROM users WHERE id = ?",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?;
    row.as_ref().map(row_to_user).transpose()
}

/// 按用户名取用户。登录走这一个入口（不走邮箱——需求未定义邮箱登录，加了等于扩面）。
///
/// 大小写：`username` 列用 `utf8mb4_unicode_ci`，比较本身不区分大小写；
/// 进程内的失败计数 key 另行统一转小写（DESIGN §18.7）。
pub async fn find_by_username(pool: &MySqlPool, username: &str) -> Result<Option<User>, AppError> {
    let row = sqlx::query(
        "SELECT id, username, email, password_hash, display_name, theme, background, font, role, status, created_at, \
         updated_at FROM users WHERE username = ?",
    )
    .bind(username)
    .fetch_optional(pool)
    .await?;
    row.as_ref().map(row_to_user).transpose()
}

/// 插入一行用户，返回新 id。
///
/// 必须在外层事务里调用：注册要「INSERT + 审计」同生共死（DESIGN §18.5）。
pub async fn insert_user(
    tx: &mut Transaction<'_, MySql>,
    new: NewUser<'_>,
) -> Result<u64, AppError> {
    let result = sqlx::query(
        "INSERT INTO users (username, email, password_hash, display_name, role, status) \
         VALUES (?, ?, ?, ?, ?, ?)",
    )
    .bind(new.username)
    .bind(new.email)
    .bind(new.password_hash)
    .bind(new.display_name)
    .bind(new.role.as_str())
    .bind(new.status.as_str())
    .execute(&mut **tx)
    .await?;

    Ok(result.last_insert_id())
}

/// 用户名或邮箱是否已被占用。
///
/// 返回 `bool` 而不是"哪个被占了"：调用方要给表单回填并给出**同一条**提示，
/// 分开告知等于提供一个账号枚举接口。真正的唯一性兜底仍靠数据库唯一索引——
/// 这里只用来把 500 提前变成 422。
pub async fn username_or_email_taken(
    pool: &MySqlPool,
    username: &str,
    email: &str,
) -> Result<bool, AppError> {
    let taken: i64 =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM users WHERE username = ? OR email = ?)")
            .bind(username)
            .bind(email)
            .fetch_one(pool)
            .await?;
    Ok(taken != 0)
}

/// 库里是否已经存在管理员（含已停用、已注销的账号）。
///
/// 判定口径故意放宽到"任何一行 role='admin'"：引导逻辑只该在**从来没有过**管理员时创建。
/// 如果连已注销的管理员也算进来，那么把最后一个管理员账号软删之后就再也引导不出来了；
/// 而如果只算 active 的，删掉管理员后别人又能用环境变量重新种一个 admin——那是提权后门。
pub async fn has_any_admin(pool: &MySqlPool) -> Result<bool, AppError> {
    let exists: i64 = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM users WHERE role = 'admin')")
        .fetch_one(pool)
        .await?;
    Ok(exists != 0)
}

/// 数一数还有几个**能用的**管理员：`role='admin' AND status='active'`。
///
/// 与 `has_any_admin` 的口径刻意不同：那个用于"有没有史以来第一个管理员"（引导用），
/// 这个用于"还能不能进管理区"（降级/停用/软删前的防线）。停用的 admin 不能登录，
/// 把它算进来会让"系统里还有管理员"变成一句谎话。
pub async fn count_active_admins(pool: &MySqlPool) -> Result<u64, AppError> {
    let total: u64 = sqlx::query_scalar(
        "SELECT CAST(COUNT(*) AS UNSIGNED) FROM users \
          WHERE role = 'admin' AND status = 'active'",
    )
    .fetch_one(pool)
    .await?;
    Ok(total)
}

/// 在**事务内**加锁数一数还能用的管理员，用于"最后一个管理员不许降级/停用/软删"。
///
/// 为什么不能用上面的 `count_active_admins`：那是事务外的一次普通读，
/// 两个管理员同时把对方降级时，两边都读到 2、都通过检查、都提交，
/// 结果一个 active admin 都不剩，而且**界面没有任何办法恢复**（只能改库）。
/// `FOR UPDATE` 让后到的那笔事务在前一笔提交后重新读到最新值，于是它看到 1 并被拒绝。
///
/// 加锁范围是"当前所有 active admin 行"：这是一个小集合（团队就几个人），
/// 但它同时钉住了"谁能进管理区"这个不变式。
pub async fn lock_active_admins(
    tx: &mut sqlx::Transaction<'_, sqlx::MySql>,
) -> Result<u64, AppError> {
    let ids: Vec<u64> = sqlx::query_scalar(
        "SELECT id FROM users WHERE role = 'admin' AND status = 'active' FOR UPDATE",
    )
    .fetch_all(&mut **tx)
    .await?;
    Ok(ids.len() as u64)
}

/// 改角色。返回是否真的改了（false = 行不存在或本来就是这个角色）。
pub async fn set_role(
    tx: &mut sqlx::Transaction<'_, sqlx::MySql>,
    id: u64,
    role: Role,
) -> Result<bool, AppError> {
    let result = sqlx::query("UPDATE users SET role = ? WHERE id = ? AND role <> ?")
        .bind(role.as_str())
        .bind(id)
        .bind(role.as_str())
        .execute(&mut **tx)
        .await?;
    Ok(result.rows_affected() == 1)
}

/// 改状态。`deleted` 不走这里（那要走 `soft_delete`，还要改名与清哈希）。
pub async fn set_status(
    tx: &mut sqlx::Transaction<'_, sqlx::MySql>,
    id: u64,
    status: UserStatus,
) -> Result<bool, AppError> {
    let result = sqlx::query("UPDATE users SET status = ? WHERE id = ? AND status <> ?")
        .bind(status.as_str())
        .bind(id)
        .bind(status.as_str())
        .execute(&mut **tx)
        .await?;
    Ok(result.rows_affected() == 1)
}

/// 改界面外观（强调色 / 背板 / 字体）。三项一次写完，返回是否真的改了。
///
/// **不写审计**：外观是纯偏好，为它写审计会把审计页变成操作流水账，
/// 而审计的价值在于"权限与内容变更可追溯"。这条取舍记在 DESIGN §28.2。
/// 调用方必须先把三个 key 各过一遍白名单（`shared::appearance::is_valid_*`），
/// 这里不做校验——它只负责把已经合法的值落库。
pub async fn set_appearance(
    tx: &mut sqlx::Transaction<'_, sqlx::MySql>,
    id: u64,
    theme: &str,
    background: &str,
    font: &str,
) -> Result<bool, AppError> {
    let result = sqlx::query(
        "UPDATE users SET theme = ?, background = ?, font = ? \
         WHERE id = ? AND (theme <> ? OR background <> ? OR font <> ?)",
    )
    .bind(theme)
    .bind(background)
    .bind(font)
    .bind(id)
    .bind(theme)
    .bind(background)
    .bind(font)
    .execute(&mut **tx)
    .await?;
    Ok(result.rows_affected() == 1)
}

/// 按 L74 软删除一个用户：改状态、改写唯一列、清空口令哈希，**保留行**。
///
/// 三段都必须在同一句 UPDATE 里：分开写的话，中间失败会留下
/// 「哈希已清空但用户名没改写」这种半状态——用户登不进去，唯一约束却没释放。
///
/// `status <> 'deleted'` 是幂等防线：重复软删会二次追加 `__deleted__{id}`，
/// 用户名越滚越长，最后撞列宽。
pub async fn soft_delete(
    tx: &mut sqlx::Transaction<'_, sqlx::MySql>,
    id: u64,
) -> Result<bool, AppError> {
    let result = sqlx::query(
        "UPDATE users \
            SET status = 'deleted', \
                username = CONCAT(username, '__deleted__', id), \
                email = CONCAT(email, '__deleted__', id), \
                password_hash = '' \
          WHERE id = ? AND status <> 'deleted'",
    )
    .bind(id)
    .execute(&mut **tx)
    .await?;
    Ok(result.rows_affected() == 1)
}

/// 行 → `User`。枚举列出现未知取值时报错（500 + 日志），**不静默兜底成默认值**。
fn row_to_user(row: &MySqlRow) -> Result<User, AppError> {
    let role_raw: String = row.try_get("role")?;
    let status_raw: String = row.try_get("status")?;

    let role = Role::from_db(&role_raw).ok_or_else(|| {
        AppError::internal(anyhow::anyhow!("users.role 出现未知取值：{role_raw}"))
    })?;
    let status = UserStatus::from_db(&status_raw).ok_or_else(|| {
        AppError::internal(anyhow::anyhow!("users.status 出现未知取值：{status_raw}"))
    })?;

    Ok(User {
        id: row.try_get("id")?,
        username: row.try_get("username")?,
        email: row.try_get("email")?,
        password_hash: row.try_get("password_hash")?,
        display_name: row.try_get("display_name")?,
        theme: row.try_get("theme")?,
        background: row.try_get("background")?,
        font: row.try_get("font")?,
        role,
        status,
        created_at: row.try_get("created_at")?,
        updated_at: row.try_get("updated_at")?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> Option<crate::config::Config> {
        let url = std::env::var("TEST_DATABASE_URL").ok()?;
        if url.trim().is_empty() {
            return None;
        }
        crate::config::Config::from_source(|key| match key {
            "DATABASE_URL" => Some(url.clone()),
            "APP_BASE_URL" => Some("http://localhost:8080".to_string()),
            _ => None,
        })
        .ok()
    }

    macro_rules! require_db {
        () => {
            match test_config() {
                Some(cfg) => cfg,
                None => {
                    eprintln!("跳过：未设置 TEST_DATABASE_URL");
                    return;
                }
            }
        };
    }

    /// 唯一后缀：并行跑测试时不会互相踩唯一索引。
    fn unique_suffix() -> String {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
            .to_string()
    }

    /// 删掉测试造出来的这个用户。
    ///
    /// users 现在被 announcement_acks 用 RESTRICT 外键引用，先删 users 会被数据库拒绝
    /// （1451）。测试夹具自己造的依赖行就自己清，别指望库替你级联——
    /// 生产上那些外键 RESTRICT 是故意的，不能为了测试方便去改。
    async fn cleanup(pool: &MySqlPool, username: &str) {
        sqlx::query(
            "DELETE FROM announcement_acks \
              WHERE user_id IN (SELECT id FROM users WHERE username = ?)",
        )
        .bind(username)
        .execute(pool)
        .await
        .expect("清理确认记录失败");

        sqlx::query("DELETE FROM users WHERE username = ?")
            .bind(username)
            .execute(pool)
            .await
            .expect("清理失败");
    }

    #[test]
    fn role_as_str_round_trips_through_from_db() {
        for role in Role::ALL {
            assert_eq!(Role::from_db(role.as_str()), Some(role));
        }
    }

    #[test]
    fn role_from_db_is_strict() {
        assert_eq!(Role::from_db("ADMIN"), None, "库里的值必须是小写");
        assert_eq!(Role::from_db(" admin"), None);
        assert_eq!(Role::from_db("root"), None);
        assert_eq!(Role::from_db(""), None);
    }

    #[test]
    fn role_from_str_is_lenient() {
        assert_eq!(" ADMIN ".parse::<Role>(), Ok(Role::Admin));
        assert_eq!("Member".parse::<Role>(), Ok(Role::Member));
        let err = "root".parse::<Role>().expect_err("应拒绝");
        assert_eq!(err.0, "root");
        assert!(err.to_string().contains("user/member/admin"));
    }

    #[test]
    fn status_as_str_round_trips_through_from_db() {
        for status in UserStatus::ALL {
            assert_eq!(UserStatus::from_db(status.as_str()), Some(status));
        }
    }

    #[test]
    fn status_from_db_is_strict() {
        assert_eq!(UserStatus::from_db("ACTIVE"), None);
        assert_eq!(UserStatus::from_db("gone"), None);
        assert_eq!(
            UserStatus::from_db("approved"),
            None,
            "审批状态不是账号状态"
        );
    }

    #[test]
    fn only_active_status_holds_a_session() {
        assert!(UserStatus::Active.is_active());
        for status in [
            UserStatus::Pending,
            UserStatus::Suspended,
            UserStatus::Deleted,
        ] {
            assert!(!status.is_active(), "{status} 不应持有会话");
        }
    }

    #[test]
    fn soft_deleted_user_cannot_hold_session_even_if_status_was_forgotten() {
        let user = User {
            id: 1,
            username: "gone__deleted__1".to_string(),
            email: "gone__deleted__1@example.com".to_string(),
            password_hash: String::new(),
            display_name: "已注销".to_string(),
            theme: crate::shared::appearance::DEFAULT_THEME.to_string(),
            background: crate::shared::appearance::DEFAULT_BACKDROP.to_string(),
            font: crate::shared::appearance::DEFAULT_FONT.to_string(),
            role: Role::User,
            status: UserStatus::Active,
            created_at: DateTime::<Utc>::MIN_UTC,
            updated_at: DateTime::<Utc>::MIN_UTC,
        };
        assert!(!user.has_password());
        assert!(
            !user.can_hold_session(),
            "口令为空串的用户不得持有会话（软删除的兜底）"
        );
    }

    #[tokio::test]
    async fn insert_then_read_back_by_id_and_username() {
        let cfg = require_db!();
        let pool = crate::db::init_pool(&cfg).await.expect("建池失败");
        let suffix = unique_suffix();
        let username = format!("modt{suffix}");
        let email = format!("modt{suffix}@example.com");

        let hash = "$argon2id$v=19$m=19456,t=2,p=1$c2FsdA$aGFzaA";
        let mut tx = pool.begin().await.expect("开事务失败");
        let id = insert_user(
            &mut tx,
            NewUser {
                username: &username,
                email: &email,
                password_hash: hash,
                display_name: "模型测试",
                role: Role::User,
                status: UserStatus::Active,
            },
        )
        .await
        .expect("插入失败");
        tx.commit().await.expect("提交失败");

        let by_id = find_by_id(&pool, id).await.expect("按 id 查失败");
        let by_name = find_by_username(&pool, &username)
            .await
            .expect("按用户名查失败");
        let user = by_id.expect("按 id 应查到");
        assert_eq!(Some(&user), by_name.as_ref(), "两个入口应返回同一行");
        assert_eq!(user.username, username);
        assert_eq!(user.email, email);
        assert_eq!(user.role, Role::User, "注册默认角色必须是 user");
        assert_eq!(user.status, UserStatus::Active);
        assert!(user.has_password());

        cleanup(&pool, &username).await;
    }

    #[tokio::test]
    async fn username_or_email_taken_reports_both_columns() {
        let cfg = require_db!();
        let pool = crate::db::init_pool(&cfg).await.expect("建池失败");
        let suffix = unique_suffix();
        let username = format!("take{suffix}");
        let email = format!("take{suffix}@example.com");

        assert!(
            !username_or_email_taken(&pool, &username, &email)
                .await
                .expect("查询失败"),
            "尚未插入时不应算被占用"
        );

        let mut tx = pool.begin().await.expect("开事务失败");
        insert_user(
            &mut tx,
            NewUser {
                username: &username,
                email: &email,
                password_hash: "x",
                display_name: "占用测试",
                role: Role::User,
                status: UserStatus::Active,
            },
        )
        .await
        .expect("插入失败");
        tx.commit().await.expect("提交失败");

        assert!(
            username_or_email_taken(&pool, &username, &format!("other{suffix}@example.com"))
                .await
                .expect("查询失败"),
            "用户名重复应被发现"
        );
        assert!(
            username_or_email_taken(&pool, &format!("other{suffix}"), &email)
                .await
                .expect("查询失败"),
            "邮箱重复应被发现"
        );

        cleanup(&pool, &username).await;
    }

    #[tokio::test]
    async fn unknown_role_in_database_is_an_error_not_a_silent_default() {
        let cfg = require_db!();
        let pool = crate::db::init_pool(&cfg).await.expect("建池失败");
        let suffix = unique_suffix();
        let username = format!("bad{suffix}");

        // 绕过 model 直接写一个非法角色：模拟"未来某个写入口漏了约束"。
        // 用 execute 而不是 query_scalar——INSERT 不返回行，query_scalar 会报 RowNotFound。
        let inserted = sqlx::query(
            "INSERT INTO users (username, email, password_hash, display_name, role, status) \
             VALUES (?, ?, 'x', '坏数据', 'wizard', 'active')",
        )
        .bind(&username)
        .bind(format!("{username}@example.com"))
        .execute(&pool)
        .await
        .expect("插入非法角色失败");
        let id = inserted.last_insert_id();

        let err = find_by_id(&pool, id)
            .await
            .expect_err("未知角色必须报错，不能兜底成默认值");
        let shown = format!("{err}");
        assert!(
            shown.contains("wizard") && shown.contains("users.role"),
            "错误应指明是哪个列、哪个值：{shown}"
        );
        assert_eq!(
            err.status_code(),
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            "未知枚举是内部错误，不是 4xx"
        );

        cleanup(&pool, &username).await;
    }
}
