//! Telegram 更新处理器:文本消息 → 跑 agent;按钮回调 → 决定工具审批。

use rig::completion::{Chat, Message as RigMessage};
use teloxide::prelude::*;
use teloxide::types::InlineKeyboardMarkup;

use crate::{AppState, approval::approval_body};

pub type HandlerResult = Result<(), Box<dyn std::error::Error + Send + Sync>>;

/// 处理用户发来的文本消息:跑一轮 agent 对话(带多轮历史)。
pub async fn on_message(state: AppState, msg: Message) -> HandlerResult {
    // 只响应配置里允许的用户
    let Some(from) = msg.from.as_ref() else {
        state
            .bot
            .send_message(msg.chat.id, "🚫 无法识别发送者,请私聊我。")
            .await?;
        return Ok(());
    };
    if !state.allows_user(from.id) {
        state
            .bot
            .send_message(msg.chat.id, "🚫 未授权用户,请找 bot 主人加白名单。")
            .await?;
        return Ok(());
    }

    let Some(text) = msg.text().map(str::to_owned) else {
        state
            .bot
            .send_message(msg.chat.id, "请发送文本消息 🙏")
            .await?;
        return Ok(());
    };

    // 简单的 /start、/help、/new 命令
    if text.starts_with("/start") || text.starts_with("/help") {
        state
            .bot
            .send_message(
                msg.chat.id,
                "👋 我是 ying!直接发文本就行。\n\
                 我可以用 `bash` 跑命令,\
                 每次调用工具前都会发按钮请你明确同意。\n\
                 发送 /new 可以开启新会话(清空对话历史)。",
            )
            .await?;
        return Ok(());
    }
    if text.starts_with("/new") {
        let mut map = state.histories.lock().await;
        map.remove(&msg.chat.id);
        state
            .bot
            .send_message(msg.chat.id, "🆕 新会话已开始,之前的对话历史已清空。")
            .await?;
        return Ok(());
    }

    log::info!(
        "收到消息: chat={} user={:?} text={:?}",
        msg.chat.id,
        msg.from.as_ref().map(|f| f.id),
        text,
    );

    let chat_id = msg.chat.id;
    let agent = state.agent_for(chat_id);

    state
        .bot
        .send_message(chat_id, "🤔 思考中…(调用工具时会发按钮请你确认)")
        .await?;

    // 每个 chat 单独维护多轮对话历史(先取出再写回)
    let mut history: Vec<RigMessage> = {
        let map = state.histories.lock().await;
        map.get(&chat_id).cloned().unwrap_or_default()
    };

    match agent.chat(text, &mut history).await {
        Ok(reply) => {
            log::info!(
                "Agent 回复完成: chat={} 共 {} 轮历史",
                chat_id,
                history.len()
            );
            state.bot.send_message(chat_id, reply).await?;
        }
        Err(e) => {
            log::error!("Agent 出错: chat={} {e}", chat_id);
            state
                .bot
                .send_message(chat_id, format!("⚠️ Agent 出错: {e}"))
                .await?;
        }
    }

    {
        let mut map = state.histories.lock().await;
        map.insert(chat_id, history);
    }
    Ok(())
}

/// 兜底分支:打印没被上面任何 handler 匹配的 update,方便排查丢失的回调等。
pub async fn on_unmatched(update: Update) -> HandlerResult {
    log::info!(
        "收到未匹配的 update: id={} kind={:?}",
        update.id.0,
        update.kind
    );
    Ok(())
}

/// 把审批消息改成「已决定」状态时,保留原来的工具/命令信息,
/// 方便用户回头看当时批了什么。
fn decided_text(label: &str, original: &str) -> String {
    let body = approval_body(original);
    if body.is_empty() {
        label.to_string()
    } else {
        format!("{label}\n\n{body}")
    }
}

/// 处理「同意 / 拒绝」按钮回调。
pub async fn on_callback(state: AppState, q: CallbackQuery) -> HandlerResult {
    log::info!("收到回调: data={:?}", q.data);

    // 只响应配置里允许的用户
    if !state.allows_user(q.from.id) {
        log::info!("未授权用户点击按钮: {:?}", q.from.id);
        let _ = state
            .bot
            .answer_callback_query(q.id.clone())
            .text("🚫 未授权用户")
            .await;
        return Ok(());
    }

    let Some((action, id)) = q.data.as_deref().and_then(|d| d.split_once(':')) else {
        log::warn!("回调 data 解析失败: {:?}", q.data);
        let _ = state
            .bot
            .answer_callback_query(q.id.clone())
            .text("⚠️ 按钮数据无效")
            .await;
        return Ok(());
    };

    let approve = match action {
        "approve" => true,
        "deny" => false,
        other => {
            log::warn!("未知 action: {other}");
            let _ = state
                .bot
                .answer_callback_query(q.id.clone())
                .text("⚠️ 未知按钮")
                .await;
            return Ok(());
        }
    };

    // resolve 后 send 仍可能失败:点击恰好落在超时边界,等待方已把 receiver 丢掉。
    // 这时按「已过期」处理,免得给用户一个假的「已同意」。
    let resolved = state
        .approvals
        .resolve(id)
        .await
        .and_then(|tx| tx.send(approve).ok())
        .is_some();
    match resolved {
        true => {
            log::info!("审批决定: {} → {}", action, id);
            // 把审批消息改成「已同意 / 已拒绝」并移除按钮,避免重复点击;
            // 保留命令详情,方便回看
            if let Some(m) = q.message.as_ref().and_then(|m| m.regular_message()) {
                let label = if approve {
                    "✅ 已同意"
                } else {
                    "❌ 已拒绝"
                };
                let text = m.text().unwrap_or_default();
                let _ = state
                    .bot
                    .edit_message_text(m.chat.id, m.id, decided_text(label, text))
                    .reply_markup(InlineKeyboardMarkup {
                        inline_keyboard: vec![],
                    })
                    .await;
            }
        }
        // 常见原因:重复点击,或点击的是上一次 bot 进程留下的旧按钮(内存里的待审批表已清空)
        false => {
            log::warn!("找不到待审批项(可能已处理或已过期): {} → {}", action, id);
            // 直接把旧消息改掉并摘掉按钮,给用户可见的反馈(同样保留命令详情)
            if let Some(m) = q.message.as_ref().and_then(|m| m.regular_message()) {
                let text = m.text().unwrap_or_default();
                let _ = state
                    .bot
                    .edit_message_text(
                        m.chat.id,
                        m.id,
                        decided_text("⏳ 该审批已处理或已过期", text),
                    )
                    .reply_markup(InlineKeyboardMarkup {
                        inline_keyboard: vec![],
                    })
                    .await;
            }
            let _ = state
                .bot
                .answer_callback_query(q.id.clone())
                .text("⏳ 该按钮已处理或已过期")
                .await;
        }
    }

    let _ = state.bot.answer_callback_query(q.id.clone()).await;
    Ok(())
}
