//! 档案（知识库）数据访问层。
//!
//! 三条实现纪律，与 forum / group 保持一致：
//!
//! 1. **SQL 一律字面量**。sqlx 0.9 的 `query()` 只收 `&'static str`（`SqlSafeStr`），
//!    任何 `format!` 拼出来的 SQL 都过不了编译——这不是限制，是防止注入的护栏。
//!    要让"根文档"和"子文档"共用一条 SQL，用 `parent_id <=> ?`（NULL 安全等值），
//!    而不是拼 WHERE 子句。
//! 2. **可见性在 SQL 里过滤**，不是取回来再 `retain`。取回来再过滤会让分页总数
//!    与实际条目对不上，而且漏过滤是"静默泄漏"——页面上少一条没人发现。
//! 3. **草稿判定与可见性判定是两个与条件**：`policy::can_view` 回答"这个级别他能不能看"，
//!    不回答"这份内容发没发"。两者都在 SQL 里表达，别指望调用方记得补。

use chrono::{DateTime, Utc};
use sqlx::mysql::MySqlRow;
use sqlx::{MySqlPool, Row, Transaction};

use crate::auth::policy::{self, CurrentUser};
use crate::error::AppError;
use crate::shared::visibility::Visibility;

/// 完整文档行（含正文）。
pub struct KbDoc {
    pub id: u64,
    pub parent_id: Option<u64>,
    pub author_id: u64,
    pub author_name: String,
    pub title: String,
    pub body: String,
    /// 净化后的富文本；`None` 表示只有纯文本（老数据或纯文本提交）。
    pub body_html: Option<String>,
    pub visibility: Visibility,
    pub group_id: Option<u64>,
    pub sort_order: i32,
    pub view_count: u64,
    pub is_pinned: bool,
    pub pin_order: i32,
    pub is_published: bool,
    pub published_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// 列表里的文档卡片（不含正文）。
pub struct DocCard {
    pub id: u64,
    pub title: String,
    pub visibility: Visibility,
    pub is_published: bool,
    pub updated_at: DateTime<Utc>,
    pub child_count: u64,
    /// 浏览数：详情页每次被非作者打开 +1，用来给列表排序。
    pub view_count: u64,
    /// 被点赞的次数（`public_likes` 里指向本文档的行数）。
    pub like_count: u64,
    pub is_pinned: bool,
    pub pin_order: i32,
}

/// 面包屑节点。
pub struct Crumb {
    pub id: u64,
    pub title: String,
}

/// 新建文档的入参。
pub struct NewDoc<'a> {
    pub parent_id: Option<u64>,
    pub author_id: u64,
    pub title: &'a str,
    pub body: &'a str,
    /// 净化后的富文本；`None` 表示只有纯文本。
    pub body_html: Option<&'a str>,
    pub visibility: Visibility,
    pub group_id: Option<u64>,
    pub sort_order: i32,
    pub is_published: bool,
}

/// 观察者口径：登录者的 id、是否 admin、能不能读 internal，游客三者皆无。
///
/// `internal_ok` 不是"已登录"的同义词：自助注册的 `user`（来宾）登录了，
/// 但 `policy::can_view(Internal)` 对他是否。列表与搜索的 SQL 里必须用这个
/// 判定，而不是用 `viewer.id.is_some()`——否则来宾会从列表/最近/搜索这三条
/// 侧门看到内部档案，而单篇详情（走 `visible_detail` → `can_see` → policy）
/// 又是拒绝的，界面上就会表现为"点进去 404 但列表里看得见标题"。
struct Viewer {
    id: Option<u64>,
    is_admin: u8,
    internal_ok: u8,
}

fn viewer_of(viewer: Option<&CurrentUser>) -> Viewer {
    let internal = policy::ContentCtx::new(Visibility::Internal, None, None);
    match viewer {
        None => Viewer {
            id: None,
            is_admin: 0,
            internal_ok: 0,
        },
        Some(user) => Viewer {
            id: Some(user.id),
            is_admin: u8::from(policy::can_admin(Some(user))),
            internal_ok: u8::from(policy::can_view(Some(user), &internal)),
        },
    }
}

fn card_from_row(row: &MySqlRow) -> Result<DocCard, AppError> {
    Ok(DocCard {
        id: row.try_get("id")?,
        title: row.try_get("title")?,
        visibility: visibility_of(row)?,
        is_published: flag(row, "is_published")?,
        updated_at: row.try_get("updated_at")?,
        child_count: row.try_get("child_count")?,
        view_count: row.try_get("view_count")?,
        like_count: row.try_get("like_count")?,
        is_pinned: flag(row, "is_pinned")?,
        pin_order: row.try_get("pin_order")?,
    })
}

/// 库里的可见性字符串必须是三个小写值之一。出现别的值是写入口出了问题，
/// 这时要让它炸出来，而不是悄悄当成默认值——两个看起来一样的值走两条权限分支
/// 是最难查的一类越权。
fn visibility_of(row: &MySqlRow) -> Result<Visibility, AppError> {
    let raw: String = row.try_get("visibility")?;
    Visibility::from_db(&raw).ok_or_else(|| {
        AppError::internal(anyhow::anyhow!("kb_docs.visibility 出现未知取值：{raw}"))
    })
}

fn doc_from_row(row: &MySqlRow) -> Result<KbDoc, AppError> {
    Ok(KbDoc {
        id: row.try_get("id")?,
        parent_id: row.try_get("parent_id")?,
        author_id: row.try_get("author_id")?,
        author_name: row.try_get("author_name")?,
        title: row.try_get("title")?,
        body: row.try_get("body")?,
        body_html: row.try_get("body_html")?,
        visibility: visibility_of(row)?,
        group_id: row.try_get("group_id")?,
        sort_order: row.try_get("sort_order")?,
        view_count: row.try_get("view_count")?,
        is_pinned: flag(row, "is_pinned")?,
        pin_order: row.try_get("pin_order")?,
        is_published: flag(row, "is_published")?,
        published_at: row.try_get("published_at")?,
        created_at: row.try_get("created_at")?,
        updated_at: row.try_get("updated_at")?,
    })
}

/// TINYINT(1) 在 sqlx 里是 i8。
fn flag(row: &MySqlRow, name: &str) -> Result<bool, AppError> {
    let raw: i8 = row.try_get(name)?;
    Ok(raw != 0)
}

/// 按 id 取完整文档，**不做可见性过滤**——调用方（service）必须自己判定。
pub async fn find_doc(pool: &MySqlPool, id: u64) -> Result<Option<KbDoc>, AppError> {
    let row = sqlx::query(
        "SELECT d.id, d.parent_id, d.author_id, u.display_name AS author_name, d.title, d.body, d.body_html, \
                d.visibility, d.group_id, d.sort_order, d.view_count, d.is_pinned, d.pin_order, d.is_published, d.published_at, \
                d.created_at, d.updated_at \
           FROM kb_docs d \
           JOIN users u ON u.id = d.author_id \
          WHERE d.id = ? AND d.deleted_at IS NULL",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?;

    match row {
        Some(row) => Ok(Some(doc_from_row(&row)?)),
        None => Ok(None),
    }
}

/// 列出根文档（`parent_id IS NULL`），按 `sort_order`。
pub async fn list_roots(
    pool: &MySqlPool,
    viewer: Option<&CurrentUser>,
) -> Result<Vec<DocCard>, AppError> {
    list_cards(pool, viewer, None).await
}

/// 列出子文档。
pub async fn list_children(
    pool: &MySqlPool,
    parent_id: u64,
    viewer: Option<&CurrentUser>,
) -> Result<Vec<DocCard>, AppError> {
    list_cards(pool, viewer, Some(parent_id)).await
}

async fn list_cards(
    pool: &MySqlPool,
    viewer: Option<&CurrentUser>,
    parent_id: Option<u64>,
) -> Result<Vec<DocCard>, AppError> {
    let viewer = viewer_of(viewer);

    // 游客：只可能看到已发布且 public 的文档。这条 SQL 里没有任何用户输入，
    // 也不接受 bind admin 标志——游客没有身份，别让"标志位"成为后门。
    if viewer.id.is_none() {
        let rows = sqlx::query(
            "SELECT d.id, d.title, d.visibility, d.is_published, d.created_at, d.updated_at, d.view_count, d.is_pinned, d.pin_order, \
                    (CAST((SELECT COUNT(*) FROM kb_docs c WHERE c.parent_id = d.id AND c.deleted_at IS NULL) AS UNSIGNED)) AS child_count, \
                    (CAST((SELECT COUNT(*) FROM public_likes l WHERE l.target_type = 'doc' AND l.target_id = d.id) AS UNSIGNED)) AS like_count \
               FROM kb_docs d \
              WHERE d.deleted_at IS NULL AND d.visibility = 'public' AND d.is_published = 1 \
                AND d.parent_id <=> ? \
              ORDER BY d.is_pinned DESC, d.pin_order ASC, \
                     (d.view_count + 5 * (SELECT COUNT(*) FROM public_likes l2 WHERE l2.target_type = 'doc' AND l2.target_id = d.id)) DESC, \
                     d.updated_at DESC, d.id DESC",
        )
        .bind(parent_id)
        .fetch_all(pool)
        .await?;

        return rows.iter().map(card_from_row).collect();
    }

    let rows = sqlx::query(
        "SELECT d.id, d.title, d.visibility, d.is_published, d.created_at, d.updated_at, d.view_count, d.is_pinned, d.pin_order, \
                (CAST((SELECT COUNT(*) FROM kb_docs c WHERE c.parent_id = d.id AND c.deleted_at IS NULL) AS UNSIGNED)) AS child_count, \
                    (CAST((SELECT COUNT(*) FROM public_likes l WHERE l.target_type = 'doc' AND l.target_id = d.id) AS UNSIGNED)) AS like_count \
           FROM kb_docs d \
          WHERE d.deleted_at IS NULL \
            AND d.parent_id <=> ? \
            AND (d.is_published = 1 OR d.author_id = ? OR ? = 1) \
            AND (d.visibility = 'public' \
                 OR (d.visibility = 'internal' AND ? = 1) \
                 OR (d.visibility = 'group' AND EXISTS ( \
                       SELECT 1 FROM group_members gm \
                         JOIN member_groups g ON g.id = gm.group_id \
                        WHERE gm.group_id = d.group_id AND gm.user_id = ? AND g.deleted_at IS NULL))) \
          ORDER BY d.is_pinned DESC, d.pin_order ASC, \
                     (d.view_count + 5 * (SELECT COUNT(*) FROM public_likes l2 WHERE l2.target_type = 'doc' AND l2.target_id = d.id)) DESC, \
                     d.updated_at DESC, d.id DESC",
    )
    .bind(parent_id)
    .bind(viewer.id)
    .bind(viewer.is_admin)
        .bind(viewer.internal_ok)
    .bind(viewer.id)
    .fetch_all(pool)
    .await?;

    rows.iter().map(card_from_row).collect()
}

/// 最近更新的文档（有上限，避免整库拉回来）。
pub async fn list_recent(
    pool: &MySqlPool,
    viewer: Option<&CurrentUser>,
    limit: u32,
) -> Result<Vec<DocCard>, AppError> {
    let viewer = viewer_of(viewer);

    if viewer.id.is_none() {
        let rows = sqlx::query(
            "SELECT d.id, d.title, d.visibility, d.is_published, d.created_at, d.updated_at, d.view_count, d.is_pinned, d.pin_order, \
                    (CAST((SELECT COUNT(*) FROM kb_docs c WHERE c.parent_id = d.id AND c.deleted_at IS NULL) AS UNSIGNED)) AS child_count, \
                    (CAST((SELECT COUNT(*) FROM public_likes l WHERE l.target_type = 'doc' AND l.target_id = d.id) AS UNSIGNED)) AS like_count \
               FROM kb_docs d \
              WHERE d.deleted_at IS NULL AND d.visibility = 'public' AND d.is_published = 1 \
              ORDER BY d.updated_at DESC, d.id DESC LIMIT ?",
        )
        .bind(limit)
        .fetch_all(pool)
        .await?;

        return rows.iter().map(card_from_row).collect();
    }

    let rows = sqlx::query(
        "SELECT d.id, d.title, d.visibility, d.is_published, d.created_at, d.updated_at, d.view_count, d.is_pinned, d.pin_order, \
                (CAST((SELECT COUNT(*) FROM kb_docs c WHERE c.parent_id = d.id AND c.deleted_at IS NULL) AS UNSIGNED)) AS child_count, \
                    (CAST((SELECT COUNT(*) FROM public_likes l WHERE l.target_type = 'doc' AND l.target_id = d.id) AS UNSIGNED)) AS like_count \
           FROM kb_docs d \
          WHERE d.deleted_at IS NULL \
            AND (d.is_published = 1 OR d.author_id = ? OR ? = 1) \
            AND (d.visibility = 'public' \
                 OR (d.visibility = 'internal' AND ? = 1) \
                 OR (d.visibility = 'group' AND EXISTS ( \
                       SELECT 1 FROM group_members gm \
                         JOIN member_groups g ON g.id = gm.group_id \
                        WHERE gm.group_id = d.group_id AND gm.user_id = ? AND g.deleted_at IS NULL))) \
          ORDER BY d.updated_at DESC, d.id DESC LIMIT ?",
    )
    .bind(viewer.id)
    .bind(viewer.is_admin)
        .bind(viewer.internal_ok)
    .bind(viewer.id)
    .bind(limit)
    .fetch_all(pool)
    .await?;

    rows.iter().map(card_from_row).collect()
}

/// 浏览数 +1。调用方保证这确实是"被看了一次"（作者自己打开不算）。
///
/// 用 `view_count = view_count + 1` 而不是先读再写：并发读改写会丢计数，
/// 而这种计数器没人愿意为它开事务。
pub async fn bump_view(pool: &MySqlPool, id: u64) -> Result<(), AppError> {
    sqlx::query(
        "UPDATE kb_docs SET view_count = view_count + 1 WHERE id = ? AND deleted_at IS NULL",
    )
    .bind(id)
    .execute(pool)
    .await?;
    Ok(())
}

/// 置顶 / 取消置顶。返回是否真的改了。
///
/// 取消置顶时把 `pin_order` 归零：留着旧序号没有意义，
/// 下次置顶的人从 0 开始填，不会莫名其妙排到别人后面。
pub async fn set_pinned(
    tx: &mut sqlx::Transaction<'_, sqlx::MySql>,
    id: u64,
    pinned: bool,
    order: i32,
) -> Result<bool, AppError> {
    let result = sqlx::query(
        "UPDATE kb_docs SET is_pinned = ?, pin_order = ? \
          WHERE id = ? AND deleted_at IS NULL AND (is_pinned <> ? OR pin_order <> ?)",
    )
    .bind(u8::from(pinned))
    .bind(if pinned { order } else { 0 })
    .bind(id)
    .bind(u8::from(pinned))
    .bind(if pinned { order } else { 0 })
    .execute(&mut **tx)
    .await?;
    Ok(result.rows_affected() == 1)
}

/// 面包屑：从根到当前文档。最多爬 `MAX_CRAWL` 层，防止脏数据造成死循环。
pub async fn breadcrumb(pool: &MySqlPool, id: u64) -> Result<Vec<Crumb>, AppError> {
    const MAX_CRAWL: usize = 16;

    let mut chain: Vec<Crumb> = Vec::new();
    let mut cursor = Some(id);

    while let Some(current) = cursor {
        if chain.len() >= MAX_CRAWL {
            break;
        }
        let Some(doc) = find_doc(pool, current).await? else {
            break;
        };
        chain.push(Crumb {
            id: doc.id,
            title: doc.title.clone(),
        });
        cursor = doc.parent_id;
    }

    chain.reverse();
    Ok(chain)
}

/// 父节点所在的层数（根 = 1）。父不存在或已删返回 `None`。
///
/// 用递归 CTE 一次问完，而不是在 Rust 里循环查库：循环查库在父链断裂时会
/// 悄悄返回 1，把"父节点丢了"伪装成"这是一个根节点"。
pub async fn ancestor_depth(pool: &MySqlPool, parent_id: u64) -> Result<Option<u32>, AppError> {
    // CAST 成 UNSIGNED 不是装饰：MySQL 对 `MAX(整数)` 与 `COUNT(*)` 都报 BIGINT（有符号），
    // sqlx 拒绝把有符号 BIGINT 解码进 u32/u64（运行期 ColumnDecode，不是编译期错误）。
    // 标量子查询里的 COUNT 因为类型从外层继承恰好是 UNSIGNED，所以 forum 那边没炸——
    // 这种"同一写法一处炸一处不炸"的坑只能靠跑起来发现。
    let depth: Option<u64> = sqlx::query_scalar(
        "WITH RECURSIVE chain AS ( \
             SELECT id, parent_id, 1 AS depth FROM kb_docs WHERE id = ? AND deleted_at IS NULL \
             UNION ALL \
             SELECT d.id, d.parent_id, c.depth + 1 \
               FROM kb_docs d JOIN chain c ON d.id = c.parent_id \
              WHERE d.deleted_at IS NULL AND c.depth < 32 \
         ) \
         SELECT CAST(MAX(depth) AS UNSIGNED) FROM chain",
    )
    .bind(parent_id)
    .fetch_one(pool)
    .await?;

    // depth 只可能是 1..=32（CTE 自带 `c.depth < 32` 截断），但截断比断言安全：
    // 真出了问题应该是"父链太深"被 MAX_DEPTH 拦下，而不是这里 panic。
    Ok(depth.map(|value| value.min(u32::MAX as u64) as u32))
}

/// 是否还有未删的子文档（删除前的守门条件）。
pub async fn has_live_children(pool: &MySqlPool, id: u64) -> Result<bool, AppError> {
    let exists: i8 = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM kb_docs WHERE parent_id = ? AND deleted_at IS NULL)",
    )
    .bind(id)
    .fetch_one(pool)
    .await?;

    Ok(exists != 0)
}

/// 插入文档，返回新 id。**必须在事务里调用**——审计与业务同事务。
pub async fn create_doc(
    tx: &mut Transaction<'_, sqlx::MySql>,
    doc: NewDoc<'_>,
) -> Result<u64, AppError> {
    let published: u8 = u8::from(doc.is_published);

    let result = if doc.is_published {
        sqlx::query(
            "INSERT INTO kb_docs \
                 (parent_id, author_id, title, body, body_html, visibility, group_id, sort_order, is_published, published_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, UTC_TIMESTAMP())",
        )
        .bind(doc.parent_id)
        .bind(doc.author_id)
        .bind(doc.title)
        .bind(doc.body)
        .bind(doc.body_html)
        .bind(doc.visibility.as_str())
        .bind(doc.group_id)
        .bind(doc.sort_order)
        .bind(published)
        .execute(&mut **tx)
        .await?
    } else {
        sqlx::query(
            "INSERT INTO kb_docs \
                 (parent_id, author_id, title, body, body_html, visibility, group_id, sort_order, is_published, published_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, NULL)",
        )
        .bind(doc.parent_id)
        .bind(doc.author_id)
        .bind(doc.title)
        .bind(doc.body)
        .bind(doc.body_html)
        .bind(doc.visibility.as_str())
        .bind(doc.group_id)
        .bind(doc.sort_order)
        .bind(published)
        .execute(&mut **tx)
        .await?
    };

    Ok(result.last_insert_id())
}

/// 一次正文修订要写进库里的全部字段。
///
/// 打包成结构体而不是六个参数：`update_doc` 之前带八个参数，clippy 直接报
/// `this function has too many arguments (8/7)`；更实际的理由是——标题、正文、
/// 可见性、组归属必须一起改，散成参数容易在调用点漏掉一个（漏了就是"改了正文
/// 却把可见性悄悄打回 internal"）。
pub struct DocRevision<'a> {
    pub title: &'a str,
    pub body: &'a str,
    pub body_html: Option<&'a str>,
    pub visibility: Visibility,
    pub group_id: Option<u64>,
    pub sort_order: i32,
}

/// 覆盖标题/正文/可见性/组归属/排序值。返回是否命中未删行。
pub async fn update_doc(
    tx: &mut Transaction<'_, sqlx::MySql>,
    id: u64,
    revision: &DocRevision<'_>,
) -> Result<bool, AppError> {
    let result = sqlx::query(
        "UPDATE kb_docs \
            SET title = ?, body = ?, body_html = ?, visibility = ?, group_id = ?, sort_order = ? \
          WHERE id = ? AND deleted_at IS NULL",
    )
    .bind(revision.title)
    .bind(revision.body)
    .bind(revision.body_html)
    .bind(revision.visibility.as_str())
    .bind(revision.group_id)
    .bind(revision.sort_order)
    .bind(id)
    .execute(&mut **tx)
    .await?;

    Ok(result.rows_affected() > 0)
}

/// 发布 / 撤回。撤回时把 `published_at` 清空——时间戳留着会让人以为"还发着"。
pub async fn set_published(
    tx: &mut Transaction<'_, sqlx::MySql>,
    id: u64,
    published: bool,
) -> Result<bool, AppError> {
    let flag_value: u8 = u8::from(published);

    let result = sqlx::query(
        "UPDATE kb_docs \
            SET is_published = ?, published_at = IF(? = 1, UTC_TIMESTAMP(), NULL) \
          WHERE id = ? AND deleted_at IS NULL",
    )
    .bind(flag_value)
    .bind(flag_value)
    .bind(id)
    .execute(&mut **tx)
    .await?;

    Ok(result.rows_affected() > 0)
}

/// 软删。返回是否命中未删行（已删的再删一次不算成功）。
pub async fn soft_delete(tx: &mut Transaction<'_, sqlx::MySql>, id: u64) -> Result<bool, AppError> {
    let result = sqlx::query(
        "UPDATE kb_docs SET deleted_at = UTC_TIMESTAMP() WHERE id = ? AND deleted_at IS NULL",
    )
    .bind(id)
    .execute(&mut **tx)
    .await?;

    Ok(result.rows_affected() > 0)
}

/// 搜索：标题或正文包含关键词。返回（当页条目, 总数）。
///
/// `LIKE '%q%'` 用不上索引，这是 DESIGN §22.10 D4 记下的已知债——
/// 中小型企业的档案量级下，先要正确、再谈索引。
pub async fn search_docs(
    pool: &MySqlPool,
    viewer: Option<&CurrentUser>,
    q: &str,
    offset: u32,
    limit: u32,
) -> Result<(Vec<DocCard>, u64), AppError> {
    let viewer = viewer_of(viewer);
    let pattern = format!("%{}%", escape_like(q));

    if viewer.id.is_none() {
        let rows = sqlx::query(
            "SELECT d.id, d.title, d.visibility, d.is_published, d.created_at, d.updated_at, d.view_count, d.is_pinned, d.pin_order, \
                    (CAST((SELECT COUNT(*) FROM kb_docs c WHERE c.parent_id = d.id AND c.deleted_at IS NULL) AS UNSIGNED)) AS child_count, \
                    (CAST((SELECT COUNT(*) FROM public_likes l WHERE l.target_type = 'doc' AND l.target_id = d.id) AS UNSIGNED)) AS like_count \
               FROM kb_docs d \
              WHERE d.deleted_at IS NULL AND d.visibility = 'public' AND d.is_published = 1 \
                AND (d.title LIKE ? ESCAPE '\\\\' OR d.body LIKE ? ESCAPE '\\\\') \
              ORDER BY d.updated_at DESC, d.id DESC LIMIT ? OFFSET ?",
        )
        .bind(&pattern)
        .bind(&pattern)
        .bind(limit)
        .bind(offset)
        .fetch_all(pool)
        .await?;

        let total: u64 = sqlx::query_scalar(
            "SELECT CAST(COUNT(*) AS UNSIGNED) FROM kb_docs d \
              WHERE d.deleted_at IS NULL AND d.visibility = 'public' AND d.is_published = 1 \
                AND (d.title LIKE ? ESCAPE '\\\\' OR d.body LIKE ? ESCAPE '\\\\')",
        )
        .bind(&pattern)
        .bind(&pattern)
        .fetch_one(pool)
        .await?;

        let items = rows
            .iter()
            .map(card_from_row)
            .collect::<Result<Vec<_>, _>>()?;
        return Ok((items, total));
    }

    let rows = sqlx::query(
        "SELECT d.id, d.title, d.visibility, d.is_published, d.created_at, d.updated_at, d.view_count, d.is_pinned, d.pin_order, \
                (CAST((SELECT COUNT(*) FROM kb_docs c WHERE c.parent_id = d.id AND c.deleted_at IS NULL) AS UNSIGNED)) AS child_count, \
                    (CAST((SELECT COUNT(*) FROM public_likes l WHERE l.target_type = 'doc' AND l.target_id = d.id) AS UNSIGNED)) AS like_count \
           FROM kb_docs d \
          WHERE d.deleted_at IS NULL \
            AND (d.is_published = 1 OR d.author_id = ? OR ? = 1) \
            AND (d.title LIKE ? ESCAPE '\\\\' OR d.body LIKE ? ESCAPE '\\\\') \
            AND (d.visibility = 'public' \
                 OR (d.visibility = 'internal' AND ? = 1) \
                 OR (d.visibility = 'group' AND EXISTS ( \
                       SELECT 1 FROM group_members gm \
                         JOIN member_groups g ON g.id = gm.group_id \
                        WHERE gm.group_id = d.group_id AND gm.user_id = ? AND g.deleted_at IS NULL))) \
          ORDER BY d.updated_at DESC, d.id DESC LIMIT ? OFFSET ?",
    )
    .bind(viewer.id)
    .bind(viewer.is_admin)
    .bind(&pattern)
    .bind(&pattern)
    .bind(viewer.internal_ok)
    .bind(viewer.id)
    .bind(limit)
    .bind(offset)
    .fetch_all(pool)
    .await?;

    let total: u64 = sqlx::query_scalar(
        "SELECT CAST(COUNT(*) AS UNSIGNED) FROM kb_docs d \
          WHERE d.deleted_at IS NULL \
            AND (d.is_published = 1 OR d.author_id = ? OR ? = 1) \
            AND (d.title LIKE ? ESCAPE '\\\\' OR d.body LIKE ? ESCAPE '\\\\') \
            AND (d.visibility = 'public' \
                 OR (d.visibility = 'internal' AND ? = 1) \
                 OR (d.visibility = 'group' AND EXISTS ( \
                       SELECT 1 FROM group_members gm \
                         JOIN member_groups g ON g.id = gm.group_id \
                        WHERE gm.group_id = d.group_id AND gm.user_id = ? AND g.deleted_at IS NULL)))",
    )
    .bind(viewer.id)
    .bind(viewer.is_admin)
    .bind(&pattern)
    .bind(&pattern)
    .bind(viewer.internal_ok)
    .bind(viewer.id)
    .fetch_one(pool)
    .await?;

    let items = rows
        .iter()
        .map(card_from_row)
        .collect::<Result<Vec<_>, _>>()?;
    Ok((items, total))
}

/// 让用户输入的字面 `%` / `_` 不再被当成通配符。
fn escape_like(input: &str) -> String {
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
