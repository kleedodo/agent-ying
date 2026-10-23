//! 角色（roles）：`~/.agent-ying/roles/` 下的每个 `.md` 文件就是一个角色（角色名 = 文件名去扩展名）。
//!
//! 系统提示词按以下顺序拼接（缺失的部分跳过）：
//! 1. `~/.agent-ying/SYSTEM.md`（不存在则用代码内通用规则 DEFAULT_SYSTEM_PROMPT）
//! 2. 角色人设：内置默认角色（default，DEFAULT_ROLE_PROMPT）或 `~/.agent-ying/roles/<角色>.md`
//!
//! skills 索引块仍由调用方追加在最末尾（拼接逻辑不变）。

use std::fs;
use std::path::PathBuf;

use crate::config::{Config, DEFAULT_ROLE_PROMPT, DEFAULT_SYSTEM_PROMPT};

/// 内置默认角色名（人设代码内置，不依赖 roles/ 目录）
pub const DEFAULT_ROLE_NAME: &str = "default";

/// 角色目录：`~/.agent-ying/roles/`
pub fn roles_dir() -> PathBuf {
    Config::dir().join("roles")
}

/// 扫描 roles/ 目录，返回角色名列表（`.md` 文件名去扩展名，按名字排序）。
/// 目录不存在或没有 `.md` 文件时返回空列表。
pub fn scan_roles() -> Vec<String> {
    let mut names = Vec::new();
    if let Ok(entries) = fs::read_dir(roles_dir()) {
        for entry in entries.flatten() {
            let path = entry.path();
            let is_md = path
                .extension()
                .is_some_and(|e| e.to_string_lossy().eq_ignore_ascii_case("md"));
            if path.is_file()
                && is_md
                && let Some(stem) = path.file_stem()
            {
                names.push(stem.to_string_lossy().into_owned());
            }
        }
    }
    names.sort();
    names
}

/// 某个角色的文件路径：`~/.agent-ying/roles/<name>.md`
pub fn role_path(name: &str) -> PathBuf {
    roles_dir().join(format!("{name}.md"))
}

/// 校验角色名是否对应 roles/ 下的一个 `.md` 文件（同时挡住路径穿越）
pub fn has_file_role(name: &str) -> bool {
    if name.is_empty()
        || name.contains('/')
        || name.contains('\\')
        || name.contains("..")
        || name.starts_with('.')
    {
        return false;
    }
    role_path(name).is_file()
}

/// 拼接系统提示词（不含 skills 块）：
/// SYSTEM.md（缺省回退 DEFAULT_SYSTEM_PROMPT） + 角色人设（None 或 "default" 用内置默认角色，
/// 否则读 roles/<角色>.md，文件不存在则回退内置默认角色）。
pub fn compose_system_prompt(role: Option<&str>) -> String {
    let mut out = String::new();

    let system_md = Config::dir().join("SYSTEM.md");
    match fs::read_to_string(&system_md) {
        Ok(content) => {
            tracing::info!("系统提示词使用 {}", system_md.display());
            out.push_str(&content);
        }
        Err(_) => out.push_str(DEFAULT_SYSTEM_PROMPT),
    }

    match role {
        None | Some(DEFAULT_ROLE_NAME) => {
            tracing::info!("系统提示词使用内置默认角色 {DEFAULT_ROLE_NAME}");
            out.push_str("\n\n");
            out.push_str(DEFAULT_ROLE_PROMPT);
        }
        Some(r) => match fs::read_to_string(role_path(r)) {
            Ok(content) => {
                tracing::info!("系统提示词使用角色 {r}: {}", role_path(r).display());
                out.push_str("\n\n");
                out.push_str(&content);
            }
            Err(_) => {
                tracing::warn!(
                    "角色文件 {} 不存在，回退到内置默认角色",
                    role_path(r).display()
                );
                out.push_str("\n\n");
                out.push_str(DEFAULT_ROLE_PROMPT);
            }
        },
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compose_without_role_uses_built_in_default_role() {
        // 本机若存在 SYSTEM.md / roles/ 则跳过（结果依赖本机文件）
        if Config::dir().join("SYSTEM.md").is_file() || !scan_roles().is_empty() {
            eprintln!("跳过：本机存在 SYSTEM.md / roles 文件");
            return;
        }
        assert_eq!(
            compose_system_prompt(None),
            format!("{DEFAULT_SYSTEM_PROMPT}\n\n{DEFAULT_ROLE_PROMPT}")
        );
    }

    #[test]
    fn has_file_role_rejects_traversal_and_missing() {
        assert!(!has_file_role(""));
        assert!(!has_file_role(".."));
        assert!(!has_file_role("../config"));
        assert!(!has_file_role("a/b"));
        assert!(!has_file_role("a\\b"));
        assert!(!has_file_role(".hidden"));
        assert!(!has_file_role("肯定不存在的角色-xyz"));
    }
}
