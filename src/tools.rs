//! rig 工具:bash、send_file。
//! 每个工具执行前都会先通过 Telegram 内联按钮请用户明确同意。

use std::path::{Path, PathBuf};

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use rig::OneOrMany;
use rig::client::CompletionClient;
use rig::completion::message::{
    DocumentSourceKind, Image as RigImage, ImageMediaType, Text as RigText, UserContent,
};
use rig::completion::{Chat, Message as RigMessage};
use rig::providers::openai;
use rig::tool::Tool;
use serde::Deserialize;
use teloxide::prelude::*;
use thiserror::Error;

use crate::approval::{ApprovalManager, request_approval};
use crate::handlers::{compress_image, is_temp_image_path};

const MAX_OUTPUT_CHARS: usize = 30000;

#[derive(Debug, Error)]
#[error("{0}")]
pub struct ToolErr(pub String);

fn truncate(s: &str) -> String {
    if s.chars().count() <= MAX_OUTPUT_CHARS {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(MAX_OUTPUT_CHARS).collect();
        out.push_str("\n…(已截断)");
        out
    }
}

/// 两个工具共用的字段:目标聊天 + 审批管理器。
#[derive(Clone)]
pub struct ToolCtx {
    pub bot: Bot,
    pub chat_id: ChatId,
    pub approvals: ApprovalManager,
    pub bash_timeout: std::time::Duration,
    pub approval_timeout: std::time::Duration,
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

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
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
                Ok(truncate(&report))
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

/// read_skill 输出上限:128K 字符
const MAX_READ_SKILL_CHARS: usize = 128 * 1024;

#[derive(Debug, Deserialize)]
pub struct ReadSkillArgs {
    /// 要读取的文件路径(绝对路径,或相对于 skills 目录)
    pub path: String,
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
            "读取 skills 目录({})下的文件，如 SKILL.md 或它的附属文件",
            self.0.display()
        )
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "要读取的文件路径(绝对路径，或相对于 skills 目录)"
                }
            },
            "required": ["path"]
        })
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
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

        let truncated = if content.chars().count() <= MAX_READ_SKILL_CHARS {
            content
        } else {
            let mut out: String = content.chars().take(MAX_READ_SKILL_CHARS).collect();
            out.push_str("\n…(已截断)");
            out
        };
        Ok(truncated)
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

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
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

// -------------------------------------------------------------------- vision

/// vision 工具输出上限:16K 字符
const MAX_VISION_CHARS: usize = 32 * 1024;

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

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
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
            let content = OneOrMany::many(vec![
                UserContent::Text(RigText {
                    text: "请看这张图片。".to_string(),
                    additional_params: None,
                }),
                image,
            ])
            .expect("至少包含文本和图片两项内容");
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
