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

use super::model;

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
        kind: "image",
        title: None,
        html: None,
        uploader_id: actor.id,
    };
    let (media_id, inserted) = model::insert_media(&mut tx, uploader).await?;

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

/// 一个已经落盘的 Word 文档。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredDoc {
    /// 查看器地址（`/doc/{sha256}`）。
    pub viewer_url: String,
    pub sha256: String,
    pub title: String,
    pub bytes: u32,
    /// 内容已存在（没有重新解析，也没有新增 media 行）。
    pub reused: bool,
    /// 解析时降级或忽略的东西，给用户看的一句话提示（可为空）。
    pub notes: Vec<String>,
}

/// 存 Word 文档：解析 → 净化 → 落盘 → 入库 → 记审计。
///
/// 解析失败一律 422：用户上传的是他手上那份文件，说"解析不了"比说"服务器错误"有用。
/// 正文里可能引用的图片（`{{img:N}}` 占位）本轮**不**导入——嵌入 word 查看器的场景里
/// 图片占多数，导出图片会让一次上传变成几十次写盘，先记为已知取舍。
pub async fn save_docx(
    pool: &MySqlPool,
    config: &Config,
    actor: &crate::auth::policy::CurrentUser,
    ip: Option<&str>,
    file_name: Option<&str>,
    data: &[u8],
) -> Result<StoredDoc, AppError> {
    if data.is_empty() {
        return Err(AppError::unprocessable("没有收到文档内容"));
    }
    if data.len() > crate::shared::docx::MAX_DOCX_BYTES {
        return Err(AppError::unprocessable(format!(
            "文档不能超过 {} MB（当前 {}）",
            crate::shared::docx::MAX_DOCX_BYTES / (1024 * 1024),
            human_size(data.len())
        )));
    }

    let sha = sha256_hex(data);
    let viewer_url = format!("/doc/{sha}");

    // 内容已经存在：直接复用库里那行（不重新解析，避免同一份文件两次解析出不同结果）。
    if let Some(existing) = model::find_by_sha(pool, &sha).await?
        && existing.kind == "docx"
    {
        return Ok(StoredDoc {
            viewer_url,
            sha256: sha,
            title: existing.title.unwrap_or_else(|| default_title(file_name)),
            bytes: existing.bytes,
            reused: true,
            notes: Vec::new(),
        });
    }

    // 解析放在阻塞线程池：一份 8 MB 的 docx 解出来是几十 MB 的 XML，解析是纯 CPU 活，
    // 直接在 async worker 上跑会把这台 2 vCPU 机器的其它请求一起卡住。
    //
    // 图片用 `Skip`：落库路径不写图片，解出来只会白烧最多 256 MB 内存。
    let owned = data.to_vec();
    let parsed = tokio::task::spawn_blocking(move || {
        crate::shared::docx::parse_docx_with(&owned, crate::shared::docx::ImagePolicy::Skip)
    })
    .await
    .map_err(|err| AppError::internal(anyhow::anyhow!("Word 解析任务异常结束：{err}")))??;
    // 标题优先级：core.xml 的 dc:title → 正文第一段（解析器已填）→ 上传文件名。
    // 三者都可能拿到空串，所以判空要在最后一步，长度也在这里收口（列宽 200）。
    let title = {
        let raw = parsed.title.trim();
        if raw.is_empty() {
            default_title(file_name)
        } else {
            raw.chars().take(200).collect()
        }
    };

    let dir = PathBuf::from(&config.upload_dir);
    let file_name_on_disk = format!("{sha}.docx");
    if !dir.join(&file_name_on_disk).exists() {
        write_atomically(&dir, &file_name_on_disk, data).await?;
    }

    let mut tx = pool.begin().await?;
    let uploader = model::NewMedia {
        sha256: &sha,
        ext: "docx",
        mime: "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        bytes: data.len() as u32,
        kind: "docx",
        title: Some(&title),
        html: Some(&parsed.html),
        uploader_id: actor.id,
    };
    let (media_id, inserted) = model::insert_media(&mut tx, uploader).await?;

    if inserted {
        audit::record(
            &mut tx,
            AuditCtx::new(Some(actor.id), ip),
            action::MEDIA_UPLOAD,
            ("media", Some(media_id)),
            Some(serde_json::json!({
                "sha256": sha,
                "bytes": data.len(),
                "kind": "docx",
                "title_len": title.chars().count(),
            })),
        )
        .await?;
    }

    tx.commit().await?;

    Ok(StoredDoc {
        viewer_url,
        sha256: sha,
        title,
        bytes: data.len() as u32,
        reused: !inserted,
        notes: parsed
            .notes
            .into_iter()
            .map(str::to_owned)
            .collect::<Vec<String>>(),
    })
}

/// 没有核心属性里的标题时，用上传的文件名去掉扩展名兜底。
fn default_title(file_name: Option<&str>) -> String {
    let raw = file_name.unwrap_or("未命名文档");
    let stem = raw.rsplit_once('.').map(|(head, _)| head).unwrap_or(raw);
    let trimmed = stem.trim();
    if trimmed.is_empty() {
        "未命名文档".to_owned()
    } else {
        trimmed.chars().take(200).collect()
    }
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
