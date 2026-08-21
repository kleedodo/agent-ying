//! rig 工具:bash、send_file。
//! 每个工具执行前都会先通过 Telegram 内联按钮请用户明确同意。

use rig::tool::Tool;
use serde::Deserialize;
use teloxide::prelude::*;
use thiserror::Error;

use crate::approval::{ApprovalManager, request_approval};

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
        "在 shell 中执行一条 bash 命令,返回 stdout 和 stderr。".into()
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
                "用户拒绝了执行命令 `{}`,请换一种方式或追问用户。",
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
                "文件 `{}` 大小 {}B 超过 Telegram 上传上限 50MB",
                args.path,
                metadata.len()
            )));
        }

        let approved = request_approval(
            &ctx.bot,
            ctx.chat_id,
            &ctx.approvals,
            ctx.approval_timeout,
            "send_file",
            &format!("发送文件:`{}`({}B)", args.path, metadata.len()),
        )
        .await
        .map_err(ToolErr)?;

        if !approved {
            tracing::info!("send_file 被用户拒绝: {}", args.path);
            return Ok(format!(
                "用户拒绝了发送文件 `{}`,请换一种方式或追问用户。",
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
                "文件 `{}` 已发送给用户({}B)。",
                args.path,
                metadata.len()
            )),
            Err(e) => Err(ToolErr(format!("发送文件 `{}` 失败: {e}", args.path))),
        }
    }
}
