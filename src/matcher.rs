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

/// 命中的名字字符下标（高亮层用）。按 原名 -> 首字母 -> 全拼 依次尝试。
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
    let (name_buf, abbr_buf, full_buf, query_buf) = scratch.buffers_mut();

    // 1. 原名（拉丁名，或直接输入汉字）
    let name_utf = Utf32Str::new(&entry.name, name_buf);
    let query_utf = Utf32Str::new(query, query_buf);
    if matcher.fuzzy_indices(name_utf, query_utf, &mut indices).is_some() {
        return dedup_sorted(indices.into_iter().map(|i| i as usize));
    }

    // 2. 首字母
    if !entry.pinyin_abbr.is_empty() {
        let abbr_utf = Utf32Str::new(&entry.pinyin_abbr, abbr_buf);
        let query_utf = Utf32Str::new(query, query_buf);
        if matcher.fuzzy_indices(abbr_utf, query_utf, &mut indices).is_some() {
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

    // 3. 全拼（异读字多个音节均可命中，映射后去重）
    if !entry.pinyin_full.is_empty() {
        let full_utf = Utf32Str::new(&entry.pinyin_full, full_buf);
        let query_utf = Utf32Str::new(query, query_buf);
        if matcher.fuzzy_indices(full_utf, query_utf, &mut indices).is_some() {
            let mut out = Vec::with_capacity(indices.len());
            let mut last = usize::MAX;
            for i in indices {
                if let Some(name_idx) = entry.indices.name_idx_for_full(i as usize) {
                    if name_idx != last {
                        out.push(name_idx);
                        last = name_idx;
                    }
                }
            }
            return out;
        }
    }

    Vec::new()
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
