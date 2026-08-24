//! rig 工具:bash、send_file、save_incoming、read_skill、vision。
//! 除 read_skill(只读)和用户发图的 vision(发图即同意)外,
//! 每个工具执行前都会先通过 Telegram 内联按钮请用户明确同意。

use std::path::{Path, PathBuf};

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
use teloxide::types::MessageId;
use thiserror::Error;
use uuid::Uuid;

use crate::approval::{ApprovalManager, request_approval};
use crate::handlers::{
    IncomingFile, IncomingFileCache, compress_image, download_file_bytes, is_temp_image_path,
};

/// bash 输出超过该字符数时落盘并返回头尾摘要
/// 输出会进多轮历史、每轮重复送给模型,故阈值偏保守:够多数命令用,超出就落盘让模型按需取
const MAX_OUTPUT_CHARS: usize = 8000;
/// 落盘后返回摘要中保留的头部字符数(与尾部之和须小于 MAX_OUTPUT_CHARS)
const SPILL_HEAD_CHARS: usize = 3000;
/// 落盘后返回摘要中保留的尾部字符数
const SPILL_TAIL_CHARS: usize = 2000;

/// bash 超长输出的落盘目录
const TOOL_OUT_DIR: &str = "/tmp/agent-ying/tool-out";

#[derive(Debug, Error)]
#[error("{0}")]
pub struct ToolErr(pub String);

/// 输出未超长时原样返回;超长则把完整内容写入
/// `/tmp/agent-ying/tool-out/<uuid>.txt`,返回头 + 尾摘要和完整文件路径,
/// 模型可以用 bash 工具(sed/grep/tail)自行查看被省略的部分。
async fn truncate_or_spill(s: &str) -> Result<String, ToolErr> {
    let total = s.chars().count();
    if total <= MAX_OUTPUT_CHARS {
        return Ok(s.to_string());
    }

    let dir = Path::new(TOOL_OUT_DIR);
    tokio::fs::create_dir_all(dir)
        .await
        .map_err(|e| ToolErr(format!("创建落盘目录 {} 失败: {e}", dir.display())))?;
    let path = dir.join(format!("{}.txt", Uuid::new_v4()));
    tokio::fs::write(&path, s)
        .await
        .map_err(|e| ToolErr(format!("写入完整输出 {} 失败: {e}", path.display())))?;

    let head: String = s.chars().take(SPILL_HEAD_CHARS).collect();
    let tail: String = s.chars().skip(total - SPILL_TAIL_CHARS).collect();
    let dropped = total - SPILL_HEAD_CHARS - SPILL_TAIL_CHARS;
    Ok(format!(
        "{head}\n…(共 {total} 字符,中间省略 {dropped} 字符;完整输出已保存到 {},\n\
         可用 `sed -n '200,300p' {}` 查看指定行、`grep 关键词 {}` 搜索、`tail -n 50 {}` 看结尾)\n{tail}",
        path.display(),
        path.display(),
        path.display(),
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
    /// 用户发来的文件元数据缓存(save_incoming 按消息 ID 查 file_id)
    pub incoming_files: IncomingFileCache,
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
        "在 shell 中执行一条 bash 命令，返回 stdout 和 stderr".into()
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "要执行的 bash 命令"
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
            &format!("执行命令:`{}`", args.command),
        )
        .await
        .map_err(ToolErr)?;

        if !approved {
            tracing::info!("bash 被用户拒绝: {}", args.command);
            return Ok(format!(
                "用户拒绝了执行命令 `{}`，立即停止尝试并追问用户原因。",
                args.command
            ));
        }

        tracing::info!("bash 开始执行: {}", args.command);

        let future = tokio::process::Command::new("bash")
            .arg("-lc")
            .arg(&args.command)
            .output();

        match tokio::time::timeout(ctx.bash_timeout, future).await {
            Ok(Ok(output)) => {
                tracing::info!(
                    "bash 完成: {} exit={}",
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
                Ok(truncate_or_spill(&report).await?)
            }
            Ok(Err(e)) => Err(ToolErr(format!("命令启动失败: {e}"))),
            Err(_) => {
                tracing::warn!(
                    "bash 超时: {} ({}s)",
                    args.command,
                    ctx.bash_timeout.as_secs()
                );
                Ok(format!(
                    "命令执行超时({}): `{}`",
                    ctx.bash_timeout.as_secs(),
                    args.command
                ))
            }
        }
    }
}

//-------------------------------------------------------------- read_skill

/// read_skill 输出上限:128K 字符(单行超长的极端情况兼做兑底)
const MAX_READ_SKILL_CHARS: usize = 128 * 1024;
/// read_skill 默认最多读取的行数
const DEFAULT_READ_LIMIT: usize = 500;

#[derive(Debug, Deserialize)]
pub struct ReadSkillArgs {
    /// 要读取的文件路径(绝对路径,或相对于 skills 目录)
    pub path: String,
    /// 从第几行开始读(1 起,默认 1)
    pub offset: Option<usize>,
    /// 最多读多少行(默认 2000)
    pub limit: Option<usize>,
}

/// 只读 skills 根目录下的文件,只读无副作用,免审批。
#[derive(Clone)]
pub struct ReadSkill(pub PathBuf);

impl std::fmt::Debug for ReadSkill {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReadSkill").finish()
    }
}

impl Tool for ReadSkill {
    const NAME: &'static str = "read_skill";

    type Error = ToolErr;
    type Args = ReadSkillArgs;
    type Output = String;

    fn description(&self) -> String {
        format!(
            "读取 skills 目录({})下的文件，如 SKILL.md 或它的附属文件。\
             返回内容带行号前缀；默认从第 1 行读最多 {} 行，\
             文件较长时可用 offset(起始行号)和 limit(行数)分段读取",
            self.0.display(),
            DEFAULT_READ_LIMIT
        )
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "要读取的文件路径(绝对路径，或相对于 skills 目录)"
                },
                "offset": {
                    "type": "integer",
                    "description": "从第几行开始读(1 起，默认 1)"
                },
                "limit": {
                    "type": "integer",
                    "description": "最多读多少行(默认 500)"
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
        let root = tokio::fs::canonicalize(&self.0)
            .await
            .map_err(|e| ToolErr(format!("skills 目录 {} 不存在: {e}", self.0.display())))?;

        let requested = if Path::new(&args.path).is_absolute() {
            PathBuf::from(&args.path)
        } else {
            self.0.join(&args.path)
        };
        let target = tokio::fs::canonicalize(&requested)
            .await
            .map_err(|e| ToolErr(format!("读取文件 `{}` 失败: {e}", args.path)))?;

        // canonicalize 后检查前缀,防止 ../ 逃逸出 skills 目录
        if !target.starts_with(&root) {
            return Err(ToolErr(format!(
                "`{}` 不在 skills 目录 {} 下",
                args.path,
                root.display()
            )));
        }

        let content = tokio::fs::read_to_string(&target)
            .await
            .map_err(|e| ToolErr(format!("读取文件 `{}` 失败: {e}", args.path)))?;

        // 按行分页:offset(1 起)~ offset+limit,输出带行号前缀
        let offset = args.offset.unwrap_or(1).max(1);
        let limit = args.limit.unwrap_or(DEFAULT_READ_LIMIT).max(1);
        let lines: Vec<&str> = content.lines().collect();
        let total = lines.len();
        let start = offset.saturating_sub(1);
        if start >= total {
            return Ok(format!(
                "(文件共 {total} 行,起始行 {} 已超出文件末尾)",
                offset
            ));
        }
        let end = (start + limit).min(total);
        let mut out = String::new();
        for (i, line) in lines[start..end].iter().enumerate() {
            out.push_str(&format!("{}: {}\n", start + i + 1, line));
        }
        if end < total {
            out.push_str(&format!(
                "…(已截断:还有 {} 行未显示,用 offset={} 继续读)",
                total - end,
                end + 1
            ));
        }

        // 兑底:单行超长导致输出超限时截断
        if out.chars().count() > MAX_READ_SKILL_CHARS {
            let mut truncated: String = out.chars().take(MAX_READ_SKILL_CHARS).collect();
            truncated.push_str("\n…(已截断)");
            out = truncated;
        }
        Ok(out)
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

// ----------------------------------------------------------------- send_file

/// Telegram Bot API 上传大小上限:50MB。
const MAX_SEND_BYTES: u64 = 50 * 1024 * 1024;

#[derive(Debug, Deserialize)]
pub struct SendFileArgs {
    /// 要发送的文件路径(绝对路径或相对当前工作目录的路径)
    pub path: String,
    /// 可选的文件说明,会显示在文件下方(caption)
    pub caption: Option<String>,
}

#[derive(Clone)]
pub struct SendFile(pub ToolCtx);

impl std::fmt::Debug for SendFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SendFile").finish()
    }
}

impl Tool for SendFile {
    const NAME: &'static str = "send_file";

    type Error = ToolErr;
    type Args = SendFileArgs;
    type Output = String;

    fn description(&self) -> String {
        "把本地的任意文件作为文档发送给用户".into()
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "要发送的文件路径(绝对路径)"
                },
                "caption": {
                    "type": "string",
                    "description": "可选的文件说明"
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

        // 先检查文件存在性和大小,避免审批通过后才发现发不出去
        let metadata = tokio::fs::metadata(&args.path)
            .await
            .map_err(|e| ToolErr(format!("读取文件 `{}` 失败: {e}", args.path)))?;
        if !metadata.is_file() {
            return Err(ToolErr(format!("`{}` 不是普通文件", args.path)));
        }
        if metadata.len() > MAX_SEND_BYTES {
            return Err(ToolErr(format!(
                "文件 `{}` 大小 {} 超过 Telegram 上传上限 50MB",
                args.path,
                human_size(metadata.len())
            )));
        }

        let approved = request_approval(
            &ctx.bot,
            ctx.chat_id,
            &ctx.approvals,
            ctx.approval_timeout,
            "send_file",
            &format!("发送文件:`{}`({})", args.path, human_size(metadata.len())),
        )
        .await
        .map_err(ToolErr)?;

        if !approved {
            tracing::info!("send_file 被用户拒绝: {}", args.path);
            return Ok(format!(
                "用户拒绝了发送文件 `{}`，立即停止尝试并追问用户原因。",
                args.path
            ));
        }

        tracing::info!("send_file 开始发送: {}", args.path);

        let mut req = ctx
            .bot
            .send_document(ctx.chat_id, teloxide::types::InputFile::file(&args.path));
        if let Some(caption) = &args.caption
            && !caption.trim().is_empty()
        {
            req = req.caption(caption.clone());
        }

        match req.await {
            Ok(_) => Ok(format!(
                "文件 `{}` 已发送给用户({})。",
                args.path,
                human_size(metadata.len())
            )),
            Err(e) => Err(ToolErr(format!("发送文件 `{}` 失败: {e}", args.path))),
        }
    }
}

// ----------------------------------------------------------- save_incoming

/// Telegram Bot API 的 getFile 下载上限:20MB(除非用本地 Bot API 服务器)。
const MAX_DOWNLOAD_BYTES: u64 = 20 * 1024 * 1024;

#[derive(Debug, Deserialize)]
pub struct SaveIncomingArgs {
    /// 用户发来文件的那条 Telegram 消息的 ID
    pub message_id: u64,
}

/// 把用户发来的文件(图片/视频/文档/音频)原样下载,存到收件箱目录。
/// 只在用户明确要求保存时才调用,避免每次都下载。
#[derive(Clone)]
pub struct SaveIncoming(pub ToolCtx);

impl std::fmt::Debug for SaveIncoming {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SaveIncoming").finish()
    }
}

/// 从 MIME 推断文件扩展名(用于没有文件名的音频等),不认识返回 None。
fn ext_from_mime(mime: &str) -> Option<&'static str> {
    match mime {
        "audio/mpeg" => Some("mp3"),
        "audio/ogg" => Some("ogg"),
        "audio/x-m4a" | "audio/mp4" | "audio/aac" => Some("m4a"),
        "audio/wav" | "audio/x-wav" => Some("wav"),
        "audio/flac" => Some("flac"),
        "video/mp4" => Some("mp4"),
        "video/webm" => Some("webm"),
        "application/pdf" => Some("pdf"),
        _ => None,
    }
}

/// 生成收件箱文件名和月份子目录:
/// 文件名 `<消息时间的 YYYYmmdd_HHMMSS>-<消息id>-<原始文件名>`,存到 `YYYY-MM/` 月份目录。
/// 没有原始文件名时按种类/MIME 补扩展名(如图片固定 .jpg)。
fn build_inbox_name(date: i64, message_id: u64, file: &IncomingFile) -> (String, String) {
    // 用消息发送时间(转本地时区)做时间戳前缀和月份目录名
    let (ts, month) = match chrono::DateTime::<chrono::Utc>::from_timestamp(date, 0) {
        Some(d) => {
            let d = d.with_timezone(&chrono::Local);
            (
                d.format("%Y%m%d_%H%M%S").to_string(),
                d.format("%Y-%m").to_string(),
            )
        }
        None => (date.to_string(), "unknown".to_string()),
    };
    // 只取文件名的最后一段,去掉路径分隔符和开头的点(避免 .. 或隐藏文件)
    let base = file
        .file_name
        .as_deref()
        .map(|n| {
            n.rsplit(['/', '\\'])
                .next()
                .unwrap_or(n)
                .trim()
                .trim_start_matches('.')
                .to_string()
        })
        .filter(|n| !n.is_empty());
    let name = match base {
        Some(name) => format!("{ts}-{message_id}-{name}"),
        None => {
            let ext = match file.kind {
                "图片" => "jpg",
                _ => ext_from_mime(file.mime.as_deref().unwrap_or_default()).unwrap_or("bin"),
            };
            format!("{ts}-{message_id}.{ext}")
        }
    };
    (month, name)
}

impl Tool for SaveIncoming {
    const NAME: &'static str = "save_incoming";

    type Error = ToolErr;
    type Args = SaveIncomingArgs;
    type Output = String;

    fn description(&self) -> String {
        format!(
            "把用户发来的文件(图片/视频/文档/音频)原样后台下载并保存到收件箱目录({}),下载完成后会自动通知用户。\
             只有用户明确要求保存/下载/留存他发来的文件时才调用,不要主动保存;\
             message_id 是用户消息里标注的消息 ID(文件是之前发的就从对话历史里找对应的消息 ID);\
             只能找到 bot 本次运行期间收到的文件,找不到时把工具报的错误原样转告用户",
            crate::config::Config::inbox_dir().display()
        )
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "message_id": {
                    "type": "integer",
                    "description": "用户发来文件的那条 Telegram 消息的 ID"
                }
            },
            "required": ["message_id"]
        })
    }

    async fn call(
        &self,
        _context: &mut ToolContext,
        args: Self::Args,
    ) -> Result<Self::Output, Self::Error> {
        let ctx = &self.0;

        // 1. 按消息 ID 查缓存拿文件元数据(消息进来时已记下,不下载本体)
        let Some(file) = ctx
            .incoming_files
            .get(ctx.chat_id, MessageId(args.message_id as i32))
            .await
        else {
            return Err(ToolErr(format!(
                "找不到消息 {} 对应的文件:消息 ID 可能不对,或文件是 bot 重启前发的(缓存已清空),或该消息里没有文件(支持图片/视频/文档/音频)",
                args.message_id
            )));
        };
        if file.file_size > MAX_DOWNLOAD_BYTES {
            return Err(ToolErr(format!(
                "文件大小 {} 超过 Telegram Bot API 的 20MB 下载上限,告知用户",
                human_size(file.file_size)
            )));
        }

        // 2. 拼好目标路径,先审批再下载(按消息时间的月份存子目录)
        let (month, name) = build_inbox_name(file.date, args.message_id, &file);
        let dir = crate::config::Config::inbox_dir().join(&month);
        let path = dir.join(&name);
        let label = file.file_name.clone().unwrap_or_else(|| name.clone());
        let size_desc = human_size(file.file_size);

        let approved = request_approval(
            &ctx.bot,
            ctx.chat_id,
            &ctx.approvals,
            ctx.approval_timeout,
            "save_incoming",
            &format!(
                "保存用户发来的{}:`{label}`({size_desc})\n到: {}",
                file.kind,
                path.display()
            ),
        )
        .await
        .map_err(ToolErr)?;

        if !approved {
            tracing::info!("save_incoming 被用户拒绝: 消息 {}", args.message_id);
            return Ok(format!(
                "用户拒绝了保存文件 `{label}`,立即停止尝试并追问用户原因。",
            ));
        }

        // 3. 后台下载:spawn 一个任务原样下载并写入,工具立即返回不阻塞 agent;
        // 下载完成(或失败)后由 bot 主动发消息通知用户
        tokio::fs::create_dir_all(&dir)
            .await
            .map_err(|e| ToolErr(format!("创建收件箱目录 {} 失败: {e}", dir.display())))?;
        let bot = ctx.bot.clone();
        let chat_id = ctx.chat_id;
        let file_id = file.file_id.clone();
        tracing::info!(
            "save_incoming 后台下载开始: 消息 {} → {}",
            args.message_id,
            path.display()
        );
        let path_str = path.display().to_string();
        tokio::spawn(async move {
            // 写入前一刻再解决同名冲突(后台期间可能有其他文件先落盘)
            let stem = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_string();
            let ext = path
                .extension()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_string();
            let mut target = path;
            let mut n = 1u32;
            while tokio::fs::try_exists(&target).await.unwrap_or(false) {
                let new_name = if ext.is_empty() {
                    format!("{stem}-{n}")
                } else {
                    format!("{stem}-{n}.{ext}")
                };
                target = dir.join(new_name);
                n += 1;
            }

            match download_file_bytes(&bot, &file_id).await {
                Ok(bytes) => {
                    if let Err(e) = tokio::fs::write(&target, &bytes).await {
                        tracing::warn!("save_incoming 写入 {} 失败: {e}", target.display());
                        let _ = bot
                            .send_message(chat_id, format!("⚠️ 文件保存失败: {e}"))
                            .await;
                        return;
                    }
                    tracing::info!(
                        "save_incoming 完成: {} ({})",
                        target.display(),
                        human_size(bytes.len() as u64)
                    );
                    let _ = bot
                        .send_message(
                            chat_id,
                            format!(
                                "✅ 文件已原样保存到: {}({})",
                                target.display(),
                                human_size(bytes.len() as u64)
                            ),
                        )
                        .await;
                }
                Err(e) => {
                    tracing::warn!("save_incoming 后台下载失败: {e}");
                    let _ = bot
                        .send_message(chat_id, format!("⚠️ 文件下载失败: {e}"))
                        .await;
                }
            }
        });

        Ok(format!(
            "已开始后台下载(共 {size_desc}),保存到: {path_str}。不用等下载完成,先告诉用户正在下载,完成后我会通知他。"
        ))
    }
}

// -------------------------------------------------------------------- vision

/// vision 工具输出上限:8K 字符
const MAX_VISION_CHARS: usize = 8192;

#[derive(Debug, Deserialize)]
pub struct VisionArgs {
    /// 要查看的图片路径(绝对路径或相对当前工作目录)
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
            "无法识别图片格式 `{}`(支持 jpg/png/gif/webp/heic/heif/svg)",
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
        "查看本地电脑上的图片文件(按路径指定)：是文字图片则按原结构提取文字，是风景/照片等非文字内容则详细描述图片内容。注意：只用于查看本地电脑上的图片；用户直接发来的图片通常已经能看到，除非消息中明确说明图片已保存到某个本地路径，否则不要调用本工具".into()
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "本地电脑上的图片文件路径(绝对路径，或相对当前工作目录)"
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

        // 先检查文件存在性和大小,避免审批通过后才发现读不到
        let metadata = tokio::fs::metadata(&args.path)
            .await
            .map_err(|e| ToolErr(format!("读取图片 `{}` 失败: {e}", args.path)))?;
        if !metadata.is_file() {
            return Err(ToolErr(format!("`{}` 不是普通文件", args.path)));
        }

        if is_temp {
            tracing::info!("vision 查看用户发来的图片(免审批): {}", args.path);
        } else {
            let approved = request_approval(
                &ctx.bot,
                ctx.chat_id,
                &ctx.approvals,
                ctx.approval_timeout,
                "vision",
                &format!("看图:`{}`({})", args.path, human_size(metadata.len())),
            )
            .await
            .map_err(ToolErr)?;

            if !approved {
                tracing::info!("vision 被用户拒绝: {}", args.path);
                return Ok(format!(
                    "用户拒绝了看图 `{}`，停止尝试并追问用户。",
                    args.path
                ));
            }
            tracing::info!("vision 开始看图: {}", args.path);
        }

        // 1–6. 读文件 → 压缩 → 调 vision 模型 → 截断输出。
        // 包在内部块里,保证任何一步失败(读文件、格式识别、网络等)都会走到下面的临时文件清理。
        let result: Result<String, ToolErr> = async {
            // 1. 读文件
            let bytes = tokio::fs::read(&args.path)
                .await
                .map_err(|e| ToolErr(format!("读取图片 `{}` 失败: {e}", args.path)))?;

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
                .map_err(|e| ToolErr(format!("vision 模型调用失败: {e}")))?;

            tracing::info!(
                "vision 完成: {} ({} 字符)",
                args.path,
                reply.chars().count()
            );

            // 6. 截断过长的输出
            if reply.chars().count() <= MAX_VISION_CHARS {
                Ok(reply)
            } else {
                let mut out: String = reply.chars().take(MAX_VISION_CHARS).collect();
                out.push_str("\n…(已截断)");
                Ok(out)
            }
        }
        .await;

        // 7. 删除临时图片(用户发来的转发图,无论调用成败都不再需要)
        if is_temp {
            match tokio::fs::remove_file(&args.path).await {
                Ok(()) => tracing::info!("已删除临时图片: {}", args.path),
                Err(e) => tracing::warn!("删除临时图片 `{}` 失败: {e}", args.path),
            }
        }
        result
    }
}
