//! rig 工具:bash、read、vision。
//! 除 read(只读)和用户发图的 vision(发图即同意)外,
//! 每个工具执行前都会先通过 Telegram 内联按钮请用户明确同意。

use std::path::Path;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use rig::client::AgentClientExt;
use rig::completion::message::{
    DocumentSourceKind, Image as RigImage, ImageMediaType, Text as RigText, UserContent,
};
use rig::completion::{Chat, Message as RigMessage};
use rig::providers::openai;
use rig::tool::{Tool, ToolContext};
use serde::Deserialize;
use teloxide::prelude::*;
use thiserror::Error;
use tokio::io::AsyncReadExt;
use uuid::Uuid;

use crate::approval::{ApprovalManager, request_approval};
use crate::handlers::{compress_image, is_temp_image_path};

/// 工具输出超过该字符数时返回头尾摘要(全文始终落盘)
/// 输出会进多轮历史、每轮重复送给模型,故阈值偏保守:够多数命令用,超出就只给摘要让模型按需取
const MAX_OUTPUT_CHARS: usize = 8000;
/// 落盘后返回摘要中保留的头部字符数(与尾部之和须小于 MAX_OUTPUT_CHARS)
const SPILL_HEAD_CHARS: usize = 3000;
/// 落盘后返回摘要中保留的尾部字符数
const SPILL_TAIL_CHARS: usize = 2000;

#[derive(Debug, Error)]
#[error("{0}")]
pub struct ToolErr(pub String);

/// 单个落盘文件的硬上限:完整输出超过该字节数时只保存前 256MB(按字符边界截断)
const MAX_SPILL_BYTES: usize = 256 * 1024 * 1024;

/// 所有工具结果都全文落盘到会话的 `toolout/` 目录(见 ToolCtx::toolout_dir):
/// 超过 [crate::journal::COMPRESS_MIN_BYTES] 的 gzip 压缩为 `<uuid>.txt.gz`,更小的保留纯文本 `<uuid>.txt`。
/// 返回给模型的文本始终注明完整结果存在哪里:未超长时原样返回 + 保存路径;
/// 超长时返回头 + 尾摘要和完整文件路径,模型可以用 bash 工具自行查看被省略的部分。
pub async fn record_tool_result(dir: &Path, s: &str) -> Result<String, ToolErr> {
    let id = Uuid::new_v4().simple().to_string();

    // 单文件硬上限:只保存前 256MB(不跨字符边界)
    let end = s.floor_char_boundary(MAX_SPILL_BYTES.min(s.len()));
    let spill = &s[..end];
    let capped_note = if end < s.len() {
        format!("完整输出共 {} 字节,仅保存了前 256MB。", s.len())
    } else {
        String::new()
    };
    let data = spill.as_bytes().to_vec();

    // 超过 50KB 才 gzip 压缩,小文件保留纯文本直接可读
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

    let is_gz = path.extension().and_then(|e| e.to_str()) == Some("gz");
    // 完整路径只在保存说明里出现一次;命令只给工具名不重复拼路径,省 token
    let cmd_hint = if is_gz {
        "可用 zcat 查看、zgrep 搜索、sed/tail 截取该文件"
    } else {
        "可用 cat 查看、grep 搜索、sed/tail 截取该文件"
    };

    let total = s.chars().count();
    if total <= MAX_OUTPUT_CHARS {
        return Ok(format!(
            "{s}\n\n（完整结果已保存到 {}。{capped_note}{cmd_hint}。）",
            path.display()
        ));
    }

    let head: String = s.chars().take(SPILL_HEAD_CHARS).collect();
    let tail: String = s.chars().skip(total - SPILL_TAIL_CHARS).collect();
    let dropped = total - SPILL_HEAD_CHARS - SPILL_TAIL_CHARS;
    Ok(format!(
        "{head}\n…（共 {total} 字符，中间省略 {dropped} 字符；完整输出已保存到 {}。{capped_note}{cmd_hint}。）\n{tail}",
        path.display()
    ))
}

/// 两个工具共用的字段:目标聊天 + 审批管理器 + 用户发来文件的缓存。
#[derive(Clone)]
pub struct ToolCtx {
    pub bot: Bot,
    pub chat_id: ChatId,
    pub approvals: ApprovalManager,
    pub bash_timeout: std::time::Duration,
    pub approval_timeout: std::time::Duration,
    /// 当前会话的 toolout/ 目录,所有工具结果的全文都落盘到这里
    pub toolout_dir: std::path::PathBuf,
}

// --------------------------------------------------------------------- bash

#[derive(Debug, Deserialize)]
pub struct BashArgs {
    /// 要在 shell 中执行的命令
    pub command: String,
}

#[derive(Clone)]
pub struct Bash(pub ToolCtx);

impl std::fmt::Debug for Bash {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Bash").finish()
    }
}

impl Tool for Bash {
    const NAME: &'static str = "bash";

    type Error = ToolErr;
    type Args = BashArgs;
    type Output = String;

    fn description(&self) -> String {
        "在 shell 中执行一条 bash 命令，返回 stdout 和 stderr。命令中涉及文件/目录时一律使用绝对路径（不接受相对路径）".into()
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "要执行的 bash 命令（涉及文件/目录时请使用绝对路径，不接受相对路径）"
                }
            },
            "required": ["command"]
        })
    }

    async fn call(
        &self,
        _context: &mut ToolContext,
        args: Self::Args,
    ) -> Result<Self::Output, Self::Error> {
        let ctx = &self.0;
        let approved = request_approval(
            &ctx.bot,
            ctx.chat_id,
            &ctx.approvals,
            ctx.approval_timeout,
            "bash",
            &format!("执行命令：`{}`", args.command),
        )
        .await
        .map_err(ToolErr)?;

        if !approved {
            tracing::info!("bash 被用户拒绝：{}", args.command);
            return Ok(format!(
                "用户拒绝了执行命令 `{}`，立即停止尝试并追问用户原因。",
                args.command
            ));
        }

        tracing::info!("bash 开始执行：{}", args.command);

        let future = tokio::process::Command::new("bash")
            .arg("-lc")
            .arg(&args.command)
            .output();

        match tokio::time::timeout(ctx.bash_timeout, future).await {
            Ok(Ok(output)) => {
                tracing::info!(
                    "bash 完成：{} exit={}",
                    args.command,
                    output.status.code().unwrap_or(-1),
                );
                let mut report = format!("exit code: {}\n", output.status.code().unwrap_or(-1));
                let stdout = String::from_utf8_lossy(&output.stdout);
                let stderr = String::from_utf8_lossy(&output.stderr);
                if !stdout.is_empty() {
                    report.push_str("\n--- stdout ---\n");
                    report.push_str(&stdout);
                }
                if !stderr.is_empty() {
                    report.push_str("\n--- stderr ---\n");
                    report.push_str(&stderr);
                }
                Ok(record_tool_result(&ctx.toolout_dir, &report).await?)
            }
            Ok(Err(e)) => Err(ToolErr(format!("命令启动失败：{e}"))),
            Err(_) => {
                tracing::warn!(
                    "bash 超时：{}（{}s）",
                    args.command,
                    ctx.bash_timeout.as_secs()
                );
                Ok(format!(
                    "命令执行超时（{}）：`{}`",
                    ctx.bash_timeout.as_secs(),
                    args.command
                ))
            }
        }
    }
}

//---------------------------------------------------------------------------- read

/// read 默认最多读取的行数
const DEFAULT_READ_LIMIT: usize = 2000;
/// read 输出字节上限(与行数上限取先到者)
const MAX_READ_BYTES: usize = 50 * 1024;

#[derive(Debug, Deserialize)]
pub struct ReadArgs {
    /// 要读取的文件路径(必须是绝对路径)
    pub path: String,
    /// 从第几行开始读(1 起,默认 1)
    pub offset: Option<usize>,
    /// 最多读多少行（默认 2000，输出达到 50KB 也会截断）
    pub limit: Option<usize>,
}

/// 只读一个文本文件(skills 文件或其他任意文件),只读无副作用,免审批。
#[derive(Clone)]
pub struct Read(pub ToolCtx);

impl std::fmt::Debug for Read {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Read").finish()
    }
}

impl Tool for Read {
    const NAME: &'static str = "read";

    type Error = ToolErr;
    type Args = ReadArgs;
    type Output = String;

    fn description(&self) -> String {
        format!(
            "读取一个文本文件（如 SKILL.md 或其他任意文件）：路径必须是绝对路径（不接受相对路径）。\
             返回原始内容（不带行号前缀）；默认最多读 {} 行或 50KB（先到者为准），\
             截断时会附提示，可用 offset（起始行号，1 起）续读，也可用 limit 指定行数",
            DEFAULT_READ_LIMIT
        )
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "要读取的文件路径（必须是绝对路径，不接受相对路径）"
                },
                "offset": {
                    "type": "integer",
                    "description": "从第几行开始读（1 起，默认 1）"
                },
                "limit": {
                    "type": "integer",
                    "description": "最多读多少行（默认 2000，输出达到 50KB 也会截断）"
                }
            },
            "required": ["path"]
        })
    }

    async fn call(
        &self,
        _context: &mut ToolContext,
        args: Self::Args,
    ) -> Result<Self::Output, Self::Error> {
        let ctx = &self.0;
        if !Path::new(&args.path).is_absolute() {
            return Err(ToolErr(format!(
                "路径 `{}` 不是绝对路径（只接受绝对路径）",
                args.path
            )));
        }
        let target = tokio::fs::canonicalize(&args.path)
            .await
            .map_err(|e| ToolErr(format!("读取文件 `{}` 失败：{e}", args.path)))?;

        // 先检查文件可读性
        let mut file = tokio::fs::File::open(&target)
            .await
            .map_err(|e| ToolErr(format!("文件 `{}` 不可读：{e}", args.path)))?;
        let mut content = String::new();
        file.read_to_string(&mut content)
            .await
            .map_err(|e| ToolErr(format!("读取文件 `{}` 失败：{e}", args.path)))?;

        // 行计数:split('\n')(末尾换行会多出一个空行,不做特殊处理)
        let all_lines: Vec<&str> = content.split('\n').collect();
        let total = all_lines.len();
        let start = args.offset.map(|o| o.saturating_sub(1)).unwrap_or(0);
        let start_display = start + 1;
        if start >= total {
            // offset 越界是工具错误
            return Err(ToolErr(format!(
                "offset {} 超出文件末尾（共 {total} 行）",
                args.offset.unwrap_or(0)
            )));
        }

        // 用户 limit 先截取,之后仍统一受 2000 行/50KB 上限约束
        let (selected, user_limited): (String, Option<usize>) = match args.limit {
            Some(l) if l > 0 => {
                let end = (start + l).min(total);
                (all_lines[start..end].join("\n"), Some(end - start))
            }
            _ => (all_lines[start..].join("\n"), None),
        };

        // 行数统计:末尾换行不产生额外行
        let mut sel_lines: Vec<&str> = selected.split('\n').collect();
        if selected.ends_with('\n') {
            sel_lines.pop();
        }

        // 未超限:原样返回(不带行号前缀)
        if sel_lines.len() <= DEFAULT_READ_LIMIT && selected.len() <= MAX_READ_BYTES {
            // 未截断但用户 limit 提前停、文件还有剩余:提示续读
            if let Some(limited) = user_limited {
                let remaining = total - (start + limited);
                if remaining > 0 {
                    let next = start + limited + 1;
                    return record_tool_result(
                        &ctx.toolout_dir,
                        &format!(
                            "{selected}\n\n[文件还有 {remaining} 行。使用 offset={next} 继续。]"
                        ),
                    )
                    .await;
                }
            }
            return record_tool_result(&ctx.toolout_dir, &selected).await;
        }

        // 首行单行即超 50KB:正常返回提示让 agent 改用 bash(不算工具错误)
        if sel_lines.first().is_some_and(|l| l.len() > MAX_READ_BYTES) {
            return record_tool_result(
                &ctx.toolout_dir,
                &format!(
                    "[第 {start_display} 行大小 {}，超过 {} 上限。可用 bash：`sed -n '{start_display}p' {} | head -c {MAX_READ_BYTES}` 查看]",
                    human_size(sel_lines[0].len() as u64),
                    human_size(MAX_READ_BYTES as u64),
                    args.path
                ),
            )
            .await;
        }

        // 从头收集完整行,直到达到行数或字节上限(永不返回半行)
        let mut out_lines: Vec<&str> = Vec::new();
        let mut bytes = 0usize;
        for (i, line) in sel_lines.iter().enumerate().take(DEFAULT_READ_LIMIT) {
            let line_bytes = line.len() + if i > 0 { 1 } else { 0 };
            if bytes + line_bytes > MAX_READ_BYTES {
                break;
            }
            out_lines.push(line);
            bytes += line_bytes;
        }
        let output_lines = out_lines.len();
        let end_display = start_display + output_lines - 1;
        let next_offset = end_display + 1;
        let mut out = out_lines.join("\n");
        if output_lines >= DEFAULT_READ_LIMIT {
            out.push_str(&format!(
                "\n\n[显示第 {start_display}-{end_display} 行，共 {total} 行。使用 offset={next_offset} 继续。]"
            ));
        } else {
            out.push_str(&format!(
                "\n\n[显示第 {start_display}-{end_display} 行，共 {total} 行（{} 上限）。使用 offset={next_offset} 继续。]",
                human_size(MAX_READ_BYTES as u64)
            ));
        }
        record_tool_result(&ctx.toolout_dir, &out).await
    }
}

/// 把字节数格式化为人类可读的大小(如 `512B`、`1.2MB`)。
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

// -------------------------------------------------------------------- vision

#[derive(Debug, Deserialize)]
pub struct VisionArgs {
    /// 要查看的图片路径(必须是绝对路径)
    pub path: String,
}

/// 看图工具:用多模态模型看图片,调用前先请用户审批。
#[derive(Clone)]
pub struct Vision {
    pub client: openai::CompletionsClient,
    pub model: String,
    /// 解析后的 vision 系统提示词(可被 VISION_SYSTEM.md 覆盖)
    pub system_prompt: String,
    /// 审批/发按钮所需的上下文(bot + chat + 审批管理器)
    pub ctx: ToolCtx,
}

impl std::fmt::Debug for Vision {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Vision")
            .field("model", &self.model)
            .finish()
    }
}

/// 根据文件扩展名推断图片 media type,无法识别则报错。
fn media_type_from_path(path: &str) -> Result<ImageMediaType, ToolErr> {
    let ext = Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();
    match ext.as_str() {
        "jpg" | "jpeg" => Ok(ImageMediaType::JPEG),
        "png" => Ok(ImageMediaType::PNG),
        "gif" => Ok(ImageMediaType::GIF),
        "webp" => Ok(ImageMediaType::WEBP),
        "heic" => Ok(ImageMediaType::HEIC),
        "heif" => Ok(ImageMediaType::HEIF),
        "svg" => Ok(ImageMediaType::SVG),
        _ => Err(ToolErr(format!(
            "无法识别图片格式 `{}`（支持 jpg/png/gif/webp/heic/heif/svg）",
            path
        ))),
    }
}

impl Tool for Vision {
    const NAME: &'static str = "vision";

    type Error = ToolErr;
    type Args = VisionArgs;
    type Output = String;

    fn description(&self) -> String {
        "查看本地电脑上的图片文件（按绝对路径指定，不接受相对路径）：是文字图片则按原结构提取文字，是风景/照片等非文字内容则详细描述图片内容。注意：只用于查看本地电脑上的图片；用户直接发来的图片通常已经能看到，除非消息中明确说明图片已保存到某个本地路径，否则不要调用本工具".into()
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "本地电脑上的图片文件路径（必须是绝对路径，不接受相对路径）"
                }
            },
            "required": ["path"]
        })
    }

    async fn call(
        &self,
        _context: &mut ToolContext,
        args: Self::Args,
    ) -> Result<Self::Output, Self::Error> {
        let ctx = &self.ctx;
        // 临时目录里的图片是「用户刚发来的图」(主模型非多模态时被转发过来):
        // 用户发图即视为同意看图,免审批;调用结束后(无论成败)自动删除
        let is_temp = is_temp_image_path(&args.path);

        if !Path::new(&args.path).is_absolute() {
            return Err(ToolErr(format!(
                "路径 `{}` 不是绝对路径（只接受绝对路径）",
                args.path
            )));
        }

        // 先检查文件存在性和大小,避免审批通过后才发现读不到
        let metadata = tokio::fs::metadata(&args.path)
            .await
            .map_err(|e| ToolErr(format!("读取图片 `{}` 失败：{e}", args.path)))?;
        if !metadata.is_file() {
            return Err(ToolErr(format!("`{}` 不是普通文件", args.path)));
        }

        if is_temp {
            tracing::info!("vision 查看用户发来的图片（免审批）：{}", args.path);
        } else {
            let approved = request_approval(
                &ctx.bot,
                ctx.chat_id,
                &ctx.approvals,
                ctx.approval_timeout,
                "vision",
                &format!("看图：`{}`（{}）", args.path, human_size(metadata.len())),
            )
            .await
            .map_err(ToolErr)?;

            if !approved {
                tracing::info!("vision 被用户拒绝：{}", args.path);
                return Ok(format!(
                    "用户拒绝了看图 `{}`，停止尝试并追问用户。",
                    args.path
                ));
            }
            tracing::info!("vision 开始看图：{}", args.path);
        }

        // 1–6. 读文件 → 压缩 → 调 vision 模型 → 截断输出。
        // 包在内部块里,保证任何一步失败(读文件、格式识别、网络等)都会走到下面的临时文件清理。
        let result: Result<String, ToolErr> = async {
            // 1. 读文件
            let bytes = tokio::fs::read(&args.path)
                .await
                .map_err(|e| ToolErr(format!("读取图片 `{}` 失败：{e}", args.path)))?;

            // 2. 推断 media type + 大图压缩到 256KB 以下
            let media_type = media_type_from_path(&args.path)?;
            let (bytes, media_type) = compress_image(bytes, media_type);

            // 3. base64 编码
            let b64 = BASE64.encode(&bytes);

            // 4. 构造「提示文字 + 图片」的用户消息
            let image = UserContent::Image(RigImage {
                data: DocumentSourceKind::base64(&b64),
                media_type: Some(media_type),
                detail: None,
                additional_params: None,
            });
            let content = vec![
                UserContent::Text(RigText {
                    text: "请看这张图片。".to_string(),
                    additional_params: None,
                }),
                image,
            ];
            let user_msg = RigMessage::User { content };

            // 5. 构建 vision agent 并调用(单轮,无需历史)
            let agent = self
                .client
                .agent(self.model.clone())
                .name("vision")
                .preamble(&self.system_prompt)
                .build();
            let mut history: Vec<RigMessage> = Vec::new();
            let reply = agent
                .chat(user_msg, &mut history)
                .await
                .map_err(|e| ToolErr(format!("vision 模型调用失败：{e}")))?;

            tracing::info!(
                "vision 完成：{}（{} 字符）",
                args.path,
                reply.chars().count()
            );

            // 6. 全文落盘并注明保存位置,超长则返回头尾摘要
            record_tool_result(&ctx.toolout_dir, &reply).await
        }
        .await;

        // 7. 删除临时图片(用户发来的转发图,无论调用成败都不再需要)
        if is_temp {
            match tokio::fs::remove_file(&args.path).await {
                Ok(()) => tracing::info!("已删除临时图片：{}", args.path),
                Err(e) => tracing::warn!("删除临时图片 `{}` 失败：{e}", args.path),
            }
        }
        result
    }
}
