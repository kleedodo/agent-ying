mod approval;
mod config;
mod handlers;
mod image;
mod journal;
mod media;
mod skills;
mod tools;
mod usermsg;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use rig::client::AgentClientExt;
use rig::completion::Message;
use rig::providers::openai;
use teloxide::prelude::*;
use teloxide::types::{BotCommand, ChatId, UpdateKind, UserId};
use tokio::sync::Mutex;

use approval::{ApprovalHook, ApprovalManager};
use config::Config;
use handlers::{on_callback, on_message, on_unmatched};
use journal::Journal;
use mimalloc::MiMalloc;
use skills::Skills;
use tools::{Bash, Edit, Read, ToolCtx, Vision, Write};

// 用 mimalloc 替换系统默认分配器（减少内存碎片，降低常驻内存）
#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

/// 使用 OpenAI Chat Completions API（兼容大部分第三方网关）。
type YingAgent = rig::agent::Agent;

#[derive(Clone)]
struct AppState {
    bot: Bot,
    client: openai::CompletionsClient,
    approvals: ApprovalManager,
    histories: Arc<Mutex<HashMap<ChatId, Vec<Message>>>>,
    /// 会话日志（journals/）记录器
    journal: Journal,
    /// 每个 chat 当前会话的 journal 文件；/new 后重新创建
    sessions: Arc<Mutex<HashMap<ChatId, journal::SessionFile>>>,
    name: String,
    model: String,
    /// 是否把用户发来的图片转发给 vision 工具；true 且 vision 已启用时，图片存会话 media/ 目录并转发
    forward_to_vision: bool,
    vision_model: String,
    /// None 表示 vision_model 留空、未启用 vision agent
    vision_client: Option<openai::CompletionsClient>,
    vision_system_prompt: String,
    system_prompt: String,
    bash_timeout: Duration,
    approval_timeout: Duration,
    allowed_user_ids: Vec<UserId>,
    temperature: f64,
    max_turns: usize,
    max_tokens: u64,
    /// 流式回复编辑间隔上限；实际每次编辑前随机等待 200ms~该值（小于 200ms 按 200ms）
    stream_edit_interval: Duration,
}

impl AppState {
    /// 列表为空则允许所有人。
    fn allows_user(&self, user_id: UserId) -> bool {
        self.allowed_user_ids.is_empty() || self.allowed_user_ids.contains(&user_id)
    }
}

impl AppState {
    fn agent_for(&self, chat_id: ChatId, toolout_dir: std::path::PathBuf) -> YingAgent {
        let ctx = ToolCtx {
            bash_timeout: self.bash_timeout,
            toolout_dir,
        };
        let mut builder = self
            .client
            .agent(self.model.clone())
            .name(&self.name)
            .preamble(&self.system_prompt)
            .tool(Bash(ctx.clone()))
            .tool(Read(ctx.clone()))
            .tool(Write(ctx.clone()))
            .tool(Edit(ctx.clone()));
        // vision_model 留空（或省略）则不启用 vision agent
        if let Some(vision_client) = &self.vision_client {
            builder = builder.tool(Vision {
                client: vision_client.clone(),
                model: self.vision_model.clone(),
                system_prompt: self.vision_system_prompt.clone(),
                ctx: ctx.clone(),
            });
        }
        // 审批钩子：每个工具体执行前统一发 Telegram 审批按钮，
        // 同意才执行，拒绝/超时以 Skip 理由喂回模型
        builder = builder.add_hook(ApprovalHook::new(
            self.bot.clone(),
            chat_id,
            self.approvals.clone(),
            self.approval_timeout,
        ));
        // 采样参数与最大轮数都从配置读取
        builder
            .temperature(self.temperature)
            .max_tokens(self.max_tokens)
            .default_max_turns(self.max_turns)
            .build()
    }
}

/// 构建 dptree handler（启动和测试共用）。
fn build_handler() -> dptree::Handler<
    'static,
    Result<(), Box<dyn std::error::Error + Send + Sync>>,
    teloxide::dispatching::DpHandlerDescription,
> {
    dptree::entry()
        .branch(Update::filter_message().endpoint(on_message))
        .branch(Update::filter_callback_query().endpoint(on_callback))
        // 兜底：没被上面匹配的 update 都记日志
        .branch(dptree::endpoint(on_unmatched))
}

/// 构建 OpenAI 兼容客户端：base_url 留空则用官方地址。
fn build_openai_client(
    api_key: &str,
    base_url: Option<&str>,
) -> Result<openai::CompletionsClient, Box<dyn std::error::Error + Send + Sync>> {
    match base_url {
        Some(b) if !b.is_empty() => Ok(openai::CompletionsClient::builder()
            .api_key(api_key)
            .base_url(b)
            .build()?),
        _ => Ok(openai::CompletionsClient::new(api_key.to_string())?),
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // 默认 Info 级别，可通过 RUST_LOG 环境变量覆盖
    // tracing-log 桥接：teloxide 等依赖的 log 宏输出也会走 tracing
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    // 所有配置都从 ~/.agent-ying/config.json 读取（不存在时写出默认模板）
    let config = Config::load()?;
    let bash_timeout = Duration::from_secs(config.bash_timeout_secs);
    let approval_timeout = Duration::from_secs(config.approval_timeout_secs);
    // vision_model 留空（或省略）则不启用 vision agent
    let vision_enabled = !config.vision_model.trim().is_empty();
    tracing::info!(
        "配置加载完成： name={}, model={}, base_url={}, bash 超时 {}s， 审批超时 {}s， 白名单 {} 人， temperature={}, max_turns={}, max_tokens={}",
        config.name,
        config.model,
        config
            .openai_base_url
            .as_deref()
            .unwrap_or("（官方 api.openai.com）"),
        config.bash_timeout_secs,
        config.approval_timeout_secs,
        config.allowed_user_ids.len(),
        config.temperature,
        config.max_turns,
        config.max_tokens,
    );
    if vision_enabled {
        tracing::info!(
            "vision agent 已启用： model={}, base_url={}",
            config.vision_model,
            config
                .vision_base_url
                .as_deref()
                .unwrap_or("（同主 base_url / 官方）")
        );
    } else {
        tracing::info!("vision agent 未启用（vision_model 留空）");
    }
    if config.forward_to_vision && !vision_enabled {
        tracing::warn!(
            "forward_to_vision=true 但 vision agent 未启用（vision_model 留空），用户发来的图片将原样内嵌发给主模型（主模型可能看不到）"
        );
    }

    let bot = Bot::new(config.telegram_bot_token.clone());
    // 主 agent 客户端：自定义 OpenAI 兼容 base_url（留空则用官方地址）
    let client = build_openai_client(&config.openai_api_key, config.openai_base_url.as_deref())?;
    // vision agent：仅当 vision_model 非空时启用；独立的 base_url / api_key，留空则回退到主配置
    let (vision_client, vision_system_prompt) = if vision_enabled {
        let vision_api_key = config
            .vision_api_key
            .as_deref()
            .filter(|s| !s.is_empty())
            .unwrap_or(&config.openai_api_key);
        // vision_base_url 留空则回退到 openai_base_url（再留空则用官方地址）
        let vision_base_url = config
            .vision_base_url
            .as_deref()
            .filter(|s| !s.is_empty())
            .or(config.openai_base_url.as_deref());
        let client = build_openai_client(vision_api_key, vision_base_url)?;
        let prompt = Config::resolve_vision_prompt();
        (Some(client), prompt)
    } else {
        (None, String::new())
    };
    // 加载 skills（固定目录 ~/.agent-ying/skills/），把索引拼到系统提示末尾
    let skills = Skills::load(Config::skills_dir());
    let mut system_prompt = config.resolve_system_prompt();
    if let Some(block) = skills.render_block() {
        system_prompt.push_str(&block);
    }
    tracing::info!("已加载 {} 个 skill", skills.skills.len());

    // 先清空已有的命令，再注册 /new，避免上次残留的命令
    bot.set_my_commands(Vec::<BotCommand>::new()).await?;
    bot.set_my_commands([BotCommand::new("new", "开启新会话（清空对话历史）")])
        .await?;
    tracing::info!("Telegram bot 就绪，已注册命令 /new");

    let state = AppState {
        bot: bot.clone(),
        client,
        approvals: ApprovalManager::new(),
        histories: Arc::new(Mutex::new(HashMap::new())),
        journal: Journal::new(),
        sessions: Arc::new(Mutex::new(HashMap::new())),
        name: config.name,
        model: config.model,
        forward_to_vision: config.forward_to_vision,
        vision_model: config.vision_model,
        vision_client,
        vision_system_prompt,
        system_prompt,
        bash_timeout,
        approval_timeout,
        allowed_user_ids: config.allowed_user_ids,
        temperature: config.temperature,
        max_turns: config.max_turns,
        max_tokens: config.max_tokens,
        stream_edit_interval: Duration::from_millis(config.stream_edit_interval_ms),
    };

    let handler = build_handler();

    tracing::info!("开始接收 Telegram 更新（等待消息）…");
    Dispatcher::builder(bot, handler)
        .dependencies(dptree::deps![state])
        .enable_ctrlc_handler()
        // teloxide 0.17 默认按 chat 分组：同一 chat 的 update 由同一个 worker 串行处理。
        // 但审批按钮的 callback_query 也属于同一个 chat:
        // on_message 阻塞等按钮 → 按钮回调排队等 on_message 结束 → 必然等到 60s 超时（死锁）。
        // 因此让 callback_query 走并行的 default worker，文本消息仍保持每 chat 串行。
        .distribution_function(|upd: &Update| match &upd.kind {
            UpdateKind::CallbackQuery(_) => None,
            _ => upd.chat().map(|c| c.id),
        })
        .build()
        .dispatch()
        .await;
    tracing::info!("已退出 dispatch（通常是 Ctrl-C）");

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
