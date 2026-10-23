# agent-ying（荧）

[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](https://opensource.org/license/mit)

一个住在你的电脑里、通过 Telegram 和你聊天的 AI agent。基于 [Rust](https://rust-lang.org) + [teloxide](https://docs.rs/teloxide) + [rig](https://github.com/0xPlaygrounds/rig) 构建，使用任意 OpenAI 兼容的 Chat Completions API。

## 功能

- **Telegram 聊天机器人**：通过 teloxide 接收消息，同一会话内消息串行处理，保持上下文
- **多模态输入**：`forward_to_vision: true`（默认，主模型非多模态时）且 vision 已启用时，用户发的图片先压缩到 256KB 以下、存到会话 `media/` 目录，并提示主 agent 调 vision 工具查看；为 `false` 或 vision 未启用时，图片压缩后以 data URL 形式直接内嵌发给主模型。文档/视频/音频 ≤50MB 时自动下载到会话 `media/` 目录，并把落盘路径与大小告诉主模型；超限只把元数据（文件名、大小、视频时长、消息 ID）告诉主模型，不下载本体
- **工具调用**:
  - `bash`：在用户的电脑上执行 shell 命令（可配置超时，输出超长自动截断）
  - `read`：读取一个文本文件（如 SKILL.md 或其他任意文件，只接受绝对路径），只读；从第 1 行起最多读 2000 行或 50KB（先到者为准），可用 offset / limit 分页续读
  - `write`：写入文件，不存在则创建、存在则覆盖，自动创建父目录；只适合新建或完整重写
  - `edit`：精确文本替换——`edits[]` 里的每条 `oldText` 必须原样唯一命中且互不重叠，一次调用可带多条互不重叠的编辑（全部对着同一份原始文件匹配，非增量）；支持智能引号/破折号/行尾空白等模糊匹配兜底，保留原文件 BOM 与 CRLF；审批卡片里直接展示改动 diff
  - `vision`：看图工具——文字图（截图/文档/代码）按原结构提取文字，风景/照片等非文字内容详细描述；由独立的多模态 agent 驱动，可单独配置模型 / API key / base URL,`vision_model` 留空则不启用
- **按钮审批**：每次调用工具前都需用户点「同意」才执行；一轮内的所有审批合并到同一条「审批日志」消息（已决定的条目内联展示，最新待审批条目带「同意 / 拒绝」按钮，本轮结束追加「🏁 本轮结束」尾注）；超时未点按拒绝处理。所有工具（包括只读的 read）都必须经过审批
- **用户白名单**：可配置只对指定的 Telegram user id 提供工具服务；未授权用户会被告知自己的 user id（每人每 30 分钟最多 3 次）
- **会话管理**:`/new` 命令清空当前对话历史，开启新会话；`/start`、`/help` 显示帮助说明
- **流式回复**：先发送「正在输入」状态，每轮首个文本到达时创建"🤔 思考中…"占位消息，模型流式输出时增量编辑刷新成回复正文；每次编辑前随机等待 200ms~`stream_edit_interval_ms`（防触发 Telegram 限频）；模型发起工具调用时，中间文本定稿为「📝 …」消息，保证最终回复始终排在审批消息之后
- **可定制人设**：系统提示词支持本地文件覆盖（见下文）

## 快速开始

### 1. 构建

```sh
cargo build --release
```

### 2. 配置

首次启动会在 `~/.agent-ying/` 下写出默认配置模板 `config.json`，编辑填入必填项：

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
  "max_tokens": 32768,
  "stream_edit_interval_ms": 750
}
```

| 字段 | 说明 |
| --- | --- |
| `telegram_bot_token` | **必填**。Telegram bot token，形如 `123456:ABC...` |
| `openai_api_key` | **必填**。OpenAI API key |
| `openai_base_url` | OpenAI 兼容服务的 base URL，留空则用官方 `https://api.openai.com/v1` |
| `name` | agent 名字，默认「荧」 |
| `model` | 模型名，如 `gpt-5-mini`、`gpt-5` |
| `forward_to_vision` | 是否把用户发来的图片转发给 vision 工具（即主模型本身非多模态、不能直接看图），默认 `true`；为 `true` 且 vision 已启用时，图片压缩后存到会话 `media/` 目录（`~/.agent-ying/journals/<月>/<会话>/media/`），由主 agent 调 vision 工具查看；为 `false` 或 vision 未启用时图片以 data URL 内嵌发给主模型 |
| `vision_model` | 看图工具（vision）用的模型，需支持多模态；**留空（或省略）则不启用 vision agent**。注意：`forward_to_vision: true`（默认）时建议启用，否则用户发来的图片会原样发给主模型（主模型可能看不到） |
| `vision_api_key` | vision 专用 API key，留空则回退到 `openai_api_key` |
| `vision_base_url` | vision 专用 base URL，留空则回退到 `openai_base_url`（再留空用官方 `https://api.openai.com/v1`） |
| `bash_timeout_secs` | bash 工具执行超时（秒），默认 60 |
| `approval_timeout_secs` | 审批等待超时（秒），超时按拒绝处理，默认 60 |
| `allowed_user_ids` | 只对这些人提供工具服务（未授权用户只会被告知其 user id），留空则对所有人开放 |
| `temperature` | 采样温度，默认 1.0 |
| `max_turns` | agent 单次对话最大工具轮数，默认 5000 |
| `max_tokens` | 单次模型响应最大 token 数，默认 32768 |
| `stream_edit_interval_ms` | 流式回复编辑间隔上限（毫秒），默认 750；每次编辑前随机等待 200ms~该值（配置小于 200ms 时按 200ms 执行），用于防触发 Telegram 限频 |

### 3. 运行

```sh
cargo run --release
```

启动后 Telegram 里会给 bot 发消息即可；用 `/new` 开启新会话。日志级别通过 `RUST_LOG` 环境变量控制（默认 `info`）。

## 系统提示词

主 agent 的系统提示词按优先级从高到低：

1. `~/.agent-ying/SYSTEM.md`（存在则用其内容）
2. 代码内默认人设（「荧」:18 岁、住在用户电脑里的女朋友）

提示词中可以使用 `{{char}}` 占位符，会被自动替换为配置里的 `name`。

vision agent（看图工具）有自己独立的系统提示词，同样支持覆盖：

1. `~/.agent-ying/VISION_SYSTEM.md`（存在则用其内容）
2. 代码内默认看图提示词（文字图按原结构提取文字、非文字内容详细描述）

> 只有 `vision_model` 非空时 vision agent 才会启用；`VISION_SYSTEM.md` 仅在启用时才会被读取。

## 技能（Skills）

技能用于给 agent 提供特定任务的专门指令，采用渐进式披露：启动时把每个技能的 name、description 和 SKILL.md 路径（`<location>`）连同使用规则（如「被问有哪些技能时不必读文件」「技能提到的环境变量不可读取/打印其值」）拼进系统提示末尾，模型判断任务匹配某个技能时，再用 `read` 工具读取完整的 SKILL.md。

### 配置方法

1. 在 `~/.agent-ying/skills/` 下为每个技能建一个子目录，并放入 `SKILL.md`:

   ```
   ~/.agent-ying/skills/
   └── gitmoji/
       ├── SKILL.md
       └── references/gitmoji-reference.md   # 附属文件可选
   ```

2. `SKILL.md` 开头用 YAML frontmatter 声明 `name` 和 `description`（description 是模型判断是否使用该技能的关键）:

   ```markdown
   ---
   name: gitmoji
   description： 按 gitmoji 规范生成 commit message
   ---

   # Gitmoji
   具体指令正文……
   ```

   没有 frontmatter 或缺字段时，name 回退为目录名，description 留空。
3. 重启后生效：启动时扫描 `~/.agent-ying/skills/*/SKILL.md`，把技能索引追加到系统提示末尾；没有技能则不追加。

技能文件里引用相对路径时，以技能目录（SKILL.md 的父目录）为基准解析；`read` 只读、同样需审批。

## 会话日志（Journals）

每一轮对话的全部消息（用户输入、assistant 中间轮、工具调用与结果）以 jsonl 追加写入，只追加、不修改，便于事后审计与回放。每个会话一个文件，`/new` 开启新会话后写入新文件（进程重启同理）。

目录布局（按会话创建时的月份分子目录，每个会话一个自包含目录）:

```
~/.agent-ying/journals/
└── 2026-08/
    └── session-123-ab3f9c2d/
        ├── messages.jsonl           # 每行一条消息：{ts, round, seq, msg}
        ├── messages-*.jsonl.gz      # jsonl 超 1MB 后轮转压缩而来
        ├── toolout/                 # 工具结果全文，纯文本 <uuid>.txt（可配合 read 工具分页）
        ├── images/                  # 用户发来的图片（内嵌消息里的 base64 解码落盘），<uuid>.jpg
        └── media/                   # 用户发来的文件自动落盘（图片、≤50MB 的文档/视频/音频），<uuid>-<原文件名>
```

- `round` 是本轮的 uuid（每次用户消息一个）,`seq` 是轮内序号；流异常时至少会记录用户消息
- 工具结果：文本完整写入 jsonl（即 agent 看到的内容；目前只有 bash 的超长输出会被截成前 50KB、原始全文落 `toolout/`）；若文本里带保存位置，附 `result_ref` 指向 `toolout/` 里的原始全文文件
- 图片：base64 不留在 jsonl 里，解码后存入同级 `images/`，消息里只留 `image_ref: "images/<uuid>.jpg"`
- 媒体：`forward_to_vision` 时用户发的图片，以及 ≤50MB 的文档/视频/音频，落盘到 `media/`（命名 `<uuid>-<原文件名>`），jsonl 里只在消息文本中带落盘路径
- 另外，所有工具返回给模型前都会把全文落盘到当前会话的 `toolout/`；目前只有 bash 的超长输出会截成前 50KB 并在返回文本中附上提示，模型用 read 工具（offset/limit）即可分页读取任意深度的全文
- toolout 文件一律纯文本（方便 read 工具分页），轮转的 jsonl 仍用 gzip，可用 `zgrep 关键词 …` 查看/搜索；单个工具结果最多保存前 256MB
- 没有定期清理；需要腾空间时按会话目录（或月目录）整删即可，会话目录是自包含单位，引用都是相对路径，搬走不会失效

## 技术要点

- **rustls 全链路**:teloxide 关闭默认 native-tls，与 rig 统一走 rustls，方便静态链接
- **mimalloc 全局分配器**：抗内存碎片，降低常驻内存，支持 musl 静态构建
- **审批与消息并行分发**:teloxide 默认按 chat 串行处理 update，但审批按钮回调若也排队会死锁（等按钮 → 按钮等消息结束），因此 `callback_query` 走并行 worker，文本消息仍保持每 chat 串行

## 开发

```sh
cargo test        # 运行单元测试
cargo clippy      # lint
```

## License

[MIT](https://opensource.org/license/mit) — 见 [LICENSE](LICENSE)
