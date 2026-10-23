//! 分页：一个查询参数类型 + 一个结果容器。
//!
//! 需求红线是"列表强制分页、默认 20 条"。所以这里**不提供**"一次取全部"的分支——
//! 分页不是开关，是默认行为，代码里不给偷懒的口子。

use serde::{Deserialize, Deserializer};

/// 每页条数。硬编码 20，不做成配置项（见 DESIGN §5「刻意不做成配置项的东西」）。
pub const PAGE_SIZE: u32 = 20;

/// 单页条数上限。防止有人手工拼 `per_page=100000` 把 2G 内存的机器拉爆。
pub const MAX_PER_PAGE: u32 = 100;

/// 页码上限。`page` 是 u32，不封顶的话 `offset()` 能算到天文数字，
/// MySQL 也要为这个 `LIMIT ?, ?` 白跑一遍索引。
pub const MAX_PAGE: u32 = 10_000;

/// 列表接口的查询参数（`?page=2`）。
///
/// 非法值（缺省、空串、非数字、0、负数、超上限）一律归一化为第 1 页。
/// 理由：给用户回一个 400 只会换来一片红，而归一化后最坏结果是"回到了第一页"，
/// 用户能自己恢复，也不会让任何人拿 `?page=abc` 把 5xx 打出来。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PageQuery {
    pub page: u32,
}

impl Default for PageQuery {
    fn default() -> Self {
        Self { page: 1 }
    }
}

impl PageQuery {
    pub fn new(page: u32) -> Self {
        Self {
            page: normalize_page_number(page),
        }
    }
}

fn normalize_page_number(page: u32) -> u32 {
    page.clamp(1, MAX_PAGE)
}

/// 从原始查询串值归一化页码。独立成函数，便于单测直接打边界。
pub fn normalize_page(raw: Option<&str>) -> u32 {
    match raw
        .map(str::trim)
        .and_then(|value| value.parse::<u32>().ok())
    {
        Some(page) => normalize_page_number(page),
        // 缺省、空串、非数字、超出 u32、负数全部走这里。
        None => 1,
    }
}

/// 反序列化用中间层：查询串里的值都是字符串，先按 `Option<String>` 收，
/// 再交给 [`normalize_page`] 归一化。这样 `?page=abc` 不会变成 400。
#[derive(Deserialize)]
struct RawPageQuery {
    page: Option<String>,
}

impl<'de> Deserialize<'de> for PageQuery {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = RawPageQuery::deserialize(deserializer)?;
        Ok(Self {
            page: normalize_page(raw.page.as_deref()),
        })
    }
}

/// 一页数据。`total` 是**满足条件的总行数**（不是本页行数），列表页要靠它算页数。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Page<T> {
    pub items: Vec<T>,
    pub page: u32,
    pub total: u64,
    pub per_page: u32,
}

impl<T> Page<T> {
    pub fn new(items: Vec<T>, page: u32, total: u64, per_page: u32) -> Self {
        let per_page = if per_page == 0 {
            PAGE_SIZE
        } else {
            per_page.min(MAX_PER_PAGE)
        };
        Self {
            items,
            page: normalize_page_number(page),
            total,
            per_page,
        }
    }

    pub fn empty() -> Self {
        Self {
            items: Vec::new(),
            page: 1,
            total: 0,
            per_page: PAGE_SIZE,
        }
    }

    /// 总页数。空结果也算 1 页，否则模板里会出现"第 1 / 0 页"。
    pub fn total_pages(&self) -> u32 {
        let per_page = u64::from(self.per_page.max(1));
        let pages = self.total.div_ceil(per_page).max(1);
        // 页码上限与 MAX_PAGE 对齐，避免溢出 u32。
        u32::try_from(pages).unwrap_or(MAX_PAGE).min(MAX_PAGE)
    }

    pub fn has_prev(&self) -> bool {
        self.page > 1
    }

    pub fn has_next(&self) -> bool {
        self.page < self.total_pages()
    }

    /// SQL 的 `OFFSET`。0-based。
    pub fn offset(&self) -> u64 {
        u64::from(self.page.saturating_sub(1)) * u64::from(self.per_page)
    }

    /// 本页第一条在全局列表里的序号（从 1 开始），给"共 N 条"用。
    pub fn first_index(&self) -> u64 {
        self.offset() + 1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_page_covers_every_illegal_shape() {
        assert_eq!(normalize_page(None), 1, "缺省应为第 1 页");
        assert_eq!(normalize_page(Some("")), 1, "空串应为第 1 页");
        assert_eq!(normalize_page(Some("abc")), 1, "非数字应为第 1 页");
        assert_eq!(normalize_page(Some("0")), 1, "0 应为第 1 页");
        assert_eq!(normalize_page(Some("-3")), 1, "负数应为第 1 页");
        assert_eq!(normalize_page(Some("  7  ")), 7, "应容忍首尾空格");
        assert_eq!(normalize_page(Some("2")), 2);
        assert_eq!(
            normalize_page(Some("999999")),
            MAX_PAGE,
            "超上限应截断到 MAX_PAGE"
        );
    }

    #[test]
    fn page_query_deserializes_from_query_map() {
        let parsed: PageQuery = serde_json::from_str(r#"{"page":"3"}"#).expect("应能解析");
        assert_eq!(parsed.page, 3);

        let missing: PageQuery = serde_json::from_str("{}").expect("缺省应能解析");
        assert_eq!(missing.page, 1);

        let garbage: PageQuery = serde_json::from_str(r#"{"page":"abc"}"#).expect("脏值应能解析");
        assert_eq!(garbage.page, 1);

        // 未被关心的其他查询参数不能把它顶掉。
        let extra: PageQuery =
            serde_json::from_str(r#"{"page":"2","q":"x"}"#).expect("多余参数应被忽略");
        assert_eq!(extra.page, 2);
    }

    #[test]
    fn total_pages_rounds_up() {
        let page: Page<u8> = Page::new(vec![], 1, 41, 20);
        assert_eq!(page.total_pages(), 3, "41 条 / 每页 20 => 3 页");
        let exact: Page<u8> = Page::new(vec![], 1, 40, 20);
        assert_eq!(exact.total_pages(), 2);
    }

    #[test]
    fn empty_result_still_has_one_page() {
        let page: Page<u8> = Page::empty();
        assert_eq!(page.total_pages(), 1, "空结果不能显示成第 1/0 页");
        assert!(!page.has_prev());
        assert!(!page.has_next());
        assert_eq!(page.offset(), 0);
        assert_eq!(page.total, 0);
        assert_eq!(page.per_page, PAGE_SIZE);
    }

    #[test]
    fn has_next_stops_on_last_page() {
        let last: Page<u8> = Page::new(vec![], 3, 60, 20);
        assert!(last.has_prev());
        assert!(!last.has_next(), "第 3/3 页不应有下一页");

        let middle: Page<u8> = Page::new(vec![], 2, 60, 20);
        assert!(middle.has_prev());
        assert!(middle.has_next());
    }

    #[test]
    fn offset_is_zero_based() {
        let page: Page<u8> = Page::new(vec![], 3, 60, 20);
        assert_eq!(page.offset(), 40);
        assert_eq!(page.first_index(), 41);
    }

    #[test]
    fn per_page_and_page_are_clamped_on_construction() {
        let page: Page<u8> = Page::new(vec![], 0, 10, 0);
        assert_eq!(page.per_page, PAGE_SIZE, "per_page=0 应回落默认值");
        assert_eq!(page.page, 1, "page=0 应回落第 1 页");

        let huge: Page<u8> = Page::new(vec![], 1, 10, 100_000);
        assert_eq!(huge.per_page, MAX_PER_PAGE, "per_page 应被压到上限");
    }

    #[test]
    fn total_pages_saturates_instead_of_overflowing() {
        let page: Page<u8> = Page::new(vec![], 1, u64::MAX, 20);
        assert_eq!(page.total_pages(), MAX_PAGE);
    }
}
