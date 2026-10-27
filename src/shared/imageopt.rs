//! Word 文档里那些图片的压缩策略。
//!
//! 为什么需要它：一份 3 MB 的 .docx 里往往塞着七八张 200 KB–1 MB 的 PNG 截图，
//! 而它们被就地展开成正文以后就成了网页资源。原样落盘意味着：打开一篇文档要拉
//! 3 MB 图，2 vCPU / 1.6 GB 的机器上还要为每张图付一次磁盘与内存。
//!
//! 目标是"平衡"，不是"尽可能小"：
//!
//! * 长边超过 [`MAX_EDGE`] 才缩，且用 Lanczos3（缩小的损失比放大明显好判断）；
//! * 不透明的图转 JPEG 质量 [`JPEG_QUALITY`]；带透明的留 PNG（截图里的透明边框、
//!   线条图转 JPEG 会出现脏边）；
//! * 已经小于 [`KEEP_PNG_BELOW`] 的 PNG 不动——为省几十 KB 去重编码一张小图，
//!   是在用画质换噪声；
//! * 压完反而变大就保留原图。这条是兜底：JPEG 对纯色区域可能比 PNG 大。
//!
//! 只有 Word 导入走这里。用户手动上传的图片保持原样——那可能是他们要交付的原图，
//! 我们不该在用户没要求的时候悄悄改画质。

use image::codecs::jpeg::JpegEncoder;
use image::codecs::png::PngEncoder;
use image::{ExtendedColorType, ImageEncoder};
use std::io::Cursor;

/// 长边上限。1600 是常见的正文插图宽度，再大在页面上也显示不出来。
pub const MAX_EDGE: u32 = 1600;

/// 允许解码的像素总数上限（2400 万 = 约 8000×3000）。
///
/// 这一条挡的是**解压炸弹**，不是"大图不好看"：PNG/JPEG 的压缩率可以到千分之一，
/// 一个 2 MB 的文件能声明 20000×20000，解码后是 20000×20000×4 = 1.6 GB。
/// 这台机器物理内存 1.6 GB、systemd 上限 700 MB，一次这样的解码就把服务打死，
/// 而且是**上传时**打死——攻击者不需要登录第二次。
///
/// 判据只读文件头，不解码：超限就原样保留（不压），宁可页面上有张大图，也不能把进程干掉。
pub const MAX_PIXELS: u64 = 24_000_000;

/// 解码时允许分配的内存上限。像素上限挡住了"尺寸谎报"，这一条挡的是
/// 尺寸合法但每像素位数异常、或解码器内部放大分配的极端情况。
const MAX_DECODE_BYTES: u64 = 256 * 1024 * 1024;

/// JPEG 质量。86 是肉眼与体积的折中点：再低在文字截图上开始出彩边，
/// 再高对照片的体积几乎没影响。
pub const JPEG_QUALITY: u8 = 86;

/// 小于这个体积的 PNG 原样保留。
pub const KEEP_PNG_BELOW: usize = 300 * 1024;

/// 可视为"不透明"的透明像素占比上限（1%）。
///
/// Word 里的截图几乎都是 RGBA 但 alpha 全满；只有超过这个比例的像素真的半透明 / 全透明，
/// 才值得为此保留 PNG。
pub const TRANSPARENT_RATIO: f64 = 0.01;

/// 只读文件头拿到声明的宽高，不做解码。
///
/// 返回 `None` 表示"读不出来"——这种输入按超限处理（见 [`exceeds_pixel_budget`]）。
///
/// 这里自己解析文件头，而不是问 `image` crate：PNG 解码器要读到像素数据才开始工作，
/// 一个只有 IHDR 的头（正是炸弹的形状）会让它报 `unexpected end of file`，
/// 拿不到尺寸就没法在解码前拦下它。
fn declared_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    png_dimensions(bytes)
        .or_else(|| jpeg_dimensions(bytes))
        .or_else(|| gif_dimensions(bytes))
        .or_else(|| webp_dimensions(bytes))
}

fn be32(bytes: &[u8], at: usize) -> Option<u32> {
    let slice = bytes.get(at..at + 4)?;
    Some(u32::from_be_bytes([slice[0], slice[1], slice[2], slice[3]]))
}

fn le16(bytes: &[u8], at: usize) -> Option<u32> {
    let slice = bytes.get(at..at + 2)?;
    Some(u32::from(u16::from_le_bytes([slice[0], slice[1]])))
}

fn le24(bytes: &[u8], at: usize) -> Option<u32> {
    let slice = bytes.get(at..at + 3)?;
    Some(u32::from(slice[0]) | (u32::from(slice[1]) << 8) | (u32::from(slice[2]) << 16))
}

/// PNG：签名 8 字节，紧接 IHDR 块，宽高在偏移 16 与 20，大端。
fn png_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    const SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
    if bytes.get(..8)? != SIGNATURE || bytes.get(12..16)? != b"IHDR" {
        return None;
    }
    Some((be32(bytes, 16)?, be32(bytes, 20)?))
}

/// JPEG：顺着段标记走，找到 SOFn 就取宽高（高在 +5，宽在 +7，大端）。
///
/// SOF0/1/2/3/5/6/7/9/10/11/13/14/15 都是"帧头"，C4(DHT)/C8(JPG)/CC(DAC) 不是。
fn jpeg_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    if bytes.get(..2)? != [0xff, 0xd8] {
        return None;
    }
    let mut at = 2usize;
    while at + 4 <= bytes.len() {
        if bytes[at] != 0xff {
            return None;
        }
        let marker = bytes[at + 1];
        // 填充字节 0xFF 允许连续出现，跳过。
        if marker == 0xff {
            at += 1;
            continue;
        }
        // 无长度的标记：RSTn、TEM。
        if (0xd0..=0xd9).contains(&marker) || marker == 0x01 {
            at += 2;
            continue;
        }
        let length = usize::from(u16::from_be_bytes([bytes[at + 2], bytes[at + 3]]));
        let is_sof = (0xc0..=0xcf).contains(&marker) && !matches!(marker, 0xc4 | 0xc8 | 0xcc);
        if is_sof {
            let height = usize::from(u16::from_be_bytes([
                *bytes.get(at + 5)?,
                *bytes.get(at + 6)?,
            ]));
            let width = usize::from(u16::from_be_bytes([
                *bytes.get(at + 7)?,
                *bytes.get(at + 8)?,
            ]));
            return Some((u32::try_from(width).ok()?, u32::try_from(height).ok()?));
        }
        if length < 2 {
            return None;
        }
        at += 2 + length;
    }
    None
}

/// GIF：`GIF87a` / `GIF89a` + 逻辑屏幕宽高（小端）。
fn gif_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    let magic = bytes.get(..6)?;
    if magic != b"GIF87a" && magic != b"GIF89a" {
        return None;
    }
    Some((le16(bytes, 6)?, le16(bytes, 8)?))
}

/// WebP：RIFF 容器，三种编码各有一种读法。
fn webp_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    if bytes.get(..4)? != b"RIFF" || bytes.get(8..12)? != b"WEBP" {
        return None;
    }
    match bytes.get(12..16)? {
        // VP8X：画布宽高各占 3 字节（小端），存的是"尺寸 - 1"。
        b"VP8X" => {
            let width = le24(bytes, 24)?.checked_add(1)?;
            let height = le24(bytes, 27)?.checked_add(1)?;
            Some((width, height))
        }
        // VP8（有损）：帧头里的宽高是 14 位 + 2 位缩放。
        b"VP8 " => {
            let width = u32::from(u16::from_le_bytes([*bytes.get(26)?, *bytes.get(27)?])) & 0x3fff;
            let height = u32::from(u16::from_le_bytes([*bytes.get(28)?, *bytes.get(29)?])) & 0x3fff;
            Some((width, height))
        }
        // VP8L（无损）：14 位宽高交错编码，读 4 字节再拆。
        b"VP8L" => {
            let bits = u32::from(bytes[21])
                | (u32::from(bytes[22]) << 8)
                | (u32::from(bytes[23]) << 16)
                | (u32::from(bytes[24]) << 24);
            Some(((bits & 0x3fff) + 1, ((bits >> 14) & 0x3fff) + 1))
        }
        _ => None,
    }
}

/// 声明尺寸是否超过 [`MAX_PIXELS`]。读不出尺寸时按"超限"处理。
fn exceeds_pixel_budget(bytes: &[u8]) -> bool {
    match declared_dimensions(bytes) {
        Some((width, height)) => (width as u64) * (height as u64) > MAX_PIXELS,
        None => true,
    }
}

/// 在内存上限内解码；任何一个限制触发都返回 `None`，调用方退回原图。
fn decode_within_limits(bytes: &[u8]) -> Option<image::DynamicImage> {
    let mut reader = image::ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .ok()?;
    let mut limits = image::Limits::default();
    limits.max_alloc = Some(MAX_DECODE_BYTES);
    reader.limits(limits);
    reader.decode().ok()
}

/// 图像是否"实际上不透明"——透明像素占比低于 [`TRANSPARENT_RATIO`]。
///
/// 没有 alpha 通道的图直接算不透明，不做整幅扫描。
fn is_effectively_opaque(image: &image::DynamicImage) -> bool {
    if !image.color().has_alpha() {
        return true;
    }
    let rgba = image.to_rgba8();
    let total = (rgba.width() as u64) * (rgba.height() as u64);
    if total == 0 {
        return true;
    }
    // alpha < 250 才算"真的透明"：255 之外还有 254 这种接近不透明的边缘像素，
    // 它们转 JPEG 也看不出来，不该因此整幅图保留 PNG。
    let translucent = rgba.pixels().filter(|pixel| pixel[3] < 250).count() as u64;
    (translucent as f64 / total as f64) < TRANSPARENT_RATIO
}

/// 压缩后的图片：字节、扩展名、MIME 三者必须一起换，否则库里记的和盘上的不一致。
pub struct Optimized {
    pub bytes: Vec<u8>,
    pub ext: &'static str,
    pub mime: &'static str,
}

impl Optimized {
    /// 原样返回（不解码、不重编码）。
    fn keep(bytes: &[u8], ext: &str, mime: &str) -> Self {
        let ext: &'static str = match ext {
            "png" => "png",
            "jpg" | "jpeg" => "jpg",
            "webp" => "webp",
            "gif" => "gif",
            _ => "png",
        };
        let mime: &'static str = match mime {
            "image/png" => "image/png",
            "image/jpeg" => "image/jpeg",
            "image/webp" => "image/webp",
            "image/gif" => "image/gif",
            _ => "image/png",
        };
        Self {
            bytes: bytes.to_vec(),
            ext,
            mime,
        }
    }
}

/// 对一张来自 .docx 的图片做有损压缩。
///
/// 任何一步失败（不认识的格式、解码出错、编码出错）都退回原图：
/// 用户上传的是一份文档，不该因为其中一张图画质优化失败而整篇上传失败。
pub fn optimize_docx_image(bytes: &[u8], ext: &str, mime: &str) -> Optimized {
    // 动图不碰：只取第一帧会把动图变成静图，那已经不是"压缩"了。
    if ext == "gif" {
        return Optimized::keep(bytes, ext, mime);
    }
    // 小 PNG 不碰：省下的字节不值得换一次重编码。
    if ext == "png" && bytes.len() < KEEP_PNG_BELOW {
        return Optimized::keep(bytes, ext, mime);
    }

    // 解压炸弹防线：先只看文件头。超限（或读不出尺寸）就原样保留，绝不解码。
    if exceeds_pixel_budget(bytes) {
        return Optimized::keep(bytes, ext, mime);
    }

    let Some(dynamic) = decode_within_limits(bytes) else {
        return Optimized::keep(bytes, ext, mime);
    };

    let (width, height) = (dynamic.width(), dynamic.height());
    let needs_resize = width.max(height) > MAX_EDGE;

    let scaled = if needs_resize {
        let scale = MAX_EDGE as f32 / width.max(height) as f32;
        let new_w = ((width as f32 * scale).round() as u32).max(1);
        let new_h = ((height as f32 * scale).round() as u32).max(1);
        dynamic.resize(new_w, new_h, image::imageops::FilterType::Lanczos3)
    } else {
        dynamic
    };

    // 格式判断放在缩放之后：这一步要遍历整幅图的 alpha 通道，
    // 在 1600×1600 上扫是几毫秒，在原始尺寸上扫既慢又多占一份 RGBA 副本。
    let target_jpeg = is_effectively_opaque(&scaled);

    if !needs_resize && !target_jpeg {
        // 真的带透明、又没超过长边上限：原样返回。重编码 PNG 省不下多少字节，
        // 却可能把它的透明边缘重新量化一遍。
        return Optimized::keep(bytes, ext, mime);
    }

    let encoded = if target_jpeg {
        // JPEG 没有透明通道：铺白底，避免透明区域被解成黑色。
        let rgb = flattened_on_white(&scaled);
        let mut out = Vec::new();
        let mut encoder = JpegEncoder::new_with_quality(&mut out, JPEG_QUALITY);
        match encoder.encode_image(&rgb) {
            Ok(()) => Some(out),
            Err(_) => None,
        }
    } else {
        let rgba = scaled.to_rgba8();
        let mut out = Vec::new();
        let encoder = PngEncoder::new(&mut out);
        match encoder.write_image(
            rgba.as_raw(),
            rgba.width(),
            rgba.height(),
            ExtendedColorType::Rgba8,
        ) {
            Ok(()) => Some(out),
            Err(_) => None,
        }
    };

    let Some(out) = encoded else {
        return Optimized::keep(bytes, ext, mime);
    };

    // 压完更大就保留原图：这张图本来就压过了。
    if out.len() >= bytes.len() && !needs_resize {
        return Optimized::keep(bytes, ext, mime);
    }

    if target_jpeg {
        Optimized {
            bytes: out,
            ext: "jpg",
            mime: "image/jpeg",
        }
    } else {
        Optimized {
            bytes: out,
            ext: "png",
            mime: "image/png",
        }
    }
}

/// 把任意图像铺到白底上转成 RGB8（JPEG 编码器的输入）。
fn flattened_on_white(image: &image::DynamicImage) -> image::RgbImage {
    let rgba = image.to_rgba8();
    let (width, height) = rgba.dimensions();
    let mut rgb = image::RgbImage::new(width, height);
    for (x, y, pixel) in rgba.enumerate_pixels() {
        let alpha = pixel[3] as u16;
        let blend = |channel: u8| -> u8 {
            // out = src * a + 255 * (1 - a)
            (((channel as u16 * alpha) + (255 * (255 - alpha))) / 255) as u8
        };
        rgb.put_pixel(
            x,
            y,
            image::Rgb([blend(pixel[0]), blend(pixel[1]), blend(pixel[2])]),
        );
    }
    rgb
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 造一张有噪声的大图：纯色图 JPEG 会压得比 PNG 还小，测不出缩放的效果。
    fn noisy_png(width: u32, height: u32) -> Vec<u8> {
        let mut img = image::RgbImage::new(width, height);
        for (x, y, pixel) in img.enumerate_pixels_mut() {
            let seed = (x * 7 + y * 13) as u8;
            *pixel = image::Rgb([seed, seed.wrapping_mul(3), seed.wrapping_add(90)]);
        }
        let mut out = Vec::new();
        let encoder = PngEncoder::new(&mut out);
        encoder
            .write_image(img.as_raw(), width, height, ExtendedColorType::Rgb8)
            .expect("编码测试图");
        out
    }

    #[test]
    fn large_photo_like_images_are_scaled_and_converted_to_jpeg() {
        let original = noisy_png(3000, 1800);
        let optimized = optimize_docx_image(&original, "png", "image/png");
        assert_eq!(optimized.ext, "jpg");
        assert_eq!(optimized.mime, "image/jpeg");
        assert!(
            optimized.bytes.len() < original.len(),
            "压缩后应该更小：{} -> {}",
            original.len(),
            optimized.bytes.len()
        );
        let decoded = image::load_from_memory(&optimized.bytes).expect("压出来的还能解码");
        assert!(decoded.width().max(decoded.height()) <= MAX_EDGE);
    }

    #[test]
    fn opaque_rgba_screenshots_are_converted_to_jpeg() {
        // Word 里的截图基本都是 RGBA 但 alpha 全满。这条钉住"不要因为有 alpha 通道就
        // 永远跳过压缩"——这正是真实文档里 7 张 PNG 一张都没被压的原因。
        let mut img = image::RgbaImage::new(1400, 900);
        for (x, y, pixel) in img.enumerate_pixels_mut() {
            let seed = (x * 5 + y * 11) as u8;
            *pixel = image::Rgba([seed, seed.wrapping_mul(2), seed.wrapping_add(40), 255]);
        }
        let mut original = Vec::new();
        PngEncoder::new(&mut original)
            .write_image(img.as_raw(), 1400, 900, ExtendedColorType::Rgba8)
            .expect("编码测试图");

        let optimized = optimize_docx_image(&original, "png", "image/png");
        assert_eq!(optimized.ext, "jpg");
        assert!(
            optimized.bytes.len() < original.len(),
            "不该因为带 alpha 通道就放弃压缩：{} -> {}",
            original.len(),
            optimized.bytes.len()
        );
    }

    #[test]
    fn transparent_images_stay_png() {
        let mut img = image::RgbaImage::new(2000, 1000);
        for (x, y, pixel) in img.enumerate_pixels_mut() {
            let a = ((x + y) % 256) as u8;
            *pixel = image::Rgba([200, 120, 60, a]);
        }
        let mut original = Vec::new();
        PngEncoder::new(&mut original)
            .write_image(img.as_raw(), 2000, 1000, ExtendedColorType::Rgba8)
            .expect("编码测试图");
        let optimized = optimize_docx_image(&original, "png", "image/png");
        assert_eq!(optimized.ext, "png");
        assert!(optimized.bytes.len() < original.len());
    }

    #[test]
    fn small_png_is_left_alone() {
        let original = noisy_png(40, 40);
        assert!(original.len() < KEEP_PNG_BELOW);
        let optimized = optimize_docx_image(&original, "png", "image/png");
        assert_eq!(optimized.bytes, original);
        assert_eq!(optimized.ext, "png");
    }

    #[test]
    fn animated_gif_is_not_flattened() {
        // 一个最小的 GIF 头就够了：函数在看到 gif 时立刻返回，不去解码。
        let gif = b"GIF89a\x01\x00\x01\x00\x00\x00\x00;".to_vec();
        let optimized = optimize_docx_image(&gif, "gif", "image/gif");
        assert_eq!(optimized.bytes, gif);
        assert_eq!(optimized.ext, "gif");
    }

    #[test]
    fn garbage_falls_back_to_the_original_bytes() {
        let junk = b"not an image at all".to_vec();
        let optimized = optimize_docx_image(&junk, "png", "image/png");
        assert_eq!(optimized.bytes, junk);
        assert_eq!(optimized.ext, "png");
        assert_eq!(optimized.mime, "image/png");
    }

    #[test]
    fn the_image_format_constant_we_rely_on_is_known() {
        // 这条只是把"我们用 image crate 解码"这件事钉住：png/jpeg 都必须被识别。
        assert_eq!(
            image::guess_format(&noisy_png(8, 8)).ok(),
            Some(image::ImageFormat::Png)
        );
    }

    /// 造一个只有文件头、没有像素数据的 PNG：声明多大的尺寸都只花几十字节。
    ///
    /// 这正是解压炸弹的形状——2 MB 的文件声明 20000×20000，解码要 1.6 GB。
    fn png_header_only(width: u32, height: u32) -> Vec<u8> {
        fn crc32(bytes: &[u8]) -> u32 {
            let mut crc = 0xffff_ffffu32;
            for byte in bytes {
                crc ^= u32::from(*byte);
                for _ in 0..8 {
                    let mask = (crc & 1).wrapping_neg();
                    crc = (crc >> 1) ^ (0xedb8_8320 & mask);
                }
            }
            !crc
        }

        let mut out = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
        let mut ihdr = Vec::new();
        ihdr.extend_from_slice(&width.to_be_bytes());
        ihdr.extend_from_slice(&height.to_be_bytes());
        ihdr.extend_from_slice(&[8, 6, 0, 0, 0]); // 8 bit RGBA，无隔行
        out.extend_from_slice(&13u32.to_be_bytes());
        out.extend_from_slice(b"IHDR");
        out.extend_from_slice(&ihdr);
        out.extend_from_slice(
            &crc32(&{
                let mut tagged = b"IHDR".to_vec();
                tagged.extend_from_slice(&ihdr);
                tagged
            })
            .to_be_bytes(),
        );
        out
    }

    #[test]
    fn declared_dimensions_are_read_from_the_header_without_decoding() {
        let file = png_header_only(2000, 1000);
        assert_eq!(declared_dimensions(&file), Some((2000, 1000)));
        assert!(!exceeds_pixel_budget(&file));
    }

    #[test]
    fn a_decompression_bomb_is_left_alone_instead_of_decoded() {
        // 20000×20000 = 4 亿像素，解码要 1.6 GB；这台机器的上限是 700 MB。
        let bomb = png_header_only(20_000, 20_000);
        assert!(exceeds_pixel_budget(&bomb), "声明尺寸必须被判定为超限");
        let optimized = optimize_docx_image(&bomb, "png", "image/png");
        // 不解码 = 原样返回，进程活着比这张图被压小重要得多。
        assert_eq!(optimized.bytes, bomb);
        assert_eq!(optimized.ext, "png");
    }

    #[test]
    fn unreadable_dimensions_count_as_over_budget() {
        // 读不出尺寸就无从判断会不会炸，按超限处理（也不解码）。
        assert!(exceeds_pixel_budget(b"not an image at all"));
    }

    /// 只有 APP0 + SOF0 的 JPEG 头，同样不花什么字节就能声明任意尺寸。
    fn jpeg_header_only(width: u16, height: u16) -> Vec<u8> {
        let mut out = vec![0xff, 0xd8];
        // APP0/JFIF：段长 16
        out.extend_from_slice(&[0xff, 0xe0, 0x00, 0x10]);
        out.extend_from_slice(b"JFIF\0");
        out.extend_from_slice(&[0x01, 0x01, 0x00, 0x00, 0x01, 0x00, 0x01, 0x00, 0x00]);
        // SOF0：段长 17（8 位精度 + 高 + 宽 + 3 个分量）
        out.extend_from_slice(&[0xff, 0xc0, 0x00, 0x11, 0x08]);
        out.extend_from_slice(&height.to_be_bytes());
        out.extend_from_slice(&width.to_be_bytes());
        out.extend_from_slice(&[0x03, 0x01, 0x22, 0x00, 0x02, 0x11, 0x01, 0x03, 0x11, 0x01]);
        out.extend_from_slice(&[0xff, 0xd9]);
        out
    }

    #[test]
    fn jpeg_dimensions_come_from_the_frame_header() {
        let file = jpeg_header_only(3200, 2400);
        assert_eq!(declared_dimensions(&file), Some((3200, 2400)));
        assert!(!exceeds_pixel_budget(&file));

        let bomb = jpeg_header_only(20000, 20000);
        assert!(exceeds_pixel_budget(&bomb));
        assert_eq!(optimize_docx_image(&bomb, "jpeg", "image/jpeg").bytes, bomb);
    }

    /// VP8X 的宽高是 3 字节小端、存"尺寸 - 1"。
    fn webp_vp8x(width: u32, height: u32) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(b"RIFF");
        out.extend_from_slice(&0u32.to_le_bytes()); // 长度不参与解析
        out.extend_from_slice(b"WEBP");
        out.extend_from_slice(b"VP8X");
        out.extend_from_slice(&10u32.to_le_bytes());
        out.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]);
        let w = width - 1;
        let h = height - 1;
        out.extend_from_slice(&w.to_le_bytes()[..3]);
        out.extend_from_slice(&h.to_le_bytes()[..3]);
        out
    }

    #[test]
    fn webp_canvas_size_is_read_and_checked() {
        let file = webp_vp8x(1920, 1080);
        assert_eq!(declared_dimensions(&file), Some((1920, 1080)));
        assert!(exceeds_pixel_budget(&webp_vp8x(16000, 16000)));
    }

    #[test]
    fn a_real_image_within_budget_still_gets_optimised() {
        // 反向对照：防线不能把所有图都当成炸弹。
        let original = noisy_png(1200, 800);
        assert!(!exceeds_pixel_budget(&original));
        let optimized = optimize_docx_image(&original, "png", "image/png");
        assert_eq!(optimized.ext, "jpg");
        assert!(optimized.bytes.len() < original.len());
    }
}
