//! 内容可见性。M0 只定义类型与两向转换，**不接入过滤逻辑**（M3 才接）。
//!
//! 数据库里存小写字符串（`public` / `internal` / `group`），因为需求禁止 MySQL ENUM——
//! 加一个可见性级别只应改代码与迁移，不该去 `ALTER TABLE` 重定义枚举。

use std::fmt;
use std::str::FromStr;

/// 三档可见性。
///
/// - `Public`：公开区前厅可见，游客也能看（M6 才真正对外）
/// - `Internal`：仅内部区/内厅成员可见（默认值，与 DDL 的 `DEFAULT 'internal'` 对齐）
/// - `Group`：限定小组可见。**当前一律不可见**——group_ids 恒空，见 `auth::policy`
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Visibility {
    Public,
    Internal,
    Group,
}

impl Visibility {
    pub const ALL: [Self; 3] = [Self::Public, Self::Internal, Self::Group];

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Public => "public",
            Self::Internal => "internal",
            Self::Group => "group",
        }
    }

    /// 从数据库读出来时用。**严格**：只认小写，不做 trim、不做大小写归一。
    ///
    /// 库里出现 `PUBLIC` 或 ` public` 说明写入口漏了约束，这时应当报错让人发现，
    /// 而不是悄悄兜住——兜住的结果是"两个看起来一样的值走两条不同的权限分支"。
    pub fn from_db(value: &str) -> Option<Self> {
        match value {
            "public" => Some(Self::Public),
            "internal" => Some(Self::Internal),
            "group" => Some(Self::Group),
            _ => None,
        }
    }

    pub fn is_public(self) -> bool {
        matches!(self, Self::Public)
    }

    /// 宽窄偏序：**数字越大越宽**（public 3 > internal 2 > group 1）。
    ///
    /// 用于"子内容不得比父内容更宽"这类比较。把它写成一处函数，而不是散落的
    /// `match`：三处各写一遍 `match`，迟早有一处把 group 和 internal 写反，
    /// 而那种 bug 的表现是"某个子文档意外可见"。
    pub fn rank(self) -> u8 {
        match self {
            Self::Public => 3,
            Self::Internal => 2,
            Self::Group => 1,
        }
    }
}

/// 与 DDL 的 `DEFAULT 'internal'` 保持一致：忘记设可见性时，倾向更严的一侧。
impl Default for Visibility {
    fn default() -> Self {
        Self::Internal
    }
}

impl fmt::Display for Visibility {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 未知的可见性取值。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidVisibility(pub String);

impl fmt::Display for InvalidVisibility {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "未知的可见性：{}（可选 public/internal/group）", self.0)
    }
}

impl std::error::Error for InvalidVisibility {}

/// 从**用户输入**解析时用。比 [`Visibility::from_db`] 宽一点：容忍首尾空格与大小写。
///
/// 分开两个入口是有意的——输入宽、存储严。库里那一路严，才拦得住写入口的 bug。
impl FromStr for Visibility {
    type Err = InvalidVisibility;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let normalized = s.trim().to_ascii_lowercase();
        Self::from_db(&normalized).ok_or_else(|| InvalidVisibility(s.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn as_str_round_trips_through_from_db() {
        for value in Visibility::ALL {
            let text = value.as_str();
            assert_eq!(
                Visibility::from_db(text),
                Some(value),
                "as_str 与 from_db 必须互逆：{text}"
            );
        }
    }

    #[test]
    fn from_db_is_strict() {
        assert_eq!(Visibility::from_db("PUBLIC"), None, "库里的值必须是小写");
        assert_eq!(Visibility::from_db(" public"), None, "不允许带空格");
        assert_eq!(Visibility::from_db(""), None);
        assert_eq!(Visibility::from_db("private"), None);
    }

    #[test]
    fn from_str_is_lenient_for_user_input() {
        assert_eq!("  PUBLIC  ".parse::<Visibility>(), Ok(Visibility::Public));
        assert_eq!("Internal".parse::<Visibility>(), Ok(Visibility::Internal));
        assert_eq!("group".parse::<Visibility>(), Ok(Visibility::Group));
    }

    #[test]
    fn from_str_rejects_unknown_values() {
        let err = "secret".parse::<Visibility>().expect_err("应拒绝");
        assert_eq!(err.0, "secret");
        assert!(err.to_string().contains("public/internal/group"));
    }

    #[test]
    fn default_is_the_strictest_of_the_two_common_ones() {
        assert_eq!(Visibility::default(), Visibility::Internal);
        assert!(!Visibility::default().is_public());
    }

    #[test]
    fn display_matches_as_str() {
        assert_eq!(Visibility::Group.to_string(), "group");
    }
}
