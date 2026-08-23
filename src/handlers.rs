//! Telegram 更新处理器:文本消息 → 跑 agent;按钮回调 → 决定工具审批。

use std::path::{Path, PathBuf};

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use rig::OneOrMany;
use rig::completion::message::{
    DocumentSourceKind, Image as RigImage, ImageMediaType, Text as RigText, UserContent,
};
use rig::completion::{Chat, Message as RigMessage};
use teloxide::net::Download;
use teloxide::prelude::*;
use teloxide::types::{FileId, InlineKeyboardMarkup};

use crate::{AppState, approval::approval_body};

pub type HandlerResult = Result<(), Box<dyn std::error::Error + Send + Sync>>;

/// 用户发来的图片的临时目录:`$TMPDIR/agent-ying/`
pub(crate) fn temp_image_dir() -> PathBuf {
    std::env::temp_dir().join("agent-ying")
}

/// 判断路径是否位于临时图片目录内(vision 工具据此免审批并在调用后自动删除)
pub(crate) fn is_temp_image_path(path: &str) -> bool {
    Path::new(path).starts_with(temp_image_dir())
}

/// 把用户发来的图片存为临时文件,命名 `<chat_id>-<消息 id>.<ext>`
async fn save_temp_image(
    msg: &Message,
    bytes: &[u8],
    ext: &str,
) -> Result<PathBuf, Box<dyn std::error::Error + Send + Sync>> {
    let dir = temp_image_dir();
    tokio::fs::create_dir_all(&dir).await?;
    let path = dir.join(format!("{}-{}.{}", msg.chat.id.0, msg.id, ext));
    tokio::fs::write(&path, bytes).await?;
    tracing::info!("用户发来的图片已存到临时文件: {}", path.display());
    Ok(path)
}

/// 转发给 vision 时,图片对应的用户消息:
/// 告诉主 agent 图片已存到哪个临时文件,请它调用 vision 工具查看。
fn temp_image_text_message(caption: String, path: PathBuf) -> RigMessage {
    let text = if caption.trim().is_empty() {
        format!(
            "用户发来一张图片,已保存到 {},请用 vision 工具查看它。",
            path.display()
        )
    } else {
        format!(
            "用户发来一张图片并附说明「{}」,图片已保存到 {},请用 vision 工具查看它。",
            caption,
            path.display()
        )
    };
    RigMessage::User {
        content: OneOrMany::one(UserContent::Text(RigText {
            text,
            additional_params: None,
        })),
    }
}

/// 把 rig 的图片类型映射为临时文件扩展名(压缩可能改变格式,扩展名以压缩后的 media type 为准)。
fn ext_for_media_type(mt: ImageMediaType) -> &'static str {
    match mt {
        ImageMediaType::JPEG => "jpg",
        ImageMediaType::PNG => "png",
        ImageMediaType::GIF => "gif",
        ImageMediaType::WEBP => "webp",
        ImageMediaType::HEIC => "heic",
        ImageMediaType::HEIF => "heif",
        ImageMediaType::SVG => "svg",
    }
}

/// 从 Telegram 消息构建发给模型的用户消息。
/// 支持:图片(photo,或 image/* 的 document,可带说明文字)、纯文本。
/// `forward_to_vision` 为 true 时,图片存到临时文件,
/// 用文本提示主 agent 调 vision 工具;否则图片直接内嵌、原样发给上游。
/// 返回 None 表示既不是文本也不是受支持的图片(如贴纸、视频等)。
async fn build_user_message(
    bot: &Bot,
    msg: &Message,
    forward_to_vision: bool,
) -> Result<Option<RigMessage>, Box<dyn std::error::Error + Send + Sync>> {
    // 1. 图片:photo 优先(总是 JPEG),其次 image/* 的 document
    // Telegram 的 photo 带多档尺寸,取宽度 ≤1080 的最大档省流量;没有则退回最大档
    if let Some(photos) = msg.photo().as_ref()
        && let Some(photo) = photos.iter().rev().find(|p| p.width <= 1080).or_else(|| photos.last())
    {
        let bytes = download_file_bytes(bot, &photo.file.id).await?;
        let caption = msg.caption().map(str::to_string).unwrap_or_default();
        if !forward_to_vision {
            return Ok(Some(image_user_message(
                caption,
                bytes,
                ImageMediaType::JPEG,
            )));
        }
        // 先压缩再存临时文件,vision 看图时就不用再压缩
        let (bytes, media_type) = compress_image(bytes, ImageMediaType::JPEG);
        let path = save_temp_image(msg, &bytes, ext_for_media_type(media_type)).await?;
        return Ok(Some(temp_image_text_message(caption, path)));
    }
    if let Some(doc) = msg.document().as_ref()
        && let Some(mime) = doc.mime_type.as_ref()
        && let Some(media_type) = mime_to_image_media_type(mime.as_ref())
    {
        let bytes = download_file_bytes(bot, &doc.file.id).await?;
        let caption = msg.caption().map(str::to_string).unwrap_or_default();
        if !forward_to_vision {
            return Ok(Some(image_user_message(caption, bytes, media_type)));
        }
        // 先压缩再存临时文件,vision 看图时就不用再压缩
        let (bytes, media_type) = compress_image(bytes, media_type);
        let path = save_temp_image(msg, &bytes, ext_for_media_type(media_type)).await?;
        return Ok(Some(temp_image_text_message(caption, path)));
    }

    // 2. 纯文本
    if let Some(text) = msg.text().map(str::to_owned) {
        return Ok(Some(RigMessage::User {
            content: OneOrMany::one(UserContent::Text(RigText {
                text,
                additional_params: None,
            })),
        }));
    }

    Ok(None)
}

/// 下载 Telegram 文件为字节。
async fn download_file_bytes(
    bot: &Bot,
    file_id: &FileId,
) -> Result<Vec<u8>, Box<dyn std::error::Error + Send + Sync>> {
    let file = bot.get_file(file_id.clone()).await?;
    let mut buf: Vec<u8> = Vec::new();
    bot.download_file(&file.path, &mut buf).await?;
    Ok(buf)
}

/// 大图压缩上限:256KB。超过则重编码为 JPEG 并逐步降质/缩小,直到不超过该值。
const MAX_IMAGE_BYTES: usize = 256 * 1024;

/// 若图片超过 256KB,则解码后重编码为 JPEG,逐步降低质量与尺寸,直到不超过上限。
/// 返回 (新字节, 对应 media_type)。未超限时原样返回。
pub(crate) fn compress_image(
    bytes: Vec<u8>,
    media_type: ImageMediaType,
) -> (Vec<u8>, ImageMediaType) {
    if bytes.len() <= MAX_IMAGE_BYTES {
        return (bytes, media_type);
    }

    let Ok(img) = image::load_from_memory(&bytes) else {
        // 解不了(如未启用解码器的 heic/svg),退而求其次:原样返回
        return (bytes, media_type);
    };

    let base_w = img.width() as f64;
    let base_h = img.height() as f64;

    // 从大到小尝试:每个尺寸只缩放/转换一次,再在该尺寸上依次降质量。
    // 这样避免对同一尺寸反复做昂贵的重采样。
    for scale in [1.0f64, 0.85, 0.7, 0.55, 0.4, 0.3, 0.2] {
        let rgb = if scale >= 1.0 {
            to_rgb8_white_bg(&img)
        } else {
            let w = (base_w * scale).max(1.0) as u32;
            let h = (base_h * scale).max(1.0) as u32;
            // Triangle 滤镜比 Lanczos3 快得多,对喂给视觉模型已足够
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

    // 兜底:最小尺寸最低质量(几乎不可能还超,但保证有返回值)
    let rgb = to_rgb8_white_bg(&img.resize_exact(64, 64, image::imageops::FilterType::Triangle));
    let mut buf = Vec::new();
    let mut encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, 30);
    encoder.encode_image(&rgb).ok();
    (buf, ImageMediaType::JPEG)
}

/// 转成 RGB8;带透明通道(PNG 等)时合成到白底,避免透明区变黑。
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

/// 把 Telegram 的 MIME 字符串映射到 rig 支持的图片类型,不支持返回 None。
fn mime_to_image_media_type(mime: &str) -> Option<ImageMediaType> {
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

/// 构造「说明文字 + 图片」的用户消息。
fn image_user_message(caption: String, bytes: Vec<u8>, media_type: ImageMediaType) -> RigMessage {
    // 大图先压缩到 256KB 以下,再 base64
    let (bytes, media_type) = compress_image(bytes, media_type);
    let b64 = BASE64.encode(&bytes);
    let image = UserContent::Image(RigImage {
        data: DocumentSourceKind::base64(&b64),
        media_type: Some(media_type),
        detail: None,
        additional_params: None,
    });
    let text = if caption.trim().is_empty() {
        "(用户发了一张图片)".to_string()
    } else {
        caption
    };
    let content = OneOrMany::many(vec![
        UserContent::Text(RigText {
            text,
            additional_params: None,
        }),
        image,
    ])
    .expect("至少包含文本和图片两项内容");
    RigMessage::User { content }
}

/// 处理用户发来的文本消息:跑一轮 agent 对话(带多轮历史)。
pub async fn on_message(state: AppState, msg: Message) -> HandlerResult {
    // 只响应配置里允许的用户
    let Some(from) = msg.from.as_ref() else {
        state
            .bot
            .send_message(msg.chat.id, "🚫 无法识别发送者,请私聊我。")
            .await?;
        return Ok(());
    };
    if !state.allows_user(from.id) {
        state
            .bot
            .send_message(msg.chat.id, "🚫 未授权用户,请找 bot 主人加白名单。")
            .await?;
        return Ok(());
    }

    let text = msg.text().map(str::to_owned);

    // 简单的 /start、/help、/new 命令(文本消息)
    if let Some(t) = &text {
        if t.starts_with("/start") || t.starts_with("/help") {
            state
                .bot
                .send_message(
                    msg.chat.id,
                    "👋 我是 ying!直接发文本或图片就行。\n\
                     我可以用 `bash` 跑命令,也能看你发的图片,\n\
                     还能看电脑上的图片(vision)、把文件直接发给你(send_file)。\n\
                     每次调用工具前都会发按钮请你明确同意。\n\
                     发送 /new 可以开启新会话(清空对话历史)。",
                )
                .await?;
            return Ok(());
        }
        if t.starts_with("/new") {
            let mut map = state.histories.lock().await;
            map.remove(&msg.chat.id);
            state
                .bot
                .send_message(msg.chat.id, "🆕 新会话已开始,之前的对话历史已清空。")
                .await?;
            return Ok(());
        }
    }

    // 构建发给模型的用户消息:纯文本,或图片(可带说明文字)
    // forward_to_vision 且 vision 已启用时,图片转存临时文件并提示调 vision 工具;
    // 否则(包括 vision 未启用的情况)图片原样内嵌发给上游
    let forward_to_vision = state.forward_to_vision && state.vision_client.is_some();
    let Some(user_msg) = build_user_message(&state.bot, &msg, forward_to_vision).await? else {
        state
            .bot
            .send_message(msg.chat.id, "请发送文本消息或图片 🙏")
            .await?;
        return Ok(());
    };

    tracing::info!(
        "收到消息: chat={} user={:?} text={:?}",
        msg.chat.id,
        msg.from.as_ref().map(|f| f.id),
        text,
    );

    let chat_id = msg.chat.id;
    let agent = state.agent_for(chat_id);

    state.bot.send_message(chat_id, "🤔 思考中…").await?;

    // 每个 chat 单独维护多轮对话历史(先取出再写回)
    let mut history: Vec<RigMessage> = {
        let map = state.histories.lock().await;
        map.get(&chat_id).cloned().unwrap_or_default()
    };

    match agent.chat(user_msg, &mut history).await {
        Ok(reply) => {
            tracing::info!(
                "Agent 回复完成: chat={} 共 {} 轮历史",
                chat_id,
                history.len()
            );
            state.bot.send_message(chat_id, reply).await?;
        }
        Err(e) => {
            tracing::error!("Agent 出错: chat={} {e}", chat_id);
            state
                .bot
                .send_message(chat_id, format!("⚠️ Agent 出错: {e}"))
                .await?;
        }
    }

    {
        let mut map = state.histories.lock().await;
        map.insert(chat_id, history);
    }
    Ok(())
}

/// 兜底分支:打印没被上面任何 handler 匹配的 update,方便排查丢失的回调等。
pub async fn on_unmatched(update: Update) -> HandlerResult {
    tracing::info!(
        "收到未匹配的 update: id={} kind={:?}",
        update.id.0,
        update.kind
    );
    Ok(())
}

/// 把审批消息改成「已决定」状态时,保留原来的工具/命令信息,
/// 方便用户回头看当时批了什么。
fn decided_text(label: &str, original: &str) -> String {
    let body = approval_body(original);
    if body.is_empty() {
        label.to_string()
    } else {
        format!("{label}\n\n{body}")
    }
}

/// 处理「同意 / 拒绝」按钮回调。
pub async fn on_callback(state: AppState, q: CallbackQuery) -> HandlerResult {
    tracing::info!("收到回调: data={:?}", q.data);

    // 只响应配置里允许的用户
    if !state.allows_user(q.from.id) {
        tracing::info!("未授权用户点击按钮: {:?}", q.from.id);
        let _ = state
            .bot
            .answer_callback_query(q.id.clone())
            .text("🚫 未授权用户")
            .await;
        return Ok(());
    }

    let Some((action, id)) = q.data.as_deref().and_then(|d| d.split_once(':')) else {
        tracing::warn!("回调 data 解析失败: {:?}", q.data);
        let _ = state
            .bot
            .answer_callback_query(q.id.clone())
            .text("⚠️ 按钮数据无效")
            .await;
        return Ok(());
    };

    let approve = match action {
        "approve" => true,
        "deny" => false,
        other => {
            tracing::warn!("未知 action: {other}");
            let _ = state
                .bot
                .answer_callback_query(q.id.clone())
                .text("⚠️ 未知按钮")
                .await;
            return Ok(());
        }
    };

    // resolve 后 send 仍可能失败:点击恰好落在超时边界,等待方已把 receiver 丢掉。
    // 这时按「已过期」处理,免得给用户一个假的「已同意」。
    let resolved = state
        .approvals
        .resolve(id)
        .await
        .and_then(|tx| tx.send(approve).ok())
        .is_some();
    match resolved {
        true => {
            tracing::info!("审批决定: {} → {}", action, id);
            // 把审批消息改成「已同意 / 已拒绝」并移除按钮,避免重复点击;
            // 保留命令详情,方便回看
            if let Some(m) = q.message.as_ref().and_then(|m| m.regular_message()) {
                let label = if approve {
                    "✅ 已同意"
                } else {
                    "❌ 已拒绝"
                };
                let text = m.text().unwrap_or_default();
                let _ = state
                    .bot
                    .edit_message_text(m.chat.id, m.id, decided_text(label, text))
                    .reply_markup(InlineKeyboardMarkup {
                        inline_keyboard: vec![],
                    })
                    .await;
            }
        }
        // 常见原因:重复点击,或点击的是上一次 bot 进程留下的旧按钮(内存里的待审批表已清空)
        false => {
            tracing::warn!("找不到待审批项(可能已处理或已过期): {} → {}", action, id);
            // 直接把旧消息改掉并摘掉按钮,给用户可见的反馈(同样保留命令详情)
            if let Some(m) = q.message.as_ref().and_then(|m| m.regular_message()) {
                let text = m.text().unwrap_or_default();
                let _ = state
                    .bot
                    .edit_message_text(
                        m.chat.id,
                        m.id,
                        decided_text("⏳ 该审批已处理或已过期", text),
                    )
                    .reply_markup(InlineKeyboardMarkup {
                        inline_keyboard: vec![],
                    })
                    .await;
            }
            let _ = state
                .bot
                .answer_callback_query(q.id.clone())
                .text("⏳ 该按钮已处理或已过期")
                .await;
        }
    }

    let _ = state.bot.answer_callback_query(q.id.clone()).await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 造一张高频噪点图,保证高质 JPEG 编码后远超 256KB。
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
            "测试图应超 256KB,实际 {}",
            crate::tools::human_size(orig.len() as u64)
        );

        let (out, mt) = compress_image(orig, ImageMediaType::JPEG);
        assert!(
            out.len() <= MAX_IMAGE_BYTES,
            "压缩后 {} 仍超 256KB",
            crate::tools::human_size(out.len() as u64)
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
}
