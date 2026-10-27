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

/// 长边上限。1600 是常见的正文插图宽度，再大在页面上也显示不出来。
pub const MAX_EDGE: u32 = 1600;

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

    let Ok(dynamic) = image::load_from_memory(bytes) else {
        return Optimized::keep(bytes, ext, mime);
    };

    let (width, height) = (dynamic.width(), dynamic.height());
    let needs_resize = width.max(height) > MAX_EDGE;

    // 先决定用什么格式：几乎不透明的转 JPEG，真有透明区域的留 PNG。
    //
    // 判据不能是 `color().has_alpha()`：Word 里的截图基本都是 RGBA，只是 alpha 全满。
    // 按"有透明通道"处理，这类图就永远不会被压，用户看到的仍是几百 KB 的 PNG。
    // 所以看的是**实际透明像素的占比**，低于 [`TRANSPARENT_RATIO`] 就当不透明。
    let target_jpeg = is_effectively_opaque(&dynamic);

    if !needs_resize && !target_jpeg {
        // 真的带透明、又没超过长边上限：原样返回。重编码 PNG 省不下多少字节，
        // 却可能把它的透明边缘重新量化一遍。
        return Optimized::keep(bytes, ext, mime);
    }

    let scaled = if needs_resize {
        let scale = MAX_EDGE as f32 / width.max(height) as f32;
        let new_w = ((width as f32 * scale).round() as u32).max(1);
        let new_h = ((height as f32 * scale).round() as u32).max(1);
        dynamic.resize(new_w, new_h, image::imageops::FilterType::Lanczos3)
    } else {
        dynamic
    };

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
}
