//! bash 工具：在 shell 中执行一条命令，返回 stdout 和 stderr。

use std::path::Path;

use rig::tool::{Tool, ToolContext};
use serde::Deserialize;

use super::{ToolCtx, ToolErr, record_tool_result};

/// bash 输出超过该字符数时返回头尾摘要（全文始终落盘）
/// 输出会进多轮历史、每轮重复送给模型，故阈值偏保守：够多数命令用，超出就只给摘要让模型按需取
const MAX_OUTPUT_CHARS: usize = 8000;
/// 摘要中保留的头部字符数（与尾部之和须小于 MAX_OUTPUT_CHARS）
const SPILL_HEAD_CHARS: usize = 3000;
/// 摘要中保留的尾部字符数
const SPILL_TAIL_CHARS: usize = 2000;

/// 把超长的 bash 输出截成头 + 尾摘要，并注明全文保存位置
fn spill_to_summary(s: &str, path: &Path) -> String {
    let total = s.chars().count();
    if total <= MAX_OUTPUT_CHARS {
        return s.to_string();
    }
    // 完整路径只在保存说明里出现一次；命令只给工具名不重复拼路径，省 token
    let is_gz = path.extension().and_then(|e| e.to_str()) == Some("gz");
    let cmd_hint = if is_gz {
        "可用 zgrep 查看/搜索该文件"
    } else {
        "可用 cat 查看、grep 搜索、sed/tail 截取该文件"
    };
    let head: String = s.chars().take(SPILL_HEAD_CHARS).collect();
    let tail: String = s.chars().skip(total - SPILL_TAIL_CHARS).collect();
    let dropped = total - SPILL_HEAD_CHARS - SPILL_TAIL_CHARS;
    format!(
        "{head}\n…（共 {total} 字符，中间省略 {dropped} 字符；完整输出已保存到 {}。{cmd_hint}。）\n{tail}",
        path.display()
    )
}

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
        "在 shell 中执行一条 bash 命令，返回退出码、stdout 和 stderr".into()
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
                let path = record_tool_result(&ctx.toolout_dir, &report).await?;
                Ok(spill_to_summary(&report, &path))
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
