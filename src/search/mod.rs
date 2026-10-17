//! 拼音检索引擎（Phase 5，核心移植自 stools `core/matcher.rs`）。
//!
//! - `pinyin_fields`：为文件名预计算全拼（含异读字多读音）与首字母缩写。
//! - `matches_query` / `score`：查询对 name / 全拼 / 简拼做模糊匹配
//!   （nucleo），支持全拼（wangyiyun）、简拼（wyy）、原文汉字、
//!   大小写不敏感、1~2 个编辑距离容错。
//! - 纯 ASCII 名走快速路径：三字段相同，无反向表开销。

use nucleo_matcher::{Matcher, Utf32Str};
use pinyin::ToPinyinMulti;

/// 一个条目的预计算拼音字段。
#[derive(Clone, Debug, Default)]
#[allow(dead_code)] // indices 反向映射供后续命中高亮使用
pub struct PinyinFields {
    pub full: Box<str>,
    pub abbr: Box<str>,
    indices: Vec<FieldIndicesEntry>,
}

#[derive(Clone, Debug, Default)]
#[allow(dead_code)]
struct FieldIndicesEntry {
    name_idx: u16,
    abbr_start: u16,
    #[allow(dead_code)]
    abbr_end: u16,
    full_start: u16,
    full_end: u16,
}

impl PinyinFields {
    #[allow(dead_code)]
    fn lookup(&self, idx: usize, abbr: bool) -> Option<usize> {
        let idx = u16::try_from(idx).ok()?;
        let start = |e: &FieldIndicesEntry| {
            if abbr {
                e.abbr_start
            } else {
                e.full_start
            }
        };
        let end = |e: &FieldIndicesEntry| {
            if abbr {
                e.abbr_end
            } else {
                e.full_end
            }
        };
        let pos = self.indices.partition_point(|e| start(e) <= idx);
        let entry = pos.checked_sub(1).and_then(|p| self.indices.get(p))?;
        (start(entry) <= idx && idx < end(entry)).then(|| entry.name_idx as usize)
    }
}

/// 计算 name 的全拼 / 简拼 / 反向索引。
pub fn pinyin_fields(input: &str) -> PinyinFields {
    if input.is_ascii() {
        let lower = input.to_ascii_lowercase();
        return PinyinFields {
            full: lower.clone().into(),
            abbr: lower.into(),
            indices: Vec::new(),
        };
    }

    let mut full = String::with_capacity(input.len() + 8);
    let mut abbr = String::with_capacity(input.len());
    let mut indices = Vec::with_capacity(input.len());
    let char_len = |s: &str| s.chars().count() as u16;

    for (name_idx, ch) in input.chars().enumerate() {
        let abbr_start = char_len(&abbr);
        let full_start = char_len(&full);
        let mut handled = false;

        if let Some(multi) = ch.to_pinyin_multi() {
            let mut seen_plain = std::collections::BTreeSet::new();
            let mut seen_abbr = std::collections::BTreeSet::new();
            for py in multi {
                let plain = py.plain();
                let letter = py.first_letter();
                if seen_plain.insert(plain) {
                    full.push_str(plain);
                    indices.push(FieldIndicesEntry {
                        name_idx: name_idx as u16,
                        abbr_start: abbr_start + seen_abbr.len() as u16,
                        abbr_end: abbr_start + seen_abbr.len() as u16 + 1,
                        full_start: char_len(&full) - char_len(plain),
                        full_end: char_len(&full),
                    });
                    handled = true;
                }
                if seen_abbr.insert(letter) {
                    if let Some(initial) = letter.chars().next() {
                        abbr.push(initial);
                    }
                }
            }
        }

        if !handled {
            full.push(ch.to_ascii_lowercase());
            abbr.push(ch.to_ascii_lowercase());
            indices.push(FieldIndicesEntry {
                name_idx: name_idx as u16,
                abbr_start,
                abbr_end: char_len(&abbr),
                full_start,
                full_end: char_len(&full),
            });
        }
    }

    indices.sort_by_key(|e| (e.abbr_start, e.full_start));
    PinyinFields {
        full: full.into(),
        abbr: abbr.into(),
        indices,
    }
}

/// 查询是否匹配一个条目（name / 全拼 / 简拼任一命中即可）。
pub fn matches_query(
    name: &str,
    fields: &PinyinFields,
    query_lower: &str,
    matcher: &mut Matcher,
    bufs: &mut (Vec<char>, Vec<char>, Vec<char>),
) -> bool {
    let (buf_a, buf_b, buf_c) = bufs;
    // 强制小写：规避 nucleo Smart-case 在「混合大小写查询 + 删除容错」
    // 下的内部断言（nucleo-matcher 0.3.1，双 ASCII 路径误报）。
    let query_owned = query_lower.to_lowercase();
    let q = Utf32Str::new(&query_owned, buf_a);
    if matcher.fuzzy_match(Utf32Str::new(name, buf_b), q).is_some() {
        return true;
    }
    if matcher
        .fuzzy_match(Utf32Str::new(&fields.abbr, buf_c), q)
        .is_some()
    {
        return true;
    }
    matcher
        .fuzzy_match(Utf32Str::new(&fields.full, buf_b), q)
        .is_some()
}

/// 打分（越大越好）：用于结果排序。None = 不匹配。
pub fn score(
    name: &str,
    fields: &PinyinFields,
    query_lower: &str,
    matcher: &mut Matcher,
    bufs: &mut (Vec<char>, Vec<char>, Vec<char>),
) -> Option<u16> {
    let (buf_a, buf_b, buf_c) = bufs;
    let query_owned = query_lower.to_lowercase();
    let q = Utf32Str::new(&query_owned, buf_a);
    let mut best = matcher.fuzzy_match(Utf32Str::new(name, buf_b), q);
    if let Some(s) = matcher.fuzzy_match(Utf32Str::new(&fields.abbr, buf_c), q) {
        best = Some(best.map_or(s, |b| b.max(s)));
    }
    if let Some(s) = matcher.fuzzy_match(Utf32Str::new(&fields.full, buf_b), q) {
        best = Some(best.map_or(s, |b| b.max(s)));
    }
    best
}

/// 命中高亮:返回 name 中需要高亮的 char 索引区间(闭开,升序不重叠)。
/// 依次尝试原文 / 简拼 / 全拼命中,并经反向索引映射回 name 的字符位置。
pub fn highlight_ranges(
    name: &str,
    fields: &PinyinFields,
    query_lower: &str,
    matcher: &mut Matcher,
    bufs: &mut (Vec<char>, Vec<char>, Vec<char>),
) -> Vec<(usize, usize)> {
    let (buf_a, buf_b, buf_c) = bufs;
    let query_owned = query_lower.to_lowercase();
    let q = Utf32Str::new(&query_owned, buf_a);
    let total = name.chars().count();
    let mut idx_buf: Vec<u32> = Vec::new();

    // 1) 原文命中:indices 直接就是 name 的 char 索引。
    if matcher
        .fuzzy_indices(Utf32Str::new(name, buf_b), q, &mut idx_buf)
        .is_some()
    {
        return merge_ranges(idx_buf.iter().map(|&i| i as usize).collect(), total);
    }
    // 2) 简拼命中:abbr 的 char 索引 → 反查 name 字符位置。
    if matcher
        .fuzzy_indices(Utf32Str::new(&fields.abbr, buf_c), q, &mut idx_buf)
        .is_some()
    {
        let mapped: Vec<usize> = idx_buf
            .iter()
            .filter_map(|&i| fields.lookup(i as usize, true))
            .collect();
        return merge_ranges(mapped, total);
    }
    // 3) 全拼命中:full 的 char 索引 → 反查 name 字符位置。
    if matcher
        .fuzzy_indices(Utf32Str::new(&fields.full, buf_b), q, &mut idx_buf)
        .is_some()
    {
        let mapped: Vec<usize> = idx_buf
            .iter()
            .filter_map(|&i| fields.lookup(i as usize, false))
            .collect();
        return merge_ranges(mapped, total);
    }
    Vec::new()
}

/// 把命中的 char 索引集合整理成升序不重叠的闭开区间(相邻合并)。
fn merge_ranges(mut indices: Vec<usize>, total: usize) -> Vec<(usize, usize)> {
    indices.retain(|&i| i < total);
    indices.sort_unstable();
    indices.dedup();
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    for i in indices {
        match ranges.last_mut() {
            Some((_, end)) if *end == i => *end = i + 1,
            _ => ranges.push((i, i + 1)),
        }
    }
    ranges
}

/// 按高亮区间把 name 切成 (文本, 是否高亮) 片段;无命中时返回单段原文。
pub fn split_highlight(name: &str, ranges: &[(usize, usize)]) -> Vec<(String, bool)> {
    if ranges.is_empty() {
        return vec![(name.to_string(), false)];
    }
    let mut parts: Vec<(String, bool)> = Vec::new();
    let mut cur = String::new();
    let mut cur_hl = false;
    let mut ri = 0usize;
    for (char_i, ch) in name.chars().enumerate() {
        while ri < ranges.len() && char_i >= ranges[ri].1 {
            ri += 1;
        }
        let in_hl = ri < ranges.len() && char_i >= ranges[ri].0 && char_i < ranges[ri].1;
        if in_hl != cur_hl && !cur.is_empty() {
            parts.push((std::mem::take(&mut cur), cur_hl));
        }
        cur_hl = in_hl;
        cur.push(ch);
    }
    if !cur.is_empty() {
        parts.push((cur, cur_hl));
    }
    parts
}

/// 一步到位:计算命中片段。结果用于 UI 的名称高亮渲染。
pub fn highlight_parts(
    name: &str,
    fields: &PinyinFields,
    query_lower: &str,
    matcher: &mut Matcher,
    bufs: &mut (Vec<char>, Vec<char>, Vec<char>),
) -> Vec<(String, bool)> {
    let ranges = highlight_ranges(name, fields, query_lower, matcher, bufs);
    split_highlight(name, &ranges)
}

#[cfg(test)]
mod tests {
    use super::*;
    use nucleo_matcher::Config;

    fn m() -> Matcher {
        Matcher::new(Config::DEFAULT)
    }

    fn matched(name: &str, query: &str) -> bool {
        let f = pinyin_fields(name);
        let mut bufs = (Vec::new(), Vec::new(), Vec::new());
        matches_query(name, &f, query, &mut m(), &mut bufs)
    }

    #[test]
    fn pinyin_fields_are_computed() {
        let f = pinyin_fields("网易云音乐");
        assert!(f.full.contains("wangyiyun"), "full={}", f.full);
        assert!(f.full.contains("yue"), "heteronym lost: {}", f.full);
        assert!(f.abbr.starts_with("wy"), "abbr={}", f.abbr);
    }

    #[test]
    fn ascii_fast_path() {
        let f = pinyin_fields("Firefox");
        assert_eq!(&*f.full, "firefox");
        assert!(f.indices.is_empty());
    }

    #[test]
    fn matches_latin_substring() {
        assert!(matched("Firefox", "fire"));
        assert!(!matched("Firefox", "zzzz"));
    }

    #[test]
    fn matches_full_pinyin() {
        assert!(matched("网易云音乐", "wangyiyun"));
        assert!(matched("网易云音乐", "wangyi"));
    }

    #[test]
    fn matches_abbr_pinyin() {
        assert!(matched("网易云音乐", "wyy"));
    }

    #[test]
    fn matches_typed_hanzi() {
        assert!(matched("网易云音乐", "音乐"));
    }

    #[test]
    fn case_insensitive() {
        assert!(matched("ReadMe.TXT", "readme"));
    }

    #[test]
    fn fuzzy_tolerance() {
        // nucleo 允许 1~2 处编辑距离（漏字/错字）。
        assert!(matched("Documents", "Documnts"));
    }

    #[test]
    fn score_ranks_prefix_higher() {
        let a = pinyin_fields("abcdef");
        let bufs = &mut (Vec::new(), Vec::new(), Vec::new());
        let s1 = score("abcdef", &a, "abc", &mut m(), bufs);
        let s2 = score("abcdef", &a, "def", &mut m(), bufs);
        assert!(s1.unwrap() > s2.unwrap());
    }

    fn parts(name: &str, query: &str) -> Vec<(String, bool)> {
        let f = pinyin_fields(name);
        let bufs = &mut (Vec::new(), Vec::new(), Vec::new());
        highlight_parts(name, &f, query, &mut m(), bufs)
    }

    #[test]
    fn highlight_latin_substring() {
        let p = parts("FirefoxSetup.exe", "fire");
        assert_eq!(p[0], ("Fire".to_string(), true));
        assert_eq!(p[1], ("foxSetup.exe".to_string(), false));
    }

    #[test]
    fn highlight_covers_whole_text() {
        let p = parts("FirefoxSetup.exe", "fire");
        let joined: String = p.iter().map(|(t, _)| t.as_str()).collect();
        assert_eq!(joined, "FirefoxSetup.exe");
    }

    #[test]
    fn highlight_no_match_returns_plain() {
        let p = parts("Firefox", "zzzz");
        assert_eq!(p, vec![("Firefox".to_string(), false)]);
    }

    #[test]
    fn highlight_via_abbr_pinyin() {
        // 简拼 wyy 命中前三个字(简拼序列 w-y-y-y 的前三项)。
        let p = parts("网易云音乐", "wyy");
        assert_eq!(p, vec![("网易云".to_string(), true), ("音乐".to_string(), false)]);
    }

    #[test]
    fn highlight_via_full_pinyin() {
        // 全拼 wangyiyun 恰好覆盖 wang-yi-yun → "网易云"。
        let p = parts("网易云音乐", "wangyiyun");
        assert_eq!(p, vec![("网易云".to_string(), true), ("音乐".to_string(), false)]);
    }

    #[test]
    fn highlight_covers_all_chars_via_pinyin() {
        // 补全查询覆盖全部汉字。
        let p = parts("网易云音乐", "wangyiyunyinyue");
        assert_eq!(p, vec![("网易云音乐".to_string(), true)]);
    }

    #[test]
    fn highlight_partial_pinyin() {
        // 只命中前两个字的全拼。
        let p = parts("网易云音乐播放器", "wangyi");
        let hl: String = p.iter().filter(|(_, h)| *h).map(|(t, _)| t.as_str()).collect();
        assert_eq!(hl, "网易");
    }

    #[test]
    fn highlight_discrete_matches() {
        // 模糊匹配(fuzzy gap):F + x 两个离散命中点。
        let p = parts("Firefox", "fx");
        let hl: String = p.iter().filter(|(_, h)| *h).map(|(t, _)| t.as_str()).collect();
        assert_eq!(hl, "Fx");
    }
}

pub mod index;
pub mod mft;
