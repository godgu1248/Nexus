//! 富文本：把用户提交的 HTML 净化成可安全嵌入页面的 `SafeHtml`，并抽一份纯文本。
//!
//! 设计取舍（DESIGN §32）：
//! 1. **不引 HTML 解析器**。ammonia / html5ever 会带进一大票依赖，而 2C2G 上每一兆都要算账；
//!    这里只需要"白名单 + 危险容器整段丢弃"，自己写反而更容易讲清楚规则。
//! 2. **白名单，不是黑名单**。见 `ALLOWED`。任何不在表里的标签一律丢掉（内容保留），
//!    `script` / `style` / `iframe` 这类连内容一起丢——把 `<script>alert(1)</script>` 的正文
//!    留下来当文字显示，是"看起来没被 XSS"的那种隐患。
//! 3. **属性也白名单**：只有 `a[href]` 与 `img[src|alt|width]` 能带属性，
//!    `style` / `class` / `on*` / `id` 全部丢弃。丢掉 `style` 意味着"用字号把内容藏起来"
//!    这类把戏也一起没了。
//! 4. `SafeHtml` 的构造函数是**私有**的：只有本模块能造出"已净化"的值。
//!    模板里那一处 `|safe` 因此不是"绕过转义"，而是"这个值只能由净化器产出"。
//! 5. 净化在**写入时**做一次（落库的是净化结果），读取时再走一次（防御历史数据）。

use std::fmt;

/// 已经过白名单净化的 HTML 片段。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SafeHtml(String);

impl SafeHtml {
    /// 空值：用于"没有富文本，只有纯文本"的老数据。
    pub fn empty() -> Self {
        Self(String::new())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn is_empty(&self) -> bool {
        self.0.trim().is_empty()
    }

    pub fn into_string(self) -> String {
        self.0
    }
}

impl fmt::Display for SafeHtml {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// 允许出现在正文里的标签（小写）。
const ALLOWED: &[&str] = &[
    "p",
    "br",
    "strong",
    "b",
    "em",
    "i",
    "u",
    "s",
    "del",
    "ul",
    "ol",
    "li",
    "h2",
    "h3",
    "blockquote",
    "code",
    "pre",
    "a",
    "img",
    "hr",
    "div",
    "span",
];

/// 自闭合标签。
const VOID: &[&str] = &["br", "hr", "img"];

/// 危险容器：整段丢弃（连内容一起）。
const DANGEROUS: &[&str] = &[
    "script", "style", "iframe", "object", "embed", "svg", "math", "template", "noscript", "form",
    "input", "textarea", "button", "select", "option", "link", "meta", "base",
];

/// 图片 URL 上限：够长到能放下 `/uploads/<64 hex>.webp`。
const URL_MAX: usize = 300;
const ALT_MAX: usize = 200;
const WIDTH_MAX: u32 = 2400;

/// 净化一段用户 HTML。
pub fn sanitize(input: &str) -> SafeHtml {
    SafeHtml(sanitize_to_string(input, false))
}

/// 从 HTML 里抽纯文本（用于搜索、摘要、`<title>` 之类的纯文本位置）。
pub fn plain_text(input: &str) -> String {
    let raw = sanitize_to_string(input, true);
    let mut out = String::with_capacity(raw.len());
    let mut last_space = true;
    for ch in raw.chars() {
        if ch.is_whitespace() {
            if !last_space {
                out.push(' ');
                last_space = true;
            }
        } else {
            out.push(ch);
            last_space = false;
        }
    }
    out.trim().to_owned()
}

fn sanitize_to_string(input: &str, text_only: bool) -> String {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    // 已输出但还没闭合的元素名。用来配对闭合标签：没有对应开标签的 `</a>` 必须丢掉，
    // 否则"URL 被拒绝、开标签被丢"之后会剩下一个孤零零的 `</a>`（渲染成可见字符）。
    let mut open: Vec<String> = Vec::new();

    while !rest.is_empty() {
        let Some(lt) = rest.find('<') else {
            push_text(&mut out, rest, text_only);
            break;
        };

        if lt > 0 {
            push_text(&mut out, &rest[..lt], text_only);
            rest = &rest[lt..];
        }

        // 注释整段丢掉。
        if let Some(after) = rest.strip_prefix("<!--") {
            match after.find("-->") {
                Some(end) => rest = &after[end + 3..],
                None => break,
            }
            continue;
        }

        // 「5 < 6」这种正文里的裸小于号不是标签开头：只有 `<` 后紧跟字母、`/`、`!`、`?`
        // 才当成标签解析，否则按文本转义输出。不这样区分的话，`<` 与后面某个 `>` 之间的
        // 整段文字会被当作未知标签丢掉——用户写「5 < 6 && 7 > 2」会只剩「5  2」。
        let looks_like_tag = rest[1..]
            .chars()
            .next()
            .is_some_and(|ch| ch.is_ascii_alphabetic() || matches!(ch, '/' | '!' | '?'));
        if !looks_like_tag {
            push_text(&mut out, "<", text_only);
            rest = &rest[1..];
            continue;
        }

        let Some(gt) = rest.find('>') else {
            // 没有闭合的 '<'：当文本处理，别把后面的内容一起吞掉。
            push_text(&mut out, "<", text_only);
            rest = &rest[1..];
            continue;
        };

        let tag = &rest[..=gt];
        rest = &rest[gt + 1..];

        let Some(parsed) = parse_tag(tag) else {
            continue;
        };

        if parsed.closing {
            let name = parsed.name.as_str();
            if let Some(pos) = open.iter().rposition(|item| item == name) {
                // 顺带把中间没闭合的元素补上收尾，避免输出畸形嵌套。
                while open.len() > pos {
                    let closed = open.pop().unwrap_or_default();
                    if !text_only && !VOID.contains(&closed.as_str()) {
                        out.push_str("</");
                        out.push_str(&closed);
                        out.push('>');
                    }
                }
            }
            continue;
        }

        if DANGEROUS.contains(&parsed.name.as_str()) {
            rest = skip_element(rest, &parsed.name);
            continue;
        }

        if text_only {
            continue;
        }

        if !ALLOWED.contains(&parsed.name.as_str()) {
            continue;
        }

        // 块级标签在文本模式下要有分隔，否则 "<p>a</p><p>b</p>" 会粘成 "ab"。
        let rendered = render_open_tag(&parsed);
        if rendered.is_empty() {
            // 属性被判废（比如 javascript: 链接）→ 连开标签一起丢，闭合标签会被配对逻辑丢掉。
            continue;
        }
        if !VOID.contains(&parsed.name.as_str()) {
            open.push(parsed.name.clone());
        }
        out.push_str(&rendered);
    }

    // 收尾：补上没闭合的元素，避免把未闭合标签泄漏到页面里影响后面所有内容。
    while let Some(name) = open.pop() {
        out.push_str("</");
        out.push_str(&name);
        out.push('>');
    }

    out
}

/// 丢掉危险容器里的全部内容，返回剩余输入。
fn skip_element<'a>(rest: &'a str, name: &str) -> &'a str {
    let needle = format!("</{name}");
    let lower = rest.to_ascii_lowercase();
    if let Some(idx) = lower.find(&needle) {
        let after = &rest[idx..];
        return match after.find('>') {
            Some(gt) => &after[gt + 1..],
            None => "",
        };
    }
    ""
}

fn push_text(out: &mut String, text: &str, text_only: bool) {
    if text_only {
        out.push_str(&decode_entities(text));
    } else {
        escape_into(out, text);
    }
}

struct Tag {
    name: String,
    closing: bool,
    attrs: Vec<(String, String)>,
}

fn parse_tag(raw: &str) -> Option<Tag> {
    let inner = raw.strip_prefix('<')?.strip_suffix('>')?;
    let inner = inner.trim();
    if inner.is_empty() {
        return None;
    }

    let (closing, body) = match inner.strip_prefix('/') {
        Some(rest) => (true, rest.trim_start()),
        None => (false, inner),
    };

    let name_end = body
        .find(|c: char| c.is_whitespace() || c == '/')
        .unwrap_or(body.len());
    let name = body[..name_end].to_ascii_lowercase();
    if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric()) {
        return None;
    }

    let mut attrs = Vec::new();
    if !closing {
        attrs = parse_attrs(&body[name_end..]);
    }

    Some(Tag {
        name,
        closing,
        attrs,
    })
}

/// 只解析 `name="value"` / `name=value` / `name`，够用即可——反正后面还要按白名单过滤。
fn parse_attrs(mut input: &str) -> Vec<(String, String)> {
    let mut attrs = Vec::new();
    loop {
        input = input.trim_start_matches(|c: char| c.is_whitespace() || c == '/');
        if input.is_empty() {
            break;
        }

        let name_end = input
            .find(|c: char| c.is_whitespace() || c == '=' || c == '/')
            .unwrap_or(input.len());
        let name = input[..name_end].to_ascii_lowercase();
        input = &input[name_end..];

        let mut value = String::new();
        if let Some(after_eq) = input.strip_prefix('=') {
            let after_eq = after_eq.trim_start();
            if let Some(quote) = after_eq.chars().next().filter(|c| *c == '"' || *c == '\'') {
                let body = &after_eq[1..];
                if let Some(end) = body.find(quote) {
                    value = body[..end].to_owned();
                    input = &body[end + 1..];
                } else {
                    value = body.to_owned();
                    input = "";
                }
            } else {
                let end = after_eq
                    .find(|c: char| c.is_whitespace())
                    .unwrap_or(after_eq.len());
                value = after_eq[..end].to_owned();
                input = &after_eq[end..];
            }
        } else {
            input = input.strip_prefix('=').unwrap_or(input);
        }

        if !name.is_empty() {
            attrs.push((name, value));
        }
    }
    attrs
}

fn render_open_tag(tag: &Tag) -> String {
    let mut out = String::from("<");
    out.push_str(&tag.name);

    match tag.name.as_str() {
        "img" => {
            let src = attr(&tag.attrs, "src")
                .and_then(safe_image_src)
                .unwrap_or_default();
            if src.is_empty() {
                // 不带 src 的 img 是废标签，直接不输出，省得浏览器显示破图边框。
                return String::new();
            }
            out.push_str(" src=\"");
            escape_into(&mut out, &src);
            out.push('"');
            if let Some(alt) = attr(&tag.attrs, "alt") {
                let alt = clamp_chars(alt, ALT_MAX);
                out.push_str(" alt=\"");
                escape_into(&mut out, &alt);
                out.push('"');
            }
            if let Some(width) = attr(&tag.attrs, "width").and_then(parse_width) {
                out.push_str(&format!(" width=\"{width}\""));
            }
            // 图片一律懒加载：正文里可能贴十几张，首屏不必全拉。
            out.push_str(" loading=\"lazy\" decoding=\"async\"");
        }
        "a" => {
            let href = attr(&tag.attrs, "href")
                .and_then(safe_href)
                .unwrap_or_default();
            if href.is_empty() {
                return String::new();
            }
            out.push_str(" href=\"");
            escape_into(&mut out, &href);
            out.push('"');
            out.push_str(" rel=\"noopener noreferrer nofollow\"");
        }
        _ => {}
    }

    // 自闭合（img/br/hr…）与普通开标签在输出上没差别：一律 <name ...>，
    // 闭合由配对逻辑负责。
    out.push('>');
    out
}

fn attr<'a>(attrs: &'a [(String, String)], name: &str) -> Option<&'a str> {
    attrs
        .iter()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.as_str())
}

fn parse_width(raw: &str) -> Option<u32> {
    let value: u32 = raw.trim().parse().ok()?;
    if value == 0 || value > WIDTH_MAX {
        return None;
    }
    Some(value)
}

/// 图片地址：只允许本站上传目录与 http(s) 外链。
fn safe_image_src(raw: &str) -> Option<String> {
    let value = clean_url(raw)?;
    if value.starts_with("/uploads/") {
        return Some(value);
    }
    if value.starts_with("https://") || value.starts_with("http://") {
        return Some(value);
    }
    None
}

/// 链接地址：允许站内相对路径、http(s)、mailto。
fn safe_href(raw: &str) -> Option<String> {
    let value = clean_url(raw)?;
    if value.starts_with('/') && !value.starts_with("//") {
        return Some(value);
    }
    if value.starts_with("https://") || value.starts_with("http://") || value.starts_with("mailto:")
    {
        return Some(value);
    }
    None
}

/// URL 预处理：去首尾空白、拒绝控制字符与超长值。
fn clean_url(raw: &str) -> Option<String> {
    let value = raw.trim();
    if value.is_empty() || value.len() > URL_MAX {
        return None;
    }
    if value.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return None;
    }
    Some(value.to_owned())
}

fn clamp_chars(value: &str, max: usize) -> String {
    value.chars().take(max).collect()
}

fn escape_into(out: &mut String, text: &str) {
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(ch),
        }
    }
}

/// 只处理我们自己会产生的五个实体——不追求完整 HTML 实体表。
fn decode_entities(text: &str) -> String {
    text.replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
}

/// 净化 + 抽纯文本之后的一段正文：纯文本用于搜索与摘要，HTML 用于渲染。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RichBody {
    pub text: String,
    /// `None` 表示这条正文只有纯文本（老数据、或用户直接提交纯文本）。
    pub html: Option<String>,
}

/// 正文上限之外再给 HTML 一个绝对上限：编辑器会把手打的内容放大十几倍（标签、样式类），
/// 不设上限的话一次粘贴就能把 20k 字的限制绕成几 MB。
const HTML_MAX: usize = 200_000;

/// 把"富文本 + 纯文本回退"整理成可落库的形状。
///
/// * `html` 非空 → 净化 → 抽纯文本 → 校验长度（只算人看到的字数，不算标签）；
/// * `html` 为空 → 用 `plain`（脚本没跑起来、或用户直接提交纯文本）。
///
/// 只有图片没有文字的帖子是合法的，所以"纯文本为空"不算错误，只要净化后的 HTML 里还有 `<img`。
pub fn prepare(
    html: Option<&str>,
    plain: Option<&str>,
    max_chars: usize,
    label: &str,
) -> Result<RichBody, crate::error::AppError> {
    let raw_html = html.unwrap_or("").trim();

    if !raw_html.is_empty() {
        if raw_html.chars().count() > HTML_MAX {
            return Err(crate::error::AppError::unprocessable(format!(
                "{label}的排版内容过大，请精简后再提交"
            )));
        }

        let sanitized = sanitize(raw_html);
        let text = plain_text(sanitized.as_str());
        let has_image = sanitized.as_str().contains("<img");

        if text.is_empty() && !has_image {
            return Err(crate::error::AppError::unprocessable(format!(
                "{label}不能为空"
            )));
        }
        if text.chars().count() > max_chars {
            return Err(crate::error::AppError::unprocessable(format!(
                "{label}最多 {max_chars} 个字"
            )));
        }

        return Ok(RichBody {
            text,
            html: Some(sanitized.into_string()),
        });
    }

    let text = plain.unwrap_or("").trim().to_owned();
    if text.chars().count() > max_chars {
        return Err(crate::error::AppError::unprocessable(format!(
            "{label}最多 {max_chars} 个字"
        )));
    }

    Ok(RichBody { text, html: None })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_tags_survive() {
        let html = sanitize("<p>你好 <strong>世界</strong></p>");
        assert_eq!(html.as_str(), "<p>你好 <strong>世界</strong></p>");
    }

    #[test]
    fn script_is_dropped_with_its_content() {
        let html = sanitize("<p>a</p><script>alert(1)</script><p>b</p>");
        assert_eq!(html.as_str(), "<p>a</p><p>b</p>");
        assert!(!html.as_str().contains("alert"));
    }

    #[test]
    fn event_handlers_are_removed() {
        let html = sanitize(r#"<img src="/uploads/abc.png" onerror="alert(1)">"#);
        assert!(html.as_str().contains("src=\"/uploads/abc.png\""));
        assert!(!html.as_str().contains("onerror"));
    }

    #[test]
    fn javascript_urls_are_rejected() {
        assert_eq!(
            sanitize(r#"<a href="javascript:alert(1)">x</a>"#).as_str(),
            "x"
        );
        assert_eq!(sanitize(r#"<img src="javascript:alert(1)">"#).as_str(), "");
    }

    #[test]
    fn style_and_class_attributes_do_not_survive() {
        let html = sanitize(r#"<p style="display:none" class="x">看不见</p>"#);
        assert_eq!(html.as_str(), "<p>看不见</p>");
    }

    #[test]
    fn iframe_and_svg_are_dropped_entirely() {
        let html =
            sanitize("<iframe src=\"https://evil\"></iframe><svg><script>x</script></svg>ok");
        assert_eq!(html.as_str(), "ok");
    }

    #[test]
    fn links_get_rel_and_images_get_lazy_loading() {
        let html = sanitize(
            r#"<a href="https://example.com">站外</a><img src="/uploads/x.webp" alt="图">"#,
        );
        assert!(
            html.as_str()
                .contains("rel=\"noopener noreferrer nofollow\"")
        );
        assert!(html.as_str().contains("loading=\"lazy\""));
    }

    #[test]
    fn text_outside_tags_is_escaped() {
        let html = sanitize("5 < 6 && 7 > 2");
        assert_eq!(html.as_str(), "5 &lt; 6 &amp;&amp; 7 &gt; 2");
    }

    #[test]
    fn unterminated_tag_does_not_swallow_the_rest() {
        let html = sanitize("a < b <p>c</p>");
        assert!(html.as_str().contains("&lt;"));
        assert!(html.as_str().contains("<p>c</p>"));
    }

    #[test]
    fn plain_text_strips_markup_and_keeps_spacing() {
        assert_eq!(plain_text("<p>第一段</p><p>第二段</p>"), "第一段第二段");
        assert_eq!(plain_text("a &amp; b"), "a & b");
        assert_eq!(plain_text("<script>x</script>ok"), "ok");
    }

    #[test]
    fn empty_input_is_empty() {
        assert!(sanitize("").is_empty());
        assert_eq!(plain_text("   "), "");
    }

    #[test]
    fn prepare_keeps_both_shapes() {
        // 富文本：文本抽出来落 body，净化后的 HTML 落 body_html。
        let rich =
            prepare(Some("<p>第一段<strong>粗</strong></p>"), None, 100, "正文").expect("应当通过");
        assert_eq!(rich.text, "第一段粗");
        let html = rich.html.expect("应当有 HTML");
        assert!(html.contains("<strong>粗</strong>"));

        // 纯文本：没有 HTML 时 html 字段必须是 None，模板才会走纯文本分支。
        let plain = prepare(None, Some("只有文字"), 100, "正文").expect("应当通过");
        assert_eq!(plain.text, "只有文字");
        assert!(plain.html.is_none());
    }

    #[test]
    fn prepare_accepts_an_image_only_body() {
        // 只有图片没有文字也是合法正文——这是富文本带来的新形态，
        // 判据是"净化后还有 <img"，不是"文本非空"。
        let body = prepare(
            Some("<p><img src=\"/uploads/a.png\"></p>"),
            None,
            100,
            "正文",
        )
        .expect("只有图片应当被接受");
        assert!(body.text.is_empty(), "纯文本可以是空串");
        assert!(body.html.expect("应当有 HTML").contains("<img"));
    }

    #[test]
    fn prepare_rejects_empty_and_oversized_bodies() {
        let empty = prepare(Some("<p>   </p>"), Some("   "), 100, "正文");
        assert!(empty.is_err(), "空正文必须报错");

        let too_long = prepare(None, Some(&"字".repeat(11)), 10, "正文");
        assert!(too_long.is_err(), "超过字数上限必须报错");

        let too_much_html = prepare(Some(&"<p>x</p>".repeat(30_000)), None, 10, "正文");
        assert!(too_much_html.is_err(), "HTML 体积上限必须生效");
    }
}
