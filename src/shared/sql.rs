//! 拼 SQL 时用得上的小工具。
//!
//! 只有"和 SQL 语法直接相关、又必须在 Rust 侧先处理"的东西才放这里。
//! 参数绑定（`?` + `.bind`）已经挡住了注入，这里解决的是另一类问题：
//! **`LIKE` 的模式元字符**——用户想搜 `50%`，不该变成"以 50 开头的任何东西"。

/// 把 `\`、`%`、`_` 转义成字面量，供 `LIKE ... ESCAPE '\\'` 使用。
///
/// 三个都要转义：`\` 是转义符本身（不转义它，用户输入 `\%` 就能反过来控制转义），
/// `%` 和 `_` 是通配符。顺序无关，因为每个字符各加一个 `\`，
/// 不会出现"转义了转义符又把它转义掉"的连锁。
pub fn escape_like(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for ch in input.chars() {
        match ch {
            '\\' | '%' | '_' => {
                out.push('\\');
                out.push(ch);
            }
            _ => out.push(ch),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wildcards_become_literal() {
        assert_eq!(escape_like("50%"), r"50\%");
        assert_eq!(escape_like("a_b"), r"a\_b");
    }

    #[test]
    fn the_escape_character_itself_is_escaped() {
        // 不转义反斜杠的话，用户输入 `\%` 会被 SQL 读成"转义后的 %"→ 又变回通配符。
        assert_eq!(escape_like(r"a\b"), r"a\\b");
        assert_eq!(escape_like(r"\%"), r"\\\%");
    }

    #[test]
    fn ordinary_text_is_untouched() {
        // 注意：动作名里到处都是下划线，**必须**被转义——否则 `user_role_change`
        // 会把 `user.role_change` 也捞出来。所以这里的"普通文本"只指没有元字符的。
        assert_eq!(escape_like("user.role_change"), r"user.role\_change");
        assert_eq!(escape_like("abc-def"), "abc-def");
        assert_eq!(escape_like("中文也能搜"), "中文也能搜");
    }
}
