# agent-ying(荧)

[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](https://opensource.org/license/mit)

一个住在你的电脑里、通过 Telegram 和你聊天的 AI agent。基于 [Rust](https://rust-lang.org) + [teloxide](https://docs.rs/teloxide) + [rig](https://github.com/0xPlaygrounds/rig) 构建,使用任意 OpenAI 兼容的 Chat Completions API。

## 功能

- **Telegram 聊天机器人**:通过 teloxide 接收消息,同一会话内消息串行处理,保持上下文
- **多模态输入**:用户发的图片会先压缩到 256KB 以下,再以 data URL 形式直接喂给主模型;`forward_to_vision: true`(默认,主模型非多模态时)且 vision 已启用时,图片会存到临时文件并转发给 vision 工具查看(免审批,调用结束后自动删除);vision 未启用时图片仍原样内嵌发给主模型
- **工具调用**:
  - `bash`:在用户的电脑上执行 shell 命令(可配置超时,输出超长自动截断)
  - `send_file`:把电脑上的文件(图片、文档、生成的代码等)发送给用户
  - `read_skill`:读取 skills 目录下的技能文件(如 SKILL.md 及其附属文件),只读、无需审批
  - `vision`:看图工具——文字图(截图/文档/代码)按原结构提取文字,风景/照片等非文字内容详细描述;由独立的多模态 agent 驱动,可单独配置模型 / API key / base URL,`vision_model` 留空则不启用
- **按钮审批**：每次调用工具前都会弹出 Telegram 内联按钮，用户点「同意」才执行；超时未点按拒绝处理。例外:用户发图转发给 vision 查看时免审批(发图即视为同意)
- **用户白名单**:可配置只响应指定的 Telegram user id
- **会话管理**:`/new` 命令清空当前对话历史,开启新会话
- **可定制人设**:系统提示词支持三级覆盖(见下文)

## 快速开始

### 1. 构建

```sh
cargo build --release
```

### 2. 配置

首次启动会在 `~/.agent-ying/` 下写出默认配置模板 `config.json`,编辑填入必填项:

```json
{
  "telegram_bot_token": "123456:ABC...",
  "openai_api_key": "sk-...",
  "openai_base_url": "https://openrouter.ai/api/v1",
  "name": "荧",
  "model": "gpt-5-mini",
  "forward_to_vision": true,
  "vision_model": "gpt-5-mini",
  "vision_api_key": "sk-...",
  "vision_base_url": "https://openrouter.ai/api/v1",
  "bash_timeout_secs": 60,
  "approval_timeout_secs": 60,
  "allowed_user_ids": [],
  "temperature": 1.0,
  "max_turns": 5000,
  "max_tokens": 32768
}
```

| 字段 | 说明 |
| --- | --- |
| `telegram_bot_token` | **必填**。Telegram bot token,形如 `123456:ABC...` |
| `openai_api_key` | **必填**。OpenAI API key |
| `openai_base_url` | OpenAI 兼容服务的 base URL,留空则用官方 `https://api.openai.com/v1` |
| `name` | agent 名字,默认「荧」 |
| `model` | 模型名,如 `gpt-5-mini`、`gpt-5` |
| `forward_to_vision` | 是否把用户发来的图片转发给 vision 工具(即主模型本身非多模态、不能直接看图),默认 `true`;为 `true` 且 vision 已启用时,图片存到临时文件(`$TMPDIR/agent-ying/`),由主 agent 调 vision 工具查看(免审批,调用结束后自动删除);为 `false` 或 vision 未启用时图片原样内嵌发给主模型 |
| `vision_model` | 看图工具(vision)用的模型,需支持多模态;**留空(或省略)则不启用 vision agent**。注意:`forward_to_vision: true`(默认)时建议启用,否则用户发来的图片会原样发给主模型(主模型可能看不到) |
| `vision_api_key` | vision 专用 API key,留空则回退到 `openai_api_key` |
| `vision_base_url` | vision 专用 base URL,留空则回退到 `openai_base_url`(再留空用官方 `https://api.openai.com/v1`) |
| `bash_timeout_secs` | bash 工具执行超时(秒),默认 60 |
| `approval_timeout_secs` | 审批等待超时(秒),超时按拒绝处理,默认 60 |
| `allowed_user_ids` | 只响应这些 Telegram user id,留空则响应所有人 |
| `temperature` | 采样温度,默认 1.0 |
| `max_turns` | agent 单次对话最大工具轮数,默认 5000 |
| `max_tokens` | 单次模型响应最大 token 数,默认 32768 |

### 3. 运行

```sh
cargo run --release
```

启动后 Telegram 里会给 bot 发消息即可;用 `/new` 开启新会话。日志级别通过 `RUST_LOG` 环境变量控制(默认 `info`)。

## 系统提示词

主 agent 的系统提示词按优先级从高到低:

1. `~/.agent-ying/SYSTEM.md`(存在则用其内容)
2. 代码内默认人设(「荧」:18 岁、住在用户电脑里的女朋友)

vision agent(看图工具)有自己独立的系统提示词,同样支持覆盖:

1. `~/.agent-ying/VISION_SYSTEM.md`(存在则用其内容)
2. 代码内默认看图提示词(文字图按原结构提取文字、非文字内容详细描述)

> 只有 `vision_model` 非空时 vision agent 才会启用;`VISION_SYSTEM.md` 仅在启用时才会被读取。

## 技能(Skills)

技能用于给 agent 提供特定任务的专门指令,采用渐进式披露:启动时只把每个技能的 name + description 拼进系统提示,模型判断任务匹配某个技能时,再用 `read_skill` 工具读取完整的 SKILL.md。

### 配置方法

1. 在 `~/.agent-ying/skills/` 下为每个技能建一个子目录,并放入 `SKILL.md`:

   ```
   ~/.agent-ying/skills/
   └── gitmoji/
       ├── SKILL.md
       └── references/gitmoji-reference.md   # 附属文件可选
   ```

2. `SKILL.md` 开头用 YAML frontmatter 声明 `name` 和 `description`(description 是模型判断是否使用该技能的关键):

   ```markdown
   ---
   name: gitmoji
   description: 按 gitmoji 规范生成 commit message
   ---

   # Gitmoji
   具体指令正文……
   ```

   没有 frontmatter 或缺字段时,name 回退为目录名,description 留空。
3. 重启后生效:启动时扫描 `~/.agent-ying/skills/*/SKILL.md`,把技能索引追加到系统提示末尾;没有技能则不追加。

技能文件里引用相对路径时,以技能目录(SKILL.md 的父目录)为基准解析;`read_skill` 只读、免审批,且防止 `../` 逃逸出 skills 目录。

## 项目结构

```
src/
├── main.rs      # 入口:配置加载、agent 构建、dptree handler 注册
├── config.rs    # ~/.agent-ying/config.json 的加载与默认模板
├── handlers.rs  # Telegram 消息 / 按钮回调处理
├── tools.rs     # rig 工具:bash、send_file、read_skill
├── skills.rs    # 扫描 ~/.agent-ying/skills/,生成系统提示里的技能索引
└── approval.rs  # 工具执行前的 Telegram 按钮审批
```

## 技术要点

- **rustls 全链路**:teloxide 关闭默认 native-tls,与 rig 统一走 rustls,方便静态链接
- **mimalloc 全局分配器**:抗内存碎片,降低常驻内存,支持 musl 静态构建
- **审批与消息并行分发**:teloxide 默认按 chat 串行处理 update,但审批按钮回调若也排队会死锁(等按钮 → 按钮等消息结束),因此 `callback_query` 走并行 worker,文本消息仍保持每 chat 串行

## 开发

```sh
cargo test        # 运行单元测试
cargo clippy      # lint
```

## License

[MIT](https://opensource.org/license/mit) — 见 [LICENSE](LICENSE)
