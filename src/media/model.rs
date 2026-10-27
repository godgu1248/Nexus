//! 图片 / Word 文档的元数据（文件本身在磁盘上，库里只记 sha256 与来源）。

use chrono::{DateTime, Utc};
use sqlx::mysql::MySqlRow;
use sqlx::{MySql, MySqlPool, Row, Transaction};

use crate::error::AppError;

/// 一条上传记录。
///
/// 文件按 `sha256` 内容寻址：同一个文件重复上传只落一份，URL 也不可枚举
/// （不能靠自增 id 猜别人的文件）。
///
/// `kind` 区分图片与 Word 文档；`title` / `html` 只对文档有意义——`html` 存的是
/// **解析并净化过**的正文，查看器直接用，不再每次重新解析（解析器将来改行为也不会
/// 让历史文档的渲染结果跟着漂移）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Media {
    pub id: u64,
    pub sha256: String,
    pub ext: String,
    pub mime: String,
    pub bytes: u32,
    pub kind: String,
    pub title: Option<String>,
    pub html: Option<String>,
    pub uploader_id: u64,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct NewMedia<'a> {
    pub sha256: &'a str,
    pub ext: &'a str,
    pub mime: &'a str,
    pub bytes: u32,
    pub kind: &'a str,
    pub title: Option<&'a str>,
    pub html: Option<&'a str>,
    pub uploader_id: u64,
}

/// 按内容哈希查已有的文件。重复上传靠它直接返回老记录。
pub async fn find_by_sha(pool: &MySqlPool, sha256: &str) -> Result<Option<Media>, AppError> {
    let row = sqlx::query(
        "SELECT id, sha256, ext, mime, bytes, kind, title, html, uploader_id, created_at \
         FROM media WHERE sha256 = ?",
    )
    .bind(sha256)
    .fetch_optional(pool)
    .await?;

    row.as_ref().map(row_to_media).transpose()
}

/// 落一行记录。返回 `(id, 是否新建)`：`false` 表示 sha 已经存在（并发下由唯一索引裁决）。
/// 刷新一份已存在的 Word 文档的转换结果（正文与标题）。
///
/// 只在"重新解析出来的东西和库里不一样"时调用——转换规则会演进（比如图片从"跳过"
/// 改成"导入并压缩"），而用户重传同一份文件时看到旧结果会以为是坏掉了。
pub async fn refresh_docx(
    tx: &mut Transaction<'_, MySql>,
    sha256: &str,
    title: &str,
    html: &str,
) -> Result<(), AppError> {
    sqlx::query("UPDATE media SET title = ?, html = ? WHERE sha256 = ? AND kind = 'docx'")
        .bind(title)
        .bind(html)
        .bind(sha256)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

pub async fn insert_media(
    tx: &mut Transaction<'_, MySql>,
    input: NewMedia<'_>,
) -> Result<(u64, bool), AppError> {
    // 用普通 INSERT + 精确识别 1062，而不是 INSERT IGNORE：
    // IGNORE 会把外键错误也吞成"重复"，最后表现为"上传成功了但查不到"。
    let result = sqlx::query(
        "INSERT INTO media (sha256, ext, mime, bytes, kind, title, html, uploader_id) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(input.sha256)
    .bind(input.ext)
    .bind(input.mime)
    .bind(input.bytes)
    .bind(input.kind)
    .bind(input.title)
    .bind(input.html)
    .bind(input.uploader_id)
    .execute(&mut **tx)
    .await;

    match result {
        Ok(done) => Ok((done.last_insert_id(), true)),
        Err(sqlx::Error::Database(db)) => {
            let duplicated = db
                .try_downcast_ref::<sqlx::mysql::MySqlDatabaseError>()
                .map(|err| err.number() == 1062)
                .unwrap_or(false);
            if !duplicated {
                return Err(AppError::Database(sqlx::Error::Database(db)));
            }
            let existing: u64 = sqlx::query_scalar("SELECT id FROM media WHERE sha256 = ?")
                .bind(input.sha256)
                .fetch_one(&mut **tx)
                .await?;
            Ok((existing, false))
        }
        Err(err) => Err(AppError::Database(err)),
    }
}

fn row_to_media(row: &MySqlRow) -> Result<Media, AppError> {
    Ok(Media {
        id: row.try_get("id")?,
        sha256: row.try_get("sha256")?,
        ext: row.try_get("ext")?,
        mime: row.try_get("mime")?,
        bytes: row.try_get("bytes")?,
        kind: row.try_get("kind")?,
        title: row.try_get("title")?,
        html: row.try_get("html")?,
        uploader_id: row.try_get("uploader_id")?,
        created_at: row.try_get("created_at")?,
    })
}
