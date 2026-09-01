//! read 工具：读取一个文本文件，只读无副作用，但仍需审批。

use std::path::Path;

use rig::tool::{Tool, ToolContext};
use serde::Deserialize;
use tokio::io::AsyncReadExt;

use crate::approval::request_approval;

use super::{ToolCtx, ToolErr, human_size, record_tool_result};

/// read 默认最多读取的行数
const DEFAULT_READ_LIMIT: usize = 2000;
/// read 输出字节上限（与行数上限取先到者）
const MAX_READ_BYTES: usize = 50 * 1024;

#[derive(Debug, Deserialize)]
pub struct ReadArgs {
    /// 要读取的文件路径（必须是绝对路径）
    pub path: String,
    /// 从第几行开始读（1 起，默认 1）
    pub offset: Option<usize>,
    /// 最多读多少行（默认 2000，输出达到 50KB 也会截断）
    pub limit: Option<usize>,
}

/// 只读一个文本文件（skills 文件或其他任意文件），只读无副作用，但仍需审批。
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
        "读取一个文本文件，返回原始内容（不带行号前缀）；输出被截断时会附提示，说明如何继续读取"
            .into()
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

        // 先确认文件存在再请审批，避免审批通过后才发现读不到
        let approved = request_approval(
            &ctx.bot,
            ctx.chat_id,
            &ctx.approvals,
            ctx.approval_timeout,
            "read",
            &format!(
                "读取文件：`{}`（offset={}，limit={limit}）",
                args.path,
                args.offset.unwrap_or(1),
                limit = args.limit.filter(|l| *l > 0).unwrap_or(DEFAULT_READ_LIMIT),
            ),
        )
        .await
        .map_err(ToolErr)?;

        if !approved {
            tracing::info!("read 被用户拒绝：{}", args.path);
            return Ok(format!(
                "用户拒绝了读取文件 `{}`，立即停止尝试并追问用户原因。",
                args.path
            ));
        }
        tracing::info!("read 开始读取：{}", args.path);

        // 先检查文件可读性
        let mut file = tokio::fs::File::open(&target)
            .await
            .map_err(|e| ToolErr(format!("文件 `{}` 不可读：{e}", args.path)))?;
        let mut content = String::new();
        file.read_to_string(&mut content)
            .await
            .map_err(|e| ToolErr(format!("读取文件 `{}` 失败：{e}", args.path)))?;

        // 行计数：split('\n')（末尾换行会多出一个空行，不做特殊处理）
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

        // 用户 limit 先截取，之后仍统一受 2000 行/50KB 上限约束
        let (selected, user_limited): (String, Option<usize>) = match args.limit {
            Some(l) if l > 0 => {
                let end = (start + l).min(total);
                (all_lines[start..end].join("\n"), Some(end - start))
            }
            _ => (all_lines[start..].join("\n"), None),
        };

        // 行数统计：末尾换行不产生额外行
        let mut sel_lines: Vec<&str> = selected.split('\n').collect();
        if selected.ends_with('\n') {
            sel_lines.pop();
        }

        // 未超限：原样返回（不带行号前缀）
        if sel_lines.len() <= DEFAULT_READ_LIMIT && selected.len() <= MAX_READ_BYTES {
            // 未截断但用户 limit 提前停、文件还有剩余：提示续读
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

        // 首行单行即超 50KB：正常返回提示让 agent 改用 bash（不算工具错误）
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

        // 从头收集完整行，直到达到行数或字节上限（永不返回半行）
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
