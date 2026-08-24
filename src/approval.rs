//! 审批管理:工具执行前,agent 在 Telegram 里发审批请求并等用户点击按钮。
//! 一轮(从首次请求审批到最终回复完毕)的所有审批记录合并到同一条「审批日志」消息:
//! 已决定的条目内联展示,最新待审批条目带「同意 / 拒绝」按钮,
//! 本轮结束时追加「🏁 本轮结束」尾注。

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use teloxide::prelude::*;
use teloxide::types::{InlineKeyboardButton, InlineKeyboardMarkup, MessageId};
use tokio::sync::{Mutex, oneshot};
use tokio::time::Duration;

/// Telegram 单条消息上限 4096 字符,留点余量
const MAX_LOG_LEN: usize = 3800;

/// 单条记录的 detail 最长字符数,防止一条巨长命令撑爆日志消息
const MAX_DETAIL_LEN: usize = 1500;

/// 单条审批记录的决定结果
#[derive(Clone, Copy)]
enum Decision {
    Approved,
    Denied,
    Timeout,
    /// 被更新的审批取代(并行工具调用),自动按拒绝处理
    Superseded,
}

fn decision_label(d: Decision) -> &'static str {
    match d {
        Decision::Approved => "✅",
        Decision::Denied => "❌",
        Decision::Timeout => "⏰",
        Decision::Superseded => "🔀",
    }
}

/// 一条审批记录
struct Entry {
    tool: String,
    detail: String,
    /// Some(id) 表示等待用户决定;None 表示已决定(见 decision)
    pending_id: Option<String>,
    decision: Option<Decision>,
}

/// 一个 chat 的审批日志:合并消息 id + 本轮全部记录
struct ChatLog {
    /// 0 表示日志消息还没发出去
    message_id: MessageId,
    entries: Vec<Entry>,
    /// 是否已追加「本轮结束」尾注;新一轮的首次审批会另起一条日志消息
    finished: bool,
}

impl ChatLog {
    fn new() -> Self {
        Self {
            message_id: MessageId(0),
            entries: Vec::new(),
            finished: false,
        }
    }
}

/// 渲染日志文本与当前按钮对应的审批 id(无待审批项时为 None)。
/// 超长时先省略较旧记录的详情,仍超长则丢弃最早的记录。
fn render_log(entries: &[Entry], finished: bool) -> (String, Option<String>) {
    fn render(
        entries: &[Entry],
        finished: bool,
        compact_old: bool,
        drop_oldest: usize,
    ) -> (String, Option<String>) {
        let mut pending: Option<String> = None;
        let mut lines: Vec<String> = Vec::new();
        if drop_oldest > 0 {
            lines.push(format!("… 省略了 {drop_oldest} 条早期记录"));
        }
        lines.push("🔧 审批日志".to_string());
        lines.push(String::new());
        // 最近两条始终保留完整详情
        let keep_full_from = entries.len().saturating_sub(2);
        for (i, e) in entries.iter().enumerate().skip(drop_oldest) {
            if let Some(id) = &e.pending_id {
                pending = Some(id.clone());
            }
            let status = match (&e.pending_id, e.decision) {
                (Some(_), _) => "🔧 待审批",
                (None, Some(d)) => decision_label(d),
                (None, None) => "❓",
            };
            let head = format!("{}. {status} `{}`", i + 1, e.tool);
            let compact = compact_old && i < keep_full_from;
            if compact || e.detail.trim().is_empty() {
                lines.push(head);
            } else {
                let detail = e.detail.replace('\n', "\n   ");
                lines.push(format!("{head}\n   {detail}"));
            }
        }
        if pending.is_some() {
            lines.push(String::new());
            lines.push("是否放行?".to_string());
        }
        if finished {
            lines.push(String::new());
            lines.push(format!("🏁 本轮结束,共 {} 次审批", entries.len()));
        }
        (lines.join("\n"), pending)
    }

    let full = render(entries, finished, false, 0);
    if full.0.chars().count() <= MAX_LOG_LEN {
        return full;
    }
    let compact = render(entries, finished, true, 0);
    if compact.0.chars().count() <= MAX_LOG_LEN {
        return compact;
    }
    let mut drop = 1;
    while drop < entries.len() {
        let (text, pending) = render(entries, finished, true, drop);
        if text.chars().count() <= MAX_LOG_LEN {
            return (text, pending);
        }
        drop += 1;
    }
    render(entries, finished, true, entries.len().saturating_sub(1))
}

/// 按钮键盘:有待审批项则带「同意 / 拒绝」,空键盘 = 摘掉按钮
fn keyboard_for(pending_id: Option<&str>) -> InlineKeyboardMarkup {
    match pending_id {
        Some(id) => InlineKeyboardMarkup::new([[
            InlineKeyboardButton::callback("✅ 同意", format!("approve:{id}")),
            InlineKeyboardButton::callback("❌ 拒绝", format!("deny:{id}")),
        ]]),
        None => InlineKeyboardMarkup {
            inline_keyboard: vec![],
        },
    }
}

/// 按钮点击的处理结果
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolveOutcome {
    /// 已标记对应记录并更新了日志消息
    Resolved,
    /// 该 chat 有日志但找不到对应审批项(如重复点击),消息已是正确状态
    Handled,
    /// 该 chat 没有日志(如上次进程遗留的旧按钮)
    NoLog,
}

/// 全局审批管理器(可 Clone,内部是 Arc)。
/// `logs` 锁覆盖「渲染 + 编辑消息」全过程,串行化同一进程的日志编辑,避免并发写同一条消息。
#[derive(Clone, Default)]
pub struct ApprovalManager {
    next_id: Arc<AtomicU64>,
    pending: Arc<Mutex<HashMap<String, oneshot::Sender<bool>>>>,
    /// 每个 chat 的审批日志(合并消息)
    logs: Arc<Mutex<HashMap<ChatId, ChatLog>>>,
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

    /// 超时等场景下把待审批项摘掉,避免迟到的点击误报「已同意」。
    pub async fn expire(&self, id: &str) {
        self.pending.lock().await.remove(id);
    }

    /// 用户点击按钮:标记对应记录并更新日志消息(按钮跟随剩余待审批项)。
    pub async fn resolve(
        &self,
        bot: &Bot,
        chat_id: ChatId,
        id: &str,
        approve: bool,
    ) -> ResolveOutcome {
        let mut logs = self.logs.lock().await;
        let Some(log) = logs.get_mut(&chat_id) else {
            return ResolveOutcome::NoLog;
        };
        let Some(entry) = log
            .entries
            .iter_mut()
            .find(|e| e.pending_id.as_deref() == Some(id))
        else {
            return ResolveOutcome::Handled;
        };
        entry.pending_id = None;
        entry.decision = Some(if approve {
            Decision::Approved
        } else {
            Decision::Denied
        });
        // 通过 oneshot 把决定传给等待中的工具,并摘掉待审批项防重复点击。
        // send 失败只可能是超时竞态(rx 已丢),记录已标记,展示仍正确
        let _ = self
            .pending
            .lock()
            .await
            .remove(id)
            .is_some_and(|tx| tx.send(approve).is_ok());
        let (text, pending) = render_log(&log.entries, log.finished);
        let _ = bot
            .edit_message_text(chat_id, log.message_id, text)
            .reply_markup(keyboard_for(pending.as_deref()))
            .await;
        ResolveOutcome::Resolved
    }

    /// 本轮结束:给日志消息追加「🏁 本轮结束」尾注;无审批记录的 chat 不做任何事。
    pub async fn finish_run(&self, bot: &Bot, chat_id: ChatId) {
        let mut logs = self.logs.lock().await;
        let Some(log) = logs.get_mut(&chat_id) else {
            return;
        };
        if log.entries.is_empty() || log.finished {
            return;
        }
        log.finished = true;
        let (text, pending) = render_log(&log.entries, log.finished);
        let _ = bot
            .edit_message_text(chat_id, log.message_id, text)
            .reply_markup(keyboard_for(pending.as_deref()))
            .await;
    }
}

/// 把新审批追加到日志消息(新 chat 或上一轮已结束则新发一条),
/// 阻塞等待用户对最新待审批项的点击。
/// 点「同意」返回 Ok(true),「拒绝」/超时/被取代返回 Ok(false)。
pub async fn request_approval(
    bot: &Bot,
    chat_id: ChatId,
    approvals: &ApprovalManager,
    timeout: Duration,
    tool: &str,
    detail: &str,
) -> Result<bool, String> {
    let (id, rx) = approvals.register().await;
    tracing::info!(
        "审批请求: chat={} tool={} detail={:?}",
        chat_id,
        tool,
        detail
    );

    // 追加新的待审批记录并发出/更新日志消息。
    // 若还有未决定的旧记录(并行工具调用),按「被取代」处理并自动拒绝,免得 agent 卡住等超时
    {
        let mut logs = approvals.logs.lock().await;
        let log = logs.entry(chat_id).or_insert_with(ChatLog::new);
        // 上一轮已收尾:新一轮从一条新的日志消息开始
        if log.finished {
            log.entries.clear();
            log.message_id = MessageId(0);
            log.finished = false;
        }
        for e in log.entries.iter_mut() {
            if let Some(old_id) = e.pending_id.take() {
                e.decision = Some(Decision::Superseded);
                if let Some(tx) = approvals.pending.lock().await.remove(&old_id) {
                    tracing::info!("旧审批被新审批取代,自动按拒绝处理: {old_id}");
                    let _ = tx.send(false);
                }
            }
        }
        let detail: String = detail.chars().take(MAX_DETAIL_LEN).collect();
        log.entries.push(Entry {
            tool: tool.to_string(),
            detail,
            pending_id: Some(id.clone()),
            decision: None,
        });
        let (text, _) = render_log(&log.entries, log.finished);
        let kb = keyboard_for(Some(&id));
        let sent = if log.message_id.0 == 0 {
            bot.send_message(chat_id, text).reply_markup(kb).await
        } else {
            match bot
                .edit_message_text(chat_id, log.message_id, text.clone())
                .reply_markup(kb.clone())
                .await
            {
                Ok(m) => Ok(m),
                // 旧消息编辑不了(如被用户删除)就重新发一条日志消息
                Err(e) => {
                    tracing::warn!("编辑审批日志消息失败,改发新消息: {e}");
                    bot.send_message(chat_id, text).reply_markup(kb).await
                }
            }
        };
        match sent {
            Ok(m) => log.message_id = m.id,
            Err(e) => return Err(e.to_string()),
        }
    }

    // 超时或 rx 被 drop(sender 没了)都按拒绝处理
    let approved = match tokio::time::timeout(timeout, rx).await {
        Ok(Ok(v)) => v,
        Ok(Err(_)) => false,
        Err(_) => {
            tracing::warn!(
                "审批超时({}s),按拒绝处理: chat={} tool={}",
                timeout.as_secs(),
                chat_id,
                tool
            );
            // 把记录标记为超时并更新日志消息(无其他待审批项则摘按钮)
            {
                let mut logs = approvals.logs.lock().await;
                if let Some(log) = logs.get_mut(&chat_id) {
                    if let Some(e) = log
                        .entries
                        .iter_mut()
                        .find(|e| e.pending_id.as_deref() == Some(id.as_str()))
                    {
                        e.pending_id = None;
                        e.decision = Some(Decision::Timeout);
                    }
                    let (text, pending) = render_log(&log.entries, log.finished);
                    let _ = bot
                        .edit_message_text(chat_id, log.message_id, text)
                        .reply_markup(keyboard_for(pending.as_deref()))
                        .await;
                }
            }
            // 摘掉待审批项,防止迟到的点击命中已超时的审批
            approvals.expire(&id).await;
            false
        }
    };
    if !approved {
        tracing::info!("审批被拒绝: chat={} tool={}", chat_id, tool);
    }
    Ok(approved)
}

/// 从日志消息原文里提取记录内容:去掉开头的 🔧 和「是否放行?」之后的行。
/// 仅用于进程重启后旧按钮点击的兜底展示。
pub fn approval_body(original: &str) -> String {
    original
        .strip_prefix("🔧 ")
        .unwrap_or(original)
        .lines()
        .take_while(|line| !line.trim().starts_with("是否放行?"))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decided(tool: &str, detail: &str) -> Entry {
        Entry {
            tool: tool.into(),
            detail: detail.into(),
            pending_id: None,
            decision: Some(Decision::Approved),
        }
    }

    /// 点击按钮后必须通过 oneshot 把决定传给等待方,
    /// 不能只从 pending 表里删掉 sender(那样 rx 会按「拒绝」处理)
    #[tokio::test]
    async fn resolve_wakes_waiter_with_decision() {
        let mgr = ApprovalManager::new();
        let (id, rx) = mgr.register().await;
        {
            let mut logs = mgr.logs.lock().await;
            let log = logs.entry(ChatId(1)).or_insert_with(ChatLog::new);
            log.entries.push(Entry {
                tool: "bash".into(),
                detail: String::new(),
                pending_id: Some(id.clone()),
                decision: None,
            });
        }
        // 指向本地不存在的端口:测试里不关心消息编辑是否成功,只求快速失败
        let bot = Bot::new("123:TEST").set_api_url("http://127.0.0.1:1/".parse().unwrap());
        let outcome = mgr.resolve(&bot, ChatId(1), &id, true).await;
        assert_eq!(outcome, ResolveOutcome::Resolved);
        assert_eq!(rx.await, Ok(true));
    }

    #[test]
    fn render_shows_history_and_pending_buttons() {
        let mut entries = vec![decided("bash", "执行命令:`git status`")];
        entries.push(Entry {
            tool: "bash".into(),
            detail: "执行命令:`npm install`".into(),
            pending_id: Some("7".into()),
            decision: None,
        });
        let (text, pending) = render_log(&entries, false);
        assert_eq!(pending.as_deref(), Some("7"));
        assert!(text.starts_with("🔧 审批日志"));
        assert!(text.contains("1. ✅ `bash`"));
        assert!(text.contains("2. 🔧 待审批 `bash`"));
        assert!(text.ends_with("是否放行?"));
        assert!(!text.contains("🏁"));
    }

    #[test]
    fn render_finished_appends_footer_and_drops_prompt() {
        let entries = vec![decided("bash", "git status")];
        let (text, pending) = render_log(&entries, true);
        assert!(pending.is_none());
        assert!(text.contains("🏁 本轮结束,共 1 次审批"));
        assert!(!text.contains("是否放行?"));
    }

    #[test]
    fn render_truncates_when_too_long() {
        let entries: Vec<Entry> = (0..200)
            .map(|i| decided("bash", &format!("cmd-{i}\n{}", "x".repeat(500))))
            .collect();
        let (text, pending) = render_log(&entries, false);
        assert!(pending.is_none());
        assert!(
            text.chars().count() <= MAX_LOG_LEN,
            "渲染结果 {} 字,超出上限",
            text.chars().count()
        );
        // 最近一条始终保留完整详情
        assert!(text.contains("cmd-199"));
    }
}
