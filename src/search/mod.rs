//! 拼音检索引擎（Phase 5，核心移植自 stools `core/matcher.rs`）。
//!
//! - `pinyin_fields`：为文件名预计算全拼（含异读字多读音）与首字母缩写。
//! - `matches_query` / `score`：查询对 name / 全拼 / 简拼做**连续**匹配，
//!   支持全拼（wangyiyun）、简拼（wyy）、原文汉字、大小写不敏感。
//!   **不支持「跳着凑」的模糊命中**：`ziyan` 只能命中连着的 `ziyan`，
//!   不许从 `Customi**z**ed … **i** … **y** … **a** … **n**` 里各取一个字母凑
//!   出来。需要放宽时由用户自己用空格分词（`m 4` 命中 `mp4`）。
//! - 纯 ASCII 名走快速路径：三字段相同，无反向表开销。

use nucleo_matcher::Matcher;
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
            // `full` 只收**首选读音**。
            //
            // 以前把每个字的全部读音首尾相接（吖 = `ya` + `a`、乐 = `le` + `yue`），
            // 于是 `吖静` 的全拼串是 `yaajingsizu` —— 用户照着读音打 `yajin`
            // （ya + 静的前半截）就再也切不出来：连续子串被夹在中间的那个 `a`
            // 打断了，而按字对齐那条路只认整音节（`jin` ≠ `jing`）。真机表现：
            // 搜 `yajin` 一片空白，搜 `yajing` 却能出来，用户没法理解。
            // 多音字的其它读音不靠这条扁平串，`pinyin_match` 会逐字按**任一读音**
            // 对齐整音节（`yinyue` 照样命中音乐）。
            let mut seen_abbr = std::collections::BTreeSet::new();
            let mut pushed = false;
            for py in multi {
                if !pushed {
                    full.push_str(py.plain());
                    pushed = true;
                }
                let letter = py.first_letter();
                if seen_abbr.insert(letter) {
                    if let Some(initial) = letter.chars().next() {
                        abbr.push(initial);
                    }
                }
            }
            if pushed {
                if collect_indices {
                    // 一个字的多个声母在 `abbr` 里是连续的，整段都算这个字的。
                    indices.push(FieldIndicesEntry {
                        name_idx: name_idx as u16,
                        abbr_start,
                        abbr_end: char_len(&abbr),
                        full_start,
                        full_end: char_len(&full),
                    });
                }
                handled = true;
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
    // `ü` → `v`：一个字符换一个字符，反向定位表的位置不受影响。
    let full = if full.contains('ü') {
        full.replace('ü', "v")
    } else {
        full
    };
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
/// 查询归一化：转小写，并把 `ü` 换成 `v`。
///
/// 拼音库给的是带两点的 `lü`，而中文用户几乎一律打 `lv`（`lüxing` 要打特殊
/// 字符）——不归一化就等于「旅行」搜不到。索引侧同样把 `ü` 存成 `v`（见
/// [`walk`]），两边一致才能命中。
pub fn normalize_query(q: &str) -> String {
    let lower = q.to_lowercase();
    if lower.contains('ü') {
        lower.replace('ü', "v")
    } else {
        lower
    }
}

/// 打分档位（越大越靠前）。数值与 pulse 的 `RankName` 同构：
/// 整串相等 > 前缀 > 词首 > 任意子串 > 整音节拼音 > 简拼。
///
/// 以前三个字段一律交给 nucleo 做**模糊**匹配：它允许顺着字符顺序跳着找，
/// 于是 `bengugu` 能命中「桥本雅(`ben`)…菇(`gu`)菇(`gu`)」这种跨字拼起来的
/// 长串 —— 满屏噪音，而真正同名的文件夹反而沉在后面甚至挤不进来。
/// 现在全是**连续**命中分档，跳着凑的路径整个删掉了（见模块头的说明）。
pub const SCORE_EXACT: u16 = 900;
pub const SCORE_PREFIX: u16 = 700;
pub const SCORE_WORD: u16 = 550;
pub const SCORE_CONTAINS: u16 = 420;
pub const SCORE_ABBR_EXACT: u16 = 340;
pub const SCORE_ABBR_PREFIX: u16 = 320;
pub const SCORE_ABBR_CONTAINS: u16 = 300;
/// 拼音**整音节**命中档：查询正好是整个字的读音拼起来的（`ziyan` = 紫+颜）。
///
/// 它必须高于「扁平拼音串里凑巧切到一段」（`子压你` 的 `ziyan|i`、`子様` 的
/// `ziyan|g`，都是 `SCORE_CONTAINS` 420）：字面一样长的一段拼音，跨整字的
/// 那个才是用户想找的。但整音节仍然低于原文命中（`SCORE_WORD` 550 以上）——
/// 打 `bengugu` 时真正叫 `bengugu` 的条目要压过拼音拼出来的一切。
const SCORE_PINYIN_EXACT: u16 = 620;
const SCORE_PINYIN_PREFIX: u16 = 600;
const SCORE_PINYIN_CONTAINS: u16 = 500;
/// 名字长短与「是不是文件夹」的加成（与 pulse 一致：文件夹 +40、短名优先）。
///
/// 同分时文件夹排前是用户在资源管理器 / pulse 里习惯的次序；短名优先让
/// 「Terminal」压过「Terminal_Server_Setup_v2.3.log」。
pub fn rank_bonus(is_dir: bool, name_chars: usize) -> u16 {
    let short = 24usize.saturating_sub(name_chars) as u16;
    (if is_dir { 40 } else { 0 }) + short
}

/// 命中位置是否落在「词首」：前一个字符不是字母数字（空格、`-`、`_`、`.`…）。
fn word_start(hay: &str, pos: usize) -> bool {
    hay[..pos]
        .chars()
        .next_back()
        .map_or(true, |c| !c.is_alphanumeric())
}

/// 原文/全拼字段的子串分档。`hay` 必须已经小写化（`full`/`abbr` 天然是）。
/// 前缀命中「正好断在分隔符上」的加分（仍远小于 `SCORE_EXACT`，不会串档）。
///
/// 拼音是扁平拼接的，`ziyan` 既能落在「紫颜」的 `ziyan` 上，也能落在
/// 「子様」（`ziyang`）的前五个字母上。两者在前缀档同分，只能靠名字长短
/// 决胜负，于是短名「子様」反而压过「紫颜」。命中后面紧跟分隔符（`-`、`_`、
/// `.`、空格）说明这是**完整音节**，给一点加分把它顶上去。
const SCORE_BOUNDARY: u16 = 30;

/// 命中（占 `len` 字符）之后是否正好是分隔符或串尾。
fn boundary_after(hay: &str, byte_end: usize) -> bool {
    match hay[byte_end..].chars().next() {
        None => true,
        Some(c) => !c.is_alphanumeric(),
    }
}

fn substring_tier(hay: &str, q: &str) -> u16 {
    let Some(pos) = hay.find(q) else {
        return 0;
    };
    if pos == 0 {
        if hay.len() == q.len() {
            SCORE_EXACT
        } else if boundary_after(hay, pos + q.len()) {
            SCORE_PREFIX + SCORE_BOUNDARY
        } else {
            SCORE_PREFIX
        }
    } else if word_start(hay, pos) {
        SCORE_WORD
    } else {
        SCORE_CONTAINS
    }
}

/// 简拼字段的档位：简拼是「首字母串」，没有词首/词中可言，整档压低。
fn abbr_tier(abbr: &str, q: &str) -> u16 {
    let Some(pos) = abbr.find(q) else {
        return 0;
    };
    if pos == 0 {
        if abbr.len() == q.len() {
            SCORE_ABBR_EXACT
        } else {
            SCORE_ABBR_PREFIX
        }
    } else {
        SCORE_ABBR_CONTAINS
    }
}

/// 拼音里的一段匹配落在名字的第几个字到第几个字（闭开）。
struct PinyinHit {
    start: usize,
    end: usize,
}

/// 逐字推进拼音匹配的「当前吃到查询的第几个字符」位图宽度上限。
/// 查询更长时直接放弃拼音这条路（那种长度基本只有字面命中才有意义）。
const PINYIN_MAX_QUERY: usize = 48;

/// 一个字符在拼音意义上的全部「读法」：汉字给它所有的读音，
/// 其它字符给「它自己」（小写、`ü`→`v`）。汉字额外给一次它本字，
/// 这样中英混排的查询（`音乐abc`）也能在这条路上走通。
fn for_each_reading(ch: char, f: &mut impl FnMut(&str)) {
    if let Some(multi) = ch.to_pinyin_multi() {
        for py in multi {
            f(py.plain());
        }
        let mut buf = [0u8; 4];
        f(ch.encode_utf8(&mut buf));
        return;
    }
    let folded = match ch {
        'A'..='Z' => ch.to_ascii_lowercase(),
        'ü' | 'Ü' => 'v',
        _ => ch,
    };
    let mut buf = [0u8; 4];
    f(folded.encode_utf8(&mut buf));
}

/// 查询从 `q[j..]` 起是否正好是读法 `r`；是则返回吃掉后的位置。
/// 一串字母出现过的字母集合（bit n = 第 n 个字母，小写后按 32 取模）。
fn letter_bits(s: &str) -> u32 {
    let mut m = 0u32;
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() {
            m |= 1u32 << ((b.to_ascii_lowercase() as u32) & 31);
        }
    }
    m
}

fn eat(q: &[char], j: usize, r: &str) -> Option<usize> {
    let mut k = j;
    for rc in r.chars() {
        if q.get(k) != Some(&rc) {
            return None;
        }
        k += 1;
    }
    Some(k)
}

/// 按字对齐的全拼子串匹配（多音字按「任一读音」算）。
///
/// 为什么不能只看 `fields.full` 做连续子串：`full` 是每个字所有读音首尾
/// 相接的一长串（乐 = `le` + `yue`），`yinyue`（音乐）会被中间那个 `le`
/// 打断而永远搜不到。这里逐字推进：每个汉字可以贡献它的任一读音，
/// 命中的是一段**连续的字**（长度不固定）。
fn pinyin_match(name: &str, query: &str, full: &str) -> Option<PinyinHit> {
    if query.is_empty() || !query.is_ascii() {
        // 非 ASCII（汉字）查询只能命中原文，走不到拼音这条路。
        return None;
    }
    let n = query.chars().count();
    if n > PINYIN_MAX_QUERY {
        return None;
    }
    // 空字段 = 调用方还没算拼音（`PinyinFields::default()`，例如拼音缓存还没
    // 到手）。此时只能走纯文本匹配，**绝不能在这里现算**：本函数每次重建都要
    // 对每个条目跑一遍，现算等于把后台缓存的意义整个抹掉（大目录逐键卡死）。
    // 以前这条约束是靠「空 `full` 让一票否决必挂」偶然成立的，现在写明白。
    if full.is_empty() {
        return None;
    }
    // 便宜的一票否决：查询里的字母必须在这条名字的拼音里出现过。
    // 绝大多数不相关条目在这里就被挡掉，逐字推进（和它的查询数组）根本不用建。
    let mut want = 0u32;
    for b in query.bytes() {
        if b.is_ascii_alphanumeric() {
            want |= 1u32 << ((b as u32) & 31);
        }
    }
    // 字母表必须把**所有读音**都算上：`full` 只留首选读音（见 `walk`），
    // 拿它一票否决会把 `乐`→`yue` 这类非首选读音直接挡死。
    let mut have = letter_bits(full);
    if !name.is_ascii() {
        for ch in name.chars() {
            if ch.is_ascii() {
                // 单字符的位图直接算，别为每个字母分配一个 `String`
                // （深搜里每条名字都要走这里）。
                let b = ch.to_ascii_lowercase() as u32;
                if (b as u8).is_ascii_alphanumeric() {
                    have |= 1u32 << (b & 31);
                }
            } else {
                for_each_reading(ch, &mut |r| have |= letter_bits(r));
            }
            if want & !have == 0 {
                break;
            }
        }
    }
    if want & !have != 0 {
        return None;
    }
    let q: Vec<char> = query.chars().collect();
    let q = &q[..];

    let full_bit: u64 = 1 << n;
    // 任意起点：bit0 永远置位（= 还没开始，随时可以从当前字起新的一段）。
    let mut any_reach: u64 = 1;
    let mut any_start = [0u16; PINYIN_MAX_QUERY + 1];
    // 前缀：只允许从名字的第一个字开始。
    let mut prefix_reach: u64 = 1;
    let name_chars = name.chars().count();
    let mut hit: Option<(usize, usize, u64)> = None; // (start, end, 命中的位)

    for (i, ch) in name.chars().enumerate() {
        let mut next_any: u64 = 1;
        let mut next_start = [0u16; PINYIN_MAX_QUERY + 1];
        next_start[0] = (i + 1) as u16;
        for_each_reading(ch, &mut |r| {
            let mut bits = any_reach;
            while bits != 0 {
                let j = bits.trailing_zeros() as usize;
                bits &= bits - 1;
                if let Some(k) = eat(q, j, r) {
                    if k <= PINYIN_MAX_QUERY {
                        next_any |= 1 << k;
                        next_start[k] = if j == 0 { i as u16 } else { any_start[j] };
                    }
                }
            }
        });
        any_reach = next_any;
        any_start = next_start;

        let mut next_prefix: u64 = 0;
        if prefix_reach != 0 {
            for_each_reading(ch, &mut |r| {
                let mut bits = prefix_reach;
                while bits != 0 {
                    let j = bits.trailing_zeros() as usize;
                    bits &= bits - 1;
                    if let Some(k) = eat(q, j, r) {
                        if k <= PINYIN_MAX_QUERY {
                            next_prefix |= 1 << k;
                        }
                    }
                }
            });
        }
        prefix_reach = next_prefix;

        if hit.is_none() && any_reach & full_bit != 0 {
            hit = Some((any_start[n] as usize, i + 1, full_bit));
        }
        if prefix_reach & full_bit != 0 {
            // 从第一个字一路吃满：就是前缀命中，优先返回它。
            return Some(PinyinHit {
                start: 0,
                end: i + 1,
            });
        }
        let _ = name_chars;
    }
    hit.map(|(start, end, _)| PinyinHit { start, end })
}

/// 拼音命中分档：整串吃完 > 从第一个字开始 > 中间某段。
fn pinyin_tier(name: &str, q: &str, full: &str) -> u16 {
    let Some(hit) = pinyin_match(name, q, full) else {
        return 0;
    };
    let total = name.chars().count();
    if hit.start == 0 {
        if hit.end >= total {
            SCORE_PINYIN_EXACT
        } else {
            SCORE_PINYIN_PREFIX
        }
    } else {
        SCORE_PINYIN_CONTAINS
    }
}

/// 查询里带非 ASCII（汉字）时只能命中原文：拼音字段里没有汉字。
fn literal_tier(name: &str, q: &str) -> u16 {
    // 深搜每一条名字都要过这里。绝大多数名字里既没有大写字母也没有 `ü`，
    // 那种情况 `to_lowercase()` 只是白分配一份字符串（整盘五十万条 = 五十万次
    // 分配），直接拿原文匹配。
    let needs_fold = name.chars().any(|c| c.is_uppercase()) || name.contains('ü');
    if !needs_fold {
        return substring_tier(name, q);
    }
    let lower = name.to_lowercase();
    if lower.contains('ü') {
        let folded = lower.replace('ü', "v");
        substring_tier(&folded, q)
    } else {
        substring_tier(&lower, q)
    }
}

pub fn matches_query(
    name: &str,
    fields: &PinyinFields,
    query_lower: &str,
    _matcher: &mut Matcher,
    _bufs: &mut (Vec<char>, Vec<char>, Vec<char>),
) -> bool {
    score_parts(name, fields.abbr_str(), &fields.full, query_lower, _matcher, _bufs).is_some()
}

/// 打分（越大越好）：用于结果排序。None = 不匹配。
///
/// 生产代码请走 `score_ranked`（带文件夹/短名加权）；这个不带加权的版本只有
/// 测试在用，用来单独验证档位本身。
#[cfg(test)]
pub fn score(
    name: &str,
    fields: &PinyinFields,
    query_lower: &str,
    _matcher: &mut Matcher,
    _bufs: &mut (Vec<char>, Vec<char>, Vec<char>),
) -> Option<u16> {
    score_parts(name, fields.abbr_str(), &fields.full, query_lower, _matcher, _bufs)
}

/// 同上，但拼音字段是**借用**的切片。
///
/// 全局索引现在是映射在磁盘文件上的：名字和拼音都在映射区里，拿不到
/// `PinyinFields`（那是 `Box<str>` 所有权类型）。为此多开一个纯借用版本，
/// 避免为了打一次分就把每条记录的名字和拼音复制一遍。
///
/// 顺序：原文/全拼/简拼的连续子串分档（带空格时按词分别判定，见
/// [`multi_word_tier`]）。ASCII 查询在 `full`
/// 里就能命中（ASCII 名的 `full` 就是小写后的原文，中文名的 `full` 里含
/// 原有的拉丁片段），所以快路径不需要为每个条目再小写化一次原文。
pub fn score_parts(
    name: &str,
    abbr: &str,
    full: &str,
    query_lower: &str,
    _matcher: &mut Matcher,
    _bufs: &mut (Vec<char>, Vec<char>, Vec<char>),
) -> Option<u16> {
    let q = normalize_query(query_lower);
    if q.is_empty() {
        return None;
    }
    // 带空格的查询按「每个词都得连续出现」处理（AND）。用户自己加空格是唯一
    // 放宽匹配的方式：打 `m 4` 能命中 `mp4`，因为 `m` 和 `4` 各自连续出现；
    // 而打 `ziyan` 不会命中那些只是「字符顺序对得上、位置全散着」的名字。
    let mut words = q.split_whitespace();
    let first = words.next()?;
    if words.next().is_some() {
        return multi_word_tier(name, abbr, full, &q);
    }
    let tier = word_tier(name, abbr, full, first);
    (tier > 0).then_some(tier)
}

/// 单个词的档位（0 = 不匹配）。**只认连续命中**，拼音是唯一允许「跨字」的地方
/// （整音节按字对齐，见 [`pinyin_tier`]）。
fn word_tier(name: &str, abbr: &str, full: &str, q: &str) -> u16 {
    // 1) 字面：ASCII 查询在 `full` 里就能命中（ASCII 名的 `full` 就是小写后的
    //    原文，中文名的 `full` 里含原有的拉丁片段），这样不必为每个条目再
    //    小写化一次原文。前缀及以上的档位就是它，不再往下比。
    let literal = if q.is_ascii() {
        substring_tier(full, q)
    } else {
        literal_tier(name, q)
    };
    if literal >= SCORE_PINYIN_EXACT {
        return literal;
    }
    // 2) 逐字拼音（整音节，多音字按任一读音）。**必须排在「扁平串子串」之前
    //    判**：`ziyan` 在「紫颜」里是两个字整段拼出来的，在「子压你」「子様」
    //    里只是切在半个音节上的一截，前者该排在前面。
    //
    //    纯 ASCII 名字不走这条路：那里的「音节」就是单个字母，对齐不对齐没
    //    意义，而字面档已经分出前缀/词首/子串了。
    let pinyin_score = if name.is_ascii() {
        0
    } else {
        pinyin_tier(name, q, full)
    };
    if pinyin_score > literal {
        return pinyin_score;
    }
    if literal > 0 {
        return literal;
    }
    // 3) 简拼（首字母串）。
    abbr_tier(abbr, q)
}

/// 多词查询：每个词都必须连续命中，取**最弱**的那一档做整体分
/// （有一个词只是勉强命中，整条就不该排前面）。
fn multi_word_tier(name: &str, abbr: &str, full: &str, q: &str) -> Option<u16> {
    let mut worst = u16::MAX;
    let mut count = 0usize;
    for word in q.split_whitespace() {
        let tier = word_tier(name, abbr, full, word);
        if tier == 0 {
            return None;
        }
        worst = worst.min(tier);
        count += 1;
    }
    (count > 0).then_some(worst)
}

/// 打分 + 排序加权（文件夹 +40、名字短优先）。
///
/// **搜索结果只该走这个入口**：主窗口列表、吸附窗、全局索引共用一套加权。
/// 以前每个调用点自己 `score(...) + rank_bonus(...)`，漏掉一处就会出现「同一
/// 批结果里短名字反而排在长名字后面」这种说不清的次序。
pub fn score_parts_ranked(
    name: &str,
    abbr: &str,
    full: &str,
    query_lower: &str,
    matcher: &mut Matcher,
    bufs: &mut (Vec<char>, Vec<char>, Vec<char>),
    is_dir: bool,
) -> Option<u16> {
    score_parts(name, abbr, full, query_lower, matcher, bufs)
        .map(|s| s + rank_bonus(is_dir, name.chars().count()))
}

/// 同上，拼音字段是带所有权的那份（界面侧 / 吸附窗用）。
pub fn score_ranked(
    name: &str,
    fields: &PinyinFields,
    query_lower: &str,
    matcher: &mut Matcher,
    bufs: &mut (Vec<char>, Vec<char>, Vec<char>),
    is_dir: bool,
) -> Option<u16> {
    score_parts_ranked(
        name,
        fields.abbr_str(),
        &fields.full,
        query_lower,
        matcher,
        bufs,
        is_dir,
    )
}

/// 在原文里找查询（大小写 / `ü` 折叠后按字节找），返回 (起始 char, char 数)。
fn find_in_name(name: &str, q: &str) -> Option<(usize, usize)> {
    let lower = name.to_lowercase();
    let hay = if lower.contains('ü') {
        lower.replace('ü', "v")
    } else {
        lower
    };
    let byte = hay.find(q)?;
    Some((hay[..byte].chars().count(), q.chars().count()))
}

/// 命中高亮:返回 name 中需要高亮的 char 索引区间(闭开,升序不重叠)。
///
/// 先按原文子串定位（打分用的就是这条路），再试简拼 / 全拼的子串，
/// 都不中就不高亮 —— 与打分顺序一致，否则会出现「排在前面却不高亮」。
pub fn highlight_ranges(
    name: &str,
    fields: &PinyinFields,
    query_lower: &str,
    _matcher: &mut Matcher,
    _bufs: &mut (Vec<char>, Vec<char>, Vec<char>),
) -> Vec<(usize, usize)> {
    let query_owned = normalize_query(query_lower);
    let total = name.chars().count();
    if query_owned.is_empty() || total == 0 {
        return Vec::new();
    }
    // 多词查询：每个词单独定位，高亮取并集（与打分的「每词都要连续命中」对齐）。
    if query_owned.split_whitespace().count() > 1 {
        let mut indices: Vec<usize> = Vec::new();
        for word in query_owned.split_whitespace() {
            for (start, end) in token_ranges(name, fields, word, total) {
                indices.extend(start..end);
            }
        }
        return merge_ranges(indices, total);
    }
    token_ranges(name, fields, &query_owned, total)
}

/// 单个词的命中区间（连续命中优先，拼音图省事按整音节推进）。
fn token_ranges(
    name: &str,
    fields: &PinyinFields,
    query_owned: &str,
    total: usize,
) -> Vec<(usize, usize)> {

    // 1) 原文命中（大小写不敏感）。
    if let Some((start, len)) = find_in_name(name, query_owned) {
        return merge_ranges((start..start + len).collect(), total);
    }
    // 2) 全拼里的子串。**这一条必须排在逐字推进之前**：打分的第一个非零档
    //    就是「`full` 里有这段」，而 `full` 是所有读音首尾相接的扁平串，
    //    命中可以停在某个字的中间（`leiyin` 命中「雷颖 leiying」的前 6 个字母）。
    //    以前这里没有这一条，于是会出现「分数排第一、名字却一个字都不高亮」。
    for (field, is_abbr) in [(fields.abbr_str(), true), (&*fields.full, false)] {
        let Some(byte) = field.find(query_owned) else {
            continue;
        };
        // `find` 给的是**字节**偏移，而定位表是按**字符**位置建的。名字里只要有
        // 多字节的非汉字符号（`“”···《》，`）两者就不相等，映射会整体后移，
        // 高亮的字跟着错位（实测 `ziyan-` 在「…“她们3个…”@紫颜-小仙紫…」里
        // 高亮到了 `紫颜-小`）。先换成字符偏移。
        let pos = field[..byte].chars().count();
        let table = indices_for(name);
        let len = query_owned.chars().count();
        let mapped: Vec<usize> = (pos..pos + len)
            .filter_map(|i| lookup_indices(&table, i, is_abbr))
            .collect();
        if !mapped.is_empty() {
            return merge_ranges(mapped, total);
        }
    }
    // 3) 逐字推进的拼音（多音字：`yinyue` 要能跨过「乐」的另一个读音）。
    if let Some(hit) = pinyin_match(name, query_owned, &fields.full) {
        return merge_ranges((hit.start..hit.end).collect(), total);
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
        assert!(f.full.starts_with("wangyiyun"), "full={}", f.full);
        assert!(f.abbr.starts_with("wy"), "abbr={}", f.abbr);
    }

    /// 多音字：`full` 只存首选读音，其余读音必须照样能搜到（按字对齐整音节）。
    ///
    /// 真机 bug：`吖` 有 `ya` / `a` 两个读音，旧的全拼串是 `yaajingsizu`，
    /// 用户打 `yajin`（`ya` + 静的前半截）切不出来，搜 `yajing` 才出来。
    #[test]
    fn heteronym_readings_still_match() {
        assert!(matched("网易云音乐", "yinyue"), "音乐的「乐」读 yue");
        assert!(matched("网易云音乐", "yunyinyue"), "整音节对齐可以跨字");
        assert!(matched("吖静sizu", "yajin"), "ya + 静的前半截");
        assert!(matched("吖静sizu", "yajing"));
        assert!(matched("吖静sizu", "ajing"), "非首选读音也还在");
        // 一票否决的字母表也必须覆盖非首选读音：`乐` 的首选是 le。
        assert!(matched("乐", "yue"));
        assert!(matched("音乐", "yinyue"));
        // 半截音节仍然只是「子串档」，压不过整音节对齐。
        let f = pinyin_fields("吖静sizu");
        let mut m = Matcher::new(nucleo_matcher::Config::DEFAULT);
        let mut b = (Vec::new(), Vec::new(), Vec::new());
        let half = score("吖静sizu", &f, "yajin", &mut m, &mut b).unwrap();
        let whole = score("吖静sizu", &f, "yajing", &mut m, &mut b).unwrap();
        assert_eq!(half, whole, "现在两条都是前缀档（以前一整条根本搜不到）");
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

    /// 用户报的真事：搜 `ziyan`，命中 `S【Customized】…Victoria-Say my name…`。
    /// 那种命中是模糊匹配顺着字符顺序**跳着**凑出来的（`z` 取自 Customized、
    /// `i`/`y`/`a`/`n` 散在后面），名字里根本没有连着的 `ziyan`。
    #[test]
    fn scattered_characters_do_not_match() {
        let noise = "S【Customized】“559”Victoria-Say my name（6June） Vertical.mp4";
        assert!(!matched(noise, "ziyan"), "跳着凑出来的命中不该算");
        // 连着的才认。
        assert!(matched("紫颜-小仙紫.mp4", "ziyan"));
        // 漏字的错字也不认了（以前 nucleo 会容错）。
        assert!(!matched("Documents", "Documnts"));
    }

    /// 用户自己用空格分词就是「放宽匹配」的唯一方式：`m 4` 命中 `mp4`，
    /// 因为每个词都**连续**出现（分别落在不同位置），而不是跳着凑。
    #[test]
    fn space_separated_words_match_independently() {
        assert!(matched("video.mp4", "m 4"));
        assert!(matched("视频合集 2024年03月.mp4", "合集 03"));
        assert!(!matched("video.mp4", "m 9"), "有一个词不出现就不算命中");
        // 每个词都得连着自己出现一次（单字母词天然连续，所以 `z y n` 是允许的
        // ——这正是用户想保留的用法），但词不出现就整条不算命中。
        assert!(matched("Customized Victoria my name.mp4", "z y n"));
        assert!(!matched("Customized Victoria my name.mp4", "z y q"));
    }

    /// 多词命中的高亮要覆盖每个词（打分说命中，高亮就得亮出来）。
    #[test]
    fn multi_word_query_highlights_every_word() {
        let p = parts("video.mp4", "m 4");
        let hl: String = p.iter().filter(|(_, h)| *h).map(|(t, _)| t.as_str()).collect();
        assert_eq!(hl, "m4");
    }

    /// 多词命中取最弱的一档：有一个词只是勉强命中，整体就不该排前面。
    #[test]
    fn multi_word_takes_the_weakest_tier() {
        let f = pinyin_fields("abc-xyz.mp4");
        let bufs = &mut (Vec::new(), Vec::new(), Vec::new());
        let prefix = score("abc-xyz.mp4", &f, "abc xyz", &mut m(), bufs).unwrap();
        assert!(prefix > SCORE_ABBR_EXACT, "两个词都是词首/前缀：{prefix}");
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

    /// 以前这里依赖模糊匹配给 `Firefox` / `fx` 高亮出两个离散字符。现在
    /// 「跳着凑」整条路径都没了：`fx` 不再命中 `Firefox`（也不高亮）。
    #[test]
    fn discrete_characters_are_not_highlighted() {
        assert!(!matched("Firefox", "fx"));
        let p = parts("Firefox", "fx");
        assert!(p.iter().all(|(_, h)| !*h), "不该亮任何一个字");
    }

    /// 用户报的真事：在视频文件夹里搜 `bengugu`，满屏 mp4 噪音，
    /// 而真正同名的子文件夹被挤没了。
    ///
    /// 噪音是模糊匹配顺着全拼跳字拼出来的：`ben` 来自「本」、两个 `gu`
    /// 来自两个「菇」。子串匹配不会跨字拼凑，这一整类噪音直接消失。
    #[test]
    fn pinyin_noise_does_not_match() {
        let noise = "一只芸喵喵,桥本雅Miyabi-菇菇哩-,二心x-在幻梦中守护你~绝美.mp4";
        assert!(
            !matched(noise, "bengugu"),
            "跨字拼出来的拼音不该命中"
        );
        assert!(matched("bengugu", "bengugu"));
        assert!(matched("Bengugu-Genshin_Impact.mp4", "bengugu"));
    }

    #[test]
    fn score_tiers_are_ordered() {
        let bufs = &mut (Vec::new(), Vec::new(), Vec::new());
        let mut m = m();
        let mut sc = |name: &str, q: &str| {
            let f = pinyin_fields(name);
            score(name, &f, q, &mut m, bufs).unwrap()
        };
        let exact = sc("bengugu", "bengugu");
        // 前缀命中后面紧跟分隔符（完整音节）多 30 分。
        let prefix = sc("bengugu-4k.mp4", "bengugu");
        // 前缀命中但后面还连着字母（半截音节），不加分。
        let prefix_glued = sc("benguguy.mp4", "bengugu");
        let word = sc("video-bengugu.mp4", "bengugu");
        let contains = sc("xbenguguy.mp4", "bengugu");
        // 拼音整音节（紫颜 = zi+yan，两个字）和扁平淡到半个音节（子压你 = `ziyan|i`）。
        let py_contains = sc("等春天过去-合肥银泰线下紫颜表演.mp4", "ziyan");
        let half = sc("大奶子压你身上.mp4", "ziyan");

        assert!(exact > prefix, "{exact} vs {prefix}");
        assert!(prefix > prefix_glued, "{prefix} vs {prefix_glued}");
        assert!(prefix_glued > word, "{prefix_glued} vs {word}");
        assert!(word > py_contains, "{word} vs {py_contains}");
        assert!(py_contains > contains, "{py_contains} vs {contains}");
        assert!(contains > SCORE_ABBR_EXACT, "字面 >= 简拼");
        assert_eq!(half, contains, "半个音节的就是普通子串档");
        assert!(py_contains > half, "整音节必须压过半个音节");
    }

    /// 多音字：`full` 里读音是首尾相接的（乐 = le + yue），
    /// 连续子串匹配会被打断，逐字推进才不会。
    #[test]
    fn pinyin_spans_heteronyms() {
        assert!(matched("网易云音乐", "yinyue"));
        assert!(!matched("网易云音乐", "yinyuejishu"));
        assert_eq!(score_tiers_cmp("网易云音乐", "yinyue") > 0, true);
    }

    fn score_tiers_cmp(name: &str, q: &str) -> u16 {
        let f = pinyin_fields(name);
        let bufs = &mut (Vec::new(), Vec::new(), Vec::new());
        score(name, &f, q, &mut m(), bufs).unwrap_or(0)
    }

    /// 打分走了「全拼扁平串的子串」这条路时，高亮也必须能标出来。
    /// `leiyin` 命中「雷颖(`leiying`)」是停在字中间的前缀命中。
    #[test]
    fn highlight_matches_flat_pinyin_prefix() {
        let p = parts("雷颖_1.mp4", "leiyin");
        let hl: String = p.iter().filter(|(_, h)| *h).map(|(t, _)| t.as_str()).collect();
        assert_eq!(hl, "雷颖", "全拼前缀命中没有高亮：{p:?}");
    }

    /// 名字里的多字节符号会让高亮错位。
    ///
    /// `field.find()` 给的是字节偏移，定位表是按字符位置建的：名字里带
    /// `“”···@` 这类多字节符号时，映射整体后移，实测搜 `ziyan-` 会亮到
    /// `紫颜-小`（多亮一个「小」）或者只亮 `紫颜`（漏掉「-」）。
    #[test]
    fn highlight_flat_pinyin_after_multibyte_symbols() {
        let cases = [
            (
                "晓丹小仙女儿-“她们3个就这样在我心里转圈圈...”@紫颜-小仙紫 @顾三73.mp4",
                "紫颜-",
            ),
            (
                "顾三73,晓丹小仙女儿,紫颜-小仙紫恋心|玥现世电台-小仙女能有什么坏心思？只想陪你过节啦.mp4",
                "紫颜-",
            ),
            ("夜喵w,十元酱少女,晓丹小仙女儿,紫颜-小仙紫,姜生花.mp4", "紫颜-"),
            (
                "乌镇bilibili次元千杯舞乐会——《寄明月》紫颜-小仙紫&晓丹小仙女儿.mp4",
                "紫颜-",
            ),
        ];
        for (name, want) in cases {
            let p = parts(name, "ziyan-");
            let hl: String = p.iter().filter(|(_, h)| *h).map(|(t, _)| t.as_str()).collect();
            assert_eq!(hl, want, "{name:?} 的高亮错位：{p:?}");
        }
    }

    /// 真机那张列表：搜 `ziyan`，名字里**含「紫颜」**的条目被 `子压你`、
    /// `子様` 这类「拼音只凑到半截」的条目挤到后面。
    ///
    /// 现在含「紫颜」的走拼音整音节档（500，或词首 550），`子压你`（`ziyan|i`）、
    /// `子様`（`ziyan|g`）停在字面子串档（420）。
    #[test]
    fn aligned_pinyin_beats_half_syllable_in_real_list() {
        let ziyan_rows = [
            "顾三73,晓丹小仙女儿,紫颜-小仙紫-“勾魂摄魄而已... 公子，可要当心了”.mp4",
            "等春天过去-合肥银泰线下紫颜表演.mp4",
            "打BUG的杰-终于看到紫颜老师的现场舞蹈啦，还是我喜欢的《落花情》.mp4",
        ];
        let half_rows = [
            "2048.cc-大奶子压你身上.mp4",
            "#NIKKE #NIKKE夏2026 王子様、こっちに見てください 🙄 [esrXGP_OI2A].webm",
            "どう？ 王子様はどっちが好み？ ✨#NIKKE#NIKKE夏2026 [YL3BozmHAgw].webm",
            "踊りを楽しみましょう、王子様。 🙂✨#NIKKE#NIKKE夏2026 [IdDWj_z9ny0].webm",
        ];
        let mut m = m();
        let mut scored: Vec<(u16, &str)> = Vec::new();
        for name in ziyan_rows.iter().chain(half_rows.iter()) {
            let f = pinyin_fields(name);
            let bufs = &mut (Vec::new(), Vec::new(), Vec::new());
            let s = score_ranked(name, &f, "ziyan", &mut m, bufs, false).unwrap_or(0);
            scored.push((s, name));
        }
        let min_ziyan = ziyan_rows
            .iter()
            .map(|n| scored.iter().find(|(_, x)| x == n).unwrap().0)
            .min()
            .unwrap();
        let max_half = half_rows
            .iter()
            .map(|n| scored.iter().find(|(_, x)| x == n).unwrap().0)
            .max()
            .unwrap();
        assert!(
            min_ziyan > max_half,
            "含「紫颜」的最低分 {min_ziyan} 应当高于拼音半截的最高分 {max_half}"
        );
    }

    /// 字面档的「免小写化」快路径不能改变结果（大小写、ü≡v 都要照旧）。
    #[test]
    fn literal_tier_fast_path_matches_slow_path() {
        let cases = [
            ("紫颜-小仙紫.mp4", "紫颜"),
            ("Video-Bengugu.mp4", "bengugu"),
            ("Über-übel.mp4", "vber"),
            ("über.mp4", "uber"),
            ("plain-lower.mp4", "plain"),
        ];
        for (name, q) in cases {
            let lower = name.to_lowercase();
            let folded = lower.replace('ü', "v");
            assert_eq!(
                literal_tier(name, q),
                substring_tier(&folded, q),
                "{name} / {q}"
            );
        }
    }

    /// 拼音前缀命中要区分「完整音节」和「切在半个音节上」。
    ///
    /// 真机：搜 `ziyan` 时「子様」（`ziyang`）排到了「紫颜」（`ziyan-…`）前面——
    /// 两者同在前缀档（700），只能靠名字长短决胜负，短名就赢了。现在命中后面
    /// 紧跟分隔符的（完整音节）多 30 分，`紫颜` 稳定在 `子様` 之前。
    #[test]
    fn prefix_ending_on_boundary_beats_mid_syllable() {
        let sc = |n: &str, q: &str| score_tiers_cmp(n, q);
        let ziyan_short = sc("紫颜-小仙紫.mp4", "ziyan");
        let ziyan_long = sc(
            "紫颜-小仙紫,姜生花_破晓序列手游-今时今日，神仙当OL也是要加班的.mp4",
            "ziyan",
        );
        let ziyang = sc("子様.mp4", "ziyan");
        assert!(ziyan_short > ziyang, "紫颜 {ziyan_short} 应高于 子様 {ziyang}");
        assert!(ziyan_long > ziyang, "长名紫颜 {ziyan_long} 也要高于 子様 {ziyang}");
        // 断在半个音节上的仍然是前缀档（只是没加分），不该掉到词首/子串档。
        assert!(ziyang >= SCORE_PREFIX, "子様 仍在 {ziyang} 前缀档");
        // 精确命中（整串相等）依然最高，加分不会串档。
        assert!(sc("ziyan", "ziyan") > SCORE_PREFIX + SCORE_BOUNDARY);
        // 带扩展名的照样是「分隔符边界」加分，710 档不到 900 档。
        assert_eq!(sc("ziyan.mp4", "ziyan"), SCORE_PREFIX + SCORE_BOUNDARY);
    }

    /// 不变量：**能命中就必须能高亮**。
    ///
    /// 打分和高亮是两条独立实现，一旦某条打分路径在高亮里没有对应分支，
    /// 用户看到的就是「排第一却没有一个字是亮的」。
    #[test]
    fn every_hit_path_has_highlight() {
        let names = [
            "雷颖_1.mp4",
            "雷音_1.mp4",
            "雷颖.mp4",
            "bengugu",
            "Bengugu-Genshin_Impact_Inazuma_Boom_4K.mp4",
            "一只芸喵喵,桥本雅Miyabi-菇菇哩-,二心x-在幻梦中守护你~绝美.mp4",
            "北北miya-难以忘怀的白月光-You're Pitiful.mp4",
            "FirefoxSetup.exe",
            "网易云音乐播放器.exe",
            "其他素材-0-1.mp4",
            "Documents.docx",
            "吖静sizu",
        ];
        let queries = [
            "leiyin", "lei", "bengugu", "BENGU", "miya", "fire", "wyy", "yinyue",
            "音乐", "雷", "mp4", "素材", "documnts", "setup",
            // 多音字后面的半截音节（真机 bug：yajin 搜不到 吖静sizu）。
            "yajin", "yajing", "ajing",
        ];
        let mut m = m();
        let bufs = &mut (Vec::new(), Vec::new(), Vec::new());
        for name in names {
            let f = pinyin_fields(name);
            for q in queries {
                let Some(score) = score(name, &f, q, &mut m, bufs) else {
                    continue;
                };
                let ranges =
                    highlight_ranges(name, &f, q, &mut m, &mut (Vec::new(), Vec::new(), Vec::new()));
                assert!(
                    !ranges.is_empty(),
                    "{name:?} 命中 {q:?}（分数 {score}）却没有高亮区间"
                );
            }
        }
    }

    #[test]
    fn dir_name_beats_file_on_tie() {
        // 同分时文件夹赢：靠的是 rank_bonus，而不是比较名字。
        let q = "bengugu";
        let bufs = &mut (Vec::new(), Vec::new(), Vec::new());
        let f = pinyin_fields("bengugu");
        let base = score("bengugu", &f, q, &mut m(), bufs).unwrap();
        let dir = base + rank_bonus(true, 7);
        let file = base + rank_bonus(false, 7);
        assert!(dir > file);
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
