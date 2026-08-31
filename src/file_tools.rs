//! write / edit 工具：
//! - write：写入文件（不存在则创建、存在则覆盖），自动建父目录
//! - edit：基于精确文本替换的多编辑，oldText 必须唯一且不重叠，
//!   全部编辑对着同一份原始内容匹配（非增量），支持模糊匹配兜底

use std::path::Path;

use rig::tool::{Tool, ToolContext};
use serde::Deserialize;
use serde::de::Deserializer;

use crate::approval::request_approval;
use crate::edits::{self, Edit as EditData};
use crate::tools::{ToolCtx, ToolErr, human_size, record_tool_result};

/// 审批卡片用的内容预览：最多取 max_chars 个字符，超长加省略号
fn preview(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(max_chars).collect::<String>())
    }
}

// --------------------------------------------------------------------- write

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

// ---------------------------------------------------------------------- edit

/// 一条编辑：oldText 必须是原文件中唯一的文本块
#[derive(Debug, Clone, Deserialize)]
pub struct EditOp {
    #[serde(rename = "oldText")]
    pub old_text: String,
    #[serde(rename = "newText")]
    pub new_text: String,
}

fn is_single_edit(v: &serde_json::Value) -> bool {
    v.get("oldText")
        .and_then(serde_json::Value::as_str)
        .is_some()
        && v.get("newText")
            .and_then(serde_json::Value::as_str)
            .is_some()
}

/// edits 的宽容反序列化：
/// 部分模型会把 edits 发成 JSON 字符串，或发成单个 {oldText,newText} 对象而非数组
fn deserialize_edits<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<EditOp>, D::Error> {
    fn from_value(v: serde_json::Value) -> Result<Vec<EditOp>, String> {
        match v {
            serde_json::Value::Array(items) => items
                .into_iter()
                .map(|e| serde_json::from_value::<EditOp>(e).map_err(|e| e.to_string()))
                .collect(),
            obj if is_single_edit(&obj) => {
                let one = serde_json::from_value::<EditOp>(obj).map_err(|e| e.to_string())?;
                Ok(vec![one])
            }
            other => Err(format!(
                "edits 应为 [{{oldText,newText}}] 数组，实际是：{other:?}"
            )),
        }
    }
    let v = serde_json::Value::deserialize(d)?;
    match v {
        serde_json::Value::String(s) => {
            let parsed: serde_json::Value = serde_json::from_str(&s)
                .map_err(|e| serde::de::Error::custom(format!("edits 字符串解析失败：{e}")))?;
            from_value(parsed).map_err(serde::de::Error::custom)
        }
        other => from_value(other).map_err(serde::de::Error::custom),
    }
}

#[derive(Debug, Deserialize)]
pub struct EditArgs {
    /// 目标文件路径（必须是绝对路径）
    pub path: String,
    /// 一组互不重叠的精确替换
    #[serde(deserialize_with = "deserialize_edits")]
    pub edits: Vec<EditOp>,
}

#[derive(Clone)]
pub struct Edit(pub ToolCtx);

impl std::fmt::Debug for Edit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Edit").finish()
    }
}

impl Tool for Edit {
    const NAME: &'static str = "edit";

    type Error = ToolErr;
    type Args = EditArgs;
    type Output = String;

    fn description(&self) -> String {
        "对单个文件做精确文本替换：每条 edits[].oldText 必须原样唯一命中、且彼此不重叠，全部编辑对着同一份原始文件匹配（非增量）。同一处或相邻行有多处改动时合并成一条编辑，不要发重叠/嵌套编辑。oldText 尽量小而唯一，不要为衔接远处的改动带大段不变内容。同一文件改多处时，用一次 edit 调用的 edits[] 带多条，而不是多次调用".into()
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "要编辑的文件路径（必须是绝对路径，不接受相对路径）"
                },
                "edits": {
                    "type": "array",
                    "description": "一组精确替换。每条编辑都对着原始文件匹配（非增量）。不要包含重叠或嵌套的编辑；同一块或相邻行有多处改动时合并成一条编辑",
                    "items": {
                        "type": "object",
                        "properties": {
                            "oldText": {
                                "type": "string",
                                "description": "要精确替换的原文，必须在原文件中唯一，且不与本次调用的其他 edits[].oldText 重叠"
                            },
                            "newText": {
                                "type": "string",
                                "description": "替换后的新文本"
                            }
                        },
                        "required": ["oldText", "newText"]
                    }
                }
            },
            "required": ["path", "edits"]
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
        if args.edits.is_empty() {
            return Err(ToolErr(
                "edits must contain at least one replacement.".into(),
            ));
        }

        // 先读文件算好 diff，再请用户审批（审批卡片里能直接看到改动内容）
        let raw = tokio::fs::read_to_string(&args.path)
            .await
            .map_err(|e| ToolErr(format!("Could not read file `{}`: {e}", args.path)))?;

        // 剥掉 BOM 再匹配（模型不会在 oldText 里带不可见 BOM）
        let (bom, content) = edits::split_bom(&raw);
        let original_ending = edits::detect_line_ending(content);
        let normalized = edits::normalize_to_lf(content);

        let data: Vec<EditData> = args
            .edits
            .iter()
            .map(|e| EditData {
                old_text: e.old_text.clone(),
                new_text: e.new_text.clone(),
            })
            .collect();
        let applied = edits::apply_edits(&normalized, &data, &args.path).map_err(ToolErr)?;
        let (diff, _) = edits::generate_diff_string(&applied.base, &applied.new);

        // 摘要：改动行数从 diff 文本统计（+N/-N 前缀行）
        let added = diff.lines().filter(|l| l.starts_with('+')).count();
        let removed = diff.lines().filter(|l| l.starts_with('-')).count();
        let mut lines = vec![format!(
            "编辑文件：`{}`（{} 处改动，+{added}/-{removed} 行）",
            args.path,
            args.edits.len()
        )];
        for (i, e) in args.edits.iter().enumerate() {
            lines.push(format!(
                "[{}] {} → {}",
                i + 1,
                preview(&e.old_text, 100),
                preview(&e.new_text, 100)
            ));
        }

        let approved = request_approval(
            &ctx.bot,
            ctx.chat_id,
            &ctx.approvals,
            ctx.approval_timeout,
            "edit",
            &lines.join("\n"),
        )
        .await
        .map_err(ToolErr)?;

        if !approved {
            tracing::info!("edit 被用户拒绝：{}", args.path);
            return Ok(format!(
                "用户拒绝了编辑文件 `{}`，立即停止尝试并追问用户原因。",
                args.path
            ));
        }

        tracing::info!("edit 写入：{}（{} 条编辑）", args.path, args.edits.len());
        // 还原原文件的换行风格后落盘
        let final_content = format!(
            "{bom}{}",
            edits::restore_line_endings(&applied.new, original_ending)
        );
        tokio::fs::write(&args.path, final_content)
            .await
            .map_err(|e| ToolErr(format!("Could not write file `{}`: {e}", args.path)))?;

        let report = format!(
            "Successfully replaced {} block(s) in `{}`.",
            args.edits.len(),
            args.path
        );
        record_tool_result(&ctx.toolout_dir, &report).await
    }
}
