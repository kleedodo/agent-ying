//! write 工具：写入文件（不存在则创建、存在则覆盖），自动建父目录。

use std::path::Path;

use rig::tool::{Tool, ToolContext};
use serde::Deserialize;

use crate::approval::request_approval;

use super::{ToolCtx, ToolErr, human_size, preview, record_tool_result};

#[derive(Debug, Deserialize)]
pub struct WriteArgs {
    /// 目标文件路径（必须是绝对路径）
    pub path: String,
    /// 要写入的完整内容
    pub content: String,
}

#[derive(Clone)]
pub struct Write(pub ToolCtx);

impl std::fmt::Debug for Write {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Write").finish()
    }
}

impl Tool for Write {
    const NAME: &'static str = "write";

    type Error = ToolErr;
    type Args = WriteArgs;
    type Output = String;

    fn description(&self) -> String {
        "写入文件：只在创建新文件或完整重写时使用 write，小改动用 edit 工具".into()
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "要写入的文件路径（必须是绝对路径，不接受相对路径）"
                },
                "content": {
                    "type": "string",
                    "description": "要写入文件的完整内容"
                }
            },
            "required": ["path", "content"]
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

        let approved = request_approval(
            &ctx.bot,
            ctx.chat_id,
            &ctx.approvals,
            ctx.approval_timeout,
            "write",
            &format!(
                "写文件：`{}`（{} 字节）\n{}",
                args.path,
                human_size(args.content.len() as u64),
                preview(&args.content, 100),
            ),
        )
        .await
        .map_err(ToolErr)?;

        if !approved {
            tracing::info!("write 被用户拒绝：{}", args.path);
            return Ok(format!(
                "用户拒绝了写文件 `{}`，立即停止尝试并追问用户原因。",
                args.path
            ));
        }

        tracing::info!("write 开始写入：{}", args.path);
        // 先建父目录再落盘
        if let Some(dir) = Path::new(&args.path).parent()
            && !dir.as_os_str().is_empty()
        {
            tokio::fs::create_dir_all(dir)
                .await
                .map_err(|e| ToolErr(format!("创建目录 {} 失败：{e}", dir.display())))?;
        }
        tokio::fs::write(&args.path, &args.content)
            .await
            .map_err(|e| ToolErr(format!("写入文件 `{}` 失败：{e}", args.path)))?;

        let report = format!(
            "Successfully wrote {} bytes to `{}`.",
            args.content.len(),
            args.path
        );
        record_tool_result(&ctx.toolout_dir, &report).await
    }
}
