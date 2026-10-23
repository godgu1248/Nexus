//! 图片落盘与元数据入库。
//!
//! 三条硬约束（DESIGN §32）：
//! 1. **按内容寻址**：文件名是 `sha256(字节)`，同一张图重复粘贴只存一份。
//! 2. **靠 magic bytes 判类型**，不信 `Content-Type`——浏览器给的 MIME 是客户端说了算的。
//! 3. **图片不进数据库**：库里只有元数据，文件放 `UPLOAD_DIR`，由反向代理直出。

use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};
use sqlx::MySqlPool;

use crate::config::Config;
use crate::error::AppError;
use crate::shared::audit::action;
use crate::shared::audit::{self, AuditCtx};

use super::model::{self, NewMedia};

/// 单张图片上限。与 nginx 的 `client_max_body_size`（8m）留出余量：
/// 应用层先拒，用户看到的是"图片太大"，而不是网关的 413 白页。
pub const MAX_UPLOAD_BYTES: usize = 4 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageKind {
    Png,
    Jpeg,
    Webp,
    Gif,
}

impl ImageKind {
    pub fn ext(self) -> &'static str {
        match self {
            Self::Png => "png",
            Self::Jpeg => "jpg",
            Self::Webp => "webp",
            Self::Gif => "gif",
        }
    }

    pub fn mime(self) -> &'static str {
        match self {
            Self::Png => "image/png",
            Self::Jpeg => "image/jpeg",
            Self::Webp => "image/webp",
            Self::Gif => "image/gif",
        }
    }

    /// 只看前 12 个字节。
    pub fn sniff(bytes: &[u8]) -> Option<Self> {
        if bytes.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]) {
            return Some(Self::Png);
        }
        if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
            return Some(Self::Jpeg);
        }
        if bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
            return Some(Self::Webp);
        }
        if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
            return Some(Self::Gif);
        }
        None
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredImage {
    pub url: String,
    pub bytes: u32,
    /// 内容已存在（没有真的写盘，也没有新增 media 行）。
    pub reused: bool,
}

pub fn sha256_hex(data: &[u8]) -> String {
    let digest = Sha256::digest(data);
    let mut out = String::with_capacity(64);
    for byte in digest {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// 存图：校验 → 落盘 → 入库 → 记审计。
pub async fn save_image(
    pool: &MySqlPool,
    config: &Config,
    actor: &crate::auth::policy::CurrentUser,
    ip: Option<&str>,
    data: &[u8],
) -> Result<StoredImage, AppError> {
    if data.is_empty() {
        return Err(AppError::unprocessable("没有收到图片内容"));
    }
    if data.len() > MAX_UPLOAD_BYTES {
        return Err(AppError::unprocessable(format!(
            "图片不能超过 {} MB（当前 {}）",
            MAX_UPLOAD_BYTES / (1024 * 1024),
            human_size(data.len())
        )));
    }

    let Some(kind) = ImageKind::sniff(data) else {
        return Err(AppError::unprocessable(
            "只支持 PNG / JPEG / WebP / GIF 四种图片",
        ));
    };

    let sha = sha256_hex(data);
    let dir = PathBuf::from(&config.upload_dir);
    let file_name = format!("{sha}.{}", kind.ext());
    let existed_on_disk = dir.join(&file_name).exists();

    if !existed_on_disk {
        write_atomically(&dir, &file_name, data).await?;
    }

    let mut tx = pool.begin().await?;
    let uploader = model::NewMedia {
        sha256: &sha,
        ext: kind.ext(),
        mime: kind.mime(),
        bytes: data.len() as u32,
        uploader_id: actor.id,
    };
    let (media_id, inserted) = model::insert_media(&mut tx, NewMedia(uploader)).await?;

    if inserted {
        audit::record(
            &mut tx,
            AuditCtx::new(Some(actor.id), ip),
            action::MEDIA_UPLOAD,
            ("media", Some(media_id)),
            Some(serde_json::json!({
                "sha256": sha,
                "bytes": data.len(),
                "mime": kind.mime(),
            })),
        )
        .await?;
    }

    tx.commit().await?;

    Ok(StoredImage {
        url: format!("{}/{}.{}", config.upload_base_url, sha, kind.ext()),
        bytes: data.len() as u32,
        reused: !inserted || existed_on_disk,
    })
}

/// 先写临时文件再 rename：中途失败不会留下"半张图"，nginx 也不会读到残缺文件。
async fn write_atomically(dir: &Path, file_name: &str, data: &[u8]) -> Result<(), AppError> {
    let dir = dir.to_owned();
    let file_name = file_name.to_owned();
    let data = data.to_owned();

    tokio::task::spawn_blocking(move || {
        std::fs::create_dir_all(&dir).map_err(|err| {
            AppError::internal(anyhow::Error::new(err).context("创建上传目录失败"))
        })?;
        let final_path = dir.join(&file_name);
        if final_path.exists() {
            return Ok(());
        }
        let tmp_path = dir.join(format!(".{file_name}.part"));
        std::fs::write(&tmp_path, &data)
            .map_err(|err| AppError::internal(anyhow::Error::new(err).context("写上传文件失败")))?;
        std::fs::rename(&tmp_path, &final_path).map_err(|err| {
            let _ = std::fs::remove_file(&tmp_path);
            AppError::internal(anyhow::Error::new(err).context("重命名上传文件失败"))
        })
    })
    .await
    .map_err(|err| AppError::internal(anyhow::Error::new(err).context("上传任务失败")))??;

    Ok(())
}

fn human_size(bytes: usize) -> String {
    if bytes >= 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    } else {
        format!("{} KB", bytes / 1024)
    }
}

// NewMedia 是带生命周期参数的结构体，这里只需要它的字段值，
// 用一个零成本包装避免在 service 里再写一遍字段名。
#[allow(non_snake_case)]
fn NewMedia(input: NewMedia<'_>) -> NewMedia<'_> {
    input
}

#[cfg(test)]
mod tests {
    use super::*;

    const PNG: &[u8] = &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0, 0, 0, 0];
    const JPEG: &[u8] = &[0xFF, 0xD8, 0xFF, 0xE0, 0, 0, 0, 0];
    const WEBP: &[u8] = b"RIFF\0\0\0\0WEBPVP8 ";
    const GIF: &[u8] = b"GIF89a\0\0";

    #[test]
    fn magic_bytes_identify_the_four_supported_kinds() {
        assert_eq!(ImageKind::sniff(PNG), Some(ImageKind::Png));
        assert_eq!(ImageKind::sniff(JPEG), Some(ImageKind::Jpeg));
        assert_eq!(ImageKind::sniff(WEBP), Some(ImageKind::Webp));
        assert_eq!(ImageKind::sniff(GIF), Some(ImageKind::Gif));
    }

    #[test]
    fn text_disguised_as_an_image_is_rejected() {
        assert_eq!(ImageKind::sniff(b"<script>alert(1)</script>"), None);
        assert_eq!(ImageKind::sniff(b""), None);
    }

    #[test]
    fn sha256_is_stable_and_lowercase_hex() {
        let a = sha256_hex(b"hello");
        assert_eq!(a.len(), 64);
        assert_eq!(a, sha256_hex(b"hello"));
        assert!(
            a.chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_uppercase())
        );
        assert_ne!(a, sha256_hex(b"hello!"));
    }

    #[test]
    fn extensions_and_mimes_line_up() {
        assert_eq!(ImageKind::Png.ext(), "png");
        assert_eq!(ImageKind::Jpeg.ext(), "jpg");
        assert_eq!(ImageKind::Jpeg.mime(), "image/jpeg");
        assert_eq!(ImageKind::Webp.mime(), "image/webp");
    }
}
