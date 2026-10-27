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
    /// 查看器地址（`/doc/{sha256}`）——现在只当"在新标签页看原文"的入口。
    pub viewer_url: String,
    pub sha256: String,
    pub title: String,
    /// 净化后的正文 HTML，图片已经换成 `/uploads/<sha>.<ext>` 的真实地址。
    ///
    /// 编辑器拿到它**就地插入**正文，不再套一层 iframe 查看器：套 iframe 等于把
    /// 我们自己的导航栏、页脚一起塞进文档里，读者看到的是"一个网页里嵌了另一个网页"，
    /// 而不是"一段文档"。就地展开之后，正文段落、标题、表格、图片与站内其它内容
    /// 用同一套排版规则，读起来才和本地打开一致。
    pub html: String,
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

    // 内容已经存在时**也要重新解析**。
    //
    // 早先这里直接返回库里那行 html，理由是"同一份文件不该解析出两种结果"。但转换规则
    // 会演进：0.1.13 之前图片是跳过的（正文里只剩一个「〔图片〕」占位），之后才导入。
    // 复用旧 html 意味着**同一份文件重新上传永远修不好**——用户看到的是"上传成功了，
    // 图片还是没有"。现在改成：解析照做，结果与库里一致就早返回，不一致就用新的覆盖。
    // 代价是一次重复解析（几 MB 的文档，几十毫秒），换来"重传一次就自动修好"。

    // 解析放在阻塞线程池：一份 8 MB 的 docx 解出来是几十 MB 的 XML，解析是纯 CPU 活，
    // 直接在 async worker 上跑会把这台 2 vCPU 机器的其它请求一起卡住。
    //
    // 图片按 `Collect` 收下来：正文要就地展开成普通内容，图片必须跟着一起进来，
    // 否则"像本地看一样"就是一句空话。总量由解析器封顶（单张 8 MiB、合计 24 MiB），
    // 所以这里的内存上界是确定的 24 MiB，而不是"32 × 8 MiB"那种乘法幻觉。
    let owned = data.to_vec();
    let parsed = tokio::task::spawn_blocking(move || {
        crate::shared::docx::parse_docx_with(&owned, crate::shared::docx::ImagePolicy::Collect)
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

    // 库里已有这份文档：只有正文与标题都一致才算"复用"，否则继续走下面的流程去刷新它。
    let existing = model::find_by_sha(pool, &sha).await?;
    if let Some(row) = existing.as_ref()
        && row.kind == "docx"
        && row.html.as_deref() == Some(parsed.html.as_str())
        && row.title.as_deref() == Some(title.as_str())
    {
        return Ok(StoredDoc {
            viewer_url,
            sha256: sha,
            title,
            html: parsed.html.clone(),
            bytes: row.bytes,
            reused: true,
            notes: Vec::new(),
        });
    }
    let refreshing = existing.is_some();

    let dir = PathBuf::from(&config.upload_dir);
    let file_name_on_disk = format!("{sha}.docx");
    if !dir.join(&file_name_on_disk).exists() {
        write_atomically(&dir, &file_name_on_disk, data).await?;
    }

    let mut tx = pool.begin().await?;

    // 图片与文档在同一个事务里入库：任何一步失败都不会留下"正文引用了不存在的图"。
    // 落盘是内容寻址的（同一个 sha 只有一个文件），重复上传同一份文档不会多出文件。
    //
    // 压缩在事务之前、阻塞线程池里做完：解码 / 缩放 / 重编码是纯 CPU 活，
    // 一次文档可能带七八张图，放在 async worker 上会把整台机器拖住。
    let mut parsed = parsed;
    let raw_images = std::mem::take(&mut parsed.images);
    let raw_total: usize = raw_images.iter().map(|image| image.bytes.len()).sum();
    let optimized = tokio::task::spawn_blocking(move || {
        raw_images
            .into_iter()
            .map(|image| {
                crate::shared::imageopt::optimize_docx_image(&image.bytes, image.ext, image.mime)
            })
            .collect::<Vec<_>>()
    })
    .await
    .map_err(|err| AppError::internal(anyhow::anyhow!("图片压缩任务异常结束：{err}")))?;
    let optimized_total: usize = optimized.iter().map(|image| image.bytes.len()).sum();

    let mut html = parsed.html.clone();
    for (index, image) in optimized.iter().enumerate() {
        let image_sha = sha256_hex(&image.bytes);
        let file_on_disk = format!("{image_sha}.{}", image.ext);
        if !dir.join(&file_on_disk).exists() {
            write_atomically(&dir, &file_on_disk, &image.bytes).await?;
        }
        model::insert_media(
            &mut tx,
            model::NewMedia {
                sha256: &image_sha,
                ext: image.ext,
                mime: image.mime,
                bytes: image.bytes.len() as u32,
                kind: "image",
                title: None,
                html: None,
                uploader_id: actor.id,
            },
        )
        .await?;
        let src = format!("{}/{file_on_disk}", config.upload_base_url);
        html = html.replace(
            &format!("{{{{img:{index}}}}}"),
            &format!(r#"<img src="{src}" alt="" loading="lazy" decoding="async">"#),
        );
    }
    // 换完占位符再过一次净化：拼进去的 `<img>` 是我们自己生成的，但"落库的 html 一定
    // 过过 sanitize"这条不变式比"这次拼得对"更值得守——将来谁在这里多拼一个属性，
    // 第二次净化会把它吃掉，而不是让它在正文里活到线上。
    let html = crate::shared::richtext::sanitize(&html).into_string();

    let uploader = model::NewMedia {
        sha256: &sha,
        ext: "docx",
        mime: "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        bytes: data.len() as u32,
        kind: "docx",
        title: Some(&title),
        html: Some(&html),
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
                "images": optimized.len(),
            })),
        )
        .await?;
    } else if refreshing {
        // 同一份文件重传，但这次解析出来的正文与库里不同（转换规则变了，或者第一次
        // 上传时图片被跳过）。刷新那一行，让"重传一次"真的能修好旧文档。
        model::refresh_docx(&mut tx, &sha, &title, &html).await?;
    }

    tx.commit().await?;

    // 压缩结果给用户看一眼：他们只看到一个"上传成功"，不知道图被压了多少。
    let mut notes = parsed
        .notes
        .into_iter()
        .map(str::to_owned)
        .collect::<Vec<String>>();
    if optimized_total > 0 && raw_total > optimized_total {
        notes.push(format!(
            "内嵌图片已压缩：{} 张，{} → {}",
            optimized.len(),
            human_size(raw_total),
            human_size(optimized_total)
        ));
    }

    Ok(StoredDoc {
        viewer_url,
        sha256: sha,
        title,
        html,
        bytes: data.len() as u32,
        reused: !inserted,
        notes,
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
