//! bash 工具：在 shell 中执行一条命令，返回 stdout 和 stderr。

use std::path::Path;

use rig::tool::{Tool, ToolContext};
use serde::Deserialize;

use super::{ToolCtx, ToolErr, record_tool_result};

/// bash 输出超过该字符数时返回头尾摘要（全文始终落盘）
/// 输出会进多轮历史、每轮重复送给模型，故阈值偏保守：够多数命令用，超出就只给摘要让模型按需取
const MAX_OUTPUT_CHARS: usize = 16000;
/// 摘要中保留的头部字符数（MAX_OUTPUT_CHARS 按 5:3 分给头部）
const SPILL_HEAD_CHARS: usize = MAX_OUTPUT_CHARS * 5 / 8;
/// 摘要中保留的尾部字符数（补齐剩余，硬性保证两者之和等于 MAX_OUTPUT_CHARS）
const SPILL_TAIL_CHARS: usize = MAX_OUTPUT_CHARS - SPILL_HEAD_CHARS;

/// 把超长的 bash 输出截成头 + 尾摘要，注明省略的行区间和全文保存位置
/// 提示措辞刻意用"仅当需要时"的条件式，避免诱导模型在信息已足够时二次调用
fn spill_to_summary(s: &str, path: &Path) -> String {
    let total = s.chars().count();
    if total <= MAX_OUTPUT_CHARS {
        return s.to_string();
    }
    let lines: Vec<&str> = s.lines().collect();
    // 从两端按行取，各自不超字符预算，保证按行边界截断不切碎行
    let mut head: Vec<&str> = Vec::new();
    let mut h = 0;
    for line in &lines {
        let n = line.chars().count() + 1;
        if h + n <= SPILL_HEAD_CHARS {
            head.push(line);
            h += n;
        } else {
            break;
        }
    }
    let mut tail: Vec<&str> = Vec::new();
    let mut t = 0;
    for line in lines.iter().rev() {
        let n = line.chars().count() + 1;
        if t + n <= SPILL_TAIL_CHARS {
            tail.insert(0, line);
            t += n;
        } else {
            break;
        }
    }

    // 完整路径只在保存说明里出现一次，省 token；提示是条件式的，且给出精准取段命令
    let is_gz = path.extension().and_then(|e| e.to_str()) == Some("gz");
    let (dropped_note, fetch_hint) = if head.len() + tail.len() < lines.len() {
        let a = head.len() + 1;
        let b = lines.len() - tail.len();
        let fetch = if is_gz {
            // read 不解压，gz 仍走 bash
            format!(
                "用 bash 执行 `zcat {} | sed -n '{a},{b}p'` 查看",
                path.display()
            )
        } else {
            format!("用 read 工具查看（offset={a}, limit={}）", b - a + 1)
        };
        (format!("共 {} 行，已省略第 {a}–{b} 行", lines.len()), fetch)
    } else {
        (format!("共 {} 字符", total), "查看该文件".into())
    };

    format!(
        "{}\n…（{dropped_note}；完整输出已保存到 {}。仅当你还需要省略的中间内容时才{}该文件，否则当前头尾摘要已足够。）\n{}",
        head.join("\n"),
        path.display(),
        fetch_hint,
        tail.join("\n")
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

        let mut cmd = tokio::process::Command::new("bash");
        cmd.arg("-lc");
        cmd.arg(&args.command);
        // 发送者的 Telegram user id 以环境变量暴露，命令里用 $TG_USER_ID 引用
        if let Some(id) = ctx.tg_user_id {
            cmd.env("TG_USER_ID", id.to_string());
        }
        let future = cmd.output();

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
