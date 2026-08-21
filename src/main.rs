mod approval;
mod config;
mod handlers;
mod tools;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use rig::client::CompletionClient;
use rig::completion::Message;
use rig::providers::openai;
use teloxide::prelude::*;
use teloxide::types::{BotCommand, ChatId, UpdateKind, UserId};
use tokio::sync::Mutex;

use approval::ApprovalManager;
use config::Config;
use handlers::{on_callback, on_message, on_unmatched};
use mimalloc::MiMalloc;
use tools::{Bash, SendFile, ToolCtx};

// 用 mimalloc 替换系统默认分配器(减少内存碎片,降低常驻内存)
#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

/// 使用 OpenAI Chat Completions API(兼容大部分第三方网关)。
type YingAgent = rig::agent::Agent<openai::CompletionModel>;

#[derive(Clone)]
struct AppState {
    bot: Bot,
    client: openai::CompletionsClient,
    approvals: ApprovalManager,
    histories: Arc<Mutex<HashMap<ChatId, Vec<Message>>>>,
    name: String,
    model: String,
    system_prompt: String,
    bash_timeout: Duration,
    approval_timeout: Duration,
    allowed_user_ids: Vec<UserId>,
    temperature: f64,
    max_turns: usize,
    max_tokens: u64,
}

impl AppState {
    /// 列表为空则允许所有人。
    fn allows_user(&self, user_id: UserId) -> bool {
        self.allowed_user_ids.is_empty() || self.allowed_user_ids.contains(&user_id)
    }
}

impl AppState {
    fn agent_for(&self, chat_id: ChatId) -> YingAgent {
        let ctx = ToolCtx {
            bot: self.bot.clone(),
            chat_id,
            approvals: self.approvals.clone(),
            bash_timeout: self.bash_timeout,
            approval_timeout: self.approval_timeout,
        };
        self.client
            .agent(self.model.clone())
            .name(&self.name)
            .preamble(&self.system_prompt)
            .tool(Bash(ctx.clone()))
            .tool(SendFile(ctx))
            // 采样参数与最大轮数都从配置读取
            .temperature(self.temperature)
            .max_tokens(self.max_tokens)
            .default_max_turns(self.max_turns)
            .build()
    }
}

/// 构建 dptree handler(启动和测试共用)。
fn build_handler() -> dptree::Handler<
    'static,
    Result<(), Box<dyn std::error::Error + Send + Sync>>,
    teloxide::dispatching::DpHandlerDescription,
> {
    dptree::entry()
        .branch(Update::filter_message().endpoint(on_message))
        .branch(Update::filter_callback_query().endpoint(on_callback))
        // 兜底:没被上面匹配的 update 都记日志
        .branch(dptree::endpoint(on_unmatched))
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // 默认 Info 级别,可通过 RUST_LOG 环境变量覆盖
    // tracing-log 桥接:teloxide 等依赖的 log 宏输出也会走 tracing
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    // 所有配置都从 ~/.agent-ying/config.json 读取(不存在时写出默认模板)
    let config = Config::load()?;
    let bash_timeout = Duration::from_secs(config.bash_timeout_secs);
    let approval_timeout = Duration::from_secs(config.approval_timeout_secs);
    tracing::info!(
        "配置加载完成: name={}, model={}, base_url={}, bash 超时 {}s, 审批超时 {}s, 白名单 {} 人, temperature={}, max_turns={}, max_tokens={}",
        config.name,
        config.model,
        config
            .openai_base_url
            .as_deref()
            .unwrap_or("(官方 api.openai.com)"),
        config.bash_timeout_secs,
        config.approval_timeout_secs,
        config.allowed_user_ids.len(),
        config.temperature,
        config.max_turns,
        config.max_tokens,
    );

    let bot = Bot::new(config.telegram_bot_token.clone());
    // 自定义 OpenAI 兼容 base_url(留空则用官方地址)
    let client = match config.openai_base_url {
        Some(ref base_url) if !base_url.is_empty() => openai::CompletionsClient::builder()
            .api_key(&config.openai_api_key)
            .base_url(base_url)
            .build()?,
        _ => openai::CompletionsClient::new(config.openai_api_key.clone())?,
    };
    let system_prompt = config.resolve_system_prompt();

    // 先清空已有的命令,再注册 /new,避免上次残留的命令
    bot.set_my_commands(Vec::<BotCommand>::new()).await?;
    bot.set_my_commands([BotCommand::new("new", "开启新会话(清空对话历史)")])
        .await?;
    tracing::info!("Telegram bot 就绪,已注册命令 /new");

    let state = AppState {
        bot: bot.clone(),
        client,
        approvals: ApprovalManager::new(),
        histories: Arc::new(Mutex::new(HashMap::new())),
        name: config.name,
        model: config.model,
        system_prompt,
        bash_timeout,
        approval_timeout,
        allowed_user_ids: config.allowed_user_ids,
        temperature: config.temperature,
        max_turns: config.max_turns,
        max_tokens: config.max_tokens,
    };

    let handler = build_handler();

    tracing::info!("开始接收 Telegram 更新(等待消息)…");
    Dispatcher::builder(bot, handler)
        .dependencies(dptree::deps![state])
        .enable_ctrlc_handler()
        // teloxide 0.17 默认按 chat 分组:同一 chat 的 update 由同一个 worker 串行处理。
        // 但审批按钮的 callback_query 也属于同一个 chat:
        // on_message 阻塞等按钮 → 按钮回调排队等 on_message 结束 → 必然等到 60s 超时(死锁)。
        // 因此让 callback_query 走并行的 default worker,文本消息仍保持每 chat 串行。
        .distribution_function(|upd: &Update| match &upd.kind {
            UpdateKind::CallbackQuery(_) => None,
            _ => upd.chat().map(|c| c.id),
        })
        .build()
        .dispatch()
        .await;
    tracing::info!("已退出 dispatch(通常是 Ctrl-C)");

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 确认 teloxide 自动 hint 的 allowed_updates 里包含 callback_query,
    /// 否则 Telegram 根本不会下发按钮点击事件。
    #[test]
    fn handler_description_contains_callback_query() {
        let handler = build_handler();
        let desc = handler.description();
        eprintln!("handler description = {desc:?}");
        let s = format!("{desc:?}");
        assert!(
            s.contains("CallbackQuery"),
            "description 里没有 CallbackQuery: {s}",
        );
    }
}
