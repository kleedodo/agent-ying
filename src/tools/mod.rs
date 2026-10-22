//! 工具模块：每个工具一个子文件（bash/read/vision/write/edit），
//! 共享的上下文、错误类型、结果落盘逻辑和通用小工具放在这里。
//! 工具执行前的审批统一由 [crate::approval::ApprovalHook] 通过 Telegram 内联按钮完成。

pub mod bash;
pub mod edit;
pub mod edit_algo;
pub mod read;
pub mod vision;
pub mod write;

pub use bash::Bash;
pub use edit::Edit;
pub use read::Read;
pub use vision::Vision;
pub use write::Write;

use std::path::Path;

use thiserror::Error;
use uuid::Uuid;

#[derive(Debug, Error)]
#[error("{0}")]
pub struct ToolErr(pub String);

/// 单个落盘文件的硬上限：完整输出超过该字节数时只保存前 256MB（按字符边界截断）
const MAX_SPILL_BYTES: usize = 256 * 1024 * 1024;

/// 所有工具结果都全文落盘到会话的 `toolout/` 目录（见 ToolCtx::toolout_dir）：
/// 超过 [crate::journal::COMPRESS_MIN_BYTES] 的 gzip 压缩为 `<uuid>.txt.gz`，更小的保留纯文本 `<uuid>.txt`。
/// 本函数只负责落盘并返回保存路径；展示给模型的文本完全由调用方决定
/// （目前只有 bash 把超长输出截成头尾摘要，其余工具原样返回）。
pub async fn record_tool_result(dir: &Path, s: &str) -> Result<std::path::PathBuf, ToolErr> {
    let id = Uuid::new_v4().simple().to_string();

    // 单文件硬上限：只保存前 256MB（不跨字符边界）
    let end = s.floor_char_boundary(MAX_SPILL_BYTES.min(s.len()));
    let data = s.as_bytes()[..end].to_vec();

    // 超过 50KB 才 gzip 压缩，小文件保留纯文本直接可读
    let (path, payload) = if data.len() > crate::journal::COMPRESS_MIN_BYTES {
        let compressed = tokio::task::spawn_blocking(move || crate::journal::gzip_bytes(&data))
            .await
            .map_err(|e| ToolErr(format!("gzip 任务 join 失败：{e}")))?;
        (dir.join(format!("{id}.txt.gz")), compressed)
    } else {
        (dir.join(format!("{id}.txt")), data)
    };
    tokio::fs::write(&path, &payload)
        .await
        .map_err(|e| ToolErr(format!("写入完整输出 {} 失败：{e}", path.display())))?;
    Ok(path)
}

/// 各工具共用的字段：bash 超时 + 输出落盘目录。
/// 审批（bot/chat/审批管理器）已上移到 [crate::approval::ApprovalHook]，工具不再感知。
#[derive(Clone)]
pub struct ToolCtx {
    pub bash_timeout: std::time::Duration,
    /// 当前会话的 toolout/ 目录，所有工具结果的全文都落盘到这里
    pub toolout_dir: std::path::PathBuf,
}

/// 把字节数格式化为人类可读的大小（如 `512B`、`1.2MB`）。
pub fn human_size(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = 1024.0 * 1024.0;
    const GB: f64 = 1024.0 * 1024.0 * 1024.0;
    match bytes {
        b if b < 1024 => format!("{b}B"),
        b if b < MB as u64 => format!("{:.1}KB", b as f64 / KB),
        b if b < GB as u64 => format!("{:.1}MB", b as f64 / MB),
        b => format!("{:.1}GB", b as f64 / GB),
    }
}

/// 审批卡片用的内容预览：最多取 max_chars 个字符，超长加省略号
pub(crate) fn preview(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(max_chars).collect::<String>())
    }
}
