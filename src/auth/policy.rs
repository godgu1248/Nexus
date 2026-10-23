//! **全站唯一的权限判定入口**。四个 `can_*` 都是纯内存函数：不查库、不读文件、不看时间。
//!
//! 为什么要收敛到这一层：权限判断一旦散落在 handler 里，就会出现"论坛帖子按 A 规则、
//! 知识库文档按 B 规则"的漂移，而漂移只会在某次越权之后才被发现。这里的纪律是——
//! handler 里出现 `role == Role::Admin` 这类比较，就是 bug。
//!
//! 四个函数的规则矩阵（DESIGN §18.4，每一格都有断言）：
//!
//! | 调用者 | view public | view internal | view group | write public | write internal | write group | moderate | admin |
//! |---|---|---|---|---|---|---|---|---|
//! | 游客 | ✅ | ❌ | ❌ | ❌ | ❌ | ❌ | ❌ | ❌ |
//! | `user` | ✅ | ❌ | ❌ | ❌ | ❌ | ❌ | ❌ | ❌ |
//! | `member` | ✅ | ✅ | 仅在组内 ✅ | ✅ | ✅ | 仅在组内 ✅ | ❌ | ❌ |
//! | `admin` | ✅ | ✅ | 仅在组内 ✅ | ✅ | ✅ | 仅在组内 ✅ | ✅ | ✅ |
//!
//! "仅在组内"是 M3 新增的规则（DESIGN §21.4），三条失败关闭：
//! `group_id IS NULL` 对所有人不可见（含 admin）、**admin 不因身份放行**（§19 A11）、
//! 非 active 账号即使 `group_ids` 有值也不可见；外加"自助注册的外人即便被误加进组也不可见"。

use crate::auth::model::{Role, User, UserStatus};
use crate::shared::Visibility;

/// 已登录者的最小画像。**不缓存权限结果**：它是每次请求现查现造的。
///
/// `group_ids` 现在是空 `Vec`（需求未定义小组表），但字段先留着——M3 接小组可见性时
/// 不需要改这里面任何一处签名。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CurrentUser {
    pub id: u64,
    pub role: Role,
    pub status: UserStatus,
    pub group_ids: Vec<u64>,
}

impl CurrentUser {
    /// 从数据库行构造。`group_ids` 恒空——只有真正查过小组的路径才该用
    /// [`CurrentUser::from_user_with_groups`]，否则"忘了加载"会被伪装成"不在任何组"。
    pub fn from_user(user: &User) -> Self {
        Self {
            id: user.id,
            role: user.role,
            status: user.status,
            group_ids: Vec::new(),
        }
    }

    /// 中间件用的构造方式：用户行 + 每请求查出来的组 id。
    pub fn from_user_with_groups(user: &User, group_ids: Vec<u64>) -> Self {
        Self {
            id: user.id,
            role: user.role,
            status: user.status,
            group_ids,
        }
    }

    /// 游客的占位画像：`id = 0` 不对应任何真实行，`status = Pending` 让 `effective_role`
    /// 返回 `None`，于是四个 `can_*` 全部按"未登录"处理。
    ///
    /// 为什么需要它：读页面（看空间、看议题）对游客开放，但 `model` 的签名统一收
    /// `&CurrentUser`。与其让调用方到处写 `Option<&CurrentUser>` 再各自 unwrap，
    /// 不如给"游客"一个显式表示——**任何地方用到 id 0 都是 bug**，这条容易查。
    pub fn guest() -> Self {
        Self {
            id: 0,
            role: Role::User,
            status: UserStatus::Pending,
            group_ids: Vec::new(),
        }
    }

    /// 把提取器拿到的 `Option<&CurrentUser>` 统一成"有就有，没有就是游客"。
    pub fn of(user: Option<&CurrentUser>) -> Self {
        user.cloned().unwrap_or_else(Self::guest)
    }
}

/// 被访问内容的最小画像。字段对 M1 用不满，但签名现在定死，M2/M3 直接复用。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContentCtx {
    pub visibility: Visibility,
    pub owner_id: Option<u64>,
    pub group_id: Option<u64>,
}

impl ContentCtx {
    pub fn new(visibility: Visibility, owner_id: Option<u64>, group_id: Option<u64>) -> Self {
        Self {
            visibility,
            owner_id,
            group_id,
        }
    }
}

/// 只有 `active` 的账号算"有效登录者"。其余三态一律按游客处理——
/// 这是纵深防御：中间件已经会把它们归一化成未登录，但万一某条路径漏了，
/// 这里不会再给出任何权限。
fn effective_role(user: Option<&CurrentUser>) -> Option<Role> {
    user.filter(|u| u.status.is_active()).map(|u| u.role)
}

/// 能不能看见。
pub fn can_view(user: Option<&CurrentUser>, ctx: &ContentCtx) -> bool {
    match ctx.visibility {
        // 公开区对游客开放，这是需求里"对外展示"的落点。
        Visibility::Public => true,
        Visibility::Internal => matches!(effective_role(user), Some(Role::Member | Role::Admin)),
        // 小组可见性：**必须真的在组里**。admin 也一样（A11 已裁：管理员要看得见，
        // 就把他加进组；给 admin 开后门等于让"可见性"这个字段对最高权限者失效，
        // 那样它就不是访问控制，只是对普通人的装饰）。
        Visibility::Group => in_group(user, ctx.group_id),
    }
}

/// 能不能写（新建/编辑/删除内容）。
///
/// 外人 `role=user` **不能写**，哪怕目标是 `public`：注册即获得发言权等于给垃圾内容开门。
/// M6 公开区的评论/点赞有自己的规则，不在这里放宽。
pub fn can_write(user: Option<&CurrentUser>, ctx: &ContentCtx) -> bool {
    let Some(role) = effective_role(user) else {
        return false;
    };
    match ctx.visibility {
        Visibility::Public | Visibility::Internal => matches!(role, Role::Member | Role::Admin),
        Visibility::Group => {
            matches!(role, Role::Member | Role::Admin) && in_group(user, ctx.group_id)
        }
    }
}

/// 小组可见性的唯一判据：**有效登录者 + 团队成员身份 + 真在组里**。
///
/// 三条失败关闭都在这里，调换顺序或漏掉任何一条都会造成静默放行：
/// 1. `group_id` 为 `None` → 拒绝。数据不完整时不能默认放行。
/// 2. 非 active → 拒绝。`effective_role` 已经保证，这里再挡一次是纵深防御。
/// 3. `role` 不是 member/admin（即自助注册的外人）→ 拒绝。外人被误加进组也不该看见内厅内容。
fn in_group(user: Option<&CurrentUser>, group_id: Option<u64>) -> bool {
    let (Some(user), Some(group_id)) = (user, group_id) else {
        return false;
    };
    if !user.status.is_active() {
        return false;
    }
    if !matches!(user.role, Role::Member | Role::Admin) {
        return false;
    }
    user.group_ids.contains(&group_id)
}

/// 能不能做管理动作（删他人内容、封帖、看审计）。
///
/// M1 只给 admin。M3 若要让 member 之间互审，只改这一处——所有调用点早就在用函数了。
pub fn can_moderate(user: Option<&CurrentUser>, _ctx: &ContentCtx) -> bool {
    matches!(effective_role(user), Some(Role::Admin))
}

/// 能不能进管理区。与 [`can_moderate`] 分开：将来可能出现"能审内容但不能改站点配置"的角色。
pub fn can_admin(user: Option<&CurrentUser>) -> bool {
    matches!(effective_role(user), Some(Role::Admin))
}

/// 是不是"来宾"——自助注册进来、还没被批准加入团队的外人。
///
/// 判定只看**角色**，不看能不能看见公开内容：member 也看得见公开区，但他不是来宾。
/// 与 [`effective_role`] 保持一致：pending/suspended/deleted 一律不算（那种账号连会话
/// 都拿不到，界面上不该出现"来宾"这种中性标签）。游客（None）也不算来宾——他是"没登录"。
pub fn is_guest(user: Option<&CurrentUser>) -> bool {
    matches!(effective_role(user), Some(Role::User))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 身份标签只跟角色走：游客（没登录）不是来宾，停用账号也不是。
    #[test]
    fn guest_is_about_role_not_about_being_logged_in() {
        assert!(!is_guest(None), "游客（未登录）不是来宾");
        assert!(is_guest(caller(Some(Role::User)).as_ref()), "user 就是来宾");
        assert!(
            !is_guest(caller(Some(Role::Member)).as_ref()),
            "member 不是来宾"
        );
        assert!(
            !is_guest(caller(Some(Role::Admin)).as_ref()),
            "admin 不是来宾"
        );
    }

    /// 四种调用者。`None` 是游客，其余按角色构造 active 账号。
    fn caller(role: Option<Role>) -> Option<CurrentUser> {
        role.map(|role| CurrentUser {
            id: 7,
            role,
            status: UserStatus::Active,
            group_ids: Vec::new(),
        })
    }

    fn ctx(visibility: Visibility) -> ContentCtx {
        ContentCtx::new(visibility, Some(1), Some(9))
    }

    /// 期望值表（DESIGN §18.4）。改这张表必须先改 DESIGN。
    fn expected_view(role: Option<Role>, vis: Visibility) -> bool {
        match vis {
            Visibility::Public => true,
            Visibility::Internal => matches!(role, Some(Role::Member | Role::Admin)),
            Visibility::Group => false,
        }
    }

    fn expected_write(role: Option<Role>, vis: Visibility) -> bool {
        match vis {
            Visibility::Public | Visibility::Internal => {
                matches!(role, Some(Role::Member | Role::Admin))
            }
            Visibility::Group => false,
        }
    }

    fn expected_moderate(role: Option<Role>) -> bool {
        matches!(role, Some(Role::Admin))
    }

    fn expected_admin(role: Option<Role>) -> bool {
        matches!(role, Some(Role::Admin))
    }

    /// **48 条断言**：4 类调用者 × 3 种可见性 × 4 个函数。
    #[test]
    fn matrix_is_4_callers_by_3_visibilities_by_4_functions() {
        let callers = [
            None,
            Some(Role::User),
            Some(Role::Member),
            Some(Role::Admin),
        ];
        let mut checked = 0usize;

        for role in callers {
            let owned = caller(role);
            let user = owned.as_ref();

            for vis in Visibility::ALL {
                let c = ctx(vis);

                assert_eq!(
                    can_view(user, &c),
                    expected_view(role, vis),
                    "can_view({role:?}, {vis})"
                );
                checked += 1;

                assert_eq!(
                    can_write(user, &c),
                    expected_write(role, vis),
                    "can_write({role:?}, {vis})"
                );
                checked += 1;

                assert_eq!(
                    can_moderate(user, &c),
                    expected_moderate(role),
                    "can_moderate({role:?}, {vis})"
                );
                checked += 1;

                assert_eq!(can_admin(user), expected_admin(role), "can_admin({role:?})");
                checked += 1;
            }
        }

        assert_eq!(checked, 48, "矩阵必须覆盖 48 格，实际 {checked}");
    }

    /// **12 条断言**：小组可见性的成员身份判定（§21.4）。48 + 12 = 60，见 §21.8 ③。
    ///
    /// 这里的调用者刻意包含"admin 但不在组里"——A11 裁决过 admin 不因身份放行，
    /// 如果哪天有人觉得"管理员当然什么都能看"，这条断言会立刻红。
    #[test]
    fn group_visibility_needs_real_membership_and_never_trusts_the_role() {
        let group_id = 9u64;
        let with_groups = |role: Role, groups: Vec<u64>| CurrentUser {
            id: 7,
            role,
            status: UserStatus::Active,
            group_ids: groups,
        };

        let in_group = with_groups(Role::Member, vec![group_id]);
        let in_group_admin = with_groups(Role::Admin, vec![group_id]);
        let outside = with_groups(Role::Member, vec![1, 2]);
        let outside_admin = with_groups(Role::Admin, vec![1, 2]);
        let outsider_role = with_groups(Role::User, vec![group_id]);

        let ctx = ContentCtx::new(Visibility::Group, Some(1), Some(group_id));

        let mut checked = 0usize;
        for (user, should_see, label) in [
            (Some(&in_group), true, "member 在组里"),
            (Some(&in_group_admin), true, "admin 在组里"),
            (Some(&outside), false, "member 不在组里"),
            (
                Some(&outside_admin),
                false,
                "admin 不在组里（A11 不开后门）",
            ),
            (Some(&outsider_role), false, "role=user 被误加进组也不行"),
            (None, false, "游客"),
        ] {
            assert_eq!(can_view(user, &ctx), should_see, "can_view：{label}");
            assert_eq!(can_write(user, &ctx), should_see, "can_write：{label}");
            checked += 2;
        }

        assert_eq!(checked, 12, "小组判定必须覆盖 12 格，实际 {checked}");
    }

    /// `group_id` 缺失时对所有人关闭——数据不完整的行不能变成"谁都能看"。
    #[test]
    fn group_visibility_without_group_id_is_closed_to_everyone() {
        let admin = CurrentUser {
            id: 1,
            role: Role::Admin,
            status: UserStatus::Active,
            group_ids: vec![9],
        };
        let broken = ContentCtx::new(Visibility::Group, Some(1), None);
        assert!(!can_view(Some(&admin), &broken));
        assert!(!can_write(Some(&admin), &broken));
    }

    /// 非 active 账号即使带着 group_ids 也不可见（纵深防御，别只靠中间件）。
    #[test]
    fn suspended_member_with_group_ids_still_sees_nothing() {
        for status in [
            UserStatus::Pending,
            UserStatus::Suspended,
            UserStatus::Deleted,
        ] {
            let user = CurrentUser {
                id: 7,
                role: Role::Member,
                status,
                group_ids: vec![9],
            };
            let ctx = ContentCtx::new(Visibility::Group, Some(1), Some(9));
            assert!(!can_view(Some(&user), &ctx), "{status:?} 不该看见组内容");
        }
    }

    #[test]
    fn guest_reaches_public_but_nothing_else() {
        let c = ctx(Visibility::Public);
        assert!(can_view(None, &c), "游客必须能看公开区");
        assert!(!can_write(None, &c), "游客不能写");
        assert!(!can_moderate(None, &c));
        assert!(!can_admin(None));
    }

    #[test]
    fn outsider_role_cannot_see_internal_space() {
        let owned = caller(Some(Role::User));
        let c = ctx(Visibility::Internal);
        assert!(
            !can_view(owned.as_ref(), &c),
            "外人注册即入伙是最常见的越权漏洞：role=user 不该看见内厅"
        );
    }

    #[test]
    fn group_visibility_ignores_the_role_and_follows_membership() {
        // M3 的语义：小组可见性只看"是不是这个组的人"，不看角色。
        // admin 不在组里照样看不见（A11：不给管理员开后门）。
        let ctx_group9 = ContentCtx::new(Visibility::Group, None, Some(9));

        let admin_outside = CurrentUser {
            id: 1,
            role: Role::Admin,
            status: UserStatus::Active,
            group_ids: Vec::new(),
        };
        assert!(!can_view(Some(&admin_outside), &ctx_group9));
        assert!(!can_write(Some(&admin_outside), &ctx_group9));

        let admin_inside = CurrentUser {
            group_ids: vec![9],
            ..admin_outside.clone()
        };
        assert!(
            can_view(Some(&admin_inside), &ctx_group9),
            "身份是成员关系给的，不是角色给的"
        );
        assert!(can_write(Some(&admin_inside), &ctx_group9));

        // 反过来：在组里的 user（外人）仍然不能看——小组是"成员之间的隔离"，不是提权通道。
        let outsider_inside = CurrentUser {
            id: 2,
            role: Role::User,
            status: UserStatus::Active,
            group_ids: vec![9],
        };
        assert!(!can_view(Some(&outsider_inside), &ctx_group9));
    }

    #[test]
    fn inactive_accounts_are_treated_as_guests() {
        for status in [
            UserStatus::Pending,
            UserStatus::Suspended,
            UserStatus::Deleted,
        ] {
            let user = CurrentUser {
                id: 2,
                role: Role::Admin,
                status,
                group_ids: Vec::new(),
            };
            let internal = ctx(Visibility::Internal);
            assert!(
                !can_view(Some(&user), &internal),
                "{status} 的账号即使角色是 admin 也不该有权"
            );
            assert!(!can_write(Some(&user), &internal));
            assert!(!can_moderate(Some(&user), &internal));
            assert!(!can_admin(Some(&user)));
            // 公开区仍然可见——这就是公开区的定义，不需要身份。
            assert!(can_view(Some(&user), &ctx(Visibility::Public)));
        }
    }

    #[test]
    fn current_user_from_row_drops_group_membership_for_now() {
        let user = User {
            id: 42,
            username: "alice".to_string(),
            email: "alice@example.com".to_string(),
            password_hash: "$argon2id$x".to_string(),
            display_name: "Alice".to_string(),
            theme: crate::shared::appearance::DEFAULT_THEME.to_string(),
            background: crate::shared::appearance::DEFAULT_BACKDROP.to_string(),
            font: crate::shared::appearance::DEFAULT_FONT.to_string(),
            role: Role::Member,
            status: UserStatus::Active,
            created_at: chrono::DateTime::<chrono::Utc>::MIN_UTC,
            updated_at: chrono::DateTime::<chrono::Utc>::MIN_UTC,
        };
        let current = CurrentUser::from_user(&user);
        assert_eq!(current.id, 42);
        assert_eq!(current.role, Role::Member);
        assert!(
            current.group_ids.is_empty(),
            "M1/M2 没有小组概念，group_ids 必须为空"
        );
    }
}
