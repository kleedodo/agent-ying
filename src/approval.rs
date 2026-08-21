//! 审批管理:工具执行前,agent 在 Telegram 里发一条带「同意 / 拒绝」按钮的消息,
//! 等待用户点击后通过 oneshot channel 把结果传回工具。

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use teloxide::prelude::*;
use teloxide::types::{InlineKeyboardButton, InlineKeyboardMarkup, MessageId};
use tokio::sync::{Mutex, oneshot};
use tokio::time::Duration;

/// 每个 chat 最近一条审批消息:消息 id、审批 id、原文(用于被取代时保留详情)
type LastApproval = HashMap<ChatId, (MessageId, String, String)>;

/// 从审批消息原文里提取工具/命令信息:去掉开头的 🔧 和结尾的「是否放行?」。
/// 用于把消息改成「已同意/已超时/已被取代」等状态时保留详情,方便回看。
pub fn approval_body(original: &str) -> String {
    original
        .strip_prefix("🔧 ")
        .unwrap_or(original)
        .lines()
        .take_while(|line| !line.trim().starts_with("是否放行?"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// 全局审批管理器(可 Clone,内部是 Arc)。
#[derive(Clone, Default)]
pub struct ApprovalManager {
    next_id: Arc<AtomicU64>,
    pending: Arc<Mutex<HashMap<String, oneshot::Sender<bool>>>>,
    /// 每个 chat 最近一条审批消息:消息 id、审批 id、原文(用于被取代时保留详情)
    last_approval_message: Arc<Mutex<LastApproval>>,
}

impl ApprovalManager {
    /// 记录本 chat 最新的审批消息;若存在旧的:
    /// 1. 旧审批若仍在等待(比如并行工具调用),自动按拒绝处理,免得 agent 卡住等超时;
    /// 2. 把旧消息的按钮摘掉并改文字,防止用户点到旧按钮「没反应」。
    pub async fn remember_approval_message(
        &self,
        bot: &Bot,
        chat_id: ChatId,
        message_id: MessageId,
        approval_id: &str,
        original_text: &str,
    ) {
        let mut map = self.last_approval_message.lock().await;
        if let Some((prev_msg_id, prev_approval_id, prev_text)) = map.insert(
            chat_id,
            (
                message_id,
                approval_id.to_string(),
                original_text.to_string(),
            ),
        ) {
            if let Some(tx) = self.pending.lock().await.remove(&prev_approval_id) {
                tracing::info!("旧审批被新审批取代,自动按拒绝处理: {}", prev_approval_id);
                let _ = tx.send(false);
            }
            if prev_msg_id != message_id {
                // 保留旧命令详情,方便回看被取代的是什么
                let body = approval_body(&prev_text);
                let text = if body.is_empty() {
                    "🔧 已被新的审批请求取代".to_string()
                } else {
                    format!("🔧 已被新的审批请求取代\n\n{body}")
                };
                let _ = bot
                    .edit_message_text(chat_id, prev_msg_id, text)
                    .reply_markup(InlineKeyboardMarkup {
                        inline_keyboard: vec![],
                    })
                    .await;
            }
        }
    }
}

impl ApprovalManager {
    pub fn new() -> Self {
        Self::default()
    }

    /// 注册一个待审批项,返回审批 id 和等待用户决定的 receiver。
    pub async fn register(&self) -> (String, oneshot::Receiver<bool>) {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed).to_string();
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id.clone(), tx);
        (id, rx)
    }

    /// 用户点击按钮后,取出对应审批项并发送决定。
    /// 返回 None 表示该审批已过期/已被处理。
    pub async fn resolve(&self, id: &str) -> Option<oneshot::Sender<bool>> {
        self.pending.lock().await.remove(id)
    }

    /// 超时等场景下把待审批项摘掉,避免迟到的点击误报「已同意」。
    pub async fn expire(&self, id: &str) {
        self.pending.lock().await.remove(id);
    }
}

/// 在指定聊天里发一条带内联按钮的审批消息,阻塞等待用户点击。
/// 用户点「同意」返回 Ok(true),「拒绝」返回 Ok(false),
/// 按钮过期(receiver 被丢)也当作拒绝。
pub async fn request_approval(
    bot: &Bot,
    chat_id: ChatId,
    approvals: &ApprovalManager,
    timeout: Duration,
    tool: &str,
    detail: &str,
) -> Result<bool, String> {
    let (id, rx) = approvals.register().await;

    let kb = InlineKeyboardMarkup::new([[
        InlineKeyboardButton::callback("✅ 同意", format!("approve:{id}")),
        InlineKeyboardButton::callback("❌ 拒绝", format!("deny:{id}")),
    ]]);

    tracing::info!(
        "审批请求: chat={} tool={} detail={:?}",
        chat_id,
        tool,
        detail
    );
    let text = format!("🔧 Agent 请求调用工具 `{tool}`\n\n{detail}\n\n是否放行?");
    let sent = bot
        .send_message(chat_id, text.clone())
        .reply_markup(kb)
        .await
        .map_err(|e| e.to_string())?;
    // 把本 chat 上一条审批消息的按钮摘掉,避免点到旧按钮「没反应」
    approvals
        .remember_approval_message(bot, chat_id, sent.id, &id, &text)
        .await;

    // 超时或 rx 被 drop(sender 没了)都按拒绝处理
    let approved = match tokio::time::timeout(timeout, rx).await {
        Ok(Ok(v)) => v,
        Ok(Err(_)) => false,
        Err(_) => {
            tracing::warn!(
                "审批超时({}s),按拒绝处理: chat={} tool={}",
                timeout.as_secs(),
                chat_id,
                tool,
            );
            // 摘掉待审批项,防止迟到的点击命中已超时的审批
            approvals.expire(&id).await;
            // 给用户可见反馈:改文字并摘按钮,免得用户以为按钮坏了反复点;
            // 保留工具/命令详情,方便回看当时超时的是什么
            let _ = bot
                .edit_message_text(
                    chat_id,
                    sent.id,
                    format!("⏰ 审批超时,已按拒绝处理\n\nAgent 请求调用工具 `{tool}`\n\n{detail}"),
                )
                .reply_markup(InlineKeyboardMarkup {
                    inline_keyboard: vec![],
                })
                .await;
            false
        }
    };
    if !approved {
        tracing::info!("审批被拒绝: chat={} tool={}", chat_id, tool);
    }
    Ok(approved)
}
