//! bash 工具：在 shell 中执行一条命令，返回 stdout 和 stderr。

use std::path::Path;

use rig::tool::{Tool, ToolContext};
use serde::Deserialize;

use super::{ToolCtx, ToolErr, record_tool_result};

/// bash 输出预算：50KB（字节）与 2000 行谁先到按谁，与 read 工具的单页上限（50KB / 2000 行）对齐
/// 输出会进多轮历史、每轮重复送给模型，故阈值偏保守：够多数命令用，超出就只给头部让模型按需取
const MAX_OUTPUT_BYTES: usize = 50 * 1024;
const MAX_OUTPUT_LINES: usize = 2000;

/// 把超长的 bash 输出截成头部（按行边界，始终保持行的完整性），
/// 注明总行数、已保留行数、下一行的行号和全文保存位置
/// 提示措辞刻意用"仅当需要时"的条件式，避免诱导模型在信息已足够时二次调用
/// toolout 文件一律纯文本，所以统一用 read 工具（offset/limit）分页，任意深度都能读到
fn spill_to_summary(s: &str, path: &Path) -> String {
    let total_lines = s.lines().count();
    // 从头按行取，字节数和行数任一预算用完即停，保证不切碎行
    let mut kept: Vec<&str> = Vec::new();
    let mut bytes = 0;
    for line in s.lines() {
        if kept.len() == MAX_OUTPUT_LINES || bytes + line.len() + 1 > MAX_OUTPUT_BYTES {
            break;
        }
        let n = line.len() + 1;
        kept.push(line);
        bytes += n;
    }
    if kept.len() == total_lines {
        return s.to_string();
    }
    let offset = kept.len() + 1;
    format!(
        "{}\n…（共 {total_lines} 行，以上为前 {} 行；完整输出已保存到 {}。仅当你还需要省略的内容时才用 read 工具从第 {offset} 行继续读取（offset={offset}，每次最多 2000 行，可反复翻页）；有关键词的话用 grep 工具直接在该文件里搜索更方便；否则当前摘要已足够。）",
        kept.join("\n"),
        kept.len(),
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
