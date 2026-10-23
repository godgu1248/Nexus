//! 配置：全部来自环境变量，缺失/非法即拒绝启动。
//!
//! 设计要点：
//! - `from_source` 接收一个查找闭包，不直接读全局环境变量。这样单元测试可以并行跑完 13 项
//!   配置的边界，不需要 `unsafe { std::env::set_var }`（Rust 2024 里它是 unsafe）。
//! - 刻意**不做**配置项的：argon2 参数（可配置=可被人为调弱）、分页 20（DESIGN §7 冻结）。

use std::net::SocketAddr;
use std::str::FromStr;

use chrono::FixedOffset;
use thiserror::Error;

use crate::shared::validate::{PASSWORD_MIN, USERNAME_MAX, USERNAME_MIN};

/// 连接池硬上限。需求明令"max_connections(5) 禁调大"，所以配得再大也只用到 5，并告警。
pub const DB_MAX_CONNECTIONS_CAP: u32 = 5;

pub const DEFAULT_BIND_ADDR: &str = "127.0.0.1:8080";
pub const DEFAULT_SESSION_COOKIE_NAME: &str = "nexus_session";
pub const DEFAULT_SESSION_TTL_HOURS: i64 = 12;
pub const DEFAULT_STATIC_BASE_URL: &str = "/static";
/// 上传目录的默认值：开发环境就在仓库里，生产由 `/etc/nexus/nexus.env` 指到 /var/lib/nexus。
pub const DEFAULT_UPLOAD_DIR: &str = "./uploads";
/// 上传文件的对外 URL 前缀。和 STATIC_BASE_URL 一样由反向代理直出，不经 Rust。
pub const DEFAULT_UPLOAD_BASE_URL: &str = "/uploads";
pub const DEFAULT_REQUEST_BODY_LIMIT: usize = 1024 * 1024;
pub const DEFAULT_DB_MAX_CONNECTIONS: u32 = 5;
pub const DEFAULT_LOG_LEVEL: &str = "info";
pub const DEFAULT_DISPLAY_TZ_OFFSET: &str = "+08:00";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppEnv {
    Dev,
    Prod,
}

impl AppEnv {
    fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "dev" | "development" => Some(AppEnv::Dev),
            "prod" | "production" => Some(AppEnv::Prod),
            _ => None,
        }
    }

    pub fn is_prod(self) -> bool {
        matches!(self, AppEnv::Prod)
    }
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("缺少必需的环境变量 {0}")]
    Missing(&'static str),
    #[error("环境变量 {key} 的值 {value:?} 非法：{reason}")]
    Invalid {
        key: &'static str,
        value: String,
        reason: &'static str,
    },
}

#[derive(Debug, Clone)]
pub struct Config {
    pub database_url: String,
    pub bind_addr: SocketAddr,
    pub app_env: AppEnv,
    pub app_base_url: String,
    pub session_cookie_name: String,
    pub session_secure: bool,
    pub session_ttl_hours: i64,
    pub static_base_url: String,
    /// 图片落盘目录（绝对或相对进程工作目录）。启动时确保存在。
    pub upload_dir: String,
    /// 图片的对外前缀，写进正文 HTML 的 `src`。反向代理把这个前缀映射到 upload_dir。
    pub upload_base_url: String,
    pub request_body_limit: usize,
    pub db_max_connections: u32,
    pub log_level: String,
    pub display_tz: FixedOffset,
    pub trust_proxy: bool,
    /// 首个管理员引导。两者要么都设、要么都不设；只在库中一个 admin 都没有时才生效。
    ///
    /// 为什么需要它：M1 之后没有任何入口能造出 admin（M7 才有审批界面），
    /// 而没有 admin 就没人能审批成员、进不了管理模块——这是一个死锁。
    /// 放在环境变量里而不是 migration 里：密码进 SQL 文件就等于进了 git 历史。
    pub bootstrap_admin_username: Option<String>,
    pub bootstrap_admin_password: Option<String>,
}

impl Config {
    /// 生产入口：从进程环境变量读取。
    pub fn from_env() -> Result<Self, ConfigError> {
        Self::from_source(|key| std::env::var(key).ok())
    }

    /// 可测试核心：`lookup` 返回 `None` 表示该变量未设置。
    pub fn from_source<F>(lookup: F) -> Result<Self, ConfigError>
    where
        F: Fn(&str) -> Option<String>,
    {
        let app_env = match lookup("APP_ENV") {
            // 未设置时默认 dev；生产必须显式声明，避免"忘了设就是生产"这种反向事故。
            None => AppEnv::Dev,
            Some(raw) => AppEnv::parse(&raw).ok_or_else(|| ConfigError::Invalid {
                key: "APP_ENV",
                value: raw.clone(),
                reason: "只接受 dev 或 prod",
            })?,
        };

        let database_url = required(&lookup, "DATABASE_URL")?;
        if !database_url.starts_with("mysql://") {
            return Err(ConfigError::Invalid {
                key: "DATABASE_URL",
                value: mask_url(&database_url),
                reason: "必须形如 mysql://user:pass@host:port/db",
            });
        }

        // TLS 硬约束：MySQL 8 起默认认证插件是 caching_sha2_password，明文连接下要求 RSA
        // 公钥交换；而 sqlx 的 mysql-rsa 特性会带进预发布依赖（见 DESIGN §2.6）。所以连接
        // 必须加密。prod 缺了就拒绝启动——宁可启动失败，也不要带着明文连接上线。
        let ssl_mode_raw = query_param(&database_url, &["ssl-mode", "sslmode"]);
        let ssl_mode = ssl_mode_raw.as_deref().map(str::to_ascii_lowercase);
        let encrypted = matches!(
            ssl_mode.as_deref(),
            Some("required" | "verify_ca" | "verify_identity")
        );
        if !encrypted {
            if app_env.is_prod() {
                return Err(ConfigError::Invalid {
                    key: "DATABASE_URL",
                    value: mask_url(&database_url),
                    reason: "生产环境必须带 ssl-mode=required|verify_ca|verify_identity",
                });
            }
            tracing::warn!(
                ssl_mode = ssl_mode_raw.as_deref().unwrap_or("<未设置>"),
                "DATABASE_URL 未启用 TLS：本地开发可以，但生产环境会拒绝启动"
            );
        }

        let bind_addr = match lookup("BIND_ADDR") {
            None => SocketAddr::from_str(DEFAULT_BIND_ADDR).expect("默认绑定地址必须可解析"),
            Some(raw) => SocketAddr::from_str(raw.trim()).map_err(|_| ConfigError::Invalid {
                key: "BIND_ADDR",
                value: raw.clone(),
                reason: "必须是 host:port 形式",
            })?,
        };

        // 对外基址：去掉尾部斜杠，保证后续拼接不会出现 `//`。
        let app_base_url = required(&lookup, "APP_BASE_URL")?;
        let app_base_url = app_base_url.trim().trim_end_matches('/').to_string();
        if !(app_base_url.starts_with("http://") || app_base_url.starts_with("https://")) {
            return Err(ConfigError::Invalid {
                key: "APP_BASE_URL",
                value: app_base_url,
                reason: "必须以 http:// 或 https:// 开头",
            });
        }

        let session_cookie_name = lookup("SESSION_COOKIE_NAME")
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| DEFAULT_SESSION_COOKIE_NAME.to_string());
        if !session_cookie_name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.')
        {
            return Err(ConfigError::Invalid {
                key: "SESSION_COOKIE_NAME",
                value: session_cookie_name,
                reason: "只能包含字母、数字与 _ - .",
            });
        }

        // 安全默认：生产环境强制 Secure，无论有没有显式写 false。
        let session_secure = match lookup("SESSION_SECURE") {
            Some(raw) => {
                let explicit = parse_bool("SESSION_SECURE", &raw)?;
                if app_env.is_prod() && !explicit {
                    tracing::warn!("生产环境 SESSION_SECURE=false 被忽略，强制启用 Secure cookie");
                }
                explicit || app_env.is_prod()
            }
            None => app_env.is_prod(),
        };

        let session_ttl_hours = match lookup("SESSION_TTL_HOURS") {
            None => DEFAULT_SESSION_TTL_HOURS,
            Some(raw) => {
                let n = parse_i64("SESSION_TTL_HOURS", &raw)?;
                if !(1..=24 * 30).contains(&n) {
                    return Err(ConfigError::Invalid {
                        key: "SESSION_TTL_HOURS",
                        value: raw,
                        reason: "必须在 1 到 720 之间",
                    });
                }
                n
            }
        };

        let static_base_url = normalize_static_base(
            lookup("STATIC_BASE_URL").unwrap_or_else(|| DEFAULT_STATIC_BASE_URL.to_string()),
        );

        // 上传目录为空串等于"没配"，回落默认值：空路径会让 create_dir_all 报错，
        // 而报错发生在启动期才最容易被误读成"权限问题"。
        let upload_dir = lookup("UPLOAD_DIR")
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| DEFAULT_UPLOAD_DIR.to_string());

        let upload_base_url = normalize_static_base(
            lookup("UPLOAD_BASE_URL").unwrap_or_else(|| DEFAULT_UPLOAD_BASE_URL.to_string()),
        );

        let request_body_limit = match lookup("REQUEST_BODY_LIMIT") {
            None => DEFAULT_REQUEST_BODY_LIMIT,
            Some(raw) => {
                let n = parse_i64("REQUEST_BODY_LIMIT", &raw)?;
                if !(1024..=64 * 1024 * 1024).contains(&n) {
                    return Err(ConfigError::Invalid {
                        key: "REQUEST_BODY_LIMIT",
                        value: raw,
                        reason: "必须在 1024 到 67108864 之间（字节）",
                    });
                }
                n as usize
            }
        };

        let db_max_connections = match lookup("DB_MAX_CONNECTIONS") {
            None => DEFAULT_DB_MAX_CONNECTIONS,
            Some(raw) => {
                let n = parse_i64("DB_MAX_CONNECTIONS", &raw)?;
                if n < 1 {
                    return Err(ConfigError::Invalid {
                        key: "DB_MAX_CONNECTIONS",
                        value: raw,
                        reason: "至少为 1",
                    });
                }
                let n = n as u32;
                if n > DB_MAX_CONNECTIONS_CAP {
                    tracing::warn!(
                        requested = n,
                        cap = DB_MAX_CONNECTIONS_CAP,
                        "DB_MAX_CONNECTIONS 超过硬上限，已收敛到 5（2核2G 上盲目加连接只会把内存耗光）"
                    );
                    DB_MAX_CONNECTIONS_CAP
                } else {
                    n
                }
            }
        };

        let log_level = lookup("LOG_LEVEL")
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| DEFAULT_LOG_LEVEL.to_string());

        let display_tz = match lookup("DISPLAY_TZ_OFFSET") {
            None => parse_tz(DEFAULT_DISPLAY_TZ_OFFSET).expect("默认时区必须合法"),
            Some(raw) => parse_tz(&raw).map_err(|reason| ConfigError::Invalid {
                key: "DISPLAY_TZ_OFFSET",
                value: raw,
                reason,
            })?,
        };

        let trust_proxy = match lookup("TRUST_PROXY") {
            None => false,
            Some(raw) => parse_bool("TRUST_PROXY", &raw)?,
        };

        // 引导管理员：成对出现，缺失一半直接拒绝启动。
        // 只设一半是最容易踩的坑——服务照常起来、日志里一行"引导已跳过"，
        // 然后你在 M7 发现没人能审批，回头翻三天前的启动日志。
        let (bootstrap_admin_username, bootstrap_admin_password) = match (
            lookup("BOOTSTRAP_ADMIN_USERNAME"),
            lookup("BOOTSTRAP_ADMIN_PASSWORD"),
        ) {
            (None, None) => (None, None),
            (Some(_), None) => {
                return Err(ConfigError::Invalid {
                    key: "BOOTSTRAP_ADMIN_PASSWORD",
                    value: String::new(),
                    reason: "设置了 BOOTSTRAP_ADMIN_USERNAME 就必须同时设置它",
                });
            }
            (None, Some(_)) => {
                return Err(ConfigError::Invalid {
                    key: "BOOTSTRAP_ADMIN_USERNAME",
                    value: String::new(),
                    reason: "设置了 BOOTSTRAP_ADMIN_PASSWORD 就必须同时设置它",
                });
            }
            (Some(username), Some(password)) => {
                let username = username.trim().to_string();
                if username.chars().count() < USERNAME_MIN
                    || username.chars().count() > USERNAME_MAX
                {
                    return Err(ConfigError::Invalid {
                        key: "BOOTSTRAP_ADMIN_USERNAME",
                        value: username,
                        reason: "长度必须在 3 到 32 个字符之间",
                    });
                }
                if !username
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
                {
                    return Err(ConfigError::Invalid {
                        key: "BOOTSTRAP_ADMIN_USERNAME",
                        value: username,
                        reason: "只能包含字母、数字与 _ -",
                    });
                }
                let password = password.trim().to_string();
                if password.chars().count() < PASSWORD_MIN {
                    return Err(ConfigError::Invalid {
                        key: "BOOTSTRAP_ADMIN_PASSWORD",
                        value: "***".to_string(),
                        reason: "密码至少 8 个字符",
                    });
                }
                (Some(username), Some(password))
            }
        };

        Ok(Config {
            database_url,
            bind_addr,
            app_env,
            app_base_url,
            session_cookie_name,
            session_secure,
            session_ttl_hours,
            static_base_url,
            upload_dir,
            upload_base_url,
            request_body_limit,
            db_max_connections,
            log_level,
            display_tz,
            trust_proxy,
            bootstrap_admin_username,
            bootstrap_admin_password,
        })
    }

    pub fn is_prod(&self) -> bool {
        self.app_env.is_prod()
    }
}

fn required<F>(lookup: &F, key: &'static str) -> Result<String, ConfigError>
where
    F: Fn(&str) -> Option<String>,
{
    lookup(key)
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .ok_or(ConfigError::Missing(key))
}

fn parse_bool(key: &'static str, raw: &str) -> Result<bool, ConfigError> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Ok(true),
        "0" | "false" | "no" | "off" => Ok(false),
        _ => Err(ConfigError::Invalid {
            key,
            value: raw.to_string(),
            reason: "只接受 true/false/1/0/yes/no/on/off",
        }),
    }
}

fn parse_i64(key: &'static str, raw: &str) -> Result<i64, ConfigError> {
    raw.trim().parse::<i64>().map_err(|_| ConfigError::Invalid {
        key,
        value: raw.to_string(),
        reason: "必须是整数",
    })
}

/// 静态资源前缀：强制以 `/` 开头、不以 `/` 结尾，空值回落到默认。
fn normalize_static_base(raw: String) -> String {
    let trimmed = raw.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return DEFAULT_STATIC_BASE_URL.to_string();
    }
    if trimmed.starts_with('/') || trimmed.starts_with("http://") || trimmed.starts_with("https://")
    {
        trimmed.to_string()
    } else {
        format!("/{trimmed}")
    }
}

/// 解析展示时区。只接受 `Z` / `UTC` / `±HH:MM` / `±HHMM`，拒绝 `Local` 这类依赖宿主的状态。
fn parse_tz(raw: &str) -> Result<FixedOffset, &'static str> {
    let t = raw.trim();
    if t.is_empty() {
        return Err("不能为空");
    }
    if t.eq_ignore_ascii_case("z") || t.eq_ignore_ascii_case("utc") {
        return FixedOffset::east_opt(0).ok_or("非法的时区偏移");
    }
    let bytes = t.as_bytes();
    let sign: i32 = match bytes[0] {
        b'+' => 1,
        b'-' => -1,
        _ => return Err("必须以 + 或 - 开头（或写 Z / UTC）"),
    };
    let rest = &t[1..];
    let (hour_str, minute_str) = match rest.split_once(':') {
        Some((h, m)) => (h, m),
        None if rest.len() == 4 => (&rest[..2], &rest[2..]),
        None => return Err("格式应为 ±HH:MM"),
    };
    let hour: i32 = hour_str.parse().map_err(|_| "小时部分不是数字")?;
    let minute: i32 = minute_str.parse().map_err(|_| "分钟部分不是数字")?;
    if !(0..=14).contains(&hour) || !(0..=59).contains(&minute) {
        return Err("偏移超出范围（小时 0-14，分钟 0-59）");
    }
    FixedOffset::east_opt(sign * (hour * 3600 + minute * 60)).ok_or("非法的时区偏移")
}

/// 从 URL 查询串里取参数原值。本项目只给 `ssl-mode` 用，值都是纯 ASCII，
/// 所以不做百分号解码——省一个依赖，也省一类解码 bug。
fn query_param(url: &str, keys: &[&str]) -> Option<String> {
    let query = url.split_once('?')?.1;
    query.split('&').find_map(|pair| {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        keys.iter()
            .any(|want| key.eq_ignore_ascii_case(want))
            .then(|| value.to_string())
    })
}

/// 报错时脱敏数据库口令：`mysql://u:secret@h/db` → `mysql://u:***@h/db`。
fn mask_url(url: &str) -> String {
    match (url.find("://"), url.rfind('@')) {
        (Some(scheme_end), Some(at)) if at > scheme_end => {
            let head = &url[..scheme_end + 3];
            let tail = &url[at..];
            let creds = &url[scheme_end + 3..at];
            let user = creds.split(':').next().unwrap_or("");
            format!("{head}{user}:***{tail}")
        }
        _ => url.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn cfg(pairs: &[(&str, &str)]) -> Result<Config, ConfigError> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        Config::from_source(move |key| map.get(key).cloned())
    }

    fn base() -> Vec<(&'static str, &'static str)> {
        vec![
            (
                "DATABASE_URL",
                "mysql://nexus:pw@127.0.0.1:3306/nexus?ssl-mode=verify_ca&ssl-ca=/tmp/ca.pem",
            ),
            ("APP_BASE_URL", "http://localhost:8080"),
        ]
    }

    #[test]
    fn missing_database_url_rejects_startup() {
        let err = cfg(&[("APP_BASE_URL", "http://localhost:8080")]).unwrap_err();
        assert!(matches!(err, ConfigError::Missing("DATABASE_URL")));
    }

    #[test]
    fn missing_app_base_url_rejects_startup() {
        let err = cfg(&[("DATABASE_URL", "mysql://nexus:pw@127.0.0.1:3306/nexus")]).unwrap_err();
        assert!(matches!(err, ConfigError::Missing("APP_BASE_URL")));
    }

    #[test]
    fn defaults_apply_when_only_two_required_are_set() {
        let c = cfg(&base()).unwrap();
        assert_eq!(c.app_env, AppEnv::Dev);
        assert_eq!(c.bind_addr.to_string(), DEFAULT_BIND_ADDR);
        assert_eq!(c.session_cookie_name, DEFAULT_SESSION_COOKIE_NAME);
        assert!(!c.session_secure, "dev 默认不强制 Secure");
        assert_eq!(c.session_ttl_hours, DEFAULT_SESSION_TTL_HOURS);
        assert_eq!(c.static_base_url, DEFAULT_STATIC_BASE_URL);
        assert_eq!(c.request_body_limit, DEFAULT_REQUEST_BODY_LIMIT);
        assert_eq!(c.db_max_connections, DEFAULT_DB_MAX_CONNECTIONS);
        assert_eq!(c.log_level, DEFAULT_LOG_LEVEL);
        assert_eq!(c.display_tz.local_minus_utc(), 8 * 3600);
        assert!(!c.trust_proxy);
    }

    #[test]
    fn db_max_connections_is_clamped_to_five() {
        let mut v = base();
        v.push(("DB_MAX_CONNECTIONS", "50"));
        assert_eq!(cfg(&v).unwrap().db_max_connections, 5);
    }

    #[test]
    fn db_max_connections_zero_is_rejected() {
        let mut v = base();
        v.push(("DB_MAX_CONNECTIONS", "0"));
        assert!(matches!(
            cfg(&v).unwrap_err(),
            ConfigError::Invalid {
                key: "DB_MAX_CONNECTIONS",
                ..
            }
        ));
    }

    #[test]
    fn prod_forces_secure_cookie_even_when_explicitly_false() {
        let mut v = base();
        v.push(("APP_ENV", "prod"));
        v.push(("SESSION_SECURE", "false"));
        assert!(cfg(&v).unwrap().session_secure);
    }

    #[test]
    fn dev_respects_explicit_true() {
        let mut v = base();
        v.push(("SESSION_SECURE", "true"));
        assert!(cfg(&v).unwrap().session_secure);
    }

    #[test]
    fn prod_without_session_secure_defaults_to_true() {
        let mut v = base();
        v.push(("APP_ENV", "prod"));
        assert!(cfg(&v).unwrap().session_secure);
    }

    #[test]
    fn invalid_app_env_is_rejected() {
        let mut v = base();
        v.push(("APP_ENV", "staging"));
        assert!(matches!(
            cfg(&v).unwrap_err(),
            ConfigError::Invalid { key: "APP_ENV", .. }
        ));
    }

    #[test]
    fn database_url_must_be_mysql_and_is_masked_in_errors() {
        let mut v = base();
        v[0] = (
            "DATABASE_URL",
            "postgres://nexus:topsecret@127.0.0.1:5432/nexus",
        );
        let msg = cfg(&v).unwrap_err().to_string();
        assert!(msg.contains("mysql://"));
        assert!(!msg.contains("topsecret"), "错误信息不得泄露口令：{msg}");
    }

    #[test]
    fn prod_requires_tls_mode_in_database_url() {
        let pairs = [
            (
                "DATABASE_URL",
                "mysql://nexus:topsecret@127.0.0.1:3306/nexus",
            ),
            ("APP_BASE_URL", "http://localhost:8080"),
            ("APP_ENV", "prod"),
        ];
        let err = cfg(&pairs).unwrap_err();
        assert!(
            matches!(
                err,
                ConfigError::Invalid {
                    key: "DATABASE_URL",
                    ..
                }
            ),
            "应指向 DATABASE_URL：{err}"
        );
        let msg = err.to_string();
        assert!(msg.contains("ssl-mode"), "报错要说清缺什么：{msg}");
        assert!(!msg.contains("topsecret"), "错误信息不得泄露口令：{msg}");
    }

    #[test]
    fn prod_accepts_every_encrypting_ssl_mode() {
        for mode in ["required", "verify_ca", "verify_identity"] {
            let url = format!("mysql://nexus:pw@127.0.0.1:3306/nexus?ssl-mode={mode}");
            let pairs = [
                ("DATABASE_URL", url.as_str()),
                ("APP_BASE_URL", "http://localhost:8080"),
                ("APP_ENV", "prod"),
            ];
            assert!(cfg(&pairs).is_ok(), "{mode} 是加密模式，生产应接受");
        }
    }

    #[test]
    fn prod_rejects_plaintext_ssl_mode() {
        for mode in ["disabled", "preferred"] {
            let url = format!("mysql://nexus:pw@127.0.0.1:3306/nexus?ssl-mode={mode}");
            let pairs = [
                ("DATABASE_URL", url.as_str()),
                ("APP_BASE_URL", "http://localhost:8080"),
                ("APP_ENV", "prod"),
            ];
            assert!(cfg(&pairs).is_err(), "{mode} 不保证加密，生产必须拒绝");
        }
    }

    #[test]
    fn dev_only_warns_when_tls_mode_is_missing() {
        let pairs = [
            ("DATABASE_URL", "mysql://nexus:pw@127.0.0.1:3306/nexus"),
            ("APP_BASE_URL", "http://localhost:8080"),
        ];
        assert!(cfg(&pairs).is_ok(), "dev 不因缺 ssl-mode 而拒绝启动");
    }

    #[test]
    fn query_param_matches_both_spellings_case_insensitively() {
        assert_eq!(
            query_param("mysql://h/db?ssl-mode=verify_ca", &["ssl-mode"]).as_deref(),
            Some("verify_ca")
        );
        assert_eq!(
            query_param(
                "mysql://h/db?sslmode=required&x=1",
                &["ssl-mode", "sslmode"]
            )
            .as_deref(),
            Some("required")
        );
        assert_eq!(
            query_param("mysql://h/db?SSL-MODE=Required", &["ssl-mode"]).as_deref(),
            Some("Required")
        );
        assert_eq!(query_param("mysql://h/db", &["ssl-mode"]), None);
    }

    #[test]
    fn display_tz_accepts_z_and_offsets_and_rejects_garbage() {
        let mut v = base();
        v.push(("DISPLAY_TZ_OFFSET", "Z"));
        assert_eq!(cfg(&v).unwrap().display_tz.local_minus_utc(), 0);

        let mut v = base();
        v.push(("DISPLAY_TZ_OFFSET", "-05:30"));
        assert_eq!(
            cfg(&v).unwrap().display_tz.local_minus_utc(),
            -(5 * 3600 + 30 * 60)
        );

        let mut v = base();
        v.push(("DISPLAY_TZ_OFFSET", "+0800"));
        assert_eq!(cfg(&v).unwrap().display_tz.local_minus_utc(), 8 * 3600);

        for bad in ["Local", "8", "+15:00", "+08:60", "0800", ""] {
            let mut v = base();
            v.push(("DISPLAY_TZ_OFFSET", bad));
            assert!(
                matches!(
                    cfg(&v).unwrap_err(),
                    ConfigError::Invalid {
                        key: "DISPLAY_TZ_OFFSET",
                        ..
                    }
                ),
                "{bad} 应被拒绝"
            );
        }
    }

    #[test]
    fn static_base_url_is_normalized() {
        let mut v = base();
        v.push(("STATIC_BASE_URL", "static/"));
        assert_eq!(cfg(&v).unwrap().static_base_url, "/static");

        let mut v = base();
        v.push(("STATIC_BASE_URL", "https://cdn.example.com/assets/"));
        assert_eq!(
            cfg(&v).unwrap().static_base_url,
            "https://cdn.example.com/assets"
        );

        let mut v = base();
        v.push(("STATIC_BASE_URL", "   "));
        assert_eq!(cfg(&v).unwrap().static_base_url, DEFAULT_STATIC_BASE_URL);
    }

    #[test]
    fn request_body_limit_bounds() {
        let mut v = base();
        v.push(("REQUEST_BODY_LIMIT", "512"));
        assert!(cfg(&v).is_err(), "小于 1KB 的限额是配置错误");

        let mut v = base();
        v.push(("REQUEST_BODY_LIMIT", "2097152"));
        assert_eq!(cfg(&v).unwrap().request_body_limit, 2 * 1024 * 1024);
    }

    #[test]
    fn session_ttl_bounds() {
        let mut v = base();
        v.push(("SESSION_TTL_HOURS", "0"));
        assert!(cfg(&v).is_err());

        let mut v = base();
        v.push(("SESSION_TTL_HOURS", "721"));
        assert!(cfg(&v).is_err());
    }

    #[test]
    fn cookie_name_rejects_separators() {
        let mut v = base();
        v.push(("SESSION_COOKIE_NAME", "bad;name"));
        assert!(cfg(&v).is_err());
    }

    #[test]
    fn bootstrap_admin_is_absent_by_default() {
        let c = cfg(&base()).unwrap();
        assert!(c.bootstrap_admin_username.is_none());
        assert!(c.bootstrap_admin_password.is_none());
    }

    #[test]
    fn bootstrap_admin_accepts_a_complete_pair() {
        let mut v = base();
        v.push(("BOOTSTRAP_ADMIN_USERNAME", "a_lan"));
        v.push(("BOOTSTRAP_ADMIN_PASSWORD", "a-long-enough-password"));
        let c = cfg(&v).unwrap();
        assert_eq!(c.bootstrap_admin_username.as_deref(), Some("a_lan"));
        assert_eq!(
            c.bootstrap_admin_password.as_deref(),
            Some("a-long-enough-password")
        );
    }

    #[test]
    fn half_a_bootstrap_pair_rejects_startup() {
        // 只设一半是最阴的配置事故：服务照常起来，M7 才发现没人能审批。
        let mut v = base();
        v.push(("BOOTSTRAP_ADMIN_USERNAME", "a_lan"));
        let err = cfg(&v).unwrap_err();
        assert!(matches!(
            err,
            ConfigError::Invalid {
                key: "BOOTSTRAP_ADMIN_PASSWORD",
                ..
            }
        ));

        let mut v = base();
        v.push(("BOOTSTRAP_ADMIN_PASSWORD", "a-long-enough-password"));
        let err = cfg(&v).unwrap_err();
        assert!(matches!(
            err,
            ConfigError::Invalid {
                key: "BOOTSTRAP_ADMIN_USERNAME",
                ..
            }
        ));
    }

    #[test]
    fn bootstrap_admin_password_must_meet_the_same_minimum_as_registration() {
        let mut v = base();
        v.push(("BOOTSTRAP_ADMIN_USERNAME", "a_lan"));
        v.push(("BOOTSTRAP_ADMIN_PASSWORD", "short"));
        assert!(cfg(&v).is_err(), "引导密码不能比注册密码更弱");

        // 报错里不能出现密码原文，哪怕是引导用的。
        // 注意这里要真的短于 8 位，否则配置是合法的，压根走不到报错分支。
        let mut v = base();
        v.push(("BOOTSTRAP_ADMIN_USERNAME", "a_lan"));
        v.push(("BOOTSTRAP_ADMIN_PASSWORD", "s3cr3t!"));
        let err = cfg(&v).unwrap_err().to_string();
        assert!(!err.contains("s3cr3t!"), "错误信息泄漏了密码：{err}");
    }

    #[test]
    fn bootstrap_admin_username_rejects_bad_characters() {
        let mut v = base();
        v.push(("BOOTSTRAP_ADMIN_USERNAME", "a lan!"));
        v.push(("BOOTSTRAP_ADMIN_PASSWORD", "a-long-enough-password"));
        assert!(cfg(&v).is_err());
    }
}
