//! 配置:启动时确保 `~/.agent-ying/` 存在,没有 `config.json` 就写出默认模板,然后加载。

use std::fs;
use std::path::PathBuf;

use teloxide::types::UserId;

pub const DEFAULT_MODEL: &str = "gpt-5-mini";
pub const DEFAULT_BASH_TIMEOUT_SECS: u64 = 60;

pub const DEFAULT_SYSTEM_PROMPT: &str = r#"你是 ying,一个跑在用户电脑上的 agent,通过 Telegram 与用户对话。

你只有一个工具:
- bash:执行 shell 命令

注意:
- 每次调用工具前都会先弹出 Telegram 按钮请用户明确同意;用户可能拒绝,被拒绝时换一种方式或追问用户,不要反复硬试。
- 用户用中文时就用中文回复。
- 回复尽量简短,代码/命令输出超过必要长度时做摘要。"#;

#[derive(Debug, serde::Deserialize, serde::Serialize)]
pub struct Config {
    /// Telegram bot token,形如 `123456:ABC...`
    pub telegram_bot_token: String,
    /// OpenAI API key
    pub openai_api_key: String,
    /// OpenAI 兼容服务的 base URL,留空则用官方 https://api.openai.com/v1
    /// 例如 OpenRouter: https://openrouter.ai/api/v1
    #[serde(default)]
    pub openai_base_url: Option<String>,
    /// 模型名,如 gpt-5-mini、gpt-5
    pub model: String,
    /// 系统提示;不写入模板。优先级最低,若 `~/.agent-ying/SYSTEM.md` 存在则用其内容覆盖
    #[serde(default, skip_serializing)]
    pub system_prompt: Option<String>,
    /// bash 工具超时(秒)
    pub bash_timeout_secs: u64,
    /// 只响应这些 Telegram user id;留空则响应所有人
    #[serde(default)]
    pub allowed_user_ids: Vec<UserId>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            telegram_bot_token: String::new(),
            openai_api_key: String::new(),
            openai_base_url: None,
            model: DEFAULT_MODEL.to_string(),
            system_prompt: None,
            bash_timeout_secs: DEFAULT_BASH_TIMEOUT_SECS,
            allowed_user_ids: Vec::new(),
        }
    }
}

fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

impl Config {
    pub fn dir() -> PathBuf {
        home_dir().join(".agent-ying")
    }

    pub fn system_md_path() -> PathBuf {
        Self::dir().join("SYSTEM.md")
    }

    pub fn path() -> PathBuf {
        Self::dir().join("config.json")
    }

    /// 创建目录、必要时写出默认配置,再加载。
    pub fn load() -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let dir = Self::dir();
        fs::create_dir_all(&dir)?;

        let path = Self::path();
        if !path.exists() {
            let template = Self::default();
            fs::write(&path, serde_json::to_string_pretty(&template)?)?;
            log::info!("已写入默认配置: {}", path.display());
        }

        let cfg: Config = serde_json::from_str(&fs::read_to_string(&path)?)?;
        if cfg.telegram_bot_token.is_empty() {
            return Err(format!("请在 {} 里填写 telegram_bot_token", path.display()).into());
        }
        if cfg.openai_api_key.is_empty() {
            return Err(format!("请在 {} 里填写 openai_api_key", path.display()).into());
        }
        Ok(cfg)
    }

    /// 解析最终系统提示,优先级从高到低:
    /// 1. `~/.agent-ying/SYSTEM.md`(存在则用其内容覆盖)
    /// 2. `config.json` 的 `system_prompt`
    /// 3. 代码内默认值
    pub fn resolve_system_prompt(&self) -> String {
        let md = Self::system_md_path();
        if let Ok(content) = fs::read_to_string(&md) {
            log::info!("系统提示词使用 {}", md.display());
            return content;
        }
        self.system_prompt
            .clone()
            .unwrap_or_else(|| DEFAULT_SYSTEM_PROMPT.to_string())
    }
}
