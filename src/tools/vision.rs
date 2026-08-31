//! vision 工具：用多模态模型看本地图片，调用前先请用户审批。

use std::path::Path;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use rig::client::AgentClientExt;
use rig::completion::message::{
    DocumentSourceKind, Image as RigImage, ImageMediaType, Text as RigText, UserContent,
};
use rig::completion::{Chat, Message as RigMessage};
use rig::providers::openai;
use rig::tool::{Tool, ToolContext};
use serde::Deserialize;

use crate::approval::request_approval;
use crate::image::compress_image;

use super::{ToolCtx, ToolErr, human_size, record_tool_result};

#[derive(Debug, Deserialize)]
pub struct VisionArgs {
    /// 要查看的图片路径（必须是绝对路径）
    pub path: String,
}

/// 看图工具：用多模态模型看图片，调用前先请用户审批。
#[derive(Clone)]
pub struct Vision {
    pub client: openai::CompletionsClient,
    pub model: String,
    /// 解析后的 vision 系统提示词（可被 VISION_SYSTEM.md 覆盖）
    pub system_prompt: String,
    /// 审批/发按钮所需的上下文（bot + chat + 审批管理器）
    pub ctx: ToolCtx,
}

impl std::fmt::Debug for Vision {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Vision")
            .field("model", &self.model)
            .finish()
    }
}

/// 根据文件扩展名推断图片 media type，无法识别则报错。
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
            "无法识别图片格式 `{}`（支持 jpg/png/gif/webp/heic/heif/svg）",
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
        "查看本地电脑上的图片：是文字图片则按原结构提取文字，是风景/照片等非文字内容则详细描述图片内容。注意：用户直接发来的图片通常已经能看到，除非消息中明确说明图片已保存到某个本地路径，否则不要调用本工具".into()
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "本地电脑上的图片文件路径（必须是绝对路径，不接受相对路径）"
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
        let ctx = &self.ctx;

        if !Path::new(&args.path).is_absolute() {
            return Err(ToolErr(format!(
                "路径 `{}` 不是绝对路径（只接受绝对路径）",
                args.path
            )));
        }

        // 先检查文件存在性和大小，避免审批通过后才发现读不到
        let metadata = tokio::fs::metadata(&args.path)
            .await
            .map_err(|e| ToolErr(format!("读取图片 `{}` 失败：{e}", args.path)))?;
        if !metadata.is_file() {
            return Err(ToolErr(format!("`{}` 不是普通文件", args.path)));
        }

        let approved = request_approval(
            &ctx.bot,
            ctx.chat_id,
            &ctx.approvals,
            ctx.approval_timeout,
            "vision",
            &format!("看图：`{}`（{}）", args.path, human_size(metadata.len())),
        )
        .await
        .map_err(ToolErr)?;

        if !approved {
            tracing::info!("vision 被用户拒绝：{}", args.path);
            return Ok(format!(
                "用户拒绝了看图 `{}`，停止尝试并追问用户。",
                args.path
            ));
        }
        tracing::info!("vision 开始看图：{}", args.path);

        // 1–6. 读文件 → 压缩 → 调 vision 模型 → 截断输出。
        let result: Result<String, ToolErr> = async {
            // 1. 读文件
            let bytes = tokio::fs::read(&args.path)
                .await
                .map_err(|e| ToolErr(format!("读取图片 `{}` 失败：{e}", args.path)))?;

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
            let content = vec![
                UserContent::Text(RigText {
                    text: "请看这张图片。".to_string(),
                    additional_params: None,
                }),
                image,
            ];
            let user_msg = RigMessage::User { content };

            // 5. 构建 vision agent 并调用（单轮，无需历史）
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
                .map_err(|e| ToolErr(format!("vision 模型调用失败：{e}")))?;

            tracing::info!(
                "vision 完成：{}（{} 字符）",
                args.path,
                reply.chars().count()
            );

            // 6. 全文落盘并注明保存位置，超长则返回头尾摘要
            record_tool_result(&ctx.toolout_dir, &reply).await
        }
        .await;

        result
    }
}
