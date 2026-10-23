//! 身份业务逻辑：注册、登录、登出，以及口令哈希与失败计数。
//!
//! 这一层是**事务边界**和**审计边界**的所在地。handler 只做三件事：取表单、
//! 调这里、把结果翻译成响应；它不许直接写库，也不许直接调 `audit::record`。
//!
//! 三个容易被"优化"掉、但一旦去掉就出事的决定：
//!
//! 1. **argon2 全程在 `spawn_blocking` 里跑，并发上限 2**。argon2id 单次约 50ms 纯 CPU，
//!    直接放在 async 任务里会把 tokio 的 worker 线程钉死——2 核机器上两个并发登录
//!    就能让 `/healthz` 一起卡住。用信号量而不是线程池大小，是因为要限的是"同时在算哈希"
//!    的数量，不是"同时存在的任务"。
//! 2. **查不到用户也要算一次哈希**（用预先算好的哑 PHC 串）。否则"用户不存在"几微秒返回、
//!    "密码错" 50ms 返回，用户名枚举就变成一个秒表。哑校验同样要占 permit——不占的话，
//!    攻击者用不存在的用户名就能绕过并发闸门。
//! 3. **登录失败的计数在进程内存里，且每次读写都清过期**。`HashMap` 不清理就是内存放大器：
//!    攻击者拿随机用户名刷，键会无限增长。2C2G 上这不是理论问题。

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use argon2::password_hash::{PasswordHasher, PasswordVerifier};
use argon2::{Algorithm, Argon2, Params, Version};
use serde_json::json;
use sqlx::MySqlPool;
use tokio::sync::Semaphore;

use crate::auth::model::{self, NewUser, Role, UserStatus};
use crate::error::AppError;
use crate::shared::audit::{self, AuditCtx, action};

/// argon2id 参数：OWASP 最低档（m=19456 KiB、t=2、p=1）。
///
/// 需求把这三个数字写死了，所以它们**不是**配置项——能被环境变量改掉的密码学参数，
/// 迟早会在某台机器上被改成 `m=1024` 换来"启动快一点"。
const ARGON2_MEMORY_KIB: u32 = 19_456;
const ARGON2_ITERATIONS: u32 = 2;
const ARGON2_PARALLELISM: u32 = 1;

/// 同一个"IP + 用户名"窗口内允许的失败次数。
const MAX_ATTEMPTS_PER_USER: u32 = 5;
/// 同一个 IP 允许的失败次数。挡住"换着用户名撞库"——只按 IP+用户名 计数时，
/// 攻击者每次换一个用户名就是一个全新的计数器。
const MAX_ATTEMPTS_PER_IP: u32 = 20;
/// 计数窗口。窗口从**第一次**失败开始算，不滑动：滑动窗口意味着持续攻击永远不会解锁，
/// 而这套机制的目的是限速，不是永久封禁。
const ATTEMPT_WINDOW: Duration = Duration::from_secs(300);

/// 计数表容量上限。超过就整体清空并告警——极端流量下选择"牺牲严格性保内存"，
/// 因为能把这个表刷爆的流量本身已经是需要人工介入的事件。
const MAX_TRACKED_KEYS: usize = 4_096;

/// 两个并发哈希名额。需求原文 `Semaphore(2)`。
static ARGON2_PERMITS: LazyLock<Semaphore> = LazyLock::new(|| Semaphore::new(2));

/// 失败计数：`{ip}|{username_lower}` 与 `ip|{ip}` 两种键共存于同一张表。
static ATTEMPTS: LazyLock<Mutex<HashMap<String, Attempt>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// 构造一次 argon2 实例。参数是常量，失败说明代码被改坏了——这种情况下宁可 500，
/// 也不要退回更弱的参数继续跑。
static ARGON2: LazyLock<Result<Argon2<'static>, String>> = LazyLock::new(|| {
    Params::new(
        ARGON2_MEMORY_KIB,
        ARGON2_ITERATIONS,
        ARGON2_PARALLELISM,
        None,
    )
    .map(|params| Argon2::new(Algorithm::Argon2id, Version::V0x13, params))
    .map_err(|err| format!("argon2 参数不合法：{err}"))
});

/// 哑校验用的口令。它永远不会匹配任何真实账号，只为了消耗与真实校验相同的时间。
const DUMMY_PASSWORD: &str = "nexus-dummy-password-never-matches";

/// 预先算好的哑 PHC 串。首次登录时算一次（约 50ms），之后复用。
static DUMMY_PHC: LazyLock<String> = LazyLock::new(|| {
    hash_password_blocking(DUMMY_PASSWORD).unwrap_or_else(|err| {
        tracing::error!(error = ?err, "无法生成哑哈希，登录耗时将随账号是否存在而变化");
        String::new()
    })
});

#[derive(Debug)]
struct Attempt {
    count: u32,
    first_seen: Instant,
}

fn argon2() -> Result<&'static Argon2<'static>, AppError> {
    ARGON2
        .as_ref()
        .map_err(|msg| AppError::internal(anyhow::anyhow!("{msg}")))
}

fn hash_password_blocking(password: &str) -> Result<String, AppError> {
    argon2()?
        .hash_password(password.as_bytes())
        .map(|hash| hash.to_string())
        .map_err(|err| AppError::internal(anyhow::anyhow!("口令哈希失败：{err}")))
}

/// 校验口令。**同步**函数：调用方负责把它放进 `spawn_blocking`。
///
/// 任何异常（空 PHC、格式坏了、参数不合）都返回 `false`——错误信息不往调用方传，
/// 因为调用方只能做一件事：告诉用户"用户名或密码不对"。
pub fn verify_password(password: &str, phc: &str) -> bool {
    if phc.is_empty() {
        // 软删除用户的 `password_hash` 是空串（不是 NULL）。
        return false;
    }
    let Ok(hasher) = argon2() else {
        return false;
    };
    hasher.verify_password(password.as_bytes(), phc).is_ok()
}

/// 在阻塞线程池里算哈希，并占用一个并发名额。
pub async fn hash_password(password: &str) -> Result<String, AppError> {
    let permit = ARGON2_PERMITS
        .acquire()
        .await
        .map_err(|_| AppError::internal(anyhow::anyhow!("argon2 并发闸门已关闭")))?;
    let password = password.to_owned();

    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        hash_password_blocking(&password)
    })
    .await
    .map_err(|err| AppError::internal(anyhow::Error::new(err).context("口令哈希任务失败")))?
}

fn attempt_key(ip: Option<&str>, username: &str) -> String {
    format!(
        "{}|{}",
        ip.unwrap_or("-"),
        username.trim().to_ascii_lowercase()
    )
}

fn ip_key(ip: &str) -> String {
    format!("ip|{ip}")
}

fn attempts() -> MutexGuard<'static, HashMap<String, Attempt>> {
    // 中毒说明别的线程 panic 在了持锁期间。计数表不是一致性数据，
    // 恢复内层数据继续用即可——这里返回 500 只会把一次 panic 放大成全站登录不可用。
    ATTEMPTS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn prune(map: &mut HashMap<String, Attempt>) {
    map.retain(|_, attempt| attempt.first_seen.elapsed() < ATTEMPT_WINDOW);
}

fn guard_capacity(map: &mut HashMap<String, Attempt>) {
    if map.len() >= MAX_TRACKED_KEYS {
        tracing::warn!(
            keys = map.len(),
            "登录失败计数表达到容量上限，整体清空以避免内存放大"
        );
        map.clear();
    }
}

/// 还要等多久才能再试。`None` 表示没被锁。
fn locked_for(key: &str, limit: u32) -> Option<Duration> {
    let mut map = attempts();
    prune(&mut map);
    let attempt = map.get(key)?;
    if attempt.count < limit {
        return None;
    }
    ATTEMPT_WINDOW.checked_sub(attempt.first_seen.elapsed())
}

fn record_failure(key: &str) {
    let mut map = attempts();
    prune(&mut map);
    guard_capacity(&mut map);
    let attempt = map.entry(key.to_owned()).or_insert(Attempt {
        count: 0,
        first_seen: Instant::now(),
    });
    attempt.count = attempt.count.saturating_add(1);
}

fn clear_failures(key: &str) {
    let mut map = attempts();
    prune(&mut map);
    map.remove(key);
}

/// 登录结果。
///
/// **除了 `Ok`，其余三态在 handler 里都翻译成同一句提示**——`Disabled` 存在只是为了
/// 让日志和审计能区分"这个人被封了"和"这个人密码错了"，不是为了告诉请求方。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoginOutcome {
    Ok { user_id: u64, role: Role },
    BadCredentials,
    Locked { retry_after: Duration },
    Disabled,
}

/// 注册入参。`display_name` 省略时回落成用户名。
#[derive(Debug, Clone, Copy)]
pub struct RegisterInput<'a> {
    pub username: &'a str,
    pub email: &'a str,
    pub password: &'a str,
    pub display_name: Option<&'a str>,
}

/// 注册一个新用户，返回新 id。
///
/// `ctx` 由 handler 用 `ConnectInfo` 构造（游客，`actor_id` 为空）。
/// INSERT 与审计在**同一个事务**里：审计写失败，用户行必须一起消失。
pub async fn register(
    pool: &MySqlPool,
    ctx: AuditCtx<'_>,
    input: RegisterInput<'_>,
) -> Result<u64, AppError> {
    let username = input.username.trim();
    let email = input.email.trim();

    // 先查重是为了给表单一个 422，而不是让用户看到唯一索引的 500。
    // 真正的唯一性仍然只由数据库保证（并发注册两个同名用户时这里都会查到"没被占用"）。
    if model::username_or_email_taken(pool, username, email).await? {
        return Err(AppError::unprocessable(USERNAME_TAKEN));
    }

    let password_hash = hash_password(input.password).await?;
    let display_name = input
        .display_name
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .unwrap_or(username);

    let mut tx = pool.begin().await?;

    let new_user = NewUser {
        username,
        email,
        password_hash: &password_hash,
        display_name,
        role: Role::User,
        status: UserStatus::Active,
    };

    let user_id = model::insert_user(&mut tx, new_user)
        .await
        .map_err(|err| match err {
            // 并发窗口里被别的请求抢先插入：唯一索引会报 23000，转成与前面同一句提示。
            AppError::Conflict(_) => AppError::unprocessable(USERNAME_TAKEN),
            other => other,
        })?;

    audit::record(
        &mut tx,
        ctx,
        action::AUTH_REGISTER,
        ("user", Some(user_id)),
        Some(json!({ "username": username })),
    )
    .await?;

    tx.commit().await?;

    Ok(user_id)
}

/// 引导首个管理员的结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BootstrapOutcome {
    /// 新建成功。
    Created { user_id: u64 },
    /// 库里已经有管理员了，什么都没做。
    Skipped,
}

/// 引导域。引导管理员需要一个占用唯一索引的邮箱，而运维没道理为此再多设一个变量。
///
/// 用 `.invalid` 是刻意的：RFC 2606 保留顶级域，永不解析，不会误发给真人。
const BOOTSTRAP_EMAIL_DOMAIN: &str = "bootstrap.invalid";

/// 首个管理员引导（DESIGN §19 A13）。
///
/// 只在库里**一个 admin 都没有**时创建。这条规则的真正作用是关掉后门：
/// 一旦有过管理员，环境变量就彻底失效——否则任何能改环境变量的人都能随时种一个 admin 进去。
///
/// 用户名已被占用时**报错而不是把那个账号提权**：把普通用户静默变成管理员，
/// 是一次没人审计过的权限变更，而且原账号的密码掌握在别人手里。
pub async fn ensure_bootstrap_admin(
    pool: &MySqlPool,
    username: &str,
    password: &str,
) -> Result<BootstrapOutcome, AppError> {
    if model::has_any_admin(pool).await? {
        return Ok(BootstrapOutcome::Skipped);
    }

    let username = username.trim();
    let email = format!("{username}@{BOOTSTRAP_EMAIL_DOMAIN}");

    if model::username_or_email_taken(pool, username, &email).await? {
        return Err(AppError::conflict(
            "引导管理员失败：这个用户名或邮箱已被占用，请换一个 BOOTSTRAP_ADMIN_USERNAME",
        ));
    }

    let password_hash = hash_password(password).await?;
    let mut tx = pool.begin().await?;

    let user_id = model::insert_user(
        &mut tx,
        NewUser {
            username,
            email: &email,
            password_hash: &password_hash,
            display_name: username,
            role: Role::Admin,
            status: UserStatus::Active,
        },
    )
    .await?;

    // 审计按"角色变更"记账，而不是发明一个 bootstrap 动作：
    // 查"谁在什么时候变成过 admin"时，不该有两套账。
    audit::record(
        &mut tx,
        AuditCtx::anonymous(None),
        action::USER_ROLE_CHANGE,
        ("user", Some(user_id)),
        Some(json!({ "username": username, "from": "none", "to": "admin", "source": "bootstrap" })),
    )
    .await?;

    tx.commit().await?;

    tracing::info!(user_id, username, "已创建首个管理员（来自环境变量引导）");
    Ok(BootstrapOutcome::Created { user_id })
}

/// 用户名/邮箱被占用的统一提示。
///
/// 分开告知"用户名被占用"和"邮箱被占用"，等于提供一个账号枚举接口。
const USERNAME_TAKEN: &str = "这个用户名或邮箱已经被使用了";

/// 登录。
///
/// 固定顺序（DESIGN §18.6）：锁检查 → 占并发名额 → 查库 → 校验（查不到也校验）
/// → 判定 → 审计 → 返回。顺序不是风格问题：把"查库"放在"占名额"之前，
/// 攻击者就能用不存在的用户名把数据库打满而不触发任何限速。
pub async fn login(
    pool: &MySqlPool,
    ip: Option<&str>,
    username: &str,
    password: &str,
) -> Result<LoginOutcome, AppError> {
    let username = username.trim();
    let user_key = attempt_key(ip, username);

    if let Some(retry_after) = locked_for(&user_key, MAX_ATTEMPTS_PER_USER) {
        return Ok(LoginOutcome::Locked { retry_after });
    }
    if let Some(ip) = ip
        && let Some(retry_after) = locked_for(&ip_key(ip), MAX_ATTEMPTS_PER_IP)
    {
        return Ok(LoginOutcome::Locked { retry_after });
    }

    let permit = ARGON2_PERMITS
        .acquire()
        .await
        .map_err(|_| AppError::internal(anyhow::anyhow!("argon2 并发闸门已关闭")))?;

    let found = model::find_by_username(pool, username).await?;

    let password = password.to_owned();
    let (verified, user) = match &found {
        Some(user) => {
            let phc = user.password_hash.clone();
            let ok = tokio::task::spawn_blocking(move || {
                let _permit = permit;
                verify_password(&password, &phc)
            })
            .await
            .map_err(|err| {
                AppError::internal(anyhow::Error::new(err).context("口令校验任务失败"))
            })?;
            (ok, Some(user.clone()))
        }
        None => {
            let phc = DUMMY_PHC.clone();
            let ok = tokio::task::spawn_blocking(move || {
                let _permit = permit;
                verify_password(&password, &phc)
            })
            .await
            .map_err(|err| {
                AppError::internal(anyhow::Error::new(err).context("口令校验任务失败"))
            })?;
            (ok, None)
        }
    };

    let accepted = verified && user.as_ref().is_some_and(|u| u.can_hold_session());

    if accepted {
        let user = user.expect("accepted 蕴含 user.is_some()");
        clear_failures(&user_key);
        if let Some(ip) = ip {
            clear_failures(&ip_key(ip));
        }
        // 审计失败就不建会话：登录是状态变更，写不进审计的登录不允许发生。
        record_audit(
            pool,
            AuditCtx::new(Some(user.id), ip),
            action::AUTH_LOGIN,
            user.id,
            json!({ "role": user.role.as_str() }),
        )
        .await?;
        return Ok(LoginOutcome::Ok {
            user_id: user.id,
            role: user.role,
        });
    }

    record_failure(&user_key);
    if let Some(ip) = ip {
        record_failure(&ip_key(ip));
    }

    let reason = match &user {
        None => "no_such_user",
        Some(u) if !u.status.is_active() => u.status.as_str(),
        Some(u) if !u.has_password() => "no_password",
        Some(_) => "bad_password",
    };
    // 登录失败**不**让审计决定业务成败：审计库挂了的时候，登录失败必须仍然返回
    // 与平时一致的提示，否则 500 就是一个"这个用户名是否存在"的探测器。
    if let Err(err) = record_audit_raw(
        pool,
        AuditCtx::anonymous(ip),
        action::AUTH_LOGIN_FAILED,
        None,
        json!({ "username": username, "reason": reason }),
    )
    .await
    {
        tracing::error!(error = ?err, "登录失败审计写入失败（已降级为只记日志）");
    }

    match &user {
        Some(u) if !u.status.is_active() => Ok(LoginOutcome::Disabled),
        _ => Ok(LoginOutcome::BadCredentials),
    }
}

/// 登出：写一条审计，然后由调用方销毁会话。
///
/// 审计写失败**不**阻断登出。理由：审计库挂掉时若禁止登出，全站没有人能退出登录——
/// 把一次可观测的故障升级成一次不可恢复的故障。
pub async fn logout(pool: &MySqlPool, actor_id: u64, ip: Option<&str>) -> Result<(), AppError> {
    let mut tx = match pool.begin().await {
        Ok(tx) => tx,
        Err(err) => {
            tracing::error!(error = ?err, "登出审计无法开启事务（已降级为只记日志）");
            return Ok(());
        }
    };

    let written = audit::record(
        &mut tx,
        AuditCtx::new(Some(actor_id), ip),
        action::AUTH_LOGOUT,
        ("user", Some(actor_id)),
        None,
    )
    .await;

    if let Err(err) = written {
        tracing::error!(error = ?err, "登出审计写入失败（已降级为只记日志）");
        return Ok(());
    }

    if let Err(err) = tx.commit().await {
        tracing::error!(error = ?err, "登出审计提交失败（已降级为只记日志）");
    }

    Ok(())
}

async fn record_audit(
    pool: &MySqlPool,
    ctx: AuditCtx<'_>,
    action_name: &str,
    user_id: u64,
    meta: serde_json::Value,
) -> Result<(), AppError> {
    record_audit_raw(pool, ctx, action_name, Some(user_id), meta).await
}

async fn record_audit_raw(
    pool: &MySqlPool,
    ctx: AuditCtx<'_>,
    action_name: &str,
    target_id: Option<u64>,
    meta: serde_json::Value,
) -> Result<(), AppError> {
    let mut tx = pool.begin().await?;
    audit::record(&mut tx, ctx, action_name, ("user", target_id), Some(meta)).await?;
    tx.commit().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attempt_key_normalizes_username_and_keeps_ip() {
        assert_eq!(
            attempt_key(Some("203.0.113.7"), "  Alice "),
            "203.0.113.7|alice"
        );
        assert_eq!(attempt_key(None, "Bob"), "-|bob");
    }

    #[test]
    fn counter_locks_on_fifth_failure_and_clears_on_success() {
        let key = "test-lock|case-one";
        for _ in 0..4 {
            record_failure(key);
        }
        assert!(
            locked_for(key, MAX_ATTEMPTS_PER_USER).is_none(),
            "第 4 次失败还不该锁"
        );
        record_failure(key);
        assert!(
            locked_for(key, MAX_ATTEMPTS_PER_USER).is_some(),
            "第 5 次失败必须锁住"
        );
        clear_failures(key);
        assert!(
            locked_for(key, MAX_ATTEMPTS_PER_USER).is_none(),
            "成功登录后计数必须清零"
        );
    }

    #[test]
    fn expired_entries_are_pruned_instead_of_counted() {
        let key = "test-lock|expired";
        {
            let mut map = attempts();
            map.insert(
                key.to_owned(),
                Attempt {
                    count: 99,
                    first_seen: Instant::now() - ATTEMPT_WINDOW - Duration::from_secs(1),
                },
            );
        }
        assert!(
            locked_for(key, MAX_ATTEMPTS_PER_USER).is_none(),
            "过窗的记录必须先被清掉，否则一次旧攻击会永久锁死账号"
        );
        assert!(
            !attempts().contains_key(key),
            "prune 必须真的从表里删除，而不只是被跳过"
        );
    }

    #[tokio::test]
    async fn hash_then_verify_roundtrip() {
        let phc = hash_password("correct horse battery staple").await.unwrap();
        assert!(phc.starts_with("$argon2id$"), "必须是 argon2id：{phc}");
        assert!(verify_password("correct horse battery staple", &phc));
        assert!(!verify_password("wrong password!", &phc));
    }

    /// 注入形状的口令按普通字符处理：口令从不拼进 SQL，全部走绑定参数。
    #[tokio::test]
    async fn quote_heavy_password_round_trips_literally() {
        let tricky = "'OR'1'='1";
        let phc = hash_password(tricky).await.unwrap();
        assert!(verify_password(tricky, &phc), "带引号的口令必须原样校验");
        assert!(!verify_password("'OR'1'='2", &phc), "差一个字符就不能通过");
    }

    #[tokio::test]
    async fn verify_rejects_empty_and_malformed_hashes_without_panicking() {
        assert!(!verify_password("anything", ""), "空串是软删除用户的标记");
        assert!(!verify_password("anything", "not-a-phc-string"));
        assert!(!verify_password(
            "anything",
            "$argon2id$v=19$m=1,t=1,p=1$broken"
        ));
    }

    #[tokio::test]
    async fn every_hash_is_salted_differently() {
        let a = hash_password("same password value").await.unwrap();
        let b = hash_password("same password value").await.unwrap();
        assert_ne!(a, b, "盐必须随机，否则相同密码会得到相同哈希");
    }

    #[test]
    fn dummy_hash_is_usable_so_timing_path_really_computes() {
        // 哑哈希算不出来时 `verify_password` 会立刻返回 false，时间差从此泄漏。
        // 这条测试盯的就是那个静默降级。
        assert!(
            !DUMMY_PHC.is_empty(),
            "哑 PHC 串生成失败，登录计时会泄漏账号是否存在"
        );
        // 哑哈希必须是**合法**的 argon2id 串，否则 verify 会在解析阶段就失败、
        // 一毫秒都不烧，计时照样泄漏。能校验通过才证明它真的跑完了整轮计算。
        assert!(
            verify_password(DUMMY_PASSWORD, &DUMMY_PHC),
            "哑哈希无法通过校验，说明它是个残串"
        );
        assert!(!verify_password("anything-else-at-all", &DUMMY_PHC));
    }
}
