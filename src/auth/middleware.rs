//! 身份中间件层：把 session 里的 `user_id` 解析成**每请求**的 `CurrentUser`，
//! 以及三件横切工具——CSRF 令牌、客户端 IP、会话起止。
//!
//! 为什么不用一层 `from_fn` 中间件，而用提取器：
//!
//! - 只有真正用到身份的 handler 才付那次查库的钱。`/healthz` 不该因为"有人可能在
//!   登着录"就去查一次 users 表——这在库挂掉时正是最关键的区别（探针必须还能回答）。
//! - 提取器里解析的结果缓存在请求扩展里，同一次请求里 `MaybeUser` 和 `MaybeUserRow`
//!   各取一次也只查一次库。
//!
//! 会话里**只存 `user_id`**，role 和 status 每请求从库里读。代价是每个已登录请求多一次
//! 主键查询；换来的是管理员把某人停用后，对方手里的旧 cookie 下一个请求就失效。
//! 把 role 塞进 session 能省这次查询，但那意味着权限撤销要等 12 小时才生效。

use std::net::SocketAddr;

use axum::extract::{ConnectInfo, FromRequestParts};
use axum::http::HeaderMap;
use axum::http::request::Parts;
use chrono::Utc;
use tower_sessions::Session;

use crate::auth::model::{self, User};
use crate::auth::policy::CurrentUser;
use crate::error::AppError;
use crate::group::model as group_model;
use crate::state::AppState;

/// session 里存当前用户主键的键名。
pub const SESSION_USER_ID: &str = "user_id";
/// session 里存 CSRF 令牌的键名。
pub const SESSION_CSRF: &str = "csrf";
/// session 里存登录时刻（Unix 秒）的键名，仅供审计与排障，不参与判定。
pub const SESSION_LOGIN_AT: &str = "login_at";

/// CSRF 令牌的字符长度：32 字节随机数转十六进制。
const CSRF_TOKEN_LEN: usize = 64;

/// 当前请求的身份。`None` 就是游客——**没有**"未知"这一态。
#[derive(Debug, Clone)]
pub struct MaybeUser(pub Option<CurrentUser>);

impl MaybeUser {
    pub fn user(&self) -> Option<&CurrentUser> {
        self.0.as_ref()
    }

    pub fn id(&self) -> Option<u64> {
        self.0.as_ref().map(|user| user.id)
    }

    pub fn is_admin(&self) -> bool {
        self.0.as_ref().is_some_and(|user| user.role.is_admin())
    }

    /// 已登录但账号不可用的情形在解析阶段就被归一化成游客，所以这里能"有值"就一定是好人。
    pub fn require(&self) -> Result<&CurrentUser, AppError> {
        self.0.as_ref().ok_or_else(|| AppError::LoginRequired {
            next: "/".to_owned(),
        })
    }
}

/// 当前用户的完整资料行（导航栏要显示昵称）。
///
/// 和 `MaybeUser` 分开，是因为权限判定只该看见 `CurrentUser` 那四个字段——
/// 让 `can_view` 顺手能拿到 email 或 password_hash，早晚有人会在策略函数里读它。
#[derive(Debug, Clone)]
pub struct MaybeUserRow(pub Option<User>);

impl MaybeUserRow {
    pub fn display_name(&self) -> &str {
        self.0
            .as_ref()
            .map(|user| user.display_name.as_str())
            .unwrap_or("")
    }

    /// 当前用户选的主题 key。游客拿到默认主题。
    ///
    /// 这里过一遍 `normalize`：库里的值可能来自更老的版本、或被人手工改过，
    /// 渲染路径不该因为一个坏字符串就 panic 或白屏。
    pub fn look(&self) -> crate::shared::appearance::Appearance {
        use crate::shared::appearance::{
            Appearance, DEFAULT_BACKDROP, DEFAULT_FONT, DEFAULT_THEME,
        };
        self.0
            .as_ref()
            .map(|user| Appearance::normalize(&user.theme, &user.background, &user.font))
            .unwrap_or(Appearance {
                theme: DEFAULT_THEME,
                backdrop: DEFAULT_BACKDROP,
                font: DEFAULT_FONT,
            })
    }
}

/// 请求扩展里的解析缓存，保证同一请求只查一次库。
///
/// 用户行与小组 id 一起缓存：`MaybeUser` 与 `MaybeUserRow` 都会调 `resolve`，
/// 分开存就会出现"一个提取器拿了用户、另一个又查了一遍小组"的局面。
#[derive(Clone)]
struct CachedUser {
    user: Option<User>,
    group_ids: Vec<u64>,
}

/// 一个人最多带多少个小组进 `CurrentUser`。
///
/// policy 是纯内存函数，`group_ids.contains()` 是线性查找；没有上限的话，
/// 一个被塞进几万组的账号会让每个请求都变慢，而这是别人能通过加组影响到的成本。
const MAX_GROUPS_PER_USER: usize = 64;

/// 解析当前用户，结果缓存进 `parts.extensions`。
///
/// 三种"有 user_id 但不能用"的情况——用户被删、被停用、密码已被清空（软删除）——
/// 一律按游客处理并把会话整个销毁，而不是返回 403：403 等于告诉对方"你存在但被禁了"，
/// 而且被封的人会卡在一个无法自救的错误页上，连"换个账号登录"都做不到。
async fn resolve(parts: &mut Parts, state: &AppState) -> Result<CachedUser, AppError> {
    if let Some(cached) = parts.extensions.get::<CachedUser>() {
        return Ok(cached.clone());
    }

    let session = <Session as FromRequestParts<AppState>>::from_request_parts(parts, state)
        .await
        .map_err(|(_, reason)| {
            AppError::internal(anyhow::anyhow!(
                "无法提取会话（缺少 SessionManagerLayer？）：{reason}"
            ))
        })?;

    let user_id: Option<u64> = session.get(SESSION_USER_ID).await.map_err(session_error)?;

    let resolved = match user_id {
        None => CachedUser {
            user: None,
            group_ids: Vec::new(),
        },
        Some(id) => {
            let found = model::find_by_id(&state.pool, id).await?;
            match found {
                Some(user) if user.can_hold_session() => {
                    // 小组**每请求**重查：把 group_ids 塞进 session 的话，管理员把人移出组后
                    // 对方要等会话过期才失效，那和没做这个功能差不多（A11 同一套逻辑）。
                    let group_ids = load_group_ids(&state.pool, id).await?;
                    CachedUser {
                        user: Some(user),
                        group_ids,
                    }
                }
                _ => {
                    // 账号已不可用：销毁会话，让下一个请求彻底变成游客。
                    if let Err(err) = session.flush().await {
                        tracing::warn!(error = ?err, user_id = id, "账号不可用时销毁会话失败");
                    }
                    CachedUser {
                        user: None,
                        group_ids: Vec::new(),
                    }
                }
            }
        }
    };

    parts.extensions.insert(resolved.clone());
    Ok(resolved)
}

/// 加载用户所在的小组 id，排序去重并封顶。
///
/// 软删的组不算（`groups_of_user` 已在 SQL 里过滤），否则"删了组还能看见组内容"。
async fn load_group_ids(pool: &sqlx::MySqlPool, user_id: u64) -> Result<Vec<u64>, AppError> {
    let mut ids = group_model::groups_of_user(pool, user_id).await?;
    ids.sort_unstable();
    ids.dedup();
    if ids.len() > MAX_GROUPS_PER_USER {
        tracing::warn!(
            user_id,
            total = ids.len(),
            kept = MAX_GROUPS_PER_USER,
            "用户所属小组过多，已截断；policy 判定将只考虑前若干个组"
        );
        ids.truncate(MAX_GROUPS_PER_USER);
    }
    Ok(ids)
}

fn session_error(err: tower_sessions::session::Error) -> AppError {
    AppError::internal(anyhow::Error::new(err).context("会话读写失败"))
}

impl FromRequestParts<AppState> for MaybeUser {
    type Rejection = AppError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let resolved = resolve(parts, state).await?;
        Ok(MaybeUser(resolved.user.as_ref().map(|user| {
            CurrentUser::from_user_with_groups(user, resolved.group_ids.clone())
        })))
    }
}

impl FromRequestParts<AppState> for MaybeUserRow {
    type Rejection = AppError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        Ok(MaybeUserRow(resolve(parts, state).await?.user))
    }
}

/// 取 CSRF 令牌，没有就签发一个。
///
/// 游客也签发——登录表单本身需要令牌，而登录之前当然还不存在"已登录用户"。
pub async fn ensure_csrf(session: &Session) -> Result<String, AppError> {
    if let Some(token) = session
        .get::<String>(SESSION_CSRF)
        .await
        .map_err(session_error)?
        && token.len() == CSRF_TOKEN_LEN
    {
        return Ok(token);
    }
    let token = new_csrf_token();
    session
        .insert(SESSION_CSRF, token.clone())
        .await
        .map_err(session_error)?;
    Ok(token)
}

/// 校验提交上来的 CSRF 令牌。
///
/// 比较用常量时间：`==` 会在第一个不同的字节处返回，理论上可被逐字节爆破。
/// 令牌只有 64 个十六进制字符，爆破也就是 64×16 次请求，但这种比较不该出现在安全路径上。
pub async fn require_csrf(session: &Session, submitted: Option<&str>) -> Result<(), AppError> {
    let expected: Option<String> = session.get(SESSION_CSRF).await.map_err(session_error)?;
    let submitted = submitted.unwrap_or("");
    match expected {
        Some(expected) if constant_time_eq(expected.as_bytes(), submitted.as_bytes()) => Ok(()),
        _ => Err(AppError::Forbidden),
    }
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    let mut diff = 0u8;
    for (a, b) in left.iter().zip(right) {
        diff |= a ^ b;
    }
    diff == 0
}

fn new_csrf_token() -> String {
    let mut bytes = [0u8; 32];
    bytes[..16].copy_from_slice(&rand::random::<u128>().to_le_bytes());
    bytes[16..].copy_from_slice(&rand::random::<u128>().to_le_bytes());
    let mut token = String::with_capacity(CSRF_TOKEN_LEN);
    for byte in bytes {
        use std::fmt::Write as _;
        // 写入 String 不会失败（内存分配失败会直接 abort），忽略返回值不影响正确性。
        let _ = write!(token, "{byte:02x}");
    }
    token
}

/// 登录成功后建立会话：换 id、写 user_id、轮换 CSRF、记登录时刻。
///
/// `cycle_id()` 必须在写 user_id **之前**：它的作用是丢弃攻击者在登录前塞给受害者的
/// session id（session fixation），登录后继续沿用旧 id 就等于没做这件事。
pub async fn start_session(session: &Session, user_id: u64) -> Result<(), AppError> {
    session.cycle_id().await.map_err(session_error)?;
    session
        .insert(SESSION_USER_ID, user_id)
        .await
        .map_err(session_error)?;
    session
        .insert(SESSION_CSRF, new_csrf_token())
        .await
        .map_err(session_error)?;
    session
        .insert(SESSION_LOGIN_AT, Utc::now().timestamp())
        .await
        .map_err(session_error)?;
    Ok(())
}

/// 登出：整个会话作废。
///
/// 用 `flush()`（清数据 **并且** 删掉 store 里的记录）而不是 `clear()`——
/// 后者只清内存视图，store 里那条记录还在，被抓到的旧 cookie 依然能换回身份。
pub async fn end_session(session: &Session) -> Result<(), AppError> {
    session.flush().await.map_err(session_error)
}

/// 客户端 IP 提取器。
///
/// 存在的理由不是"少写两行"，而是**参数个数是有语义的**：一个 handler 同时要
/// `HeaderMap` 和 `ConnectInfo` 只为了算一个 IP，会让签名长到看不出业务意图
/// （clippy 的 `too_many_arguments` 正是在提醒这件事）。合成一个提取器之后，
/// handler 只声明"我需要客户端 IP"，不再关心它是从代理头还是 socket 来的。
///
/// 另外它让 handler **不再强制要求 `ConnectInfo` 扩展**：测试里忘了注入对端地址
/// 只会让 IP 变成 `None`，而不是整个请求 500。
pub struct ClientIp(pub Option<String>);

impl FromRequestParts<AppState> for ClientIp {
    type Rejection = AppError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let forwarded = parts.extensions.get::<ConnectInfo<SocketAddr>>().cloned();
        Ok(Self(client_ip(
            &parts.headers,
            state.config.trust_proxy,
            forwarded.as_ref(),
        )))
    }
}

/// 取客户端 IP，用于失败计数与审计。
///
/// 只有显式信任代理（`TRUST_PROXY=true`）时才读代理头。默认不读：
/// 任何人都能伪造这些头，按伪造的 IP 计数等于把限速开关交给攻击者。
///
/// 顺序是 `X-Real-IP` → `X-Forwarded-For` 的**最后一段** → socket 对端：
/// nginx 用 `$remote_addr` 覆盖 `X-Real-IP`，用 `$proxy_add_x_forwarded_for`
/// **追加** `X-Forwarded-For`，所以真实客户端在 XFF 的末尾，开头那一段是
/// 客户端自己带进来的、完全可控的值。
pub fn client_ip(
    headers: &HeaderMap,
    trust_proxy: bool,
    connect_info: Option<&ConnectInfo<SocketAddr>>,
) -> Option<String> {
    if trust_proxy {
        if let Some(value) = headers.get("x-real-ip").and_then(|v| v.to_str().ok())
            && let Some(ip) = parse_ip(value)
        {
            return Some(ip);
        }
        if let Some(value) = headers.get("x-forwarded-for").and_then(|v| v.to_str().ok())
            && let Some(last) = value.rsplit(',').next()
            && let Some(ip) = parse_ip(last)
        {
            return Some(ip);
        }
    }
    connect_info.map(|ConnectInfo(addr)| addr.ip().to_string())
}

/// 只接受**真正的 IP 文本**，不是"看起来像 IP 的字符串"。
///
/// 早先这里只检查字符集（十六进制字母 + `.` + `:`），于是 `abc`、`deadbeef`
/// 都被当成合法客户端 IP：限速键随便换，审计表的 `ip` 列也能被写成任意字符串。
/// 现在交给 `IpAddr::from_str`，并归一化（`::1` 不会写成 `0:0:0:0:0:0:0:1`）。
fn parse_ip(candidate: &str) -> Option<String> {
    let candidate = candidate.trim();
    // 45 是 IPv6 文本形式的最大长度；再长也塞不进 audit_logs.ip（VARCHAR(45)）。
    if candidate.is_empty() || candidate.len() > 45 {
        return None;
    }
    candidate
        .parse::<std::net::IpAddr>()
        .ok()
        .map(|ip| ip.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn csrf_token_is_64_hex_chars_and_never_repeats() {
        let a = new_csrf_token();
        let b = new_csrf_token();
        assert_eq!(a.len(), CSRF_TOKEN_LEN);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b, "两次签发必须是不同的令牌");
    }

    #[test]
    fn constant_time_eq_behaves_like_equality() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
        assert!(constant_time_eq(b"", b""));
    }

    #[test]
    fn real_ip_header_wins_because_nginx_overwrites_it() {
        let mut headers = HeaderMap::new();
        // 攻击者带的 XFF 在最左边；nginx 追加真实地址；X-Real-IP 是 nginx 覆盖写的。
        headers.insert("x-forwarded-for", "1.2.3.4, 203.0.113.9".parse().unwrap());
        headers.insert("x-real-ip", "203.0.113.9".parse().unwrap());
        let connect = ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 5000)));

        assert_eq!(
            client_ip(&headers, true, Some(&connect)).as_deref(),
            Some("203.0.113.9"),
            "X-Real-IP 是唯一不可伪造的来源，必须优先"
        );
    }

    #[test]
    fn forwarded_for_falls_back_to_the_last_hop_not_the_first() {
        let mut headers = HeaderMap::new();
        // 左边是客户端自己写的，右边是 nginx 追加的真实来源。
        headers.insert("x-forwarded-for", "6.6.6.6, 198.51.100.7".parse().unwrap());
        let connect = ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 5000)));

        assert_eq!(
            client_ip(&headers, true, Some(&connect)).as_deref(),
            Some("198.51.100.7"),
            "取最左边等于把限速键交给攻击者（每次换一个伪造值就能绕过锁定）"
        );
        assert_eq!(
            client_ip(&headers, false, Some(&connect)).as_deref(),
            Some("127.0.0.1"),
            "不信任代理时必须忽略代理头，直接用 socket 对端"
        );
    }

    #[test]
    fn spoofed_garbage_is_not_an_ip() {
        // 这些值字符集合法（十六进制字母），但都不是 IP；早先的字符集检查会放行。
        for junk in ["abc", "deadbeef", "not-an-ip", "1.2.3", ""] {
            let mut headers = HeaderMap::new();
            headers.insert("x-real-ip", junk.parse().unwrap());
            headers.insert("x-forwarded-for", junk.parse().unwrap());
            assert_eq!(
                client_ip(&headers, true, None),
                None,
                "{junk} 不该被当成客户端 IP"
            );
        }
    }

    #[test]
    fn overlong_values_are_dropped_before_they_reach_the_audit_table() {
        let mut headers = HeaderMap::new();
        let victim = "1".repeat(46);
        headers.insert("x-forwarded-for", victim.parse().unwrap());
        assert_eq!(
            client_ip(&headers, true, None),
            None,
            "超过 45 字符的值会撑爆 audit_logs.ip"
        );
    }

    #[test]
    fn ipv6_is_normalised() {
        let mut headers = HeaderMap::new();
        headers.insert("x-real-ip", "2001:0db8::0001".parse().unwrap());
        assert_eq!(
            client_ip(&headers, true, None).as_deref(),
            Some("2001:db8::1")
        );
    }
}
