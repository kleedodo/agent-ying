//! 发消息辅助：优先用 Markdown 格式，发送失败再回退纯文本。
//!
//! Telegram 的 Markdown 解析比较挑剔（未闭合的 `*`/`_`/反引号等都会导致 400），
//! agent 输出或工具详情里经常出现这类字符，所以统一走「Markdown 优先、失败回退纯文本」
//! 的路径，避免因为个别特殊字符导致整条消息发不出去。
//!
//! 这里用的是 legacy Markdown（而非 MarkdownV2）：后者要求几乎所有特殊字符都转义，
//! 对 LLM 输出几乎必然报错，会退化成「永远纯文本」；legacy 解析更宽松，
//! 能渲染出更多格式，实在不合法才走纯文本回退。

use teloxide::RequestError;
use teloxide::payloads::{EditMessageText, SendMessage};
use teloxide::prelude::*;
use teloxide::requests::JsonRequest;
use teloxide::types::{Message, ParseMode};

/// 发文本消息：先按 Markdown 解析发送，失败（通常是 Markdown 不合法）则回退纯文本重发。
/// 调用方原有的 `reply_markup` 等参数不受影响。
pub async fn send_md(builder: JsonRequest<SendMessage>) -> Result<Message, RequestError> {
    // legacy Markdown 已被标记 deprecated（官方建议 MarkdownV2），这里有意使用，见模块注释
    #[allow(deprecated)]
    let parsed = builder.clone().parse_mode(ParseMode::Markdown);
    match parsed.await {
        Ok(m) => Ok(m),
        Err(e) => {
            tracing::debug!("Markdown 发送失败，回退纯文本： {e}");
            builder.await
        }
    }
}

/// 编辑消息文本：先按 Markdown 解析发送，失败则回退纯文本重试。
pub async fn edit_md(builder: JsonRequest<EditMessageText>) -> Result<Message, RequestError> {
    #[allow(deprecated)]
    let parsed = builder.clone().parse_mode(ParseMode::Markdown);
    match parsed.await {
        Ok(m) => Ok(m),
        Err(e) => {
            tracing::debug!("Markdown 编辑失败，回退纯文本： {e}");
            builder.await
        }
    }
}
