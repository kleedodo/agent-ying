//! 用户发来的媒体（图片/文档/视频/音频）：下载、落盘到会话 media/ 目录、文件名命名。

use std::path::{Path, PathBuf};

use teloxide::net::Download;
use teloxide::prelude::*;
use teloxide::types::FileId;
use uuid::Uuid;

use crate::journal::SessionFile;
use crate::tools::human_size;

/// 用户发来的文件自动下载上限：50MB
pub(crate) const MAX_MEDIA_BYTES: u64 = 50 * 1024 * 1024;

/// 把用户发来的图片存到会话 media/ 目录，命名 `<uuid>-<name>`
pub(crate) async fn save_media_image(
    session: &SessionFile,
    bytes: &[u8],
    name: &str,
    mime: &str,
) -> Result<PathBuf, Box<dyn std::error::Error + Send + Sync>> {
    let path = session.media_dir().join(media_file_name(name, mime));
    tokio::fs::write(&path, bytes).await?;
    tracing::info!(
        "用户发来的图片已存到： {}（{}）",
        path.display(),
        human_size(bytes.len() as u64)
    );
    Ok(path)
}

/// 非图片文件消息（文档/视频/音频）的自动下载处理：
/// ≤ 50MB 时把文件体下载到会话 media/ 目录，返回要附到消息文本末尾的提示
/// （落盘路径 + 实际大小）；超限或下载失败则返回对应的说明文本。
pub(crate) async fn media_download_note(
    bot: &Bot,
    session: &SessionFile,
    file_id: &FileId,
    name: &str,
    mime: &str,
    size: u64,
) -> String {
    if size > MAX_MEDIA_BYTES {
        return format!(
            "；超过 {} 的自动下载上限，未下载",
            human_size(MAX_MEDIA_BYTES)
        );
    }
    match download_media_file(bot, session, file_id, name, mime).await {
        Ok(path) => format!(
            "；已自动下载到 {}（大小 {}）",
            path.display(),
            human_size(size)
        ),
        Err(e) => {
            tracing::warn!("用户发来的文件自动下载失败： {e}");
            "；自动下载失败，需要时请让用户重发".to_string()
        }
    }
}

/// 下载文件到会话 media/ 目录，命名 `<uuid>-<原文件名>`（无扩展名时按 MIME 补上）。
async fn download_media_file(
    bot: &Bot,
    session: &SessionFile,
    file_id: &FileId,
    name: &str,
    mime: &str,
) -> Result<PathBuf, Box<dyn std::error::Error + Send + Sync>> {
    let bytes = download_file_bytes(bot, file_id).await?;
    let path = session.media_dir().join(media_file_name(name, mime));
    tokio::fs::write(&path, &bytes).await?;
    tracing::info!(
        "用户发来的文件已存到： {}（{}）",
        path.display(),
        human_size(bytes.len() as u64)
    );
    Ok(path)
}

/// media/ 里的文件名：`<uuid>-<原文件名>`；原文件名去掉路径分隔符等不安全字符，
/// 没有扩展名时按 MIME 推断一个。
fn media_file_name(name: &str, mime: &str) -> String {
    let safe: String = name
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' | '\x00'..='\x1f' => '_',
            c => c,
        })
        .collect();
    let safe = safe.trim().to_string();
    let base = if safe.is_empty() {
        "file".to_string()
    } else {
        safe
    };
    let id = Uuid::new_v4();
    if Path::new(&base).extension().is_some_and(|e| !e.is_empty()) {
        format!("{id}-{base}")
    } else {
        format!("{id}-{base}.{}", ext_for_mime(mime))
    }
}

/// 从 MIME 推断文件扩展名，无法识别时退回 bin。
/// 注意：与 `crate::image::mime_to_image_media_type` 的 MIME 列表保持同步。
fn ext_for_mime(mime: &str) -> &'static str {
    match mime.to_ascii_lowercase().as_str() {
        "image/jpeg" | "image/jpg" => "jpg",
        "image/png" => "png",
        "image/gif" => "gif",
        "image/webp" => "webp",
        "image/heic" => "heic",
        "image/heif" => "heif",
        "image/svg+xml" => "svg",
        "audio/mpeg" | "audio/mp3" => "mp3",
        "audio/ogg" => "ogg",
        "audio/webm" => "webm",
        "audio/flac" => "flac",
        "audio/wav" => "wav",
        "video/mp4" => "mp4",
        "video/quicktime" => "mov",
        "video/webm" => "webm",
        "video/x-matroska" => "mkv",
        "application/pdf" => "pdf",
        "application/zip" => "zip",
        "application/gzip" | "application/x-gzip" => "gz",
        "text/plain" => "txt",
        "text/markdown" => "md",
        "text/csv" => "csv",
        _ => "bin",
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn media_file_name_keeps_original_and_prefixes_uuid() {
        let name = media_file_name("hello.pdf", "application/pdf");
        assert!(name.ends_with("-hello.pdf"));
        let uuid_part = name.strip_suffix("-hello.pdf").unwrap();
        assert!(Uuid::parse_str(uuid_part).is_ok());
    }

    #[test]
    fn media_file_name_sanitizes_and_appends_mime_ext() {
        // 路径分隔符等不安全字符被替换；无扩展名时按 MIME 补上
        let name = media_file_name("a/b\nc.mp3", "audio/mpeg");
        assert!(!name.contains(['/', '\\', '\n', '\r']));
        assert!(name.ends_with("a_b_c.mp3"));
        let noext = media_file_name("录音", "audio/mpeg");
        assert!(noext.ends_with("录音.mp3"));
        let empty = media_file_name("  ", "application/pdf");
        assert!(empty.ends_with("file.pdf"));
    }

    #[test]
    fn ext_for_mime_fallback() {
        assert_eq!(ext_for_mime("image/png"), "png");
        assert_eq!(ext_for_mime("audio/mpeg"), "mp3");
        assert_eq!(ext_for_mime("video/MP4"), "mp4");
        assert_eq!(ext_for_mime("application/x-unknown"), "bin");
    }
}
