//! journals：每轮的全部消息以 jsonl 追加写入（只追加、不修改），每个会话一个自包含目录。
//!
//！ 目录布局（`~/.agent-ying/journals/` 下）:
//! ```text
//! journals/
//!   2026-08/                        # 按会话创建时的月份（YYYY-MM）分子目录
//!     session-123-ab3f9c2d/         # 每个会话一个目录；/new 之后开新目录
//!       messages.jsonl              # 每行一条消息；超 1MB 轮转压缩为
//!                                   # messages-<ts>.jsonl.gz（可 zgrep）
//!       toolout/                    # 工具结果全文；超 50KB 的 gzip 压缩（可 zgrep）
//!         <uuid>.txt / <uuid>.txt.gz
//!       images/                     # 图片二进制（jsonl 里只留 image_ref）
//!         <uuid>.jpg
//!       media/                      # 用户发来的文件（≤50MB 自动下载）
//!         <uuid>-<原文件名>
//! ```
//!
//！ 每行一条消息：`{"ts", "round", "seq", "msg"}`。
//! - `round` 是本轮的 uuid（每次用户消息一个）,`seq` 是轮内序号
//! - 工具结果文本完整写入 jsonl（即 agent 看到的内容；上游 record_tool_result
//!   已把长输出限长到 ~9KB）；若文本里带保存位置，附 `result_ref` 指向 toolout/ 里的原始全文
//! - 图片内容存 `images/` 下的文件，消息里只留 `image_ref`，不保留 base64
//! - 没有定期清理；需要腾空间时按会话目录或月目录整删即可

use std::io::Write as _;
use std::path::PathBuf;

use flate2::Compression;
use flate2::write::GzEncoder;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use chrono::{Local, SecondsFormat};
use serde_json::{Value, json};
use tokio::io::AsyncWriteExt;
use uuid::Uuid;

use rig::completion::Message as RigMessage;

use crate::config::Config;

/// messages.jsonl 超过该字节数后轮转压缩为 messages-<ts>.jsonl.gz
const ROTATE_BYTES: u64 = 1024 * 1024;
/// toolout 文件超过该字节数才 gzip 压缩，更小的保留纯文本直接可读
pub const COMPRESS_MIN_BYTES: usize = 50 * 1024;

/// gzip 压缩（纯 CPU，调用方在 spawn_blocking 里执行，避免阻塞异步运行时）
pub(crate) fn gzip_bytes(data: &[u8]) -> Vec<u8> {
    let mut enc = GzEncoder::new(Vec::new(), Compression::fast());
    // 目标是内存缓冲，写入/结束都不会 I/O 失败
    enc.write_all(data).expect("写内存缓冲失败");
    enc.finish().expect("结束内存 gzip 流失败")
}

/// journals 根目录访问器（布局固定，不随配置变化）
#[derive(Clone)]
pub struct Journal {
    dir: PathBuf,
}

impl Journal {
    pub fn new() -> Self {
        Self {
            dir: Config::journals_dir(),
        }
    }

    /// 为新会话创建会话目录（顺带建 toolout/、images/、media/ 目录）。
    /// 月目录按创建时间确定，之后的轮次始终写同一个会话目录。
    pub async fn create_session(&self, chat_id: i64) -> Result<SessionFile, std::io::Error> {
        let month = self.dir.join(Local::now().format("%Y-%m").to_string());
        let dir = month.join(format!(
            "session-{chat_id}-{}",
            &Uuid::new_v4().simple().to_string()[..8]
        ));
        tokio::fs::create_dir_all(dir.join("toolout")).await?;
        tokio::fs::create_dir_all(dir.join("images")).await?;
        tokio::fs::create_dir_all(dir.join("media")).await?;
        let path = dir.join("messages.jsonl");
        Ok(SessionFile { dir, path })
    }
}

/// 一个会话：它的自包含目录与当前活跃的 jsonl 文件
#[derive(Clone)]
pub struct SessionFile {
    /// journals/YYYY-MM/session-<chat_id>-<id>
    pub dir: PathBuf,
    /// …/messages.jsonl（超限时轮转为 messages-<ts>.jsonl.gz）
    pub path: PathBuf,
}

impl SessionFile {
    /// 工具结果落盘目录（与 jsonl 同级的 toolout/）
    pub fn toolout_dir(&self) -> PathBuf {
        self.dir.join("toolout")
    }

    /// 图片落盘目录（与 jsonl 同级的 images/）
    pub fn images_dir(&self) -> PathBuf {
        self.dir.join("images")
    }

    /// 用户发来的文件落盘目录（与 jsonl 同级的 media/）
    pub fn media_dir(&self) -> PathBuf {
        self.dir.join("media")
    }

    /// 把一轮的全部消息追加到文件末尾（每条消息一行）。
    /// 记录失败只 warn，不阻塞 bot 回复。
    pub async fn append_round(&self, round_id: Uuid, messages: &[RigMessage]) {
        self.rotate_if_needed().await;
        let mut buf: Vec<u8> = Vec::new();
        for (i, msg) in messages.iter().enumerate() {
            let Ok(v) = serde_json::to_value(msg) else {
                tracing::warn!("journal： 消息序列化失败，跳过第 {} 条", i + 1);
                continue;
            };
            let v = Self::strip_inlines(self, v).await;
            let line = json!({
                "ts": Local::now().to_rfc3339_opts(SecondsFormat::Secs, true),
                "round": round_id.to_string(),
                "seq": i + 1,
                "msg": v,
            });
            match serde_json::to_string(&line) {
                Ok(mut s) => {
                    s.push('\n');
                    buf.extend_from_slice(s.as_bytes());
                }
                Err(e) => tracing::warn!("journal： 行编码失败，跳过第 {} 条： {e}", i + 1),
            }
        }
        if buf.is_empty() {
            return;
        }
        let Ok(mut file) = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .await
        else {
            tracing::warn!("journal： 打开 {} 失败", self.path.display());
            return;
        };
        // 注意：tokio::fs::File 的 write_all 只把字节收进内部缓冲（上限 2MB）,
        // 真正落盘在后台任务里进行，必须 flush 完成才算写完
        if let Err(e) = file.write_all(&buf).await {
            tracing::warn!("journal： 写入 {} 失败： {e}", self.path.display());
        } else if let Err(e) = file.flush().await {
            tracing::warn!("journal: flush {} 失败： {e}", self.path.display());
        }
    }

    /// messages.jsonl 超过 ROTATE_BYTES 时压缩为 messages-<ts>.jsonl.gz 并清空当前文件
    /// （轮转也是只追加新文件，不改写旧数据；失败只 warn，不阻塞）
    async fn rotate_if_needed(&self) {
        let Ok(meta) = tokio::fs::metadata(&self.path).await else {
            return;
        };
        if meta.len() < ROTATE_BYTES {
            return;
        }
        let Ok(old) = tokio::fs::read(&self.path).await else {
            return;
        };
        let compressed = match tokio::task::spawn_blocking(move || gzip_bytes(&old)).await {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!("journal: gzip 任务 join 失败： {e}");
                return;
            }
        };
        let name = format!(
            "messages-{}.jsonl.gz",
            Local::now().format("%Y%m%d-%H%M%S-%f")
        );
        let gz_path = self.dir.join(&name);
        if let Err(e) = tokio::fs::write(&gz_path, &compressed).await {
            tracing::warn!("journal： 写入 {} 失败： {e}", gz_path.display());
            return;
        }
        // 重写为空，开始新文件
        if let Err(e) = tokio::fs::write(&self.path, Vec::new()).await {
            tracing::warn!("journal： 清空 {} 失败： {e}", self.path.display());
        }
    }

    /// 把消息里不适合进 jsonl 的内联内容替换为文件引用：
    /// 图片 → images/ + image_ref；工具结果文本带保存位置 → result_ref 指向 toolout/。
    async fn strip_inlines(&self, mut v: Value) -> Value {
        if let Some(items) = v.get_mut("content").and_then(Value::as_array_mut) {
            for item in items {
                match item.get("type").and_then(Value::as_str) {
                    Some("image") => self.demote_image(item).await,
                    // UserContent::ToolResult 的 serde 标签是 lowercase 的 "toolresult"
                    Some("toolresult") => {
                        if let Some(results) = item.get_mut("content").and_then(Value::as_array_mut)
                        {
                            for r in results.iter_mut() {
                                if r.get("type").and_then(Value::as_str) == Some("text") {
                                    self.annotate_tool_text(r).await;
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        v
    }

    /// 图片：解码 base64（或 data URL）存 images/，消息里只留 image_ref
    async fn demote_image(&self, item: &mut Value) {
        let Some(b64) = item.get("data").and_then(Value::as_object).and_then(|d| {
            match d.get("type").and_then(Value::as_str) {
                // DocumentSourceKind::Base64
                Some("base64") => d.get("value").and_then(Value::as_str).map(str::to_string),
                // DocumentSourceKind::Url，仅处理 data:<mime>;base64,<payload>
                Some("url") => d.get("value").and_then(Value::as_str).and_then(|u| {
                    u.strip_prefix("data:")
                        .and_then(|rest| rest.split_once(";base64,").map(|(_, b)| b.to_string()))
                }),
                _ => None,
            }
        }) else {
            return;
        };
        let Ok(bytes) = BASE64.decode(b64.as_bytes()) else {
            tracing::warn!("journal： 图片 base64 解码失败，保留原样");
            return;
        };
        let ext = match item
            .get("media_type")
            .and_then(Value::as_str)
            .map(|s| s.to_ascii_uppercase())
            .as_deref()
        {
            Some("JPEG") => "jpg",
            Some("PNG") => "png",
            Some("GIF") => "gif",
            Some("WEBP") => "webp",
            Some("HEIC") => "heic",
            Some("HEIF") => "heif",
            Some("SVG") => "svg",
            _ => "bin",
        };
        let name = format!("{}.{}", Uuid::new_v4(), ext);
        let path = self.images_dir().join(&name);
        if let Err(e) = tokio::fs::write(&path, &bytes).await {
            tracing::warn!("journal： 写入图片 {} 失败，保留原样： {e}", path.display());
            return;
        }
        let obj = match item.as_object_mut() {
            Some(o) => o,
            None => return,
        };
        obj.remove("data");
        obj.insert("image_ref".into(), json!(format!("images/{name}")));
    }

    /// 工具结果文本完整写入 jsonl（即 agent 看到的内容；上游 record_tool_result
    /// 已把长输出限长到 ~9KB）。若文本里带 record_tool_result 的保存位置提示，
    /// 附 result_ref 指向 toolout/ 里的原始全文（模型可用它取回被省略的中间部分）
    async fn annotate_tool_text(&self, r: &mut Value) {
        let Some(text) = r.get("text").and_then(Value::as_str) else {
            return;
        };
        let Some(name) = extract_toolout_ref(text) else {
            return;
        };
        let exists = tokio::fs::try_exists(self.toolout_dir().join(&name))
            .await
            .unwrap_or(false);
        if !exists {
            return;
        }

        let obj = match r.as_object_mut() {
            Some(o) => o,
            None => return,
        };
        obj.insert("result_ref".into(), json!(format!("toolout/{name}")));
    }
}

/// 从工具结果文本中提取 record_tool_result 提示里引用的文件名（<uuid>.txt[.gz]）。
/// 提示里是绝对路径，这里只取 toolout/ 后的文件名部分
fn extract_toolout_ref(text: &str) -> Option<String> {
    let idx = text.rfind("toolout/")?;
    let rest = &text[idx + "toolout/".len()..];
    let end = rest.find(|c: char| !c.is_ascii_alphanumeric() && !"._-".contains(c))?;
    let name = &rest[..end];
    if name.ends_with(".txt") || name.ends_with(".txt.gz") {
        Some(name.to_string())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::read::GzDecoder;
    use std::io::Read as _;

    #[test]
    fn gzip_roundtrip() {
        let data = b"hello journals \xe4\xbd\xa0\xe5\xa5\xbd";
        let gz = gzip_bytes(data);
        let mut dec = GzDecoder::new(&gz[..]);
        let mut out = Vec::new();
        dec.read_to_end(&mut out).unwrap();
        assert_eq!(&out, data);
    }

    /// 构造一条「用户消息：超长工具结果 + base64 图片」
    fn sample_message() -> RigMessage {
        let long_len = 60 * 1024; // 60KB 长文本：验证 jsonl 完整保留
        let long = "x".repeat(long_len);
        serde_json::from_value(json!({
            "role": "user",
            "content": [
                {
                    "type": "toolresult",
                    "call": "c1",
                    "name": "bash",
                    "content": [{ "type": "text", "text": long }]
                },
                {
                    "type": "image",
                    "data": { "type": "base64", "value": "aGVsbG8=" },
                    "media_type": "png"
                }
            ]
        }))
        .unwrap()
    }

    #[tokio::test]
    async fn append_round_keeps_full_tool_result_and_demotes_image() {
        let dir = std::env::temp_dir().join(format!("journal-test-{}", Uuid::new_v4()));
        let session = SessionFile {
            path: dir.join("messages.jsonl"),
            dir: dir.clone(),
        };
        std::fs::create_dir_all(session.toolout_dir()).unwrap();
        std::fs::create_dir_all(session.images_dir()).unwrap();

        session
            .append_round(Uuid::new_v4(), &[sample_message()])
            .await;

        let line = std::fs::read_to_string(&session.path).unwrap();
        let v: Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(v["seq"], 1);
        let msg = &v["msg"];
        // 工具结果：文本完整保留（即 agent 看到的内容），无 result_ref
        let tr = &msg["content"][0];
        assert_eq!(tr["type"], "toolresult");
        let text_item = &tr["content"][0];
        assert_eq!(
            text_item["text"].as_str().unwrap().chars().count(),
            60 * 1024
        );
        assert!(text_item.get("result_ref").is_none());
        // toolout/ 里不另存文件（文本已完整在 jsonl 里）
        assert_eq!(std::fs::read_dir(session.toolout_dir()).unwrap().count(), 0);
        // 图片：存文件，jsonl 里只留 image_ref，没有 base64
        let img = &msg["content"][1];
        assert!(img.get("data").is_none());
        let img_ref = img["image_ref"].as_str().unwrap().to_string();
        assert_eq!(std::fs::read(dir.join(&img_ref)).unwrap(), b"hello");
        assert!(!line.contains("aGVsbG8="));

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn extract_toolout_ref_parses_note() {
        let note = format!(
            "（完整结果已保存到 /home/x/journals/2026-08/session-1-ab/toolout/{}.txt。可用 `cat` 查看。）",
            "a1b2c3d4"
        );
        assert_eq!(extract_toolout_ref(&note).as_deref(), Some("a1b2c3d4.txt"));
        let gz = "…完整输出已保存到 /a/b/toolout/eff.txt.gz。";
        assert_eq!(extract_toolout_ref(gz).as_deref(), Some("eff.txt.gz"));
        assert_eq!(extract_toolout_ref("没有提示的普通文本"), None);
    }

    #[tokio::test]
    async fn append_round_refers_existing_toolout_file() {
        let dir = std::env::temp_dir().join(format!("journal-test-{}", Uuid::new_v4()));
        let session = SessionFile {
            path: dir.join("messages.jsonl"),
            dir: dir.clone(),
        };
        std::fs::create_dir_all(session.toolout_dir()).unwrap();
        // 预先模拟 record_tool_result 已落盘的文件
        let existing = "cafe0001.txt";
        std::fs::write(session.toolout_dir().join(existing), b"raw output").unwrap();

        let long = "y".repeat(600);
        let note = format!(
            "完整结果已保存到 {}。",
            session.toolout_dir().join(existing).display()
        );
        let msg: RigMessage = serde_json::from_value(json!({
            "role": "user",
            "content": [{
                "type": "toolresult",
                "call": "c1",
                "name": "bash",
                "content": [{ "type": "text", "text": format!("{long}\n\n（{note}）") }]
            }]
        }))
        .unwrap();

        session.append_round(Uuid::new_v4(), &[msg]).await;

        let line =
            std::fs::read_to_string(&session.path).unwrap_or_else(|e| panic!("read failed: {e}"));
        let v: Value = serde_json::from_str(line.trim()).unwrap_or_else(|e| {
            panic!(
                "parse failed: {e}; len={}; size={}",
                line.len(),
                std::fs::metadata(&session.path)
                    .map(|m| m.len())
                    .unwrap_or(u64::MAX)
            )
        });
        let item = &v["msg"]["content"][0]["content"][0];
        // 文本完整保留（即 agent 看到的内容）,result_ref 指向已存在的落盘文件
        assert_eq!(item["text"], format!("{long}\n\n（{note}）"));
        assert_eq!(item["result_ref"], format!("toolout/{existing}"));
        // 没有再落第二份文件：toolout/ 里只有预置的那一个
        let count = std::fs::read_dir(session.toolout_dir()).unwrap().count();
        assert_eq!(count, 1);

        std::fs::remove_dir_all(&dir).unwrap();
    }
}
