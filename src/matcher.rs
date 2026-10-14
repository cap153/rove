//! 拼音检索与高亮引擎（移植自 `stools/src/core/matcher.rs`）。
//!
//! * `pinyin_fields`：预计算全拼、首字母与「拼音下标 -> 名字下标」反查表，
//!   支持多音字（异读音全部收录，任一副读音命中即可匹配）；
//! * `highlight_indices`：按「原名 -> 首字母 -> 全拼」优先级模糊匹配，
//!   返回命中的**名字字符下标**，供 UI 高亮；
//! * `build_highlight_spans`：把命中下标切成连续的渲染片段。

use nucleo_matcher::{Config, Matcher, Utf32Str};
use pinyin::ToPinyinMulti;

/// 一个参与搜索的条目（名称 + 预计算拼音字段）
#[derive(Debug, Clone, Default)]
pub struct MatchEntry {
    pub name: String,
    /// 全拼（无声调 ASCII，异读拼接）
    pub pinyin_full: String,
    /// 首字母（异读首字母去重后拼接）
    pub pinyin_abbr: String,
    /// 拼音下标 -> 名字下标反查表
    pub indices: FieldIndices,
}

impl MatchEntry {
    /// 由名称构造（预计算拼音字段）
    pub fn new(name: &str) -> Self {
        let (full, abbr, indices) = pinyin_fields(name);
        Self {
            name: name.to_string(),
            pinyin_full: full,
            pinyin_abbr: abbr,
            indices,
        }
    }
}

/// 单个字符的拼音区间（下标均为字符数，纯 ASCII 时字符==字节）
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FieldIndicesEntry {
    pub name_idx: u16,
    pub abbr_start: u16,
    pub abbr_end: u16,
    pub full_start: u16,
    pub full_end: u16,
}

/// 反查表：按 `abbr_start` 排序，可二分查找
#[derive(Debug, Clone, Default)]
pub struct FieldIndices {
    pub entries: Vec<FieldIndicesEntry>,
}

impl FieldIndices {
    /// 首字母下标对应的名字字符下标。
    /// 纯 ASCII 名（反查表为空）时为恒等映射。
    pub fn name_idx_for_abbr(&self, idx: usize) -> Option<usize> {
        match self.entries.is_empty() {
            true => Some(idx),
            false => self.lookup(idx, true).map(usize::from),
        }
    }

    /// 全拼下标对应的名字字符下标（同上，空表为恒等映射）
    #[allow(dead_code)]
    pub fn name_idx_for_full(&self, idx: usize) -> Option<usize> {
        match self.entries.is_empty() {
            true => Some(idx),
            false => self.lookup(idx, false).map(usize::from),
        }
    }

    fn lookup(&self, idx: usize, abbr: bool) -> Option<u16> {
        let idx = u16::try_from(idx).ok()?;
        let start = |e: &FieldIndicesEntry| if abbr { e.abbr_start } else { e.full_start };
        let end = |e: &FieldIndicesEntry| if abbr { e.abbr_end } else { e.full_end };
        let pos = self.entries.partition_point(|e| start(e) <= idx);
        let entry = pos.checked_sub(1).and_then(|p| self.entries.get(p))?;
        (start(entry) <= idx && idx < end(entry)).then_some(entry.name_idx)
    }
}

/// 计算全拼、首字母与反查表（移植自 stools）
pub fn pinyin_fields(input: &str) -> (String, String, FieldIndices) {
    // 纯 ASCII 名：三个字段相同，反查表为空（恒等映射）
    if input.is_ascii() {
        let lower = input.to_ascii_lowercase();
        return (lower.clone(), lower, FieldIndices::default());
    }

    let mut full = String::with_capacity(input.len() + 8);
    let mut abbr = String::with_capacity(input.len());
    let mut entries = Vec::with_capacity(input.len());
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
                    entries.push(FieldIndicesEntry {
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
            entries.push(FieldIndicesEntry {
                name_idx: name_idx as u16,
                abbr_start,
                abbr_end: char_len(&abbr),
                full_start,
                full_end: char_len(&full),
            });
        }
    }

    entries.sort_by_key(|e| (e.abbr_start, e.full_start));
    (full, abbr, FieldIndices { entries })
}

/// 一段渲染文本（命中段带高亮）
#[derive(Debug, Clone)]
pub struct TextSpan {
    pub text: String,
    pub is_match: bool,
}

const MAX_HIGHLIGHT_CHARS: usize = 256;

/// 把名字切成连续的命中 / 未命中片段
pub fn build_highlight_spans(name: &str, matched_indices: &[usize]) -> Vec<TextSpan> {
    if matched_indices.is_empty() {
        return vec![TextSpan {
            text: name.to_string(),
            is_match: false,
        }];
    }

    let mut is_match_map = [false; MAX_HIGHLIGHT_CHARS];
    for &idx in matched_indices {
        if idx < MAX_HIGHLIGHT_CHARS {
            is_match_map[idx] = true;
        }
    }

    let mut spans: Vec<TextSpan> = Vec::with_capacity(4);
    for (idx, ch) in name.chars().enumerate() {
        let is_match = idx < MAX_HIGHLIGHT_CHARS && is_match_map[idx];
        match spans.last_mut() {
            Some(span) if span.is_match == is_match => span.text.push(ch),
            _ => spans.push(TextSpan {
                text: ch.to_string(),
                is_match,
            }),
        }
    }
    spans
}

/// 命中的名字字符下标（高亮层用）。
///
/// 匹配策略（按优先级）：
/// 1. **原名**：模糊子序列匹配（容忍拉丁名跳字，如 `qute` → `qutebrowser`）；
/// 2. **首字母**：模糊子序列匹配（容忍跳过中间字，如 `wym` → 网易云音乐）；
/// 3. **全拼**：**音节边界对齐的前缀匹配**。
///
/// 全拼不能用字母级模糊/子串匹配：`kai` 中间的 `a`、`mian` 中间的 `a` 会被
/// 单字母查询命中，导致搜 `a` 出「开始」「桌面」这类杂讯。
/// 音节前缀匹配保证查询只能从音节边界开始，`k`/`ka`/`kai` 命中开，
/// 而 `a` 只命中阿、鞍这类首字母为 `a` 的字。
pub fn highlight_indices(
    entry: &MatchEntry,
    query: &str,
    matcher: &mut Matcher,
    scratch: &mut MatcherScratch,
) -> Vec<usize> {
    let query = query.trim();
    if query.is_empty() {
        return Vec::new();
    }

    let mut indices: Vec<u32> = Vec::new();
    let (name_buf, abbr_buf, _full_buf, query_buf) = scratch.buffers_mut();

    // 1. 原名（拉丁名，或直接输入汉字）
    let name_utf = Utf32Str::new(&entry.name, name_buf);
    let query_utf = Utf32Str::new(query, query_buf);
    if matcher
        .fuzzy_indices(name_utf, query_utf, &mut indices)
        .is_some()
    {
        return dedup_sorted(indices.into_iter().map(|i| i as usize));
    }

    // 2. 首字母
    if !entry.pinyin_abbr.is_empty() {
        let abbr_utf = Utf32Str::new(&entry.pinyin_abbr, abbr_buf);
        let query_utf = Utf32Str::new(query, query_buf);
        if matcher
            .fuzzy_indices(abbr_utf, query_utf, &mut indices)
            .is_some()
        {
            let mut out = Vec::with_capacity(indices.len());
            let mut last = usize::MAX;
            for i in indices {
                if let Some(name_idx) = entry.indices.name_idx_for_abbr(i as usize) {
                    if name_idx != last {
                        out.push(name_idx);
                        last = name_idx;
                    }
                }
            }
            return out;
        }
    }

    // 3. 全拼：**音节边界对齐**的前缀匹配
    if !entry.pinyin_full.is_empty() {
        if let Some((begin, finish)) = match_pinyin_readings(&entry.indices, &entry.pinyin_full, query)
        {
            return (begin..finish).collect();
        }
    }

    Vec::new()
}

/// 按名字字符聚合拼音读音，返回 `(name_idx, [读音...])`，按名字顺序排列。
///
/// `full_start/full_end` 是**字符**下标（不是字节下标）；多音字会为每个读音
/// 各产出一条 entry，因此这里按 `name_idx` 归并去重。
fn pinyin_readings(indices: &FieldIndices, full: &str) -> Vec<(usize, Vec<String>)> {
    let chars: Vec<char> = full.chars().collect();
    let mut groups: Vec<(usize, Vec<String>)> = Vec::new();
    for entry in &indices.entries {
        let begin = (entry.full_start as usize).min(chars.len());
        let finish = (entry.full_end as usize).min(chars.len());
        if begin >= finish {
            continue;
        }
        let syllable: String = chars[begin..finish].iter().collect();
        let name_idx = entry.name_idx as usize;
        if let Some(slot) = groups.iter_mut().find(|(idx, _)| *idx == name_idx) {
            if !slot.1.contains(&syllable) {
                slot.1.push(syllable);
            }
        } else {
            groups.push((name_idx, vec![syllable]));
        }
    }
    groups.sort_by_key(|(idx, _)| *idx);
    groups
}

/// 音节边界对齐的前缀匹配，返回命中的**名字字符**区间 `[begin, finish)`。
///
/// 查询串必须能被切成「完整读音 + 末尾可以是某个读音的前缀」的序列，
/// 且起始位置必须落在读音边界上：
///
/// * `k` / `ka` / `kai` → 开 ✓
/// * `kaish` / `kaishi` → 开始 ✓
/// * `a` → 开 ✗（`a` 不是 `kai` 的前缀，避免把韵母中间的字母当匹配目标）
/// * `aa` → 「开始」菜单 ✗（不存在以 `aa` 开头的读音序列）
///
/// 多音字会尝试每一种读音（如 `hang` / `xing`）。
fn match_pinyin_readings(
    indices: &FieldIndices,
    full: &str,
    query: &str,
) -> Option<(usize, usize)> {
    let groups = pinyin_readings(indices, full);
    if groups.is_empty() {
        return None;
    }
    let query: Vec<char> = query.chars().flat_map(|c| c.to_lowercase()).collect();
    if query.is_empty() {
        return None;
    }

    for start in 0..groups.len() {
        if let Some(finish) = match_from(&groups, start, &query) {
            return Some((groups[start].0, groups[finish - 1].0 + 1));
        }
    }
    None
}

/// 从第 `start` 个名字字符开始，尝试把查询串消费完。
/// 返回消耗到的名字字符下标（不含）。
fn match_from(groups: &[(usize, Vec<String>)], start: usize, query: &[char]) -> Option<usize> {
    let mut cursor = 0usize; // 查询串消费位置
    let mut index = start; // 当前名字字符下标
    while cursor < query.len() {
        if index >= groups.len() {
            return None;
        }
        let mut advanced = false;
        for reading in &groups[index].1 {
            let syllable: Vec<char> = reading.chars().collect();
            let remaining = query.len() - cursor;
            if remaining <= syllable.len() {
                // 查询剩余部分必须是当前读音的前缀（只能停在末尾读音的中间）
                if syllable.starts_with(&query[cursor..]) {
                    return Some(index + 1);
                }
            } else if query[cursor..cursor + syllable.len()] == syllable[..] {
                // 查询更长：当前读音必须被完整消费
                cursor += syllable.len();
                index += 1;
                advanced = true;
                break;
            }
        }
        if !advanced {
            return None;
        }
    }
    Some(index)
}

/// 是否匹配（用于过滤，内部复用 `highlight_indices`）
pub fn matches(entry: &MatchEntry, query: &str, matcher: &mut Matcher, scratch: &mut MatcherScratch) -> bool {
    !highlight_indices(entry, query, matcher, scratch).is_empty()
}

fn dedup_sorted(it: impl IntoIterator<Item = usize>) -> Vec<usize> {
    let mut out = Vec::new();
    let mut last = usize::MAX;
    for v in it {
        if v != last {
            out.push(v);
            last = v;
        }
    }
    out
}

/// 复用的匹配器与scratch 缓冲（避免每次搜索重新分配）
#[derive(Default)]
pub struct MatcherScratch {
    pub name_buf: Vec<char>,
    pub abbr_buf: Vec<char>,
    pub full_buf: Vec<char>,
    pub query_buf: Vec<char>,
}

impl MatcherScratch {
    pub fn buffers_mut(
        &mut self,
    ) -> (
        &mut Vec<char>,
        &mut Vec<char>,
        &mut Vec<char>,
        &mut Vec<char>,
    ) {
        (
            &mut self.name_buf,
            &mut self.abbr_buf,
            &mut self.full_buf,
            &mut self.query_buf,
        )
    }
}

/// 新建一个默认配置的模糊匹配器
pub fn new_matcher() -> Matcher {
    Matcher::new(Config::DEFAULT)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn highlighted(name: &str, query: &str) -> Vec<usize> {
        let entry = MatchEntry::new(name);
        let mut matcher = new_matcher();
        let mut scratch = MatcherScratch::default();
        let mut idxs = highlight_indices(&entry, query, &mut matcher, &mut scratch);
        idxs.sort_unstable();
        idxs
    }

    #[test]
    fn highlights_latin_substring() {
        assert_eq!(highlighted("qutebrowser", "qute"), vec![0, 1, 2, 3]);
    }

    #[test]
    fn highlights_pinyin_initials() {
        assert_eq!(highlighted("网易云音乐", "wyy"), vec![0, 1, 2]);
    }

    #[test]
    fn highlights_full_pinyin() {
        assert_eq!(highlighted("网易云音乐", "wangyi"), vec![0, 1]);
    }

    #[test]
    fn highlights_typed_characters() {
        assert_eq!(highlighted("网易云音乐", "音乐"), vec![3, 4]);
    }

    #[test]
    fn heteronym_matches_alternate_reading() {
        // 乐 默认 le，异读 yue
        assert!(!highlighted("网易云音乐", "yue").is_empty());
    }

    #[test]
    fn no_false_positive_on_pinyin_full() {
        // 「kaishi」caidan 中不存在以 aa 开头的音节序列，不应命中
        assert!(highlighted("「开始」菜单", "aa").is_empty());
    }

    #[test]
    fn single_letter_only_matches_syllable_start() {
        // 单字母 a 不应命中「开」(kai) 与「面」(mian) 中间的韵母
        assert!(highlighted("开始", "a").is_empty());
        assert!(highlighted("桌面", "a").is_empty());
        // 但应命中首字母为 a 的字
        assert_eq!(highlighted("阿里", "a"), vec![0]);
        assert_eq!(highlighted("鞍山", "a"), vec![0]);
    }

    #[test]
    fn syllable_prefix_matches() {
        assert_eq!(highlighted("开始", "k"), vec![0]);
        assert_eq!(highlighted("开始", "ka"), vec![0]);
        assert_eq!(highlighted("开始", "kai"), vec![0]);
        assert_eq!(highlighted("开始", "kaish"), vec![0, 1]);
        assert_eq!(highlighted("开始", "kaishi"), vec![0, 1]);
    }

    #[test]
    fn still_matches_full_pinyin_substring() {
        assert_eq!(highlighted("网易云音乐", "wangyi"), vec![0, 1]);
        assert_eq!(highlighted("网易云音乐", "wangy"), vec![0, 1]);
    }

    #[test]
    fn still_matches_full_pinyin_prefix() {
        assert!(!highlighted("网易云音乐", "wangyiyun").is_empty());
    }

    #[test]
    fn ascii_substring_still_matches() {
        // 纯 ASCII 名走原名模糊匹配（子序列），命中即可
        let idx = highlighted("AppData", "aa");
        assert!(idx.contains(&0), "got {idx:?}");
        assert!(idx.len() >= 2, "got {idx:?}");
    }

    #[test]
    fn spans_merge_adjacent_runs() {
        let spans = build_highlight_spans("网易云音乐", &[0, 1, 2]);
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0].text, "网易云");
        assert!(spans[0].is_match);
        assert_eq!(spans[1].text, "音乐");
        assert!(!spans[1].is_match);
    }

    #[test]
    fn ascii_names_map_identity() {
        let (full, abbr, idx) = pinyin_fields("7-Zip");
        assert_eq!(full, "7-zip");
        assert_eq!(abbr, "7-zip");
        assert!(idx.entries.is_empty());
        assert_eq!(idx.name_idx_for_abbr(3), Some(3));
    }
}
