//! 从 Telegram 消息构建发给模型的用户消息（`RigMessage`）。

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use rig::completion::Message as RigMessage;
use rig::completion::message::{
    DocumentSourceKind, Image as RigImage, ImageMediaType, Text as RigText, UserContent,
};
use teloxide::prelude::*;

use crate::image::{compress_image, mime_to_image_media_type};
use crate::journal::SessionFile;
use crate::media::{download_file_bytes, media_download_note, save_media_image};
use crate::tools::human_size;

/// 从 Telegram 消息构建发给模型的用户消息。
/// 支持：图片（photo，或 image/* 的 document，可带说明文字）、文档、视频、音频、纯文本。
/// 图片：`forward_to_vision` 为 true 时存到会话 media/ 目录并用文本提示主 agent 调 vision 工具；
/// 否则直接内嵌、原样发给上游。
/// 文档/视频/音频：≤ 50MB 时自动下载到会话的 media/ 目录，并把落盘路径与大小告诉主 agent；
/// 超过 50MB 只把元数据（文件名、大小、消息 ID，以及说明文字 caption）以文本形式告诉主 agent。
/// 所有文件类消息的文本里都会带上消息 ID 以便追溯。
/// 返回 None 表示既不是文本也不是受支持的文件类型（如贴纸等）。
pub(crate) async fn build_user_message(
    bot: &Bot,
    msg: &Message,
    session: &SessionFile,
    forward_to_vision: bool,
) -> Result<Option<RigMessage>, Box<dyn std::error::Error + Send + Sync>> {
    let msg_id = msg.id.0;
    let caption = msg.caption().map(str::to_string).unwrap_or_default();

    // 1. 图片：photo 优先（总是 JPEG），其次 image/* 的 document
    // Telegram 的 photo 带多档尺寸，取宽度 ≤1080 的最大档省流量；没有则退回最大档
    if let Some(photos) = msg.photo().as_ref()
        && let Some(photo) = photos
            .iter()
            .rev()
            .find(|p| p.width <= 1080)
            .or_else(|| photos.last())
    {
        let bytes = download_file_bytes(bot, &photo.file.id).await?;
        return Ok(Some(
            build_image_message(
                session,
                forward_to_vision,
                ImageSource {
                    msg_id,
                    caption,
                    bytes,
                    media_type: ImageMediaType::JPEG,
                    name: "photo".to_string(),
                    mime: "image/jpeg".to_string(),
                },
            )
            .await?,
        ));
    }
    if let Some(doc) = msg.document() {
        let mime = doc
            .mime_type
            .as_ref()
            .map(|m| m.as_ref().to_string())
            .unwrap_or_default();
        let name = doc
            .file_name
            .clone()
            .unwrap_or_else(|| "未知文件".to_string());
        if let Some(media_type) = mime_to_image_media_type(&mime) {
            let bytes = download_file_bytes(bot, &doc.file.id).await?;
            return Ok(Some(
                build_image_message(
                    session,
                    forward_to_vision,
                    ImageSource {
                        msg_id,
                        caption,
                        bytes,
                        media_type,
                        name,
                        mime,
                    },
                )
                .await?,
            ));
        }
        // 非图片文档：≤50MB 自动下载到会话 media/ 目录并告知路径，超限只传元数据
        let size = human_size(doc.file.size as u64);
        let note = media_download_note(
            bot,
            session,
            &doc.file.id,
            &name,
            &mime,
            doc.file.size as u64,
        )
        .await;
        return Ok(Some(text_user_message(with_caption(
            format!("用户发来一个文档 `{name}`(MIME: {mime}，大小 {size}，消息 ID {msg_id}){note}"),
            &caption,
        ))));
    }

    // 2. 视频 / 音频：同样 ≤50MB 自动下载，超限只传元数据
    if let Some(v) = msg.video() {
        let name = v.file_name.clone().unwrap_or_else(|| "视频".to_string());
        let mime = v
            .mime_type
            .as_ref()
            .map(|m| m.as_ref().to_string())
            .unwrap_or_else(|| "video/mp4".to_string());
        let size = human_size(v.file.size as u64);
        let note =
            media_download_note(bot, session, &v.file.id, &name, &mime, v.file.size as u64).await;
        return Ok(Some(text_user_message(with_caption(
            format!(
                "用户发来一个视频 `{name}`（大小 {size}，时长 {}s，消息 ID {msg_id}）{note}",
                v.duration.seconds()
            ),
            &caption,
        ))));
    }
    if let Some(a) = msg.audio() {
        let title = a.title.clone().unwrap_or_else(|| "音频".to_string());
        let mime = a
            .mime_type
            .as_ref()
            .map(|m| m.as_ref().to_string())
            .unwrap_or_else(|| "audio/mpeg".to_string());
        let size = human_size(a.file.size as u64);
        let note =
            media_download_note(bot, session, &a.file.id, &title, &mime, a.file.size as u64).await;
        return Ok(Some(text_user_message(with_caption(
            format!("用户发来一段音频 `{title}`（大小 {size}，消息 ID {msg_id}）{note}"),
            &caption,
        ))));
    }

    // 3. 纯文本
    if let Some(text) = msg.text().map(str::to_owned) {
        return Ok(Some(text_user_message(text)));
    }

    Ok(None)
}

/// 图片消息素材：已下载的字节 + 元数据（名称/MIME 用于落盘 media/ 目录命名）
struct ImageSource {
    /// 消息 ID（文本里带上以便追溯）
    msg_id: i32,
    /// 图片说明文字（caption，可为空）
    caption: String,
    /// 图片字节
    bytes: Vec<u8>,
    media_type: ImageMediaType,
    /// 原文件名（落盘 media/ 目录用）
    name: String,
    /// MIME 类型（落盘 media/ 目录时推断扩展名用）
    mime: String,
}

/// 图片消息的统一处理：
/// `forward_to_vision` 时先压缩再存到会话 media/ 目录（vision 看图时就不用再压缩），
/// 并生成提示主 agent 调 vision 工具的文本消息；
/// 否则压缩后 base64 内嵌、原样发给上游。
async fn build_image_message(
    session: &SessionFile,
    forward_to_vision: bool,
    img: ImageSource,
) -> Result<RigMessage, Box<dyn std::error::Error + Send + Sync>> {
    if !forward_to_vision {
        let text = if img.caption.trim().is_empty() {
            format!("（用户发了一张图片，消息 ID {}）", img.msg_id)
        } else {
            format!("{}（消息 ID {}）", img.caption, img.msg_id)
        };
        return Ok(image_user_message(text, img.bytes, img.media_type));
    }
    let (bytes, _media_type) = compress_image(img.bytes, img.media_type);
    let path = save_media_image(session, &bytes, &img.name, &img.mime).await?;
    let size = human_size(bytes.len() as u64);
    Ok(media_image_text_message(
        img.msg_id,
        img.caption,
        path,
        size,
    ))
}

/// 转发给 vision 时，图片对应的用户消息：
/// 告诉主 agent 图片已存到哪个文件（及大小），请它调用 vision 工具查看；
/// 同时带上消息 ID 以便追溯。
fn media_image_text_message(
    msg_id: i32,
    caption: String,
    path: std::path::PathBuf,
    size: String,
) -> RigMessage {
    let text = if caption.trim().is_empty() {
        format!(
            "用户发来一张图片（消息 ID {msg_id}），已保存到 {}（大小 {size}），请用 vision 工具查看它。",
            path.display()
        )
    } else {
        format!(
            "用户发来一张图片并附说明「{}」（消息 ID {msg_id}），图片已保存到 {}（大小 {size}），请用 vision 工具查看它。",
            caption,
            path.display()
        )
    };
    text_user_message(text)
}

/// 构造纯文本用户消息。
fn text_user_message(text: String) -> RigMessage {
    RigMessage::User {
        content: vec![UserContent::Text(RigText {
            text,
            additional_params: None,
        })],
    }
}

/// 构造「说明文字 + 图片」的用户消息。
fn image_user_message(text: String, bytes: Vec<u8>, media_type: ImageMediaType) -> RigMessage {
    // 大图先压缩到 256KB 以下，再 base64
    let (bytes, media_type) = compress_image(bytes, media_type);
    let b64 = BASE64.encode(&bytes);
    let image = UserContent::Image(RigImage {
        data: DocumentSourceKind::base64(&b64),
        media_type: Some(media_type),
        detail: None,
        additional_params: None,
    });
    let content = vec![
        UserContent::Text(RigText {
            text,
            additional_params: None,
        }),
        image,
    ];
    RigMessage::User { content }
}

/// 若消息带说明文字（caption），按图片消息的格式附到文本末尾。
fn with_caption(text: String, caption: &str) -> String {
    if caption.trim().is_empty() {
        text
    } else {
        format!("{text}，并附说明「{caption}」")
    }
}
