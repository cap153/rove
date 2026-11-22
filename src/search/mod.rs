//! 拼音检索引擎（Phase 5，核心移植自 stools `core/matcher.rs`）。
//!
//! - `pinyin_fields`：为文件名预计算全拼（含异读字多读音）与首字母缩写。
//! - `matches_query` / `score`：查询对 name / 全拼 / 简拼做模糊匹配
//!   （nucleo），支持全拼（wangyiyun）、简拼（wyy）、原文汉字、
//!   大小写不敏感、1~2 个编辑距离容错。
//! - 纯 ASCII 名走快速路径：三字段相同，无反向表开销。

use nucleo_matcher::{Matcher, Utf32Str};
use pinyin::ToPinyinMulti;

/// 单个字符的全部拼音写法（多音字给出多个）；非汉字返回空。
///
/// 与 `pinyin_fields` 的区别在于**不摊平**：正因为 `full` 会把多音字的所有读音
/// 并排拼起来（喜 = `xi` + `chi`），拿它做连续子串匹配时 `xiqing` 会断在中间，
/// 「喜庆」就永远搜不到。需要「按字对齐」的调用方用这个自己逐字匹配。
pub fn char_pinyins(ch: char) -> Vec<&'static str> {
    match ch.to_pinyin_multi() {
        Some(multi) => multi.into_iter().map(|p| p.plain()).collect(),
        None => Vec::new(),
    }
}

/// 一个条目的预计算拼音字段。
///
/// **不含**反向定位表（`indices`）：那张表按「每个字 10 字节」计费，中文名的
/// 开销比字段本身还大，而它只在「屏幕上几十行要做命中高亮」时才用得上。
/// 全盘索引上百万条、每个目录几千条都常驻，存它就是白占内存；
/// 高亮时按名字现算（见 [`indices_for`]），代价只有微秒级。
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct PinyinFields {
    pub full: Box<str>,
    /// 简拼。**空串表示「与 `full` 相同」**：纯 ASCII 名的快路径里简拼和全拼
    /// 本来就是同一份小写名字，再各存一份等于把文件名存三遍。
    pub abbr: Box<str>,
}

impl PinyinFields {
    /// 简拼视图：ASCII 快路径下 `abbr` 是空的，此时简拼就是 `full`。
    fn abbr_str(&self) -> &str {
        if self.abbr.is_empty() {
            &self.full
        } else {
            &self.abbr
        }
    }
}

#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
#[allow(dead_code)]
struct FieldIndicesEntry {
    name_idx: u16,
    abbr_start: u16,
    #[allow(dead_code)]
    abbr_end: u16,
    full_start: u16,
    full_end: u16,
}

/// 计算 name 的全拼 / 简拼；需要反向定位表时用 [`indices_for`]。
pub fn pinyin_fields(input: &str) -> PinyinFields {
    walk(input, false).0
}

/// 只为高亮反查计算「简拼/全拼字符位置 → 原文第几个字」的定位表。
///
/// 拼接顺序与 [`walk`] 完全一致，所以这些 offset 能对上 [`pinyin_fields`]
/// 算出来的 `full` / `abbr`。
fn indices_for(input: &str) -> Vec<FieldIndicesEntry> {
    walk(input, true).1
}

/// 遍历一遍名字，同时产出拼音字段与（可选的）反向定位表。
fn walk(input: &str, collect_indices: bool) -> (PinyinFields, Vec<FieldIndicesEntry>) {
    if input.is_ascii() {
        let lower = input.to_ascii_lowercase();
        // 简拼留空 = 与 full 相同（见 `PinyinFields::abbr_str`）。
        return (
            PinyinFields {
                full: lower.into(),
                abbr: Box::default(),
            },
            Vec::new(),
        );
    }

    let mut full = String::with_capacity(input.len() + 8);
    let mut abbr = String::with_capacity(input.len());
    let mut indices = if collect_indices {
        Vec::with_capacity(input.len())
    } else {
        Vec::new()
    };
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
                    if collect_indices {
                        indices.push(FieldIndicesEntry {
                            name_idx: name_idx as u16,
                            abbr_start: abbr_start + seen_abbr.len() as u16,
                            abbr_end: abbr_start + seen_abbr.len() as u16 + 1,
                            full_start: char_len(&full) - char_len(plain),
                            full_end: char_len(&full),
                        });
                    }
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
            if collect_indices {
                indices.push(FieldIndicesEntry {
                    name_idx: name_idx as u16,
                    abbr_start,
                    abbr_end: char_len(&abbr),
                    full_start,
                    full_end: char_len(&full),
                });
            }
        }
    }

    indices.sort_by_key(|e| (e.abbr_start, e.full_start));
    (
        PinyinFields {
            full: full.into(),
            abbr: abbr.into(),
        },
        indices,
    )
}

/// 定位表反查：全拼/简拼里的第 `idx` 个字符来自原文的第几个字。
fn lookup_indices(indices: &[FieldIndicesEntry], idx: usize, abbr: bool) -> Option<usize> {
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
    let pos = indices.partition_point(|e| start(e) <= idx);
    let entry = pos.checked_sub(1).and_then(|p| indices.get(p))?;
    (start(entry) <= idx && idx < end(entry)).then(|| entry.name_idx as usize)
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
        .fuzzy_match(Utf32Str::new(fields.abbr_str(), buf_c), q)
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
    score_parts(name, fields.abbr_str(), &fields.full, query_lower, matcher, bufs)
}

/// 同上，但拼音字段是**借用**的切片。
///
/// 全局索引现在是映射在磁盘文件上的：名字和拼音都在映射区里，拿不到
/// `PinyinFields`（那是 `Box<str>` 所有权类型）。为此多开一个纯借用版本，
/// 避免为了打一次分就把每条记录的名字和拼音复制一遍。
pub fn score_parts(
    name: &str,
    abbr: &str,
    full: &str,
    query_lower: &str,
    matcher: &mut Matcher,
    bufs: &mut (Vec<char>, Vec<char>, Vec<char>),
) -> Option<u16> {
    let (buf_a, buf_b, buf_c) = bufs;
    let query_owned = query_lower.to_lowercase();
    let q = Utf32Str::new(&query_owned, buf_a);
    let mut best = matcher.fuzzy_match(Utf32Str::new(name, buf_b), q);
    if let Some(s) = matcher.fuzzy_match(Utf32Str::new(abbr, buf_c), q) {
        best = Some(best.map_or(s, |b| b.max(s)));
    }
    if let Some(s) = matcher.fuzzy_match(Utf32Str::new(full, buf_b), q) {
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
    // 反向定位表按需现算（只有当前可见的几十行会走到这里）。
    let mut indices: Option<Vec<FieldIndicesEntry>> = None;

    // 1) 原文命中:indices 直接就是 name 的 char 索引。
    if matcher
        .fuzzy_indices(Utf32Str::new(name, buf_b), q, &mut idx_buf)
        .is_some()
    {
        return merge_ranges(idx_buf.iter().map(|&i| i as usize).collect(), total);
    }
    // 2) 简拼命中:abbr 的 char 索引 → 反查 name 字符位置。
    if matcher
        .fuzzy_indices(Utf32Str::new(fields.abbr_str(), buf_c), q, &mut idx_buf)
        .is_some()
    {
        let table = indices.get_or_insert_with(|| indices_for(name));
        let mapped: Vec<usize> = idx_buf
            .iter()
            .filter_map(|&i| lookup_indices(table, i as usize, true))
            .collect();
        return merge_ranges(mapped, total);
    }
    // 3) 全拼命中:full 的 char 索引 → 反查 name 字符位置。
    if matcher
        .fuzzy_indices(Utf32Str::new(&fields.full, buf_b), q, &mut idx_buf)
        .is_some()
    {
        let table = indices.get_or_insert_with(|| indices_for(name));
        let mapped: Vec<usize> = idx_buf
            .iter()
            .filter_map(|&i| lookup_indices(table, i as usize, false))
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
        // 简拼留空表示「与 full 相同」，不再重复存一份小写名。
        assert!(f.abbr.is_empty());
        assert_eq!(f.abbr_str(), "firefox");
        assert!(indices_for("Firefox").is_empty());
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

    #[test]
    fn parse_drive_prefix_works() {
        assert_eq!(parse_drive_prefix("c: videos"), (Some('C'), "videos"));
        assert_eq!(parse_drive_prefix("C:\\videos"), (Some('C'), "videos"));
        assert_eq!(parse_drive_prefix("d:videos"), (Some('D'), "videos"));
        assert_eq!(parse_drive_prefix("c:"), (Some('C'), ""));
        assert_eq!(parse_drive_prefix("c:\\"), (Some('C'), ""));
        assert_eq!(parse_drive_prefix("videos"), (None, "videos"));
    }
}

/// 解析查询词中的盘符限定前缀，支持 "c: videos"、"c:videos"、"c:\\videos"、"c:"。
/// 返回 (盘符大写, 剥离盘符后的纯净关键词)。
pub fn parse_drive_prefix(query: &str) -> (Option<char>, &str) {
    let trimmed = query.trim_start();
    let mut chars = trimmed.chars();
    if let Some(drive) = chars.next() {
        if drive.is_ascii_alphabetic() && chars.next() == Some(':') {
            let rest = &trimmed[drive.len_utf8() + 1..];
            let clean = rest.trim_start_matches(|c| c == '\\' || c == '/').trim_start();
            return (Some(drive.to_ascii_uppercase()), clean);
        }
    }
    (None, trimmed)
}

pub mod filter;
pub mod index;
pub mod mapfile;
pub mod mft;
pub mod jump;
