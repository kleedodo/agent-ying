//! edit 工具：基于精确文本替换的多编辑，oldText 必须唯一且不重叠，
//! 全部编辑对着同一份原始内容匹配（非增量），支持模糊匹配兜底。
//! 实际的文本 diff/替换算法在 [super::edit_algo]。

use std::path::Path;

use rig::tool::{Tool, ToolContext};
use serde::Deserialize;
use serde::de::Deserializer;

use crate::tools::edit_algo::{self, Edit as EditData};

use super::{ToolCtx, ToolErr, record_tool_result};

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
        "对单个文件做精确文本替换；同一文件改多处时，用一次 edit 调用的 edits[] 带多条，而不是多次调用".into()
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
                    "description": "一组精确替换，每条编辑都对着同一份原始文件匹配（非增量），不要包含重叠或嵌套的编辑；同一处或相邻行有多处改动时合并成一条编辑",
                    "items": {
                        "type": "object",
                        "properties": {
                            "oldText": {
                                "type": "string",
                                "description": "要精确替换的原文，必须在原文件中唯一，且不与本次调用的其他 edits[].oldText 重叠。尽量小而唯一，不要为衔接远处的改动带大段不变内容"
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

        // 先读文件并应用编辑（审批卡片里的 diff/预览由 ApprovalHook 另行计算）
        let raw = tokio::fs::read_to_string(&args.path)
            .await
            .map_err(|e| ToolErr(format!("Could not read file `{}`: {e}", args.path)))?;

        // 剥掉 BOM 再匹配（模型不会在 oldText 里带不可见 BOM）
        let (bom, content) = edit_algo::split_bom(&raw);
        let original_ending = edit_algo::detect_line_ending(content);
        let normalized = edit_algo::normalize_to_lf(content);

        let data: Vec<EditData> = args
            .edits
            .iter()
            .map(|e| EditData {
                old_text: e.old_text.clone(),
                new_text: e.new_text.clone(),
            })
            .collect();
        let applied = edit_algo::apply_edits(&normalized, &data, &args.path).map_err(ToolErr)?;

        tracing::info!("edit 写入：{}（{} 条编辑）", args.path, args.edits.len());
        // 还原原文件的换行风格后落盘
        let final_content = format!(
            "{bom}{}",
            edit_algo::restore_line_endings(&applied.new, original_ending)
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
