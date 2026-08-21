//! rig 工具:bash。
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
            "bash",
            &format!("执行命令:`{}`", args.command),
        )
        .await
        .map_err(ToolErr)?;

        if !approved {
            log::info!("bash 被用户拒绝: {}", args.command);
            return Ok(format!(
                "用户拒绝了执行命令 `{}`,请换一种方式或追问用户。",
                args.command
            ));
        }

        log::info!("bash 开始执行: {}", args.command);

        let future = tokio::process::Command::new("bash")
            .arg("-lc")
            .arg(&args.command)
            .output();

        match tokio::time::timeout(ctx.bash_timeout, future).await {
            Ok(Ok(output)) => {
                log::info!(
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
                log::warn!(
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
