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

/// 工具输出超过该字符数时返回头尾摘要（全文始终落盘）
/// 输出会进多轮历史、每轮重复送给模型，故阈值偏保守：够多数命令用，超出就只给摘要让模型按需取
const MAX_OUTPUT_CHARS: usize = 8000;
/// 落盘后返回摘要中保留的头部字符数（与尾部之和须小于 MAX_OUTPUT_CHARS）
const SPILL_HEAD_CHARS: usize = 3000;
/// 落盘后返回摘要中保留的尾部字符数
const SPILL_TAIL_CHARS: usize = 2000;

#[derive(Debug, Error)]
#[error("{0}")]
pub struct ToolErr(pub String);

/// 单个落盘文件的硬上限：完整输出超过该字节数时只保存前 256MB（按字符边界截断）
const MAX_SPILL_BYTES: usize = 256 * 1024 * 1024;

/// 所有工具结果都全文落盘到会话的 `toolout/` 目录（见 ToolCtx::toolout_dir）：
/// 超过 [crate::journal::COMPRESS_MIN_BYTES] 的 gzip 压缩为 `<uuid>.txt.gz`，更小的保留纯文本 `<uuid>.txt`。
/// 未超长时原样返回（全文仍落盘备查）；超长时返回头 + 尾摘要和完整文件路径，
/// 并注明可用 bash 工具自行查看被省略的部分。
pub async fn record_tool_result(dir: &Path, s: &str) -> Result<String, ToolErr> {
    let id = Uuid::new_v4().simple().to_string();

    // 单文件硬上限：只保存前 256MB（不跨字符边界）
    let end = s.floor_char_boundary(MAX_SPILL_BYTES.min(s.len()));
    let spill = &s[..end];
    let capped_note = if end < s.len() {
        format!("完整输出共 {} 字节，仅保存了前 256MB。", s.len())
    } else {
        String::new()
    };
    let data = spill.as_bytes().to_vec();

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

    let total = s.chars().count();
    if total <= MAX_OUTPUT_CHARS {
        // 未截断：原样返回，不给保存路径提示，省 token
        return Ok(s.to_string());
    }

    let is_gz = path.extension().and_then(|e| e.to_str()) == Some("gz");
    // 完整路径只在保存说明里出现一次；命令只给工具名不重复拼路径，省 token
    let cmd_hint = if is_gz {
        "可用 zgrep 查看/搜索该文件"
    } else {
        "可用 cat 查看、grep 搜索、sed/tail 截取该文件"
    };

    let head: String = s.chars().take(SPILL_HEAD_CHARS).collect();
    let tail: String = s.chars().skip(total - SPILL_TAIL_CHARS).collect();
    let dropped = total - SPILL_HEAD_CHARS - SPILL_TAIL_CHARS;
    Ok(format!(
        "{head}\n…（共 {total} 字符，中间省略 {dropped} 字符；完整输出已保存到 {}。{capped_note}{cmd_hint}。）\n{tail}",
        path.display()
    ))
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
