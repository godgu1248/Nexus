//! 字段级校验。返回的错误是**给人看的**：不含内部路径、不含 SQL、不含列宽。
//!
//! 为什么不走 [`crate::error::AppError`]：表单校验失败不是异常，是正常业务分支。
//! 出错时要把用户刚才填的原样回填、在对应输入框下面显示一行红字——那需要返回
//! `Vec<FieldError>` 让模板整体重渲染，而不是甩一个 400 错误页把内容全清空。

/// 单个字段的校验失败。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldError {
    /// 表单字段名，模板据此把错误放到对应输入框下面。
    pub field: String,
    /// 中文提示，直接展示给用户。**不要**往这里塞数据库原文。
    pub message: String,
}

impl FieldError {
    pub fn new(field: &str, message: impl Into<String>) -> Self {
        Self {
            field: field.to_string(),
            message: message.into(),
        }
    }
}

/// 密码下限。
///
/// 需求原文写的是"至少 10 位"，2026-10-22 由 owner 下调到 8 位。
/// 上限仍是 128：不是为了安全，是为了别让 argon2 被超长输入拖死。
/// 不 trim —— 空格虽然被字符集挡在外面，但这条规则本身不该靠"顺手 trim 一下"成立。
pub const PASSWORD_MIN: usize = 8;

/// 密码允许的 ASCII 符号。字母、数字之外只有这些再加下划线（下划线就在里面）。
///
/// 为什么白名单而不是黑名单：黑名单永远漏，白名单至少能给出确定的错误提示。
/// 注意：这**不是**防注入手段 —— 密码从来不进 SQL 文本，全部走绑定参数；
/// 见 `password_charset_does_not_depend_on_sql` 那条测试，注入串会被当成普通字符
/// 原样哈希与校验。这里是为了让口令可预期（避免同形字、控制字符、表情符号）。
pub const PASSWORD_SYMBOLS: &str = "!\"#$%&'()*+,-./:;<=>?@[\\]^_`{|}~";

/// 允许的全角/中文标点。用户的键盘上 `￥（）——` 这些是一等公民，
/// 直接拒掉会得到"我明明设了 12 位却说过不了"这种最烦人的提示。
pub const PASSWORD_SYMBOLS_WIDE: &str = "￥（）：；！？，。、“”‘’《》【】…—·";

/// 密码上限。需求没写，但必须有：argon2 的耗时随输入线性增长，
/// 一个 1MB 的"密码"能把 2 核 CPU 按在地上，等于免费 DoS。
pub const PASSWORD_MAX: usize = 128;

/// 邮箱最大长度。**不是**随便取的：users.email 列宽 255，注销改名要追加
/// `__deleted__{id}` 最长 31 个字符，255 - 31 = 224，再留余量取 191（见 DESIGN §6.2）。
pub const EMAIL_MAX: usize = 191;

/// 用户名规则：3–32 个字符，仅 `[A-Za-z0-9_-]`。
pub const USERNAME_MIN: usize = 3;
pub const USERNAME_MAX: usize = 32;

/// 非空 + 长度上限。用于标题、简介这类自由文本。
///
/// 只按 `trim()` 之后的内容判空和计长——用户眼里"  "就是空的。
pub fn non_empty(field: &str, value: &str, max: usize) -> Result<(), FieldError> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(FieldError::new(field, "不能为空"));
    }
    check_len(field, trimmed, 1, max)
}

/// 用户名：长度 + 字符集。不做大小写归一，也不做保留字过滤（那是 service 的事）。
pub fn username(value: &str) -> Result<(), FieldError> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(FieldError::new("username", "不能为空"));
    }
    check_len("username", trimmed, USERNAME_MIN, USERNAME_MAX)?;

    if !trimmed
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return Err(FieldError::new(
            "username",
            "只能包含字母、数字、下划线和连字符",
        ));
    }
    Ok(())
}

/// 邮箱：恰好一个 `@`、两侧非空、无空白、长度不超上限。
///
/// 刻意不做 RFC 5322 全量校验——那玩意儿永远写不对，而且写对了也会拒掉真实可用的地址。
/// 真正的验证手段是发信，M1 之前先按"形状"卡住明显错误。
pub fn email(value: &str) -> Result<(), FieldError> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(FieldError::new("email", "不能为空"));
    }
    check_len("email", trimmed, 3, EMAIL_MAX)?;

    if trimmed.chars().any(char::is_whitespace) {
        return Err(FieldError::new("email", "不能包含空格"));
    }

    let mut parts = trimmed.split('@');
    let local = parts.next().unwrap_or_default();
    let domain = parts.next().unwrap_or_default();
    let extra = parts.next();

    if extra.is_some() || local.is_empty() || domain.is_empty() {
        return Err(FieldError::new("email", "邮箱格式不正确"));
    }
    Ok(())
}

/// 密码：长度 + 字符集两道门，不卡复杂度。
///
/// 复杂度规则（必须大写/符号/数字）在真实数据上是负收益：用户会变成 `Passw0rd!` 世代，
/// 熵反而更低。长度才是第一位的；字符集只用来挡空格、控制字符、表情与同形字。
/// 也**不要** trim——被拒就是被拒，不静默改写用户输入。
pub fn password(value: &str) -> Result<(), FieldError> {
    check_len("password", value, PASSWORD_MIN, PASSWORD_MAX)?;
    if let Some(bad) = value.chars().find(|c| !password_char_allowed(*c)) {
        let shown = if bad.is_control() {
            "控制字符".to_string()
        } else {
            format!("「{bad}」")
        };
        return Err(FieldError::new(
            "password",
            format!("只能用字母、数字和常见符号，不允许 {shown}"),
        ));
    }
    Ok(())
}

/// 单字符是否在口令字符集里：ASCII 字母数字 + 常见符号 + 全角标点。
fn password_char_allowed(c: char) -> bool {
    c.is_ascii_alphanumeric() || PASSWORD_SYMBOLS.contains(c) || PASSWORD_SYMBOLS_WIDE.contains(c)
}

/// 统一的长度检查。字符数而不是字节数——中文用户不该因为"一个汉字 3 字节"被误判超长。
fn check_len(field: &str, value: &str, min: usize, max: usize) -> Result<(), FieldError> {
    let len = value.chars().count();
    if len < min {
        return Err(FieldError::new(field, format!("至少需要 {min} 个字符")));
    }
    if len > max {
        return Err(FieldError::new(field, format!("最多 {max} 个字符")));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn non_empty_rejects_blank_and_whitespace_only() {
        assert!(non_empty("title", "", 10).is_err());
        assert!(non_empty("title", "   \t\n ", 10).is_err());
        assert!(non_empty("title", "标题", 10).is_ok());
    }

    #[test]
    fn non_empty_counts_characters_not_bytes() {
        // 5 个汉字 = 15 字节，但只有 5 个字符，应当通过 max=5。
        assert!(non_empty("title", "五个汉字啊", 5).is_ok());
        assert!(non_empty("title", "六个汉字啊啊", 5).is_err());
    }

    #[test]
    fn username_enforces_length_and_charset() {
        assert!(username("ab").is_err(), "太短");
        assert!(username("abc").is_ok(), "刚好 3 个");
        assert!(username(&"a".repeat(32)).is_ok(), "刚好 32 个");
        assert!(username(&"a".repeat(33)).is_err(), "超 32 个");
        assert!(username("abc_123-def").is_ok());
        assert!(username("张三").is_err(), "中文不允许");
        assert!(username("a b").is_err(), "空格不允许");
        assert!(username("a.b").is_err(), "点号不允许");
        assert!(username("  abc  ").is_ok(), "首尾空格应被 trim 掉");
    }

    #[test]
    fn email_requires_exactly_one_at_sign() {
        assert!(email("a@b.com").is_ok());
        assert!(email("no-at-sign").is_err());
        assert!(email("@b.com").is_err(), "本地部分为空");
        assert!(email("a@").is_err(), "域名部分为空");
        assert!(email("a@b@c").is_err(), "两个 @");
        assert!(email("a b@c.com").is_err(), "含空格");
        assert!(email("").is_err());
    }

    #[test]
    fn email_respects_column_width_budget() {
        let ok = format!("{}@x.com", "a".repeat(EMAIL_MAX - 6));
        assert_eq!(ok.chars().count(), EMAIL_MAX);
        assert!(email(&ok).is_ok(), "刚好占满预算应通过");

        let too_long = format!("{}@x.com", "a".repeat(EMAIL_MAX - 5));
        assert!(
            email(&too_long).is_err(),
            "超预算必须拒绝（否则注销改名会炸列宽）"
        );
    }

    #[test]
    fn password_is_length_only_and_never_trimmed() {
        assert!(password("1234567").is_err(), "7 位不够");
        assert!(password("12345678").is_ok(), "8 位刚好");
        assert!(
            password("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa").is_ok(),
            "纯小写也允许"
        );
        assert!(password(&"a".repeat(PASSWORD_MAX)).is_ok());
        assert!(
            password(&"a".repeat(PASSWORD_MAX + 1)).is_err(),
            "超长必须拒绝"
        );
        // 不做 trim：空格不在字符集里，直接拒，而不是悄悄替用户删掉。
        assert!(password("        ").is_err(), "空格不是合法口令字符");
        assert!(password("  abc  ").is_err(), "带空格必须被拒");
    }

    #[test]
    fn password_accepts_letters_digits_underscore_and_symbols() {
        for ok in [
            "abcd1234",
            "AbCd_123",
            "~!@#$%^&*",
            "()-_=+[]{}",
            "a_1!B@2#c",
            "￥（）！？，。a1", // 6 个全角标点 + 两位，凑够 8 位
        ] {
            assert!(
                password(ok).is_ok(),
                "应当允许：{ok} · {:?}",
                password(ok).unwrap_err().message
            );
        }
        // 全角与 ASCII 可以混用。
        assert!(password("abc_￥（）12").is_ok());
    }

    #[test]
    fn password_rejects_spaces_chinese_and_control_chars() {
        for bad in [
            "abc defg",     // 空格
            "密码12345678", // 汉字
            "abc12345😀",   // 表情
            "abc\t12345",   // 控制字符
            "abc12345\n",   // 换行
        ] {
            assert!(password(bad).is_err(), "应当拒绝：{bad:?}");
        }
    }

    /// 字符集与注入无关：口令从不进 SQL 文本，全部走绑定参数。
    /// 这条测试锁住"看起来像注入的串按普通字符处理"，防止有人日后把它当黑名单。
    #[test]
    fn password_charset_does_not_depend_on_sql() {
        let tricky = "'OR'1'='1";
        assert!(
            password(tricky).is_ok(),
            "注入形状的口令应作为普通字符被接受"
        );
        assert!(
            password("1;DROP TABLE users--").is_err(),
            "空格会被字符集挡下"
        );
    }
}
