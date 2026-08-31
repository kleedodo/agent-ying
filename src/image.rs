//! 图片处理：大图压缩到 256KB 以下（喂给视觉模型前）、MIME 到 rig 图片类型的映射。

use rig::completion::message::ImageMediaType;

/// 大图压缩上限：256KB。超过则重编码为 JPEG 并逐步降质/缩小，直到不超过该值。
pub(crate) const MAX_IMAGE_BYTES: usize = 256 * 1024;

/// 若图片超过 256KB，则解码后重编码为 JPEG，逐步降低质量与尺寸，直到不超过上限。
/// 返回 （新字节， 对应 media_type）。未超限时原样返回。
pub(crate) fn compress_image(
    bytes: Vec<u8>,
    media_type: ImageMediaType,
) -> (Vec<u8>, ImageMediaType) {
    if bytes.len() <= MAX_IMAGE_BYTES {
        return (bytes, media_type);
    }

    let Ok(img) = image::load_from_memory(&bytes) else {
        // 解不了（如未启用解码器的 heic/svg），退而求其次：原样返回
        return (bytes, media_type);
    };

    let base_w = img.width() as f64;
    let base_h = img.height() as f64;

    // 从大到小尝试：每个尺寸只缩放/转换一次，再在该尺寸上依次降质量。
    // 这样避免对同一尺寸反复做昂贵的重采样。
    for scale in [1.0f64, 0.85, 0.7, 0.55, 0.4, 0.3, 0.2] {
        let rgb = if scale >= 1.0 {
            to_rgb8_white_bg(&img)
        } else {
            let w = (base_w * scale).max(1.0) as u32;
            let h = (base_h * scale).max(1.0) as u32;
            // Triangle 滤镜比 Lanczos3 快得多，对喂给视觉模型已足够
            let resized = img.resize_exact(w, h, image::imageops::FilterType::Triangle);
            to_rgb8_white_bg(&resized)
        };
        for quality in [85u8, 75, 65, 55, 45, 35] {
            let mut buf = Vec::new();
            let mut encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, quality);
            if encoder.encode_image(&rgb).is_ok() && buf.len() <= MAX_IMAGE_BYTES {
                return (buf, ImageMediaType::JPEG);
            }
        }
    }

    // 兜底：最小尺寸最低质量（几乎不可能还超，但保证有返回值）
    let rgb = to_rgb8_white_bg(&img.resize_exact(64, 64, image::imageops::FilterType::Triangle));
    let mut buf = Vec::new();
    let mut encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, 30);
    encoder.encode_image(&rgb).ok();
    (buf, ImageMediaType::JPEG)
}

/// 转成 RGB8；带透明通道（PNG 等）时合成到白底，避免透明区变黑。
fn to_rgb8_white_bg(img: &image::DynamicImage) -> image::RgbImage {
    match img {
        image::DynamicImage::ImageRgba8(rgba) => {
            let mut out = image::RgbImage::new(rgba.width(), rgba.height());
            for (x, y, px) in rgba.enumerate_pixels() {
                let a = px[3] as f32 / 255.0;
                let r = (px[0] as f32 * a + 255.0 * (1.0 - a)) as u8;
                let g = (px[1] as f32 * a + 255.0 * (1.0 - a)) as u8;
                let b = (px[2] as f32 * a + 255.0 * (1.0 - a)) as u8;
                out.put_pixel(x, y, image::Rgb([r, g, b]));
            }
            out
        }
        other => other.to_rgb8(),
    }
}

/// 把 Telegram 的 MIME 字符串映射到 rig 支持的图片类型，不支持返回 None。
/// 注意：与 `crate::media::ext_for_mime` 的 MIME 列表保持同步。
pub(crate) fn mime_to_image_media_type(mime: &str) -> Option<ImageMediaType> {
    match mime.to_ascii_lowercase().as_str() {
        "image/jpeg" | "image/jpg" => Some(ImageMediaType::JPEG),
        "image/png" => Some(ImageMediaType::PNG),
        "image/gif" => Some(ImageMediaType::GIF),
        "image/webp" => Some(ImageMediaType::WEBP),
        "image/heic" => Some(ImageMediaType::HEIC),
        "image/heif" => Some(ImageMediaType::HEIF),
        "image/svg+xml" => Some(ImageMediaType::SVG),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::human_size;

    /// 造一张高频噪点图，保证高质 JPEG 编码后远超 256KB。
    fn noisy_jpeg(w: u32, h: u32, quality: u8) -> Vec<u8> {
        let mut img = image::RgbImage::new(w, h);
        for (x, y, px) in img.enumerate_pixels_mut() {
            let v = (((x as u64) ^ (y as u64).wrapping_mul(2654435761)) % 256) as u8;
            *px = image::Rgb([v, v.wrapping_add(1), v.wrapping_add(2)]);
        }
        let mut buf = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, quality)
            .encode_image(&img)
            .unwrap();
        buf
    }

    #[test]
    fn large_image_compressed_under_limit() {
        let orig = noisy_jpeg(1200, 1200, 95);
        assert!(
            orig.len() > MAX_IMAGE_BYTES,
            "测试图应超 256KB，实际 {}",
            human_size(orig.len() as u64)
        );

        let (out, mt) = compress_image(orig, ImageMediaType::JPEG);
        assert!(
            out.len() <= MAX_IMAGE_BYTES,
            "压缩后 {} 仍超 256KB",
            human_size(out.len() as u64)
        );
        assert!(matches!(mt, ImageMediaType::JPEG));
        // 结果仍是合法图片
        assert!(image::load_from_memory(&out).is_ok());
    }

    #[test]
    fn small_image_passes_through_unchanged() {
        let small = b"tiny-bytes-well-under-limit".to_vec();
        let (out, mt) = compress_image(small.clone(), ImageMediaType::PNG);
        assert_eq!(out, small);
        assert!(matches!(mt, ImageMediaType::PNG));
    }

    #[test]
    fn mime_mapping() {
        assert_eq!(
            mime_to_image_media_type("image/PNG"),
            Some(ImageMediaType::PNG)
        );
        assert_eq!(
            mime_to_image_media_type("image/jpeg"),
            Some(ImageMediaType::JPEG)
        );
        assert_eq!(mime_to_image_media_type("application/pdf"), None);
    }
}
