//! 审计日志入口。**全站唯一**写 `audit_logs` 的地方。
//!
//! 三条硬规则（需求原文，不是建议）：
//! 1. 只能追加，永不 UPDATE / DELETE（所以表上没有 `updated_at`）；
//! 2. 必须与业务写在**同一个 MySQL 事务**里，审计失败则业务必须一起失败；
//! 3. handler 不许直接调用这里，只有 service 层能调。
//!
//! 第 3 条是分层的锚：一个 handler 里出现 `audit::record` 就意味着有人在跳过事务边界。

use serde_json::Value;
use sqlx::{MySql, Transaction};

use crate::error::AppError;

/// 审计动作常量。**禁止**在业务代码里写裸字符串——拼错一个字母就是一条孤儿记录，
/// 而审计表是 append-only 的，写错了连修都修不了。
///
/// 这一份清单与 DESIGN §9 的表一一对应；新增动作先改 DESIGN 再加常量。
pub mod action {
    // 身份（M1）
    pub const AUTH_REGISTER: &str = "auth.register";
    pub const AUTH_LOGIN: &str = "auth.login";
    pub const AUTH_LOGOUT: &str = "auth.logout";
    pub const AUTH_LOGIN_FAILED: &str = "auth.login_failed";

    // 用户（M7）
    pub const USER_ROLE_CHANGE: &str = "user.role_change";
    pub const USER_STATUS_CHANGE: &str = "user.status_change";

    // 入组申请（M7）
    pub const JOIN_REQUEST_SUBMIT: &str = "join_request.submit";
    pub const JOIN_REQUEST_APPROVE: &str = "join_request.approve";
    pub const JOIN_REQUEST_REJECT: &str = "join_request.reject";

    // 论坛（M2）
    pub const FORUM_SPACE_CREATE: &str = "forum.space_create";
    pub const FORUM_THREAD_CREATE: &str = "forum.thread_create";
    pub const FORUM_THREAD_RESOLVE: &str = "forum.thread_resolve";
    pub const FORUM_THREAD_ARCHIVE: &str = "forum.thread_archive";
    pub const FORUM_POST_CREATE: &str = "forum.post_create";
    pub const FORUM_POST_DELETE: &str = "forum.post_delete";

    // 知识库（M4）
    pub const KB_DOC_CREATE: &str = "kb.doc_create";
    pub const KB_DOC_UPDATE: &str = "kb.doc_update";
    pub const KB_DOC_PUBLISH: &str = "kb.doc_publish";
    pub const KB_DOC_DELETE: &str = "kb.doc_delete";
    /// 置顶/取消置顶。它不改变可见性，但改变"所有人打开档案先看到什么"，
    /// 属于内容治理，必须留痕。
    pub const KB_DOC_PIN: &str = "kb.doc_pin";
    /// 发布/撤下公告。公告会出现在所有人的页面顶部，属于全员广播，必须留痕。
    pub const NOTICE_CREATE: &str = "notice.create";
    pub const NOTICE_DELETE: &str = "notice.delete";

    /// 上传一张图片。正文里出现外链图片是小事，但"谁在什么时候往站内存了一份文件"
    /// 属于存储占用与内容来源问题，留痕成本极低。
    pub const MEDIA_UPLOAD: &str = "media.upload";

    // 任务（M5）
    pub const TASK_CREATE: &str = "task.create";
    pub const TASK_ASSIGN: &str = "task.assign";
    pub const TASK_STATUS_CHANGE: &str = "task.status_change";
    /// 需求 §9 的动作表里没有"删任务"——但它和 `kb.doc_delete` 是同一个道理：
    /// 拿 status_change 冒充会让"谁删了任务"永远查不出来。
    pub const TASK_DELETE: &str = "task.delete";

    // 小组（M3）
    pub const GROUP_CREATE: &str = "group.create";
    pub const GROUP_DELETE: &str = "group.delete";
    pub const GROUP_MEMBER_ADD: &str = "group.member_add";
    pub const GROUP_MEMBER_REMOVE: &str = "group.member_remove";

    // 安全（M3）
    pub const CONTENT_VISIBILITY_CHANGE: &str = "content.visibility_change";

    // 前厅（M6）
    /// 评论发布。**点赞不记审计**——见 DESIGN §24.7：它全站最高频、当前态即行本身，
    /// 写进来只会把表撑爆并淹没真正的管理动作。
    pub const PUBLIC_COMMENT_CREATE: &str = "public.comment_create";
    pub const PUBLIC_COMMENT_DELETE: &str = "public.comment_delete";
    pub const PUBLIC_SUBMISSION_SUBMIT: &str = "public.submission_submit";
    /// 投稿被采纳（M7 管理区）。与 `submit` 分开：一个记"外人投了"，
    /// 一个记"团队收了"，两条都得能单独查到。
    pub const PUBLIC_SUBMISSION_APPROVE: &str = "public.submission_approve";
    pub const PUBLIC_SUBMISSION_REJECT: &str = "public.submission_reject";

    // 管理（M7）
    pub const ADMIN_AUDIT_VIEW: &str = "admin.audit_view";

    /// 全量清单。测试靠它保证"常量都在列宽之内且互不重复"。
    pub const ALL: &[&str] = &[
        AUTH_REGISTER,
        AUTH_LOGIN,
        AUTH_LOGOUT,
        AUTH_LOGIN_FAILED,
        USER_ROLE_CHANGE,
        USER_STATUS_CHANGE,
        JOIN_REQUEST_SUBMIT,
        JOIN_REQUEST_APPROVE,
        JOIN_REQUEST_REJECT,
        FORUM_SPACE_CREATE,
        FORUM_THREAD_CREATE,
        FORUM_THREAD_RESOLVE,
        FORUM_THREAD_ARCHIVE,
        FORUM_POST_CREATE,
        FORUM_POST_DELETE,
        KB_DOC_CREATE,
        KB_DOC_UPDATE,
        KB_DOC_PUBLISH,
        KB_DOC_DELETE,
        KB_DOC_PIN,
        NOTICE_CREATE,
        NOTICE_DELETE,
        MEDIA_UPLOAD,
        TASK_CREATE,
        TASK_ASSIGN,
        TASK_STATUS_CHANGE,
        TASK_DELETE,
        GROUP_CREATE,
        GROUP_DELETE,
        GROUP_MEMBER_ADD,
        GROUP_MEMBER_REMOVE,
        CONTENT_VISIBILITY_CHANGE,
        PUBLIC_COMMENT_CREATE,
        PUBLIC_COMMENT_DELETE,
        PUBLIC_SUBMISSION_SUBMIT,
        PUBLIC_SUBMISSION_APPROVE,
        PUBLIC_SUBMISSION_REJECT,
        ADMIN_AUDIT_VIEW,
    ];
}

/// `audit_logs.action` 列宽。
pub const MAX_ACTION_LEN: usize = 64;
/// `audit_logs.target_type` 列宽。
pub const MAX_TARGET_TYPE_LEN: usize = 32;
/// `audit_logs.ip` 列宽（IPv6 全文最长 45）。
pub const MAX_IP_LEN: usize = 45;

/// 谁、从哪来。`ip` 由 handler 从 `ConnectInfo`/可信代理头取出后传进来，
/// 允许为空（内部任务、CLI 迁移）。
#[derive(Debug, Clone, Copy, Default)]
pub struct AuditCtx<'a> {
    pub actor_id: Option<u64>,
    pub ip: Option<&'a str>,
}

impl<'a> AuditCtx<'a> {
    pub fn new(actor_id: Option<u64>, ip: Option<&'a str>) -> Self {
        Self { actor_id, ip }
    }

    /// 游客触发的动作（登录失败、注册）。
    pub fn anonymous(ip: Option<&'a str>) -> Self {
        Self { actor_id: None, ip }
    }
}

/// 写一条审计。
///
/// - `tx`：**调用方的**业务事务。不要在这里自己开事务，那样审计与业务就是两个原子域。
/// - `target`：`(target_type, target_id)`，例如 `("thread", Some(42))`。
/// - `meta`：结构化附加信息。**禁止**放密码、token、原始请求体（见 DESIGN §4）。
pub async fn record(
    tx: &mut Transaction<'_, MySql>,
    ctx: AuditCtx<'_>,
    action: &str,
    target: (&str, Option<u64>),
    meta: Option<Value>,
) -> Result<(), AppError> {
    let (target_type, target_id) = target;

    // 先做形状校验再落库。理由：这些是**代码写错**，不是用户输入出错。
    // 让它以 500 炸掉，好过 MySQL 在严格模式下截断/报错得更晚、更难定位。
    check_shape(action, target_type, ctx.ip)?;

    // 为什么把 JSON 序列化成字符串再绑：sqlx 的 `serde_json::Value` 绑定需要额外开
    // `json` feature，而 MySQL 的 JSON 列本来就能直接吃合法 JSON 文本，没必要为此扩 feature。
    let meta_json = match meta {
        Some(value) => Some(serde_json::to_string(&value).map_err(AppError::internal)?),
        None => None,
    };

    sqlx::query(
        "INSERT INTO audit_logs (actor_id, action, target_type, target_id, meta, ip) \
         VALUES (?, ?, ?, ?, ?, ?)",
    )
    .bind(ctx.actor_id)
    .bind(action)
    .bind(target_type)
    .bind(target_id)
    .bind(meta_json)
    .bind(ctx.ip)
    .execute(&mut **tx)
    .await
    .map_err(AppError::Database)?;

    Ok(())
}

/// 领域事件挂载点。M0 是空实现，只留一个 debug 日志。
///
/// 存在的意义：将来要接 webhook / 通知 / 统计时，加在这里，而不是往每个 service 里
/// 塞一份副作用代码。**只允许 service 层调用。**
pub fn on_event(action: &str, target_type: &str, target_id: Option<u64>) {
    tracing::debug!(action, target_type, target_id, "审计事件（M0 空实现）");
}

/// 纯内存的形状校验，不碰数据库，所以可以直接单测。
fn check_shape(action: &str, target_type: &str, ip: Option<&str>) -> Result<(), AppError> {
    if action.is_empty() || action.len() > MAX_ACTION_LEN {
        return Err(AppError::internal(anyhow::anyhow!(
            "审计 action 长度非法（{} 字节，上限 {MAX_ACTION_LEN}）：{action:?}",
            action.len()
        )));
    }
    if target_type.is_empty() || target_type.len() > MAX_TARGET_TYPE_LEN {
        return Err(AppError::internal(anyhow::anyhow!(
            "审计 target_type 长度非法（{} 字节，上限 {MAX_TARGET_TYPE_LEN}）：{target_type:?}",
            target_type.len()
        )));
    }
    if let Some(ip) = ip
        && ip.len() > MAX_IP_LEN
    {
        return Err(AppError::internal(anyhow::anyhow!(
            "审计 ip 长度非法（{} 字节，上限 {MAX_IP_LEN}）",
            ip.len()
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_action_constant_fits_the_column_and_passes_shape_check() {
        for action in action::ALL {
            assert!(
                action.len() <= MAX_ACTION_LEN,
                "action 超列宽：{action} ({} 字节)",
                action.len()
            );
            check_shape(action, "thread", None)
                .unwrap_or_else(|err| panic!("action 形状校验失败：{action} / {err:?}"));
        }
    }

    #[test]
    fn action_constants_are_unique() {
        let mut seen = std::collections::HashSet::new();
        for action in action::ALL {
            assert!(seen.insert(*action), "审计 action 重复：{action}");
        }
        assert_eq!(seen.len(), action::ALL.len());
    }

    #[test]
    fn action_constants_use_dotted_namespace() {
        for action in action::ALL {
            let (ns, rest) = action.split_once('.').expect("action 必须带命名空间");
            assert!(
                !ns.is_empty() && !rest.is_empty(),
                "命名空间格式不对：{action}"
            );
            assert!(
                !rest.contains('.'),
                "action 只允许一段点号（namespace.verb）：{action}"
            );
        }
    }

    #[test]
    fn shape_check_rejects_overlong_values() {
        let long_action = "a".repeat(MAX_ACTION_LEN + 1);
        assert!(check_shape(&long_action, "thread", None).is_err());

        let long_target = "t".repeat(MAX_TARGET_TYPE_LEN + 1);
        assert!(check_shape("auth.login", &long_target, None).is_err());

        let long_ip = "1".repeat(MAX_IP_LEN + 1);
        assert!(check_shape("auth.login", "user", Some(&long_ip)).is_err());
    }

    #[test]
    fn shape_check_rejects_empty_values() {
        assert!(check_shape("", "user", None).is_err(), "空 action 必须炸");
        assert!(
            check_shape("auth.login", "", None).is_err(),
            "空 target_type 必须炸"
        );
    }

    #[test]
    fn shape_check_accepts_boundary_values() {
        let action = "a".repeat(MAX_ACTION_LEN);
        let target = "t".repeat(MAX_TARGET_TYPE_LEN);
        let ip = "1".repeat(MAX_IP_LEN);
        assert!(
            check_shape(&action, &target, Some(&ip)).is_ok(),
            "边界值应通过"
        );
        assert!(
            check_shape("auth.login", "user", None).is_ok(),
            "ip 允许缺失（内部任务）"
        );
    }

    #[test]
    fn context_helpers_do_not_pretend_to_know_the_actor() {
        let anonymous = AuditCtx::anonymous(Some("203.0.113.7"));
        assert_eq!(anonymous.actor_id, None, "游客不能有 actor_id");
        assert_eq!(anonymous.ip, Some("203.0.113.7"));

        let system = AuditCtx::default();
        assert_eq!(system.actor_id, None);
        assert_eq!(system.ip, None);
    }

    #[test]
    fn on_event_is_callable_without_a_database() {
        // M0 是空实现，只要不 panic 就够。它存在的意义是给 M1+ 留挂载点。
        on_event(action::AUTH_LOGIN, "user", Some(1));
        on_event(action::AUTH_LOGIN, "user", None);
    }
}
