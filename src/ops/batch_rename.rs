//! 批量重命名：规则计算、校验、冲突检测与执行。
//!
//! 语义对齐 PowerToys 的 PowerRename，但只保留「搜索 / 替换 / 正则 / 应用于」这一支：
//!
//! - 未勾选「使用正则表达式」时，搜索串按**字面文本**处理，替换串也按字面写回
//!   （`$1` 不会被展开）；勾选后走正则，替换串支持 `$1` / `${name}` / `$$`。
//!   两种模式共用同一台匹配引擎（字面模式对搜索串做 `regex::escape`），因此
//!   大小写不敏感、命中区间等行为天然一致，也不可能出现「字面模式算出来的高亮
//!   和实际替换位置不一致」这类分裂。
//! - 未勾选「匹配所有出现的对象」时只替换第一处。
//! - 未勾选「区分大小写」时按 Unicode 不区分大小写匹配。
//! - 目录没有「扩展名」概念，整名当文件名处理。
//!
//! 预览阶段**不碰文件系统**：是否「目标已存在」通过调用方传入的「目录现有名字
//! 集合」判断（界面线程上做磁盘查询会在网络盘上卡住整个窗口）。

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use regex::{Regex, RegexBuilder};

/// 替换作用在名字的哪一部分。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ApplyTo {
    NameAndExt,
    NameOnly,
    ExtOnly,
}

impl ApplyTo {
    pub fn from_index(i: i32) -> Self {
        match i {
            1 => Self::NameOnly,
            2 => Self::ExtOnly,
            _ => Self::NameAndExt,
        }
    }

    /// 反向：给界面下拉框用的下标。
    pub fn to_index(self) -> i32 {
        match self {
            Self::NameAndExt => 0,
            Self::NameOnly => 1,
            Self::ExtOnly => 2,
        }
    }
}

/// 一次批量重命名的规则。
#[derive(Clone, Debug)]
pub struct Options {
    pub search: String,
    pub replace: String,
    pub use_regex: bool,
    pub match_all: bool,
    pub case_sensitive: bool,
    pub apply_to: ApplyTo,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            search: String::new(),
            replace: String::new(),
            use_regex: false,
            match_all: false,
            case_sensitive: false,
            apply_to: ApplyTo::NameAndExt,
        }
    }
}

/// 一行的状态。
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum RowState {
    /// 名字不变（没命中，或替换后与原样相同）。
    #[default]
    Unchanged,
    /// 可以改名。
    Ready,
    /// 新名字不合法（含非法字符 / 保留名 / 超长 / 空名）。
    Invalid,
    /// 目标名已被占用（目录里已存在，或与另一行的新名字撞车）。
    Conflict,
}

/// 新名字不合法的具体原因（界面显示在行尾）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Reason {
    Empty,
    BadChar,
    TrailingDotOrSpace,
    ReservedName,
    TooLong,
    Exists,
    DuplicateTarget,
}

impl Reason {
    /// 对应 `i18n.rs` 里的中文文案（Rust 侧用 `i18n::text` 取当前语言版本）。
    pub fn zh_text(self) -> &'static str {
        match self {
            Self::Empty => "名称为空",
            Self::BadChar => "名称包含非法字符",
            Self::TrailingDotOrSpace => "名称不能以空格或点结尾",
            Self::ReservedName => "名称是系统保留名",
            Self::TooLong => "名称过长",
            Self::Exists => "目标名已存在",
            Self::DuplicateTarget => "与另一项的新名字重复",
        }
    }
}

/// 单行预览结果。`orig_parts` / `new_parts` 是给界面渲染高亮用的分段：
/// `(文本, 是否高亮)`。原始列高亮「会被替换掉的片段」，新名列高亮「替换进去的片段」。
#[derive(Clone, Debug, Default)]
pub struct Preview {
    pub new_name: String,
    pub orig_parts: Vec<(String, bool)>,
    pub new_parts: Vec<(String, bool)>,
    pub state: RowState,
    pub reason: Option<Reason>,
}

impl Preview {
    fn plain(name: &str) -> Self {
        Self {
            new_name: name.to_string(),
            orig_parts: vec![(name.to_string(), false)],
            new_parts: vec![(name.to_string(), false)],
            state: RowState::Unchanged,
            reason: None,
        }
    }
}

/// 编译搜索规则。返回 `Ok(None)` 表示「搜索框为空，什么都不做」。
///
/// 正则在两种模式下都用同一台引擎：字面模式把搜索串转义，因此「不区分大小写」
/// 与「命中区间」的实现只有一份。
pub fn compile(options: &Options) -> Result<Option<Regex>, String> {
    if options.search.is_empty() {
        return Ok(None);
    }
    let pattern = if options.use_regex {
        options.search.clone()
    } else {
        regex::escape(&options.search)
    };
    RegexBuilder::new(&pattern)
        .case_insensitive(!options.case_sensitive)
        // 文件名场景不需要 `.` 跨行，保持默认即可（`.` 不匹配换行，文件名里也不会有换行）。
        .build()
        .map(Some)
        .map_err(|err| format!("正则无效：{}", first_line(&err.to_string())))
}

fn first_line(text: &str) -> String {
    text.lines().next().unwrap_or(text).trim().to_string()
}

/// 名字拆成 (主干, 扩展名)。目录没有扩展名。
///
/// 用 `Path` 的语义而不是自己找最后一个点：这样 `.gitignore`（无扩展名）、
/// `a.tar.gz`（扩展名 `gz`）与资源管理器的判断一致。
pub fn split_name(name: &str) -> (String, String) {
    let path = Path::new(name);
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| name.to_string());
    let ext = path
        .extension()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    (stem, ext)
}

fn join_name(stem: &str, ext: &str) -> String {
    if ext.is_empty() {
        stem.to_string()
    } else {
        format!("{stem}.{ext}")
    }
}

/// 在 `text` 上应用一次规则，返回 (结果文本, 原文命中区间, 结果插入区间)。
///
/// 区间都是**字符**下标（`split_highlight` 用的单位），不是字节。
fn apply_to_text(
    text: &str,
    re: &Regex,
    options: &Options,
) -> (String, Vec<(usize, usize)>, Vec<(usize, usize)>) {
    // 字节偏移 → 字符下标的映射，只需建一次。
    let mut byte_to_char = vec![0usize; text.len() + 1];
    let mut chars = 0usize;
    for (byte_idx, _) in text.char_indices() {
        byte_to_char[byte_idx] = chars;
        chars += 1;
    }
    byte_to_char[text.len()] = chars;

    let mut out = String::with_capacity(text.len() + 16);
    let mut out_chars = 0usize;
    let mut last_byte = 0usize;
    let mut orig_ranges = Vec::new();
    let mut new_ranges = Vec::new();

    for caps in re.captures_iter(text) {
        let Some(m) = caps.get(0) else { continue };
        if m.start() < last_byte {
            continue;
        }
        // 命中之前的原文原样搬运。
        let gap = &text[last_byte..m.start()];
        out.push_str(gap);
        out_chars += gap.chars().count();

        // 展开替换串：正则模式支持 $1/${name}，字面模式原样写入。
        let inserted = if options.use_regex {
            let mut buf = String::new();
            caps.expand(&options.replace, &mut buf);
            buf
        } else {
            options.replace.clone()
        };

        let ins_chars = inserted.chars().count();
        if ins_chars > 0 {
            new_ranges.push((out_chars, out_chars + ins_chars));
        }
        out.push_str(&inserted);
        out_chars += ins_chars;

        orig_ranges.push((byte_to_char[m.start()], byte_to_char[m.end()]));
        last_byte = m.end();

        if !options.match_all {
            break;
        }
    }
    out.push_str(&text[last_byte..]);
    (out, orig_ranges, new_ranges)
}

/// 计算一行的预览。
///
/// `existing` 是**所在目录现有条目名**的小写集合（用于判断目标名占用）；
/// `own_name` 是自己原来的名字，判断「目标已存在」时要排除自己。
pub fn preview(
    name: &str,
    is_dir: bool,
    options: &Options,
    re: Option<&Regex>,
    existing: &HashSet<String>,
) -> Preview {
    let Some(re) = re else {
        return Preview::plain(name);
    };

    let (stem, ext) = if is_dir {
        (name.to_string(), String::new())
    } else {
        split_name(name)
    };

    let (new_stem, stem_orig, stem_new) = match options.apply_to {
        ApplyTo::NameAndExt | ApplyTo::NameOnly => apply_to_text(&stem, re, options),
        ApplyTo::ExtOnly => (stem.clone(), Vec::new(), Vec::new()),
    };
    let (new_ext, ext_orig, ext_new) = match options.apply_to {
        ApplyTo::NameAndExt | ApplyTo::ExtOnly => apply_to_text(&ext, re, options),
        ApplyTo::NameOnly => (ext.clone(), Vec::new(), Vec::new()),
    };

    let new_name = join_name(&new_stem, &new_ext);

    // 原始列高亮：主干的命中区间 + 扩展名的命中区间（带上主干长度与那个点的偏移）。
    let stem_chars = stem.chars().count();
    let mut orig_ranges = stem_orig;
    let ext_offset = if ext.is_empty() { stem_chars } else { stem_chars + 1 };
    for (a, b) in ext_orig {
        orig_ranges.push((a + ext_offset, b + ext_offset));
    }
    orig_ranges.sort_unstable();

    // 新名列高亮：主干里插入的片段 + 扩展名里插入的片段（同样平移）。
    let new_stem_chars = new_stem.chars().count();
    let mut new_ranges = stem_new;
    let new_ext_offset = if new_ext.is_empty() {
        new_stem_chars
    } else {
        new_stem_chars + 1
    };
    for (a, b) in ext_new {
        new_ranges.push((a + new_ext_offset, b + new_ext_offset));
    }
    new_ranges.sort_unstable();

    let matched = !orig_ranges.is_empty();
    let orig_parts = crate::search::split_highlight(name, &orig_ranges);
    let new_parts = crate::search::split_highlight(&new_name, &new_ranges);

    let mut preview = Preview {
        new_name: new_name.clone(),
        orig_parts,
        new_parts,
        state: RowState::Unchanged,
        reason: None,
    };

    if !matched || new_name == name {
        return preview;
    }

    if let Some(reason) = validate_name(&new_name) {
        preview.state = RowState::Invalid;
        preview.reason = Some(reason);
        return preview;
    }

    // 目标名占用：目录里已经有一个同名条目（排除自己），且不是「只改大小写」。
    let same_name_ci = new_name.eq_ignore_ascii_case(name) || new_name.to_lowercase() == name.to_lowercase();
    if !same_name_ci && existing.contains(&new_name.to_lowercase()) {
        preview.state = RowState::Conflict;
        preview.reason = Some(Reason::Exists);
        return preview;
    }

    preview.state = RowState::Ready;
    preview
}

/// 名字合法性检查（Windows 规则）。返回 `None` 表示合法。
pub fn validate_name(name: &str) -> Option<Reason> {
    if name.is_empty() {
        return Some(Reason::Empty);
    }
    if name.chars().any(|c| "\\/:*?\"<>|".contains(c)) || name.chars().any(|c| (c as u32) < 0x20) {
        return Some(Reason::BadChar);
    }
    if name.ends_with('.') || name.ends_with(' ') {
        return Some(Reason::TrailingDotOrSpace);
    }
    if is_reserved_name(name) {
        return Some(Reason::ReservedName);
    }
    // NTFS 上限是 255 个 UTF-16 码元。
    if name.encode_utf16().count() > 255 {
        return Some(Reason::TooLong);
    }
    None
}

fn is_reserved_name(name: &str) -> bool {
    const RESERVED: [&str; 22] = [
        "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7",
        "COM8", "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
    ];
    let base = name.split('.').next().unwrap_or(name).trim_end();
    let upper = base.to_ascii_uppercase();
    RESERVED.iter().any(|r| upper == *r)
}

/// 重命名计划中的一项。
#[derive(Clone, Debug)]
pub struct PlanItem {
    pub from: PathBuf,
    pub to: PathBuf,
}

/// 执行结果。
#[derive(Clone, Debug, Default)]
pub struct Summary {
    pub renamed: usize,
    /// (原路径, 失败原因)
    pub failed: Vec<(PathBuf, String)>,
    /// 执行时发现目标已存在而跳过的项。
    pub skipped_existing: Vec<PathBuf>,
    /// 已成功改名的项（调用方据此更新自己的名字/路径快照）。
    pub renamed_items: Vec<PlanItem>,
}

/// 逐项改名。**必须在后台线程调用**（网络盘上 `exists()` 与 `rename()` 都可能卡）。
///
/// `rename` 在 Windows 上是 `MoveFileExW(MOVEFILE_REPLACE_EXISTING)` —— 会**覆盖**
/// 已存在的目标。因此这里先查一次 `to.exists()`：预览阶段用的是目录快照，
/// 快照与此刻之间可能有别的程序建了同名文件，覆盖掉别人的文件是不可接受的。
pub fn execute(items: &[PlanItem]) -> Summary {
    let mut summary = Summary::default();
    for item in items {
        // 只改大小写（`abc.txt` → `ABC.txt`）：NTFS 不区分大小写，目标在磁盘上
        // 「已经存在」——那其实就是它自己。一步改名会被下面的存在性检查拦成
        // 「跳过」，所以中间先换一个临时名，再改到目标名。
        let case_only = item.from != item.to
            && item.to.exists()
            && item.from.exists()
            && item.from.to_string_lossy().to_lowercase()
                == item.to.to_string_lossy().to_lowercase();
        if case_only {
            match rename_via_temp(&item.from, &item.to) {
                Ok(()) => {
                    summary.renamed += 1;
                    summary.renamed_items.push(item.clone());
                }
                Err(err) => summary.failed.push((item.from.clone(), err.to_string())),
            }
            continue;
        }
        if item.to.exists() {
            summary.skipped_existing.push(item.from.clone());
            continue;
        }
        match std::fs::rename(&item.from, &item.to) {
            Ok(()) => {
                summary.renamed += 1;
                summary.renamed_items.push(item.clone());
            }
            Err(err) => summary.failed.push((item.from.clone(), err.to_string())),
        }
    }
    summary
}

/// 两步改名：`from` → 同目录下的临时名 → `to`。
///
/// 临时名带进程号和序号，避免撞上用户自己的文件；第二步失败会回滚到原名，
/// 不把用户的文件留在临时名上。
fn rename_via_temp(from: &Path, to: &Path) -> std::io::Result<()> {
    let dir = to.parent().unwrap_or_else(|| Path::new("."));
    let pid = std::process::id();
    for n in 0..64u32 {
        let tmp = dir.join(format!(".rove-rename-{pid}-{n}.tmp"));
        if tmp.exists() {
            continue;
        }
        std::fs::rename(from, &tmp)?;
        return match std::fs::rename(&tmp, to) {
            Ok(()) => Ok(()),
            Err(err) => {
                let _ = std::fs::rename(&tmp, from);
                Err(err)
            }
        };
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        "找不到可用的临时文件名",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts(search: &str, replace: &str) -> Options {
        Options {
            search: search.to_string(),
            replace: replace.to_string(),
            ..Options::default()
        }
    }

    fn set(names: &[&str]) -> HashSet<String> {
        names.iter().map(|n| n.to_lowercase()).collect()
    }

    fn run(name: &str, is_dir: bool, o: &Options, existing: &[&str]) -> Preview {
        let re = compile(o).expect("compile");
        preview(name, is_dir, o, re.as_ref(), &set(existing))
    }

    /// 只改大小写也要真的改成功：不能因为「目标已存在」（那其实是它自己）被跳过。
    #[test]
    fn case_only_rename_uses_two_steps() {
        let dir = std::env::temp_dir().join(format!("rove-br-case-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("mkdir");
        let from = dir.join("abc.txt");
        std::fs::write(&from, b"x").expect("write");

        let summary = execute(&[PlanItem { from: from.clone(), to: dir.join("ABC.txt") }]);
        assert_eq!(summary.renamed, 1, "只改大小写必须真的改名");
        assert!(summary.skipped_existing.is_empty());
        // 用目录列表比对精确文件名：Windows 上 `from.exists()` 大小写不敏感，不可靠。
        let names: Vec<String> = std::fs::read_dir(&dir)
            .expect("read_dir")
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["ABC.txt".to_string()]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn split_handles_dotfiles_and_multi_dot() {
        assert_eq!(split_name("a.tar.gz"), ("a.tar".into(), "gz".into()));
        assert_eq!(split_name(".gitignore"), (".gitignore".into(), "".into()));
        assert_eq!(split_name("noext"), ("noext".into(), "".into()));
        assert_eq!(split_name("名.字.mp4"), ("名.字".into(), "mp4".into()));
    }

    #[test]
    fn literal_replaces_first_only_by_default() {
        let o = opts("ab", "X");
        let p = run("abab.mp4", false, &o, &[]);
        assert_eq!(p.new_name, "Xab.mp4");
        assert_eq!(p.state, RowState::Ready);
    }

    #[test]
    fn literal_replaces_all_when_match_all() {
        let o = Options {
            match_all: true,
            ..opts("ab", "X")
        };
        let p = run("abab.mp4", false, &o, &[]);
        assert_eq!(p.new_name, "XX.mp4");
    }

    #[test]
    fn literal_mode_does_not_expand_dollar_groups() {
        let o = opts("a", "$1");
        let p = run("a.txt", false, &o, &[]);
        assert_eq!(p.new_name, "$1.txt");
    }

    #[test]
    fn case_insensitive_by_default_and_switchable() {
        let p = run("ABC.txt", false, &opts("abc", "x"), &[]);
        assert_eq!(p.new_name, "x.txt");

        let strict = Options {
            case_sensitive: true,
            ..opts("abc", "x")
        };
        let p = run("ABC.txt", false, &strict, &[]);
        assert_eq!(p.new_name, "ABC.txt");
        assert_eq!(p.state, RowState::Unchanged);
    }

    #[test]
    fn regex_capture_group_expands() {
        let o = Options {
            use_regex: true,
            ..opts(r"(\d+)-(\d+)", "$2-$1")
        };
        let p = run("12-34.mp4", false, &o, &[]);
        assert_eq!(p.new_name, "34-12.mp4");
    }

    #[test]
    fn apply_to_name_only_leaves_extension() {
        let o = Options {
            apply_to: ApplyTo::NameOnly,
            ..opts("a", "b")
        };
        let p = run("a.a.a", false, &o, &[]);
        assert_eq!(p.new_name, "b.a.a");
    }

    #[test]
    fn apply_to_extension_only() {
        let o = Options {
            apply_to: ApplyTo::ExtOnly,
            ..opts("mp4", "mkv")
        };
        let p = run("a.mp4", false, &o, &[]);
        assert_eq!(p.new_name, "a.mkv");
        assert_eq!(p.state, RowState::Ready);
    }

    #[test]
    fn directory_has_no_extension() {
        let o = opts("a", "b");
        let p = run("a.txt", true, &o, &[]);
        assert_eq!(p.new_name, "b.txt");
        let o = Options {
            apply_to: ApplyTo::ExtOnly,
            ..opts("txt", "x")
        };
        let p = run("a.txt", true, &o, &[]);
        assert_eq!(p.new_name, "a.txt");
        assert_eq!(p.state, RowState::Unchanged);
    }

    #[test]
    fn orig_highlight_covers_the_matched_span() {
        let o = opts("二", "1");
        let p = run("糖芯二瑶.mp4", false, &o, &[]);
        assert_eq!(p.new_name, "糖芯1瑶.mp4");
        // 原始列：第 2 个字符（从 0 数）被高亮。
        let hl: Vec<&str> = p
            .orig_parts
            .iter()
            .filter(|(_, hot)| *hot)
            .map(|(t, _)| t.as_str())
            .collect();
        assert_eq!(hl, vec!["二"]);
    }

    #[test]
    fn new_highlight_covers_the_inserted_span() {
        let o = opts("二", "1");
        let p = run("糖芯二瑶.mp4", false, &o, &[]);
        let hl: Vec<&str> = p
            .new_parts
            .iter()
            .filter(|(_, hot)| *hot)
            .map(|(t, _)| t.as_str())
            .collect();
        assert_eq!(hl, vec!["1"]);
    }

    #[test]
    fn new_highlight_in_extension_is_offset_correctly() {
        let o = Options {
            apply_to: ApplyTo::ExtOnly,
            ..opts("mp4", "MKV")
        };
        let p = run("abc.mp4", false, &o, &[]);
        assert_eq!(p.new_name, "abc.MKV");
        let joined: String = p.new_parts.iter().map(|(t, _)| t.as_str()).collect();
        assert_eq!(joined, "abc.MKV");
        let hl: String = p
            .new_parts
            .iter()
            .filter(|(_, hot)| *hot)
            .map(|(t, _)| t.as_str())
            .collect();
        assert_eq!(hl, "MKV");
    }

    #[test]
    fn deleting_matched_text_highlights_nothing_in_new_column() {
        let o = opts("前缀", "");
        let p = run("前缀文件.txt", false, &o, &[]);
        assert_eq!(p.new_name, "文件.txt");
        assert!(p.new_parts.iter().all(|(_, hot)| !hot));
        assert_eq!(p.state, RowState::Ready);
    }

    #[test]
    fn empty_search_keeps_everything_unchanged() {
        let o = opts("", "x");
        let re = compile(&o).expect("compile");
        assert!(re.is_none());
        let p = preview("a.txt", false, &o, None, &set(&[]));
        assert_eq!(p.state, RowState::Unchanged);
        assert_eq!(p.new_name, "a.txt");
    }

    #[test]
    fn invalid_regex_is_reported() {
        let o = Options {
            use_regex: true,
            ..opts("([a-", "x")
        };
        assert!(compile(&o).is_err());
    }

    #[test]
    fn invalid_characters_and_reserved_names_are_rejected() {
        assert_eq!(validate_name("a/b"), Some(Reason::BadChar));
        assert_eq!(validate_name("a:b"), Some(Reason::BadChar));
        assert_eq!(validate_name("a."), Some(Reason::TrailingDotOrSpace));
        assert_eq!(validate_name("a "), Some(Reason::TrailingDotOrSpace));
        assert_eq!(validate_name(""), Some(Reason::Empty));
        assert_eq!(validate_name("CON"), Some(Reason::ReservedName));
        assert_eq!(validate_name("con.txt"), Some(Reason::ReservedName));
        assert_eq!(validate_name("COM1"), Some(Reason::ReservedName));
        assert_eq!(validate_name("CONSOLE"), None);
        assert_eq!(validate_name(&"a".repeat(256)), Some(Reason::TooLong));
        assert_eq!(validate_name(&"a".repeat(255)), None);
    }

    #[test]
    fn existing_target_is_a_conflict() {
        let o = opts("a", "b");
        let p = run("aa.txt", false, &o, &["ba.txt"]);
        assert_eq!(p.new_name, "ba.txt");
        assert_eq!(p.state, RowState::Conflict);
        assert_eq!(p.reason, Some(Reason::Exists));
    }

    #[test]
    fn case_only_rename_is_not_a_conflict_with_itself() {
        let o = opts("abc", "ABC");
        let p = run("abc.txt", false, &o, &["abc.txt"]);
        assert_eq!(p.new_name, "ABC.txt");
        assert_eq!(p.state, RowState::Ready);
    }

    #[test]
    fn no_match_is_unchanged() {
        let p = run("readme.md", false, &opts("zzz", "x"), &[]);
        assert_eq!(p.state, RowState::Unchanged);
    }

    #[test]
    fn unicode_case_insensitive_matching() {
        let p = run("ÄRGER.txt", false, &opts("ärger", "ok"), &[]);
        assert_eq!(p.new_name, "ok.txt");
    }

    #[test]
    fn duplicate_targets_are_detected_by_the_caller() {
        // preview 只负责单行；撞车由 app 层用「已经分配出去的名字」集合叠加判定。
        // 这里确认两行独立算出同一个新名字，交给调用方标记 DuplicateTarget。
        let o = Options {
            use_regex: true,
            ..opts("a|b", "x")
        };
        let re = compile(&o).expect("compile");
        let existing = set(&[]);
        let p1 = preview("a.txt", false, &o, re.as_ref(), &existing);
        let p2 = preview("b.txt", false, &o, re.as_ref(), &existing);
        assert_eq!(p1.new_name, "x.txt");
        assert_eq!(p2.new_name, "x.txt");
        assert_eq!(p1.state, RowState::Ready);
        assert_eq!(p2.state, RowState::Ready);
    }
}