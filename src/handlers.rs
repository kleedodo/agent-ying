//! Telegram 更新处理器：文本消息 → 跑 agent；按钮回调 → 决定工具审批。

use std::time::Duration;

use futures::StreamExt;
use rig::agent::StreamingError;
use rig::completion::Message as RigMessage;
use rig::prelude::{MultiTurnStreamItem, StreamingChat};
use rig::streaming::StreamedAssistantContent;
use std::collections::HashMap;
use teloxide::prelude::*;

use teloxide::types::{ChatAction, ChatId, InlineKeyboardMarkup, MessageId, UserId};
use tokio::time::Instant;
use uuid::Uuid;

use crate::{
    AppState,
    approval::{ResolveOutcome, approval_body},
    journal::SessionFile,
    usermsg::build_user_message,
};

pub type HandlerResult = Result<(), Box<dyn std::error::Error + Send + Sync>>;

/// 处理用户发来的文本消息：跑一轮 agent 对话（带多轮历史）。
pub async fn on_message(state: AppState, msg: Message) -> HandlerResult {
    // 只响应配置里允许的用户
    let Some(from) = msg.from.as_ref() else {
        tracing::warn!("无法识别消息发送者（无 from 字段）: chat={}", msg.chat.id);
        return Ok(());
    };
    if !state.allows_user(from.id) {
        // 未授权用户：直接回复其 from.id；每人每 30 分钟最多回 3 次
        let allowed = {
            let mut map = state.unauth_replies.lock().await;
            allow_unauth_reply(&mut map, from.id)
        };
        if allowed {
            tracing::info!("未授权用户 {:?}，回复其 from.id", from.id);
            state
                .bot
                .send_message(msg.chat.id, from.id.0.to_string())
                .await?;
        } else {
            tracing::info!(
                "未授权用户 {:?}，半小时内已回复 {} 次，不再回复",
                from.id,
                UNAUTH_MAX_PER_WINDOW
            );
        }
        return Ok(());
    }

    let chat_id = msg.chat.id;
    let text = msg.text().map(str::to_owned);

    // 文件类消息没有 text，日志里改打 caption；没有 caption 则打 [XX消息] 占位
    let log_text = text.clone().or_else(|| describe_for_log(&msg));

    // 简单的 /start、/help、/new 命令（文本消息）
    if let Some(t) = &text
        && handle_command(&state, chat_id, t).await?
    {
        return Ok(());
    }

    // 当前会话文件：chat 还没有则新建（/new 或进程重启后都会是新文件）。
    // 需先于 build_user_message 拿到：文件消息会自动下载到该会话的 media/ 目录
    let session = {
        let mut map = state.sessions.lock().await;
        match map.get(&chat_id).cloned() {
            Some(s) => s,
            None => {
                let s = state
                    .journal
                    .create_session(chat_id.0)
                    .await
                    .map_err(|e| format!("创建 journal 会话文件失败： {e}"))?;
                map.insert(chat_id, s.clone());
                s
            }
        }
    };

    // 构建发给模型的用户消息：纯文本，或图片（可带说明文字）
    // forward_to_vision 且 vision 已启用时，图片转存会话 media/ 目录并提示调 vision 工具；
    // 否则（包括 vision 未启用的情况）图片原样内嵌发给上游；
    // 非图片文件（文档/视频/音频）≤50MB 时自动下载到会话 media/ 目录并告知 agent
    let forward_to_vision = state.forward_to_vision && state.vision_client.is_some();
    let Some(user_msg) = build_user_message(&state.bot, &msg, &session, forward_to_vision).await?
    else {
        state
            .bot
            .send_message(chat_id, "请发送文本、图片、文档、视频或音频 🙏")
            .await?;
        return Ok(());
    };

    tracing::info!(
        "收到消息： chat={} user={:?} text={:?}",
        chat_id,
        msg.from.as_ref().map(|f| f.id),
        log_text,
    );

    // 本轮的 round id:journal 里关联本轮全部消息
    let round_id = Uuid::new_v4();
    let agent = state.agent_for(chat_id, session.toolout_dir());

    // 流异常（拿不到 FinalResponse）时，至少把本轮用户消息记进 journal
    let logged_user_msg = user_msg.clone();

    // 先发「正在输入」状态，让用户立刻有反馈
    let _ = state
        .bot
        .send_chat_action(chat_id, ChatAction::Typing)
        .await;

    // 每个 chat 单独维护多轮对话历史（先取出再写回）
    let history: Vec<RigMessage> = {
        let map = state.histories.lock().await;
        map.get(&chat_id).cloned().unwrap_or_default()
    };

    // 流式跑 agent：文本增量实时刷到占位消息（节流防触发 Telegram 限频）,
    // 最终回复与本轮新增历史以 FinalResponse 为准
    let mut stream = agent.stream_chat(user_msg, history.clone()).await;
    let mut collector = StreamCollector::new(
        state.bot.clone(),
        chat_id,
        state.stream_edit_interval,
        history,
    );

    let terminal = loop {
        let Some(item) = stream.next().await else {
            break StreamTerminal::Ended;
        };
        if let Some(t) = collector.handle(item).await {
            break t;
        }
    };

    // 收尾：按流的终态决定回复文本；异常轮次只记用户消息并提前结束
    let reply = match terminal {
        StreamTerminal::Finished => {
            tracing::info!(
                "Agent 回复完成： chat={} 共 {} 轮历史",
                chat_id,
                collector.history.len()
            );
            // 本轮全部消息追加进 journal（只追加、不修改）
            session
                .append_round(round_id, &collector.new_messages)
                .await;
            collector.take_final_output()
        }
        StreamTerminal::Error(e) => {
            tracing::error!("Agent 出错： chat={} {e}", chat_id);
            collector.cleanup_empty_placeholder().await;
            state
                .bot
                .send_message(chat_id, format!("⚠️ Agent 出错： {e}"))
                .await?;
            record_abnormal_round(&session, round_id, &logged_user_msg).await;
            // 本轮收尾：审批日志消息追加「🏁 本轮结束」尾注（本轮无审批则不做任何事）
            state.approvals.finish_run(&state.bot, chat_id).await;
            return Ok(());
        }
        // 流结束却没收到 FinalResponse（异常）：有预览文本就保留，否则报错
        StreamTerminal::Ended if collector.preview.trim().is_empty() => {
            collector.delete_placeholder().await;
            state
                .bot
                .send_message(chat_id, "⚠️ Agent 出错： 流结束但没有最终响应")
                .await?;
            record_abnormal_round(&session, round_id, &logged_user_msg).await;
            state.approvals.finish_run(&state.bot, chat_id).await;
            return Ok(());
        }
        StreamTerminal::Ended => collector.preview.clone(),
    };

    // 最终回复写入当前占位消息（与最后一次预览相同则跳过，
    // Telegram 对相同文本的编辑会报错）；没有占位（异常）则新发一条；
    // 回复为空则删掉占位，避免残留「思考中…」
    collector.write_final(&reply).await?;

    // 本轮收尾：审批日志消息追加「🏁 本轮结束」尾注（本轮无审批则不做任何事）
    state.approvals.finish_run(&state.bot, chat_id).await;

    {
        let mut map = state.histories.lock().await;
        map.insert(chat_id, collector.history);
    }
    Ok(())
}

/// 简单的 /start、/help、/new 命令（文本消息）；返回 true 表示已处理、无需跑 agent
async fn handle_command(
    state: &AppState,
    chat_id: ChatId,
    text: &str,
) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
    if text.starts_with("/start") || text.starts_with("/help") {
        state
            .bot
            .send_message(
                chat_id,
                "👋 我是 ying！直接发文本或图片就行。\n\
                 我可以用 `bash` 跑命令、`read` 读文件，\n\
                 也能看你发的图片、看电脑上的图片（vision）。\n\
                 每次调用工具前都会发按钮请你明确同意。\n\
                 发送 /new 可以开启新会话（清空对话历史）。",
            )
            .await?;
        return Ok(true);
    }
    if text.starts_with("/new") {
        state.histories.lock().await.remove(&chat_id);
        // 会话结束：下一条消息会创建新的会话文件
        state.sessions.lock().await.remove(&chat_id);
        state
            .bot
            .send_message(chat_id, "🆕 新会话已开始，之前的对话历史已清空。")
            .await?;
        return Ok(true);
    }
    Ok(false)
}

/// 未授权用户回复节流窗口：30 分钟
const UNAUTH_WINDOW: Duration = Duration::from_secs(30 * 60);
/// 每个未授权 from.id 在窗口内最多回复次数
const UNAUTH_MAX_PER_WINDOW: u32 = 3;
/// 最多记录多少个不同的未授权 from.id
const UNAUTH_MAX_ENTRIES: usize = 128;

/// 记一次未授权用户回复；返回本次是否允许回复（每个 from.id 每 30 分钟最多 3 次）。
/// map 元素为 (窗口内首次回复时间, 窗口内已回复次数)。
/// 懒重置：已有条目超过窗口则重新计数；
/// 懒清理：新增前删除已过期条目，仍超过 128 个则淘汰最旧的。
fn allow_unauth_reply(map: &mut HashMap<UserId, (Instant, u32)>, user_id: UserId) -> bool {
    let now = Instant::now();
    if let Some((first, count)) = map.get_mut(&user_id) {
        if now.duration_since(*first) >= UNAUTH_WINDOW {
            *first = now;
            *count = 1;
            return true;
        }
        if *count >= UNAUTH_MAX_PER_WINDOW {
            return false;
        }
        *count += 1;
        return true;
    }
    // 新增前懒清理：先删过期条目，仍满则淘汰最旧的一个
    if map.len() >= UNAUTH_MAX_ENTRIES {
        map.retain(|_, (first, _)| now.duration_since(*first) < UNAUTH_WINDOW);
        if map.len() >= UNAUTH_MAX_ENTRIES {
            let oldest = map.iter().min_by_key(|(_, (t, _))| *t).map(|(id, _)| *id);
            if let Some(id) = oldest {
                map.remove(&id);
            }
        }
    }
    map.insert(user_id, (now, 1));
    true
}

/// 非文本消息的日志文案：优先 caption，没有则打 [XX消息] 占位
fn describe_for_log(msg: &Message) -> Option<String> {
    let caption = msg
        .caption()
        .map(str::to_string)
        .filter(|c| !c.trim().is_empty());
    let is_image = msg.photo().is_some()
        || msg.document().as_ref().is_some_and(|d| {
            d.mime_type
                .as_ref()
                .is_some_and(|m| m.to_string().starts_with("image/"))
        });
    if is_image {
        caption.or_else(|| Some("[图片消息]".to_string()))
    } else if let Some(doc) = msg.document() {
        caption.or_else(|| {
            Some(format!(
                "[文档消息] {}",
                doc.file_name.clone().unwrap_or_default()
            ))
        })
    } else if msg.video().is_some() {
        caption.or_else(|| Some("[视频消息]".to_string()))
    } else if msg.audio().is_some() {
        caption.or_else(|| Some("[音频消息]".to_string()))
    } else {
        None
    }
}

/// 异常轮次的 journal 记录：只记本轮用户消息
async fn record_abnormal_round(session: &SessionFile, round_id: Uuid, user_msg: &RigMessage) {
    session
        .append_round(round_id, std::slice::from_ref(user_msg))
        .await;
}

/// 流的终态：拿到最终响应 / 流出错 / 流结束但没有 FinalResponse（异常）
enum StreamTerminal {
    Finished,
    Error(String),
    Ended,
}

/// 流式回复收集器：
/// 占位消息始终代表「当前正在生成的回复」——每轮首个文本到达时创建，
/// 工具调用时定稿/删除、hook 拒绝重试时删除，
/// 这样最终回复始终位于所有工具审批消息之后，不会被「顶上去」。
/// 文本增量按随机间隔节流刷到占位消息（防触发 Telegram 限频）。
struct StreamCollector {
    bot: Bot,
    chat_id: ChatId,
    edit_interval: Duration,
    /// 多轮对话历史：FinalResponse 到达时把本轮新增消息接在后面
    history: Vec<RigMessage>,
    placeholder: Option<MessageId>,
    /// 本轮已累积的临时文本
    preview: String,
    /// 已推到占位消息的文本（收尾时据此判断是否需要再编辑一次）
    preview_sent: String,
    last_edit: Instant,
    next_wait: Duration,
    /// FinalResponse 的最终回复文本
    final_output: Option<String>,
    /// FinalResponse 携带的本轮新增消息（含用户输入）
    new_messages: Vec<RigMessage>,
}

impl StreamCollector {
    fn new(bot: Bot, chat_id: ChatId, edit_interval: Duration, history: Vec<RigMessage>) -> Self {
        let next_wait = random_edit_wait(edit_interval);
        Self {
            bot,
            chat_id,
            edit_interval,
            history,
            placeholder: None,
            preview: String::new(),
            preview_sent: String::new(),
            last_edit: Instant::now(),
            next_wait,
            final_output: None,
            new_messages: Vec::new(),
        }
    }

    /// 处理一个流项；返回 Some 表示流到达终态
    async fn handle(
        &mut self,
        item: Result<MultiTurnStreamItem, StreamingError>,
    ) -> Option<StreamTerminal> {
        match item {
            Ok(MultiTurnStreamItem::StreamAssistantItem(StreamedAssistantContent::Text(text))) => {
                self.push_text(text.text).await;
                None
            }
            // 模型发起工具调用：之前的文本是中间轮次的输出，定稿/删除占位消息
            Ok(MultiTurnStreamItem::StreamAssistantItem(StreamedAssistantContent::ToolCall {
                ..
            })) => {
                self.finalize_intermediate().await;
                None
            }
            // 该轮被 hook 拒绝重试：临时文本作废，占位消息一并删除，新一轮文本会重建
            Ok(MultiTurnStreamItem::ModelTurnRetried { .. }) => {
                self.drop_placeholder().await;
                None
            }
            Ok(MultiTurnStreamItem::FinalResponse(resp)) => {
                // resp.messages 是本轮新增消息（含用户输入），接在旧历史后面
                self.new_messages = resp.messages.unwrap_or_default();
                self.history.extend(self.new_messages.clone());
                self.final_output = Some(resp.output);
                Some(StreamTerminal::Finished)
            }
            Ok(_) => None,
            Err(e) => Some(StreamTerminal::Error(e.to_string())),
        }
    }

    /// 文本增量：懒创建占位消息，并按随机间隔节流刷到占位消息
    async fn push_text(&mut self, text: String) {
        self.preview.push_str(&text);
        if self.placeholder.is_none() {
            match self.bot.send_message(self.chat_id, "🤔 思考中…").await {
                Ok(m) => self.placeholder = Some(m.id),
                Err(e) => tracing::warn!("占位消息发送失败（下个文本再试）: {e}"),
            }
        }
        // 节流：距上次编辑不足随机等待时长就先攒着，最后统一补发
        if let Some(pid) = self.placeholder
            && self.last_edit.elapsed() >= self.next_wait
        {
            match self
                .bot
                .edit_message_text(self.chat_id, pid, self.preview.clone())
                .await
            {
                Ok(_) => {
                    self.preview_sent = self.preview.clone();
                    self.last_edit = Instant::now();
                    self.next_wait = random_edit_wait(self.edit_interval);
                }
                Err(e) => tracing::warn!("流式更新消息失败（继续尝试）: {e}"),
            }
        }
    }

    /// 工具调用：占位消息有内容则定稿（补发节流余量并标记为中间输出），空则删除，
    /// 让后续审批消息与最终回复都排在它之后
    async fn finalize_intermediate(&mut self) {
        if let Some(pid) = self.placeholder.take() {
            if self.preview.trim().is_empty() {
                if let Err(e) = self.bot.delete_message(self.chat_id, pid).await {
                    tracing::warn!("删除占位消息失败： {e}");
                }
            } else if let Err(e) = self
                .bot
                .edit_message_text(self.chat_id, pid, format!("📝 {}", self.preview))
                .await
            {
                tracing::warn!("中间输出定稿失败： {e}");
            }
        }
        self.preview.clear();
        self.preview_sent.clear();
    }

    /// 删除占位消息并清空临时文本
    async fn drop_placeholder(&mut self) {
        if let Some(pid) = self.placeholder.take()
            && let Err(e) = self.bot.delete_message(self.chat_id, pid).await
        {
            tracing::warn!("删除占位消息失败： {e}");
        }
        self.preview.clear();
        self.preview_sent.clear();
    }

    /// 取 FinalResponse 的最终回复（Finished 后调用）
    fn take_final_output(&mut self) -> String {
        self.final_output.take().unwrap_or_default()
    }

    /// 出错的占位消息还是空的就删掉，避免残留「思考中…」
    async fn cleanup_empty_placeholder(&mut self) {
        if let Some(pid) = self.placeholder.take()
            && self.preview.trim().is_empty()
        {
            let _ = self.bot.delete_message(self.chat_id, pid).await;
        }
    }

    /// 删掉占位消息（不管有没有内容）
    async fn delete_placeholder(&mut self) {
        if let Some(pid) = self.placeholder.take() {
            let _ = self.bot.delete_message(self.chat_id, pid).await;
        }
    }

    /// 最终回复写入当前占位消息（与最后一次预览相同则跳过，
    /// Telegram 对相同文本的编辑会报错）；没有占位（异常）则新发一条；
    /// 回复为空则删掉占位，避免残留「思考中…」
    async fn write_final(&mut self, reply: &str) -> HandlerResult {
        match self.placeholder {
            Some(pid) if !reply.is_empty() && reply != self.preview_sent => {
                self.bot
                    .edit_message_text(self.chat_id, pid, reply.to_string())
                    .await?;
            }
            None if !reply.is_empty() => {
                self.bot
                    .send_message(self.chat_id, reply.to_string())
                    .await?;
            }
            Some(pid) if reply.is_empty() => {
                let _ = self.bot.delete_message(self.chat_id, pid).await;
            }
            _ => {}
        }
        Ok(())
    }
}

/// 计算下一次流式编辑前的随机等待时长：
/// 在 `200ms ~ interval` 内均匀随机；`interval` 小于 200ms 时按 200ms 执行。
fn random_edit_wait(interval: Duration) -> Duration {
    const FLOOR: Duration = Duration::from_millis(200);
    let ceiling = interval.max(FLOOR);
    if ceiling == FLOOR {
        return FLOOR;
    }
    let range_ms = ceiling.as_millis() as u64 - FLOOR.as_millis() as u64;
    FLOOR + Duration::from_millis(rand::random_range(0..=range_ms))
}

/// 把审批消息改成「已决定」状态时，保留原来的工具/命令信息，
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
    tracing::info!("收到回调： data={:?}", q.data);

    // 只响应配置里允许的用户
    if !state.allows_user(q.from.id) {
        tracing::info!("未授权用户点击按钮： {:?}", q.from.id);
        let _ = state
            .bot
            .answer_callback_query(q.id.clone())
            .text("🚫 未授权用户")
            .await;
        return Ok(());
    }

    let Some((action, id)) = q.data.as_deref().and_then(|d| d.split_once(':')) else {
        tracing::warn!("回调 data 解析失败： {:?}", q.data);
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
            tracing::warn!("未知 action: {other}");
            let _ = state
                .bot
                .answer_callback_query(q.id.clone())
                .text("⚠️ 未知按钮")
                .await;
            return Ok(());
        }
    };

    // 更新审批日志消息：标记对应记录，按钮跟随剩余待审批项
    let outcome = if let Some(m) = q.message.as_ref().and_then(|m| m.regular_message()) {
        state
            .approvals
            .resolve(&state.bot, m.chat.id, id, approve)
            .await
    } else {
        ResolveOutcome::NoLog
    };
    match outcome {
        ResolveOutcome::Resolved => {
            tracing::info!("审批决定： {} → {}", action, id);
            // 日志消息已由 resolve 更新（含该条记录的决定结果）
        }
        // 该 chat 有日志但找不到对应审批项（如重复点击）：消息已是正确状态，不用改
        ResolveOutcome::Handled => {
            tracing::warn!("审批项已处理（重复点击？）: {} → {}", action, id);
            let _ = state
                .bot
                .answer_callback_query(q.id.clone())
                .text("⏳ 该按钮已处理或已过期")
                .await;
        }
        // 该 chat 没有日志：常见于点击上一次 bot 进程留下的旧按钮（内存里的日志已清空）,
        // 直接把旧消息改掉并摘掉按钮，给用户可见的反馈（保留记录内容）
        ResolveOutcome::NoLog => {
            tracing::warn!("找不到审批日志（可能已处理或已过期）: {} → {}", action, id);
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

/// 兜底分支：打印没被上面任何 handler 匹配的 update，方便排查丢失的回调等。
pub async fn on_unmatched(update: Update) -> HandlerResult {
    tracing::info!(
        "收到未匹配的 update: id={} kind={:?}",
        update.id.0,
        update.kind
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn random_edit_wait_below_floor_clamps_to_200ms() {
        // 配置小于 200ms 时按 200ms 执行
        for _ in 0..50 {
            assert_eq!(
                random_edit_wait(Duration::from_millis(50)),
                Duration::from_millis(200)
            );
        }
    }

    #[test]
    fn unauth_reply_allows_three_then_blocks() {
        let mut map = HashMap::new();
        let id = UserId(42);
        assert!(allow_unauth_reply(&mut map, id));
        assert!(allow_unauth_reply(&mut map, id));
        assert!(allow_unauth_reply(&mut map, id));
        assert!(!allow_unauth_reply(&mut map, id));
    }

    #[test]
    fn unauth_reply_lazily_resets_after_window() {
        let mut map = HashMap::new();
        let id = UserId(42);
        for _ in 0..3 {
            assert!(allow_unauth_reply(&mut map, id));
        }
        // 窗口过期：懒重置，重新允许回复
        map.insert(
            id,
            (Instant::now() - UNAUTH_WINDOW - Duration::from_secs(1), 3),
        );
        assert!(allow_unauth_reply(&mut map, id));
        assert_eq!(map[&id].1, 1);
    }

    #[test]
    fn unauth_reply_evicts_oldest_beyond_limit() {
        let mut map = HashMap::new();
        // 填满 128 个（时间各不相同，方便淘汰最旧）
        for i in 0..UNAUTH_MAX_ENTRIES as i64 {
            let id = UserId(i.try_into().unwrap());
            assert!(allow_unauth_reply(&mut map, id));
            map.insert(id, (Instant::now() - Duration::from_secs(i as u64), 1));
        }
        // 新增一个：最旧的（UserId(127)，时间最旧）被淘汰
        let new_id = UserId(999_999);
        assert!(allow_unauth_reply(&mut map, new_id));
        assert_eq!(map.len(), UNAUTH_MAX_ENTRIES);
        assert!(map.contains_key(&new_id));
        assert!(!map.contains_key(&UserId(127)));
    }

    #[test]
    fn random_edit_wait_stays_within_bounds() {
        let floor = Duration::from_millis(200);
        let ceiling = Duration::from_millis(750);
        for _ in 0..200 {
            let w = random_edit_wait(ceiling);
            assert!((floor..=ceiling).contains(&w), "wait {w:?} 越界");
        }
    }
}
