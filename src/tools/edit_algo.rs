//！ 编辑核心算法：
//！ 换行符规范化、BOM 剥离、模糊匹配、多处替换应用、展示向 diff 生成。

use similar::{DiffTag, TextDiff};
use unicode_normalization::UnicodeNormalization;

/// 一条替换规则
#[derive(Debug, Clone)]
pub struct Edit {
    pub old_text: String,
    pub new_text: String,
}

/// 探测文件换行风格：首个 CRLF 出现在首个独立 LF 之前则为 "\r\n"，否则 "\n"
pub fn detect_line_ending(content: &str) -> &'static str {
    let lf = content.find('\n');
    let crlf = content.find("\r\n");
    match (lf, crlf) {
        (None, _) => "\n",
        (Some(_), None) => "\n",
        (Some(lf), Some(crlf)) if crlf < lf => "\r\n",
        _ => "\n",
    }
}

/// 统一换行为 LF
pub fn normalize_to_lf(s: &str) -> String {
    s.replace("\r\n", "\n").replace('\r', "\n")
}

/// 按原换行风格还原
pub fn restore_line_endings(s: &str, ending: &str) -> String {
    if ending == "\r\n" {
        s.replace('\n', "\r\n")
    } else {
        s.to_string()
    }
}

/// 剥离 BOM，返回 （bom， 正文）。模型不会在 oldText 里带不可见 BOM，匹配前先剥掉
pub fn split_bom(s: &str) -> (&str, &str) {
    match s.strip_prefix('\u{feff}') {
        Some(rest) => ("\u{feff}", rest),
        None => ("", s),
    }
}

/// 模糊匹配用的归一化：
/// NFKC + 每行去尾部空白 + 智能引号/破折号/特殊空格归一为 ASCII
fn fuzzy_char(c: char) -> char {
    match c {
        // 智能单引号 → '
        '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}' => '\'',
        // 智能双引号 → "
        '\u{201C}' | '\u{201D}' | '\u{201E}' | '\u{201F}' => '"',
        // 各种破折号/连字符（U+2010~U+2015、U+2212 减号）→ -
        '\u{2010}' | '\u{2011}' | '\u{2012}' | '\u{2013}' | '\u{2014}' | '\u{2015}'
        | '\u{2212}' => '-',
        // 特殊空格（NBSP、各类 Unicode 空格、窄 NBSP、中数学空格、全角空格）→ 普通空格
        '\u{00A0}' | '\u{202F}' | '\u{205F}' | '\u{3000}' => ' ',
        c if ('\u{2002}'..='\u{200A}').contains(&c) => ' ',
        c => c,
    }
}

pub fn normalize_for_fuzzy_match(text: &str) -> String {
    let norm = text.nfkc().to_string();
    let mut out = String::with_capacity(norm.len());
    for (i, line) in norm.split('\n').enumerate() {
        if i > 0 {
            out.push('\n');
        }
        for c in line.trim_end().chars() {
            out.push(fuzzy_char(c));
        }
    }
    out
}

pub struct FuzzyMatch {
    /// 匹配起点（字节偏移，位于 contentForReplacement 空间）
    pub index: usize,
    /// 匹配长度（字节，同空间）
    pub match_length: usize,
    /// 是否走了模糊匹配（false = 精确命中）
    pub used_fuzzy: bool,
}

/// 先精确匹配，再在归一化空间模糊匹配
pub fn fuzzy_find_text(content: &str, old_text: &str) -> Option<FuzzyMatch> {
    if let Some(i) = content.find(old_text) {
        return Some(FuzzyMatch {
            index: i,
            match_length: old_text.len(),
            used_fuzzy: false,
        });
    }
    let fuzzy_content = normalize_for_fuzzy_match(content);
    let fuzzy_old = normalize_for_fuzzy_match(old_text);
    fuzzy_content.find(&fuzzy_old).map(|i| FuzzyMatch {
        index: i,
        match_length: fuzzy_old.len(),
        used_fuzzy: true,
    })
}

// ---------------------------------------------------------------- 多编辑应用

struct LineSpan {
    start: usize,
    end: usize,
}

/// 按行（保留行尾换行符）计算每行的字节区间
fn line_spans(content: &str) -> Vec<LineSpan> {
    let mut spans = Vec::new();
    let mut offset = 0usize;
    for line in content.split_inclusive('\n') {
        spans.push(LineSpan {
            start: offset,
            end: offset + line.len(),
        });
        offset += line.len();
    }
    spans
}

#[derive(Clone)]
struct TextReplacement {
    start: usize,
    len: usize,
    new_text: String,
}

/// 替换区间覆盖的行范围 [start_line, end_line)
fn replacement_line_range(
    lines: &[LineSpan],
    start: usize,
    end: usize,
) -> Result<(usize, usize), String> {
    let start_line = lines
        .iter()
        .position(|l| start >= l.start && start < l.end)
        .ok_or_else(|| "Replacement range is outside the base content.".to_string())?;
    let mut end_line = start_line;
    while end_line < lines.len() && lines[end_line].end < end {
        end_line += 1;
    }
    if end_line >= lines.len() {
        return Err("Replacement range is outside the base content.".to_string());
    }
    Ok((start_line, end_line + 1))
}

/// 按起点升序、从后往前应用替换，保证偏移稳定
fn apply_replacements(content: &str, repls: &[TextReplacement], offset: usize) -> String {
    let mut result = content.to_string();
    for r in repls.iter().rev() {
        let start = r.start - offset;
        result.replace_range(start..start + r.len, &r.new_text);
    }
    result
}

/// 归一化空间匹配、原文空间落盘：把每个替换扩展到它实际触及的行，
/// 触及的行用归一化基底重写，其余行原样从原文拷回，
/// 未改动行块保留原始字节（如行尾空白、CRLF 前的原貌）。
fn apply_replacements_preserving_unchanged_lines(
    original: &str,
    base: &str,
    repls: &[TextReplacement],
) -> Result<String, String> {
    let original_lines: Vec<&str> = original.split_inclusive('\n').collect();
    let base_lines = line_spans(base);
    if original_lines.len() != base_lines.len() {
        return Err(
            "Cannot preserve unchanged lines because the base content has a different line count."
                .to_string(),
        );
    }

    // 按起点排序，重叠/相邻的行范围合并为一组
    let sorted: Vec<&TextReplacement> = {
        let mut v: Vec<&TextReplacement> = repls.iter().collect();
        v.sort_by_key(|r| r.start);
        v
    };
    let mut groups: Vec<(usize, usize, Vec<TextReplacement>)> = Vec::new();
    for r in sorted {
        let range = replacement_line_range(&base_lines, r.start, r.start + r.len)?;
        let merge = groups.last_mut().is_some_and(|g| range.0 < g.1);
        if merge {
            let g = groups.last_mut().unwrap();
            g.1 = g.1.max(range.1);
            g.2.push(r.clone());
        } else {
            groups.push((range.0, range.1, vec![r.clone()]));
        }
    }

    let mut out = String::with_capacity(original.len());
    let mut original_line_index = 0usize;
    for (start_line, end_line, group_repls) in &groups {
        out.push_str(&original_lines[original_line_index..*start_line].concat());
        let group_start = base_lines[*start_line].start;
        let group_end = base_lines[end_line - 1].end;
        out.push_str(&apply_replacements(
            &base[group_start..group_end],
            group_repls,
            group_start,
        ));
        original_line_index = *end_line;
    }
    out.push_str(&original_lines[original_line_index..].concat());
    Ok(out)
}

#[derive(Debug)]
pub struct AppliedEdits {
    /// 改动前的 LF 规范化内容（供 diff 对比）
    pub base: String,
    /// 改动后的 LF 规范化内容
    pub new: String,
}

fn not_found_err(path: &str, i: usize, total: usize) -> String {
    if total == 1 {
        format!(
            "Could not find the exact text in {path}. The oldText must match exactly including all whitespace and newlines."
        )
    } else {
        format!(
            "Could not find edits[{i}] in {path}. The oldText must match exactly including all whitespace and newlines."
        )
    }
}

fn duplicate_err(path: &str, i: usize, total: usize, occurrences: usize) -> String {
    if total == 1 {
        format!(
            "Found {occurrences} occurrences of the text in {path}. The text must be unique. Please provide more context to make it unique."
        )
    } else {
        format!(
            "Found {occurrences} occurrences of edits[{i}] in {path}. Each oldText must be unique. Please provide more context to make it unique."
        )
    }
}

fn empty_old_text_err(path: &str, i: usize, total: usize) -> String {
    if total == 1 {
        format!("oldText must not be empty in {path}.")
    } else {
        format!("edits[{i}].oldText must not be empty in {path}.")
    }
}

fn no_change_err(path: &str, total: usize) -> String {
    if total == 1 {
        format!(
            "No changes made to {path}. The replacement produced identical content. This might indicate an issue with special characters or the text not existing as expected."
        )
    } else {
        format!("No changes made to {path}. The replacements produced identical content.")
    }
}

/// 在 LF 规范化内容上应用一组精确文本替换。
///
/// 所有编辑都对着同一份原始内容匹配（不是增量匹配），替换按起点排序后倒序应用；
/// 任一编辑走了模糊匹配时，在归一化空间做替换再把改动行回贴到原文，
/// 未改动行块保留原始字节。
pub fn apply_edits(
    normalized_content: &str,
    edits: &[Edit],
    path: &str,
) -> Result<AppliedEdits, String> {
    let total = edits.len();
    let norm_edits: Vec<(String, String)> = edits
        .iter()
        .map(|e| (normalize_to_lf(&e.old_text), normalize_to_lf(&e.new_text)))
        .collect();

    for (i, (old, _)) in norm_edits.iter().enumerate() {
        if old.is_empty() {
            return Err(empty_old_text_err(path, i, total));
        }
    }

    // 有任一编辑需要模糊匹配，则整体切换到归一化空间做替换
    let used_fuzzy = norm_edits
        .iter()
        .any(|(old, _)| fuzzy_find_text(normalized_content, old).is_some_and(|m| m.used_fuzzy));
    let replacement_base: CowStr = if used_fuzzy {
        CowStr::Owned(normalize_for_fuzzy_match(normalized_content))
    } else {
        CowStr::Borrowed(normalized_content)
    };
    // 计数统一在归一化空间做
    let fuzzy_base = replacement_base.as_ref();

    let mut matched: Vec<(usize, TextReplacement)> = Vec::with_capacity(total);
    for (i, (old, new)) in norm_edits.iter().enumerate() {
        let m = fuzzy_find_text(replacement_base.as_ref(), old)
            .ok_or_else(|| not_found_err(path, i, total))?;
        let fuzzy_old = normalize_for_fuzzy_match(old);
        let occurrences = fuzzy_base.matches(&fuzzy_old).count();
        if occurrences > 1 {
            return Err(duplicate_err(path, i, total, occurrences));
        }
        matched.push((
            i,
            TextReplacement {
                start: m.index,
                len: m.match_length,
                new_text: new.clone(),
            },
        ));
    }

    matched.sort_by_key(|a| a.1.start);
    for w in matched.windows(2) {
        let (prev_idx, prev) = &w[0];
        let (curr_idx, curr) = &w[1];
        if prev.start + prev.len > curr.start {
            return Err(format!(
                "edits[{prev_idx}] and edits[{curr_idx}] overlap in {path}. Merge them into one edit or target disjoint regions."
            ));
        }
    }

    let repls: Vec<TextReplacement> = matched.iter().map(|(_, r)| r.clone()).collect();
    let base_content = normalized_content.to_string();
    let new_content = if used_fuzzy {
        apply_replacements_preserving_unchanged_lines(
            normalized_content,
            replacement_base.as_ref(),
            &repls,
        )?
    } else {
        apply_replacements(replacement_base.as_ref(), &repls, 0)
    };

    if base_content == new_content {
        return Err(no_change_err(path, total));
    }
    Ok(AppliedEdits {
        base: base_content,
        new: new_content,
    })
}

/// 替换基底要么是原文，要么是归一化副本
enum CowStr<'a> {
    Borrowed(&'a str),
    Owned(String),
}

impl CowStr<'_> {
    fn as_ref(&self) -> &str {
        match self {
            CowStr::Borrowed(s) => s,
            CowStr::Owned(s) => s,
        }
    }
}

// ---------------------------------------------------------------- diff 生成

/// 展示向 diff：带行号、改动前后各保留 context_lines 行上下文、
/// 过长上下文用 `...` 省略。
/// 返回 （diff 文本， 新文件中第一个被改动行的行号）。
pub fn generate_diff_string(old: &str, new: &str) -> (String, Option<usize>) {
    const CONTEXT: usize = 4;

    let diff = TextDiff::from_lines(old, new);
    // diff 的 op range 以行为单位，预先按行拆开
    let old_lines: Vec<&str> = old.lines().collect();
    let new_lines: Vec<&str> = new.lines().collect();
    let width = old_lines.len().max(new_lines.len()).to_string().len();

    // 把 opcodes 展平成 added/removed/context 片段（Replace 拆成 removed+added，对齐 diffLines）
    #[derive(PartialEq)]
    enum Kind {
        Added,
        Removed,
        Context,
    }
    let mut parts: Vec<(Kind, Vec<&str>)> = Vec::new();
    for op in diff.ops() {
        let (tag, old_range, new_range) = op.as_tag_tuple();
        match tag {
            DiffTag::Equal => parts.push((Kind::Context, old_lines[old_range].to_vec())),
            DiffTag::Delete => parts.push((Kind::Removed, old_lines[old_range].to_vec())),
            DiffTag::Insert => parts.push((Kind::Added, new_lines[new_range].to_vec())),
            DiffTag::Replace => {
                parts.push((Kind::Removed, old_lines[old_range].to_vec()));
                parts.push((Kind::Added, new_lines[new_range].to_vec()));
            }
        }
    }

    let mut out: Vec<String> = Vec::new();
    let mut old_num: usize = 1;
    let mut new_num: usize = 1;
    let mut last_was_change = false;
    let mut first_changed: Option<usize> = None;
    let n_parts = parts.len();

    for (i, (kind, raw_lines)) in parts.iter().enumerate() {
        let raw: &[&str] = raw_lines;
        match kind {
            Kind::Added | Kind::Removed => {
                if first_changed.is_none() {
                    first_changed = Some(new_num);
                }
                for line in raw {
                    if *kind == Kind::Added {
                        out.push(format!("+{:>width$} {line}", new_num, width = width));
                        new_num += 1;
                    } else {
                        out.push(format!("-{:>width$} {line}", old_num, width = width));
                        old_num += 1;
                    }
                }
                last_was_change = true;
            }
            Kind::Context => {
                let next_is_change = i + 1 < n_parts && parts[i + 1].0 != Kind::Context;
                let leading = last_was_change;
                let trailing = next_is_change;
                if leading && trailing {
                    if raw.len() <= CONTEXT * 2 {
                        for line in raw {
                            out.push(format!(" {:>width$} {line}", old_num, width = width));
                            old_num += 1;
                            new_num += 1;
                        }
                    } else {
                        let skipped = raw.len() - CONTEXT * 2;
                        for line in &raw[..CONTEXT] {
                            out.push(format!(" {:>width$} {line}", old_num, width = width));
                            old_num += 1;
                            new_num += 1;
                        }
                        out.push(format!(" {:>width$} ...", "", width = width));
                        old_num += skipped;
                        new_num += skipped;
                        for line in &raw[raw.len() - CONTEXT..] {
                            out.push(format!(" {:>width$} {line}", old_num, width = width));
                            old_num += 1;
                            new_num += 1;
                        }
                    }
                } else if leading {
                    let shown = &raw[..CONTEXT.min(raw.len())];
                    for line in shown {
                        out.push(format!(" {:>width$} {line}", old_num, width = width));
                        old_num += 1;
                        new_num += 1;
                    }
                    let skipped = raw.len() - shown.len();
                    if skipped > 0 {
                        out.push(format!(" {:>width$} ...", "", width = width));
                        old_num += skipped;
                        new_num += skipped;
                    }
                } else if trailing {
                    let skipped = raw.len().saturating_sub(CONTEXT);
                    if skipped > 0 {
                        out.push(format!(" {:>width$} ...", "", width = width));
                        old_num += skipped;
                        new_num += skipped;
                    }
                    for line in &raw[skipped..] {
                        out.push(format!(" {:>width$} {line}", old_num, width = width));
                        old_num += 1;
                        new_num += 1;
                    }
                } else {
                    // 远离任何改动的上下文：整段跳过
                    old_num += raw.len();
                    new_num += raw.len();
                }
                last_was_change = false;
            }
        }
    }

    (out.join("\n"), first_changed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edit(old: &str, new: &str) -> Edit {
        Edit {
            old_text: old.into(),
            new_text: new.into(),
        }
    }

    #[test]
    fn exact_single_edit() {
        let content = "foo\nbar\nbaz\n";
        let r = apply_edits(content, &[edit("bar", "BAR")], "f.txt").unwrap();
        assert_eq!(r.new, "foo\nBAR\nbaz\n");
    }

    #[test]
    fn multiple_disjoint_edits() {
        let content = "aaa\nbbb\nccc\n";
        let r = apply_edits(content, &[edit("ccc", "CCC"), edit("aaa", "AAA")], "f.txt").unwrap();
        assert_eq!(r.new, "AAA\nbbb\nCCC\n");
    }

    #[test]
    fn not_found() {
        let e = apply_edits("foo\n", &[edit("nope", "x")], "f.txt").unwrap_err();
        assert!(e.contains("Could not find the exact text"));
    }

    #[test]
    fn duplicate_occurrence() {
        let e = apply_edits("x\nx\n", &[edit("x", "y")], "f.txt").unwrap_err();
        assert!(e.contains("Found 2 occurrences"));
    }

    #[test]
    fn overlap() {
        let e = apply_edits("12345\n", &[edit("123", "a"), edit("345", "b")], "f.txt").unwrap_err();
        assert!(e.contains("overlap"));
    }

    #[test]
    fn no_change() {
        let e = apply_edits("foo\n", &[edit("foo", "foo")], "f.txt").unwrap_err();
        assert!(e.contains("No changes made"));
    }

    #[test]
    fn fuzzy_match_strips_trailing_ws_and_smart_quotes() {
        let content = "let a = “b”   \nnext line\n";
        let r = apply_edits(content, &[edit("let a = \"b\"", "let a = 'B'")], "f.txt").unwrap();
        // 改动行按归一化空间重写，其余行原样保留
        assert_eq!(r.new, "let a = 'B'\nnext line\n");
    }

    #[test]
    fn empty_old_text() {
        let e = apply_edits("foo\n", &[edit("", "x")], "f.txt").unwrap_err();
        assert!(e.contains("must not be empty"));
    }

    #[test]
    fn detect_ending() {
        assert_eq!(detect_line_ending("a\r\nb\r\n"), "\r\n");
        assert_eq!(detect_line_ending("a\nb"), "\n");
        assert_eq!(detect_line_ending("a\nb\r\nc"), "\n");
        assert_eq!(detect_line_ending("a\r\nb\nc"), "\r\n");
        assert_eq!(detect_line_ending("no newlines"), "\n");
    }

    #[test]
    fn crlf_bom_roundtrip() {
        // edit 工具完整文件流程：BOM + CRLF 文件改动后应保留 BOM 和 CRLF
        let raw = "\u{feff}a\r\nb\r\nc\r\n";
        let (bom, content) = split_bom(raw);
        let ending = detect_line_ending(content);
        let norm = normalize_to_lf(content);
        let r = apply_edits(&norm, &[edit("b", "B")], "f.txt").unwrap();
        let out = format!("{bom}{}", restore_line_endings(&r.new, ending));
        assert_eq!(out, "\u{feff}a\r\nB\r\nc\r\n");
    }

    #[test]
    fn diff_format() {
        let old = "1\n2\n3\n4\n5\n6\n7\n";
        let new = "1\nX\n3\n4\n5\n6\n7\n";
        let (diff, first) = generate_diff_string(old, new);
        assert_eq!(first, Some(2));
        assert!(diff.contains("+2 X"));
        assert!(diff.contains("-2 2"));
        assert!(diff.contains(" 3 3"));
    }

    #[test]
    fn diff_elides_long_context() {
        let old: String = (1..=30).map(|i| format!("{i}\n")).collect();
        let new = old.replace("15\n", "X\n");
        let (diff, _) = generate_diff_string(&old, &new);
        assert!(diff.contains("..."));
        assert!(!diff.contains("  5 5\n"));
    }
}
