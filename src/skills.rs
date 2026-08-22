//! 技能(skills):扫描 `~/.agent-ying/skills/<name>/SKILL.md`。
//!
//! 渐进式披露:启动时只把每个 skill 的 name + description 拼进系统提示,
//! 模型判断任务匹配某个 skill 时,再用 read_skill 工具读取完整 SKILL.md。

use std::fs;
use std::path::PathBuf;

#[derive(Clone)]
pub struct Skill {
    pub name: String,
    pub description: String,
    /// SKILL.md 的绝对路径
    pub path: PathBuf,
}

#[derive(Clone)]
pub struct Skills {
    pub skills: Vec<Skill>,
}

impl Skills {
    /// 扫描目录下每个子目录的 SKILL.md;目录不存在或为空都返回空列表。
    pub fn load(dir: PathBuf) -> Self {
        let mut skills = Vec::new();
        if let Ok(entries) = fs::read_dir(&dir) {
            for entry in entries.flatten() {
                let skill_dir = entry.path();
                if !skill_dir.is_dir() {
                    continue;
                }
                let skill_md = skill_dir.join("SKILL.md");
                let Ok(content) = fs::read_to_string(&skill_md) else {
                    continue;
                };
                let fallback_name = skill_dir
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                let (name, description) = parse_frontmatter(&content, &fallback_name);
                skills.push(Skill {
                    name,
                    description,
                    path: skill_md,
                });
            }
        }
        skills.sort_by(|a, b| a.name.cmp(&b.name));
        Self { skills }
    }

    /// 追加到系统提示末尾的文本块(pi 的 <available_skills> 格式);没有 skill 时返回 None。
    pub fn render_block(&self) -> Option<String> {
        if self.skills.is_empty() {
            return None;
        }
        let mut out = String::from("\n\n## 技能\n\n以下技能为特定任务提供专门指令。\n");
        out.push_str("当任务匹配某个技能的描述时,用 read_skill 工具读取该技能的文件。\n");
        out.push_str(
            "当技能文件引用相对路径时,以技能目录(SKILL.md 的父目录 / 路径的 dirname)为基准解析,并在工具命令中使用该绝对路径。\n",
        );
        out.push_str("\n<available_skills>\n");
        for s in &self.skills {
            out.push_str(&format!(
                "  <skill>\n    <name>{}</name>\n    <description>{}</description>\n    <location>{}</location>\n  </skill>\n",
                s.name, s.description, s.path.display()
            ));
        }
        out.push_str("</available_skills>");
        Some(out)
    }
}

/// 解析 SKILL.md 开头的 YAML frontmatter,只取 name / description 两个单行字段。
/// 没有 frontmatter 或缺字段时,name 回退为目录名,description 留空。
fn parse_frontmatter(content: &str, fallback_name: &str) -> (String, String) {
    let mut lines = content.lines();
    let mut name = String::new();
    let mut description = String::new();
    if lines.next().map(str::trim) == Some("---") {
        for line in lines {
            let trimmed = line.trim();
            if trimmed == "---" {
                break;
            }
            if let Some(v) = trimmed.strip_prefix("name:") {
                name = v.trim().to_string();
            } else if let Some(v) = trimmed.strip_prefix("description:") {
                description = v.trim().to_string();
            }
        }
    }
    if name.is_empty() {
        name = fallback_name.to_string();
    }
    (name, description)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_frontmatter() {
        let md = "---\nname: gitmoji\ndescription: 写 gitmoji commit\n---\n正文";
        let (name, desc) = parse_frontmatter(md, "fallback");
        assert_eq!(name, "gitmoji");
        assert_eq!(desc, "写 gitmoji commit");
    }

    #[test]
    fn falls_back_to_dir_name() {
        let md = "没有 frontmatter 的正文";
        let (name, desc) = parse_frontmatter(md, "myskill");
        assert_eq!(name, "myskill");
        assert_eq!(desc, "");
    }

    #[test]
    fn render_block_none_when_empty() {
        let skills = Skills { skills: Vec::new() };
        assert!(skills.render_block().is_none());
    }

    #[test]
    fn render_block_lists_skills() {
        let skills = Skills {
            skills: vec![Skill {
                name: "gitmoji".into(),
                description: "写 commit".into(),
                path: PathBuf::from("/tmp/x/gitmoji/SKILL.md"),
            }],
        };
        let block = skills.render_block().unwrap();
        assert!(block.contains("gitmoji"));
        assert!(block.contains("写 commit"));
        assert!(block.contains("/tmp/x/gitmoji/SKILL.md"));
    }
}
