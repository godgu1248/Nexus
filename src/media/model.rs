//! 图片元数据（文件本身在磁盘上，库里只记 sha256 与来源）。

use chrono::{DateTime, Utc};
use sqlx::mysql::MySqlRow;
use sqlx::{MySql, MySqlPool, Row, Transaction};

use crate::error::AppError;

/// 一条上传记录。
///
/// 文件按 `sha256` 内容寻址：同一张图重复上传只落一份，URL 也不可枚举
/// （不能靠自增 id 猜别人的图）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Media {
    pub id: u64,
    pub sha256: String,
    pub ext: String,
    pub mime: String,
    pub bytes: u32,
    pub uploader_id: u64,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct NewMedia<'a> {
    pub sha256: &'a str,
    pub ext: &'a str,
    pub mime: &'a str,
    pub bytes: u32,
    pub uploader_id: u64,
}

/// 按内容哈希查已有的图。重复上传靠它直接返回老记录。
pub async fn find_by_sha(pool: &MySqlPool, sha256: &str) -> Result<Option<Media>, AppError> {
    let row = sqlx::query(
        "SELECT id, sha256, ext, mime, bytes, uploader_id, created_at \
         FROM media WHERE sha256 = ?",
    )
    .bind(sha256)
    .fetch_optional(pool)
    .await?;

    row.as_ref().map(row_to_media).transpose()
}

/// 落一行记录。返回 `(id, 是否新建)`：`false` 表示 sha 已经存在（并发下由唯一索引裁决）。
pub async fn insert_media(
    tx: &mut Transaction<'_, MySql>,
    input: NewMedia<'_>,
) -> Result<(u64, bool), AppError> {
    // 用普通 INSERT + 精确识别 1062，而不是 INSERT IGNORE：
    // IGNORE 会把外键错误也吞成"重复"，最后表现为"上传成功了但查不到"。
    let result = sqlx::query(
        "INSERT INTO media (sha256, ext, mime, bytes, uploader_id) VALUES (?, ?, ?, ?, ?)",
    )
    .bind(input.sha256)
    .bind(input.ext)
    .bind(input.mime)
    .bind(input.bytes)
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
        uploader_id: row.try_get("uploader_id")?,
        created_at: row.try_get("created_at")?,
    })
}
