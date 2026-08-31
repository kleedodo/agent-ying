//! Telegram 更新处理器：文本消息 → 跑 agent；按钮回调 → 决定工具审批。

use std::path::{Path, PathBuf};

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use futures::StreamExt;
use rig::agent::{PromptResponse, StreamingError};
use rig::completion::Message as RigMessage;
use rig::completion::message::{
    DocumentSourceKind, Image as RigImage, ImageMediaType, Text as RigText, UserContent,
};
use rig::prelude::{MultiTurnStreamItem, StreamingChat};
use rig::streaming::StreamedAssistantContent;

use teloxide::net::Download;
use teloxide::prelude::*;
use teloxide::types::{ChatAction, FileId, InlineKeyboardMarkup, MessageId};
use uuid::Uuid;

use crate::{
    AppState,
    approval::{ResolveOutcome, approval_body},
};

pub type HandlerResult = Result<(), Box<dyn std::error::Error + Send + Sync>>;

/// 用户发来的图片的临时目录：`$TMPDIR/agent-ying/`
pub(crate) fn temp_image_dir() -> PathBuf {
    std::env::temp_dir().join("agent-ying")
}

/// 判断路径是否位于临时图片目录内（vision 工具据此在调用后自动删除）
pub(crate) fn is_temp_image_path(path: &str) -> bool {
    Path::new(path).starts_with(temp_image_dir())
}

/// 把用户发来的图片存为临时文件，命名 `<chat_id>-<消息 id>.<ext>`
async fn save_temp_image(
    msg: &Message,
    bytes: &[u8],
    ext: &str,
) -> Result<PathBuf, Box<dyn std::error::Error + Send + Sync>> {
    let dir = temp_image_dir();
    tokio::fs::create_dir_all(&dir).await?;
    let path = dir.join(format!("{}-{}.{}", msg.chat.id.0, msg.id, ext));
    tokio::fs::write(&path, bytes).await?;
    tracing::info!("用户发来的图片已存到临时文件： {}", path.display());
    Ok(path)
}

/// 转发给 vision 时，图片对应的用户消息：
/// 告诉主 agent 图片已存到哪个临时文件，请它调用 vision 工具查看；
/// 同时带上消息 ID 以便追溯。
fn temp_image_text_message(msg_id: i32, caption: String, path: PathBuf) -> RigMessage {
    let text = if caption.trim().is_empty() {
        format!(
            "用户发来一张图片（消息 ID {msg_id}），已保存到 {}，请用 vision 工具查看它。",
            path.display()
        )
    } else {
        format!(
            "用户发来一张图片并附说明「{}」（消息 ID {msg_id}），图片已保存到 {}，请用 vision 工具查看它。",
            caption,
            path.display()
        )
    };
    RigMessage::User {
        content: vec![UserContent::Text(RigText {
            text,
            additional_params: None,
        })],
    }
}

/// 把 rig 的图片类型映射为临时文件扩展名（压缩可能改变格式，扩展名以压缩后的 media type 为准）。
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
/// 支持：图片（photo，或 image/* 的 document，可带说明文字）、文档、视频、音频、纯文本。
/// 图片：`forward_to_vision` 为 true 时存到临时文件并用文本提示主 agent 调 vision 工具；
/// 否则直接内嵌、原样发给上游。
/// 文档/视频/音频只把元数据（文件名、大小、消息 ID，以及说明文字 caption）以文本形式
/// 告诉主 agent，不下载文件本体。
/// 所有文件类消息的文本里都会带上消息 ID 以便追溯。
/// 返回 None 表示既不是文本也不是受支持的文件类型（如贴纸等）。
async fn build_user_message(
    bot: &Bot,
    msg: &Message,
    forward_to_vision: bool,
) -> Result<Option<RigMessage>, Box<dyn std::error::Error + Send + Sync>> {
    let msg_id = msg.id.0;
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
        let caption = msg.caption().map(str::to_string).unwrap_or_default();
        if !forward_to_vision {
            let text = if caption.trim().is_empty() {
                format!("（用户发了一张图片，消息 ID {msg_id}）")
            } else {
                format!("{caption}（消息 ID {msg_id}）")
            };
            return Ok(Some(image_user_message(text, bytes, ImageMediaType::JPEG)));
        }
        // 先压缩再存临时文件，vision 看图时就不用再压缩
        let (bytes, media_type) = compress_image(bytes, ImageMediaType::JPEG);
        let path = save_temp_image(msg, &bytes, ext_for_media_type(media_type)).await?;
        return Ok(Some(temp_image_text_message(msg_id, caption, path)));
    }
    if let Some(doc) = msg.document() {
        let mime = doc
            .mime_type
            .as_ref()
            .map(|m| m.as_ref().to_string())
            .unwrap_or_default();
        if let Some(media_type) = mime_to_image_media_type(&mime) {
            let bytes = download_file_bytes(bot, &doc.file.id).await?;
            let caption = msg.caption().map(str::to_string).unwrap_or_default();
            if !forward_to_vision {
                let text = if caption.trim().is_empty() {
                    format!("（用户发了一张图片，消息 ID {msg_id}）")
                } else {
                    format!("{caption}（消息 ID {msg_id}）")
                };
                return Ok(Some(image_user_message(text, bytes, media_type)));
            }
            // 先压缩再存临时文件，vision 看图时就不用再压缩
            let (bytes, media_type) = compress_image(bytes, media_type);
            let path = save_temp_image(msg, &bytes, ext_for_media_type(media_type)).await?;
            return Ok(Some(temp_image_text_message(msg_id, caption, path)));
        }
        // 非图片文档：只传元数据，不下载
        let name = doc
            .file_name
            .clone()
            .unwrap_or_else(|| "未知文件".to_string());
        let size = crate::tools::human_size(doc.file.size as u64);
        let caption = msg.caption().map(str::to_string).unwrap_or_default();
        return Ok(Some(text_user_message(with_caption(
            format!("用户发来一个文档 `{name}`(MIME: {mime}，大小 {size}，消息 ID {msg_id})"),
            &caption,
        ))));
    }
    // 2. 视频 / 音频：同样只传元数据
    if let Some(v) = msg.video() {
        let name = v.file_name.clone().unwrap_or_else(|| "视频".to_string());
        let size = crate::tools::human_size(v.file.size as u64);
        let caption = msg.caption().map(str::to_string).unwrap_or_default();
        return Ok(Some(text_user_message(with_caption(
            format!(
                "用户发来一个视频 `{name}`（大小 {size}，时长 {}s，消息 ID {msg_id}）",
                v.duration.seconds()
            ),
            &caption,
        ))));
    }
    if let Some(a) = msg.audio() {
        let title = a.title.clone().unwrap_or_else(|| "音频".to_string());
        let size = crate::tools::human_size(a.file.size as u64);
        let caption = msg.caption().map(str::to_string).unwrap_or_default();
        return Ok(Some(text_user_message(with_caption(
            format!("用户发来一段音频 `{title}`（大小 {size}，消息 ID {msg_id}）"),
            &caption,
        ))));
    }

    // 3. 纯文本
    if let Some(text) = msg.text().map(str::to_owned) {
        return Ok(Some(text_user_message(text)));
    }

    Ok(None)
}

/// 若消息带说明文字（caption），按图片消息的格式附到文本末尾。
fn with_caption(text: String, caption: &str) -> String {
    if caption.trim().is_empty() {
        text
    } else {
        format!("{text}，并附说明「{caption}」")
    }
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

/// 下载 Telegram 文件为字节。
pub(crate) async fn download_file_bytes(
    bot: &Bot,
    file_id: &FileId,
) -> Result<Vec<u8>, Box<dyn std::error::Error + Send + Sync>> {
    let file = bot.get_file(file_id.clone()).await?;
    let mut buf: Vec<u8> = Vec::new();
    bot.download_file(&file.path, &mut buf).await?;
    Ok(buf)
}

/// 大图压缩上限：256KB。超过则重编码为 JPEG 并逐步降质/缩小，直到不超过该值。
const MAX_IMAGE_BYTES: usize = 256 * 1024;

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

/// 计算下一次流式编辑前的随机等待时长：
/// 在 `200ms ~ interval` 内均匀随机；`interval` 小于 200ms 时按 200ms 执行。
fn random_edit_wait(interval: std::time::Duration) -> std::time::Duration {
    use std::time::Duration;
    const FLOOR: Duration = Duration::from_millis(200);
    let ceiling = interval.max(FLOOR);
    if ceiling == FLOOR {
        return FLOOR;
    }
    let range_ms = ceiling.as_millis() as u64 - FLOOR.as_millis() as u64;
    FLOOR + Duration::from_millis(rand::random::<u64>() % (range_ms + 1))
}

/// 处理用户发来的文本消息：跑一轮 agent 对话（带多轮历史）。
pub async fn on_message(state: AppState, msg: Message) -> HandlerResult {
    // 只响应配置里允许的用户
    let Some(from) = msg.from.as_ref() else {
        state
            .bot
            .send_message(msg.chat.id, "🚫 无法识别发送者，请私聊我。")
            .await?;
        return Ok(());
    };
    if !state.allows_user(from.id) {
        state
            .bot
            .send_message(msg.chat.id, "🚫 未授权用户，请找 bot 主人加白名单。")
            .await?;
        return Ok(());
    }

    let text = msg.text().map(str::to_owned);
    // 文件类消息没有 text，日志里改打 caption；没有 caption 则打 [XX消息] 占位
    let log_text = text.clone().or_else(|| {
        let caption = msg
            .caption()
            .map(str::to_string)
            .filter(|c| !c.trim().is_empty());
        let is_image = msg.photo().is_some()
            || msg.document().as_ref().is_some_and(|d| {
                d.mime_type
                    .as_ref()
                    .is_some_and(|m| m.to_string().starts_with("image/"))
            });
        if is_image {
            caption.or_else(|| Some("[图片消息]".to_string()))
        } else if let Some(doc) = msg.document() {
            caption.or_else(|| {
                Some(format!(
                    "[文档消息] {}",
                    doc.file_name.clone().unwrap_or_default()
                ))
            })
        } else if msg.video().is_some() {
            caption.or_else(|| Some("[视频消息]".to_string()))
        } else if msg.audio().is_some() {
            caption.or_else(|| Some("[音频消息]".to_string()))
        } else {
            None
        }
    });

    // 简单的 /start、/help、/new 命令（文本消息）
    if let Some(t) = &text {
        if t.starts_with("/start") || t.starts_with("/help") {
            state
                .bot
                .send_message(
                    msg.chat.id,
                    "👋 我是 ying！直接发文本或图片就行。\n\
                     我可以用 `bash` 跑命令、`read` 读文件，\n\
                     也能看你发的图片、看电脑上的图片（vision）。\n\
                     每次调用工具前都会发按钮请你明确同意。\n\
                     发送 /new 可以开启新会话（清空对话历史）。",
                )
                .await?;
            return Ok(());
        }
        if t.starts_with("/new") {
            let mut map = state.histories.lock().await;
            map.remove(&msg.chat.id);
            // 会话结束：下一条消息会创建新的会话文件
            state.sessions.lock().await.remove(&msg.chat.id);
            state
                .bot
                .send_message(msg.chat.id, "🆕 新会话已开始，之前的对话历史已清空。")
                .await?;
            return Ok(());
        }
    }

    // 构建发给模型的用户消息：纯文本，或图片（可带说明文字）
    // forward_to_vision 且 vision 已启用时，图片转存临时文件并提示调 vision 工具；
    // 否则（包括 vision 未启用的情况）图片原样内嵌发给上游
    let forward_to_vision = state.forward_to_vision && state.vision_client.is_some();
    let Some(user_msg) = build_user_message(&state.bot, &msg, forward_to_vision).await? else {
        state
            .bot
            .send_message(msg.chat.id, "请发送文本、图片、文档、视频或音频 🙏")
            .await?;
        return Ok(());
    };

    tracing::info!(
        "收到消息： chat={} user={:?} text={:?}",
        msg.chat.id,
        msg.from.as_ref().map(|f| f.id),
        log_text,
    );

    let chat_id = msg.chat.id;

    // 本轮的 round id:journal 里关联本轮全部消息
    let round_id = Uuid::new_v4();
    // 当前会话文件：chat 还没有则新建（/new 或进程重启后都会是新文件）
    let session = {
        let mut map = state.sessions.lock().await;
        match map.get(&chat_id).cloned() {
            Some(s) => s,
            None => {
                let s = state
                    .journal
                    .create_session(chat_id.0)
                    .await
                    .map_err(|e| format!("创建 journal 会话文件失败： {e}"))?;
                map.insert(chat_id, s.clone());
                s
            }
        }
    };
    let agent = state.agent_for(chat_id, session.toolout_dir());

    // 流异常（拿不到 FinalResponse）时，至少把本轮用户消息记进 journal
    let logged_user_msg = user_msg.clone();

    // 先发「正在输入」状态，让用户立刻有反馈；
    // 占位消息改为每轮首个文本到达时懒创建（见下方 placeholder）
    let _ = state
        .bot
        .send_chat_action(chat_id, ChatAction::Typing)
        .await;

    // 占位消息始终代表「当前正在生成的回复」:
    // 每轮首个文本到达时创建，工具调用时定稿/删除，
    // 这样最终回复始终位于所有工具审批消息之后，不会被「顶上去」
    let mut placeholder: Option<MessageId> = None;

    // 每个 chat 单独维护多轮对话历史（先取出再写回）
    let mut history: Vec<RigMessage> = {
        let map = state.histories.lock().await;
        map.get(&chat_id).cloned().unwrap_or_default()
    };

    // 流式跑 agent：文本增量实时刷到占位消息（节流防触发 Telegram 限频）,
    // 最终回复与本轮新增历史以 FinalResponse 为准
    let mut stream = agent.stream_chat(user_msg, history.clone()).await;

    let mut preview = String::new();
    let mut preview_sent = String::new();
    let mut last_edit = tokio::time::Instant::now();
    let mut next_wait = random_edit_wait(state.stream_edit_interval);
    let mut final_response: Option<PromptResponse> = None;
    let mut stream_error: Option<StreamingError> = None;

    while let Some(item) = stream.next().await {
        match item {
            Ok(MultiTurnStreamItem::StreamAssistantItem(StreamedAssistantContent::Text(text))) => {
                preview.push_str(&text.text);
                // 新一轮的首个文本（首轮，或工具调用之后）：先新建占位消息
                if placeholder.is_none() {
                    match state.bot.send_message(chat_id, "🤔 思考中…").await {
                        Ok(m) => placeholder = Some(m.id),
                        Err(e) => tracing::warn!("占位消息发送失败（下个文本再试）: {e}"),
                    }
                }
                // 节流：距上次编辑不足随机等待时长就先攒着，最后统一补发
                if let Some(pid) = placeholder
                    && last_edit.elapsed() >= next_wait
                {
                    match state
                        .bot
                        .edit_message_text(chat_id, pid, preview.clone())
                        .await
                    {
                        Ok(_) => {
                            preview_sent = preview.clone();
                            last_edit = tokio::time::Instant::now();
                            next_wait = random_edit_wait(state.stream_edit_interval);
                        }
                        Err(e) => tracing::warn!("流式更新消息失败（继续尝试）: {e}"),
                    }
                }
            }
            // 模型发起工具调用：之前的文本是中间轮次的输出。
            // 占位消息有内容则定稿（补发节流余量并标记为中间输出），空则删除，
            // 让后续审批消息与最终回复都排在它之后
            Ok(MultiTurnStreamItem::StreamAssistantItem(StreamedAssistantContent::ToolCall {
                ..
            })) => {
                if let Some(pid) = placeholder.take() {
                    if preview.trim().is_empty() {
                        if let Err(e) = state.bot.delete_message(chat_id, pid).await {
                            tracing::warn!("删除占位消息失败： {e}");
                        }
                    } else if let Err(e) = state
                        .bot
                        .edit_message_text(chat_id, pid, format!("📝 {preview}"))
                        .await
                    {
                        tracing::warn!("中间输出定稿失败： {e}");
                    }
                }
                preview.clear();
                preview_sent.clear();
            }
            // 该轮被 hook 拒绝重试：临时文本作废，占位消息一并删除，新一轮文本会重建
            Ok(MultiTurnStreamItem::ModelTurnRetried { .. }) => {
                if let Some(pid) = placeholder.take()
                    && let Err(e) = state.bot.delete_message(chat_id, pid).await
                {
                    tracing::warn!("删除占位消息失败： {e}");
                }
                preview.clear();
                preview_sent.clear();
            }
            Ok(MultiTurnStreamItem::FinalResponse(resp)) => {
                final_response = Some(resp);
            }
            Ok(_) => {}
            Err(e) => {
                stream_error = Some(e);
                break;
            }
        }
    }

    let reply = match (stream_error, final_response) {
        (Some(e), _) => {
            tracing::error!("Agent 出错： chat={} {e}", chat_id);
            // 占位消息还是空的就删掉，避免残留「思考中…」
            if let Some(pid) = placeholder.take()
                && preview.trim().is_empty()
            {
                let _ = state.bot.delete_message(chat_id, pid).await;
            }
            state
                .bot
                .send_message(chat_id, format!("⚠️ Agent 出错： {e}"))
                .await?;
            // 异常轮次只记用户消息
            session
                .append_round(round_id, std::slice::from_ref(&logged_user_msg))
                .await;
            // 本轮收尾：审批日志消息追加「🏁 本轮结束」尾注（本轮无审批则不做任何事）
            state.approvals.finish_run(&state.bot, chat_id).await;
            return Ok(());
        }
        (None, Some(resp)) => {
            // resp.messages 是本轮新增消息（含用户输入），接在旧历史后面
            let new_messages = resp.messages.unwrap_or_default();
            history.extend(new_messages.clone());
            // 本轮全部消息追加进 journal（只追加、不修改）
            session.append_round(round_id, &new_messages).await;
            resp.output
        }
        (None, None) => {
            // 流结束却没收到 FinalResponse（异常）：有预览文本就保留，否则报错
            if preview.is_empty() {
                // 删掉占位消息，避免残留「思考中…」
                if let Some(pid) = placeholder.take() {
                    let _ = state.bot.delete_message(chat_id, pid).await;
                }
                state
                    .bot
                    .send_message(chat_id, "⚠️ Agent 出错： 流结束但没有最终响应")
                    .await?;
                // 异常轮次只记用户消息
                session
                    .append_round(round_id, std::slice::from_ref(&logged_user_msg))
                    .await;
                state.approvals.finish_run(&state.bot, chat_id).await;
                return Ok(());
            }
            preview.clone()
        }
    };

    tracing::info!(
        "Agent 回复完成： chat={} 共 {} 轮历史",
        chat_id,
        history.len()
    );

    // 收尾：最终回复写入当前占位消息（与最后一次预览相同则跳过，
    // Telegram 对相同文本的编辑会报错）；没有占位（异常）则新发一条；
    // 回复为空则删掉占位，避免残留「思考中…」
    match placeholder {
        Some(pid) if !reply.is_empty() && reply != preview_sent => {
            state.bot.edit_message_text(chat_id, pid, reply).await?;
        }
        None if !reply.is_empty() => {
            state.bot.send_message(chat_id, reply).await?;
        }
        Some(pid) if reply.is_empty() => {
            let _ = state.bot.delete_message(chat_id, pid).await;
        }
        _ => {}
    }

    // 本轮收尾：审批日志消息追加「🏁 本轮结束」尾注（本轮无审批则不做任何事）
    state.approvals.finish_run(&state.bot, chat_id).await;

    {
        let mut map = state.histories.lock().await;
        map.insert(chat_id, history);
    }
    Ok(())
}

/// 兜底分支：打印没被上面任何 handler 匹配的 update，方便排查丢失的回调等。
pub async fn on_unmatched(update: Update) -> HandlerResult {
    tracing::info!(
        "收到未匹配的 update: id={} kind={:?}",
        update.id.0,
        update.kind
    );
    Ok(())
}

/// 把审批消息改成「已决定」状态时，保留原来的工具/命令信息，
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
    tracing::info!("收到回调： data={:?}", q.data);

    // 只响应配置里允许的用户
    if !state.allows_user(q.from.id) {
        tracing::info!("未授权用户点击按钮： {:?}", q.from.id);
        let _ = state
            .bot
            .answer_callback_query(q.id.clone())
            .text("🚫 未授权用户")
            .await;
        return Ok(());
    }

    let Some((action, id)) = q.data.as_deref().and_then(|d| d.split_once(':')) else {
        tracing::warn!("回调 data 解析失败： {:?}", q.data);
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

    // 更新审批日志消息：标记对应记录，按钮跟随剩余待审批项
    let outcome = if let Some(m) = q.message.as_ref().and_then(|m| m.regular_message()) {
        state
            .approvals
            .resolve(&state.bot, m.chat.id, id, approve)
            .await
    } else {
        ResolveOutcome::NoLog
    };
    match outcome {
        ResolveOutcome::Resolved => {
            tracing::info!("审批决定： {} → {}", action, id);
            // 日志消息已由 resolve 更新（含该条记录的决定结果）
        }
        // 该 chat 有日志但找不到对应审批项（如重复点击）：消息已是正确状态，不用改
        ResolveOutcome::Handled => {
            tracing::warn!("审批项已处理（重复点击？）: {} → {}", action, id);
            let _ = state
                .bot
                .answer_callback_query(q.id.clone())
                .text("⏳ 该按钮已处理或已过期")
                .await;
        }
        // 该 chat 没有日志：常见于点击上一次 bot 进程留下的旧按钮（内存里的日志已清空）,
        // 直接把旧消息改掉并摘掉按钮，给用户可见的反馈（保留记录内容）
        ResolveOutcome::NoLog => {
            tracing::warn!("找不到审批日志（可能已处理或已过期）: {} → {}", action, id);
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
    use std::time::Duration;

    #[test]
    fn random_edit_wait_below_floor_clamps_to_200ms() {
        // 配置小于 200ms 时按 200ms 执行
        for _ in 0..50 {
            assert_eq!(
                random_edit_wait(Duration::from_millis(50)),
                Duration::from_millis(200)
            );
        }
    }

    #[test]
    fn random_edit_wait_stays_within_bounds() {
        let floor = Duration::from_millis(200);
        let ceiling = Duration::from_millis(750);
        for _ in 0..200 {
            let w = random_edit_wait(ceiling);
            assert!((floor..=ceiling).contains(&w), "wait {w:?} 越界");
        }
    }

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
