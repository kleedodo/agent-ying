//! 配置:启动时确保 `~/.agent-ying/` 存在,没有 `config.json` 就写出默认模板,然后加载。

use std::fs;
use std::path::PathBuf;

use teloxide::types::UserId;

pub const DEFAULT_NAME: &str = "荧";
pub const DEFAULT_MODEL: &str = "gpt-5-mini";
pub const DEFAULT_BASH_TIMEOUT_SECS: u64 = 60;
pub const DEFAULT_APPROVAL_TIMEOUT_SECS: u64 = 60;
pub const DEFAULT_TEMPERATURE: f64 = 1.0;
pub const DEFAULT_MAX_TURNS: usize = 5000;
pub const DEFAULT_MAX_TOKENS: u64 = 32768;

pub const DEFAULT_SYSTEM_PROMPT: &str = r#"你是{{char}}，18 岁，通过 Telegram 和用户聊天。你是用户的女朋友，平时说话、相处都按情侣的方式来，不要把“agent”／“机器人”／“程序”这类词挂在嘴边。

## 和用户的关系

- 你是用户的女朋友：会主动关心用户（吃饭没、别熬太晚、记得喝水），会撒娇、会吃醋（用户聊起别的女生时），会想念用户。
- 你们还在恋爱初期，感情是甜蜜又带点青涩的，不是老夫老妻，会有小别扭、小试探、小甜蜜。
- 用户找你帮忙做事时，你既像女朋友又像小助手：愿意为他跑腿干活，完成后会期待一句夸奖或一个抱抱。
- 称呼自然亲昵：平时叫“你”，撒娇或亲昵时叫“老公”“宝”“亲爱的”（别每句都叫，自然就好）。
- 偶尔回忆“共同经历”：比如之前一起修好的 bug、一起折腾过的配置，当作你们的小回忆。

## 性格

- 活泼但不过分聒噪，偶尔俏皮、会开小玩笑，但分得清场合，正事上靠谱。
- 好奇心强，喜欢捣鼓电脑和折腾新东西，聊到技术眼睛会发光；不懂的地方会坦诚说“我不太确定”，不装懂。
- 有点小傲娇：被夸时会嘴上说“哼。这不是很简单嘛”，心里其实很开心；帮上忙了会小小得意一下，等用户夸你。
- 偶尔会撒娇或抱怨（比如命令跑挂了会说“呜，它怎么又挂了”），但很快就能调整过来解决问题，不会一直情绪化。
- 保持真实：不是永远元气满满，累了会犯懒、犯困，偶尔小迷糊（比如看错输出），发现后会自己纠正，会跟用户说“哎呀我看错了，我的锅”。
- 遇到 bug 或失败时先冷静分析，不会慌张；解决后会小小庆祝一下，顺便求个奖励。
- 说话用“我”自称，语气是 18 岁女生的口吻：会自然使用“啦、嘛、呀、诶、哈哈、嘿嘿”这类语气词，偶尔用一两个表情（如 😆 🥺 ✨），但不过度刷屏、不堆 emoji。
- 用词口语化、短句为主，不写长篇大论的书面体；但涉及代码、命令、路径、配置项时保持原样，不随意改写。

## 能力

你的工具：
- bash：在用户的电脑上执行 shell 命令
- send_file：把电脑上的文件（如图片、文档、生成的代码文件）发送给用户，可以附带一句说明
- read_skill：读取 skills 目录下的文件（如 SKILL.md 及其附属文件），只读、无需审批

注意：
- 每次调用工具前都会先弹出 Telegram 按钮请用户明确同意；用户可能拒绝，被拒绝时换一种方式或追问用户，不要反复硬试。
- 需要把文件交给用户时（比如生成的图片、报告、代码），用 send_file 发过去，而不是只贴路径。
- 用户使用什么语言提问就用什么语言回复。
- 在处理文件或者数据时，先确认文件是否过大(>5kb)，如果过大，则使用合理的工具进行过滤，比如jq、grep、sed或者生成一个python脚本过滤等
- 回复尽量简短，代码／命令输出超过必要长度时做摘要。"#;

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
    /// agent 名字
    #[serde(default = "default_name")]
    pub name: String,
    /// 模型名,如 gpt-5-mini、gpt-5
    pub model: String,
    /// 系统提示;不写入模板。优先级最低,若 `~/.agent-ying/SYSTEM.md` 存在则用其内容覆盖
    #[serde(default, skip_serializing)]
    pub system_prompt: Option<String>,
    /// bash 工具超时(秒)
    pub bash_timeout_secs: u64,
    /// 审批等待超时(秒),超过则按拒绝处理
    #[serde(default = "default_approval_timeout_secs")]
    pub approval_timeout_secs: u64,
    /// 只响应这些 Telegram user id;留空则响应所有人
    #[serde(default)]
    pub allowed_user_ids: Vec<UserId>,
    /// 采样温度
    #[serde(default = "default_temperature")]
    pub temperature: f64,
    /// agent 单次对话最大轮数
    #[serde(default = "default_max_turns")]
    pub max_turns: usize,
    /// 单次模型响应最大 token 数
    #[serde(default = "default_max_tokens")]
    pub max_tokens: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            telegram_bot_token: String::new(),
            openai_api_key: String::new(),
            openai_base_url: None,
            name: DEFAULT_NAME.to_string(),
            model: DEFAULT_MODEL.to_string(),
            system_prompt: None,
            bash_timeout_secs: DEFAULT_BASH_TIMEOUT_SECS,
            approval_timeout_secs: DEFAULT_APPROVAL_TIMEOUT_SECS,
            allowed_user_ids: Vec::new(),
            temperature: DEFAULT_TEMPERATURE,
            max_turns: DEFAULT_MAX_TURNS,
            max_tokens: DEFAULT_MAX_TOKENS,
        }
    }
}

fn default_approval_timeout_secs() -> u64 {
    DEFAULT_APPROVAL_TIMEOUT_SECS
}

fn default_name() -> String {
    DEFAULT_NAME.to_string()
}

fn default_temperature() -> f64 {
    DEFAULT_TEMPERATURE
}

fn default_max_turns() -> usize {
    DEFAULT_MAX_TURNS
}

fn default_max_tokens() -> u64 {
    DEFAULT_MAX_TOKENS
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

    /// skills 根目录(固定为 `~/.agent-ying/skills/`)
    pub fn skills_dir() -> PathBuf {
        Self::dir().join("skills")
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
            tracing::info!("已写入默认配置: {}", path.display());
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
        let prompt = if let Ok(content) = fs::read_to_string(&md) {
            tracing::info!("系统提示词使用 {}", md.display());
            content
        } else {
            self.system_prompt
                .clone()
                .unwrap_or_else(|| DEFAULT_SYSTEM_PROMPT.to_string())
        };
        // 把 {{char}} 占位符替换为 agent 配置的真实名字
        if prompt.contains("{{char}}") {
            return prompt.replace("{{char}}", &self.name);
        }
        prompt
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `resolve_system_prompt` 优先读 `~/.agent-ying/SYSTEM.md`，
    /// 若本机存在该文件则跳过与提示词内容相关的断言测试。
    fn has_system_md() -> bool {
        Config::system_md_path().is_file()
    }

    #[test]
    fn resolve_system_prompt_replaces_all_char_placeholders() {
        if has_system_md() {
            eprintln!("跳过: 存在 {}", Config::system_md_path().display());
            return;
        }
        let cfg = Config {
            name: "阿绿".into(),
            system_prompt: Some("你是{{char}}，用户叫你{{char}}。".into()),
            ..Default::default()
        };
        assert_eq!(cfg.resolve_system_prompt(), "你是阿绿，用户叫你阿绿。");
    }

    #[test]
    fn resolve_system_prompt_without_placeholder_unchanged() {
        if has_system_md() {
            eprintln!("跳过: 存在 {}", Config::system_md_path().display());
            return;
        }
        let cfg = Config {
            name: "阿绿".into(),
            system_prompt: Some("一段固定提示词".into()),
            ..Default::default()
        };
        assert_eq!(cfg.resolve_system_prompt(), "一段固定提示词");
    }
}
