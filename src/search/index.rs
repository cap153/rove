//! 全盘文件索引（Phase 5）：后台线程递归扫描固定驱动器，
//! 构建拼音预计算的内存索引；查询走 RwLock 读锁，毫秒级返回。
//!
//! 范围控制：
//! - 跳过系统目录（Windows / Program Files* / $Recycle.Bin / System Volume
//!   Information / AppData）避免噪声与超长扫描；
//! - 总量上限 [`MAX_ENTRIES`]（达上限即停止，防止极端磁盘拖垮内存）。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};

use super::pinyin_fields;

/// 索引条目上限（防失控）。MFT 路径下每条约 80B（路径 + 名称 + 拼音字段），
/// 100 万条约 80MB，足以覆盖绝大多数家用电脑的全盘文件量。
pub const MAX_ENTRIES: usize = 1_000_000;

/// 一条全局索引记录。
pub struct IndexEntry {
    pub path: PathBuf,
    pub name: String,
    pub fields: crate::search::PinyinFields,
    pub is_dir: bool,
}

/// 全局索引（后台构建，读多写少）。
#[derive(Default)]
pub struct GlobalIndex {
    pub entries: Vec<IndexEntry>,
    /// 索引是否已构建完成（false = 构建中）。
    pub ready: bool,
}

/// 一次全局索引查询的结果。
pub struct SearchHits<'a> {
    /// 按得分降序的前 `limit` 条命中。
    pub hits: Vec<(&'a IndexEntry, u16)>,
    /// 命中总数（**未截断**）。`hits.len() < total` 表示结果被 `limit` 截断，
    /// 调用方应据此在状态栏提示用户。
    pub total: usize,
}

impl GlobalIndex {
    /// 拼音/模糊查询，按得分排序取前 `limit` 条，并回报未截断的命中总数。
    pub fn search(&self, query: &str, limit: usize) -> SearchHits<'_> {
        let query_lower = query.to_lowercase();
        if query_lower.is_empty() {
            return SearchHits { hits: Vec::new(), total: 0 };
        }
        let mut matcher = nucleo_matcher::Matcher::new(nucleo_matcher::Config::DEFAULT);
        let mut bufs = (Vec::new(), Vec::new(), Vec::new());
        let mut scored: Vec<(&IndexEntry, u16)> = self
            .entries
            .iter()
            .filter_map(|e| {
                crate::search::score(&e.name, &e.fields, &query_lower, &mut matcher, &mut bufs)
                    .map(|s| (e, s))
            })
            .collect();
        scored.sort_by(|a, b| b.1.cmp(&a.1));
        let total = scored.len();
        scored.truncate(limit);
        SearchHits { hits: scored, total }
    }
}

/// 共享句柄。
pub type SharedIndex = Arc<RwLock<GlobalIndex>>;

/// 全局索引单例（首次调用初始化，后台线程异步填充）。
static INDEX: std::sync::OnceLock<SharedIndex> = std::sync::OnceLock::new();

/// 获取（必要时懒初始化）全局索引。首次调用会拉起后台构建线程。
pub fn shared() -> SharedIndex {
    INDEX
        .get_or_init(|| {
            let idx: SharedIndex = Arc::new(RwLock::new(GlobalIndex::default()));
            spawn_builder(idx.clone());
            idx
        })
        .clone()
}

/// 已初始化的索引句柄；尚未初始化时返回 `None`（**不会**触发构建）。
///
/// 供 UI 轮询「索引是否就绪」：不能为了显示一个状态提示就把整个
/// 全盘扫描拉起来，那会让从不搜索的用户也白白扫一遍盘。
pub fn try_shared() -> Option<SharedIndex> {
    INDEX.get().cloned()
}

fn spawn_builder(idx: SharedIndex) {
    let stop = Arc::new(AtomicBool::new(false));
    std::thread::Builder::new()
        .name("rove-indexer".into())
        .spawn(move || {
            let mut count = 0usize;
            let roots = list_roots();
            // 任务 3.2：优先用 NTFS $MFT 加速全盘索引；无管理员权限或全盘
            // 失败时回退到原有 std::fs 递归 walker。
            // 两种路径都记录条目数与耗时，便于实机对比直读收益。
            let t0 = std::time::Instant::now();
            match crate::search::mft::build_via_mft(&idx, &stop) {
                Some(_) => {
                    // MFT 路径：重新统计条目数（build_via_mft 已写入）。
                    if let Ok(w) = idx.read() {
                        count = w.entries.len();
                    }
                    logln!(
                        "index: NTFS $MFT direct read -> {} entries in {:.1} ms",
                        count,
                        t0.elapsed().as_secs_f64() * 1000.0
                    );
                }
                None => {
                    logln!(
                        "index: $MFT unavailable (need admin / non-NTFS), fallback to std::fs walk (probe {:.1} ms)",
                        t0.elapsed().as_secs_f64() * 1000.0
                    );
                    for root in roots {
                        if count >= MAX_ENTRIES {
                            break;
                        }
                        count += walk(&root, &idx, count, &stop);
                    }
                    logln!(
                        "index: std::fs walk -> {} entries in {:.1} ms",
                        count,
                        t0.elapsed().as_secs_f64() * 1000.0
                    );
                }
            }
            if let Ok(mut w) = idx.write() {
                w.ready = true;
            }
            logln!("global index built: {} entries", count);
            // 通知 UI 索引就绪。用 Slint 原生跨线程投递，而非定时器轮询：
            // 零空转开销，且就绪与刷新落在同一事件循环 tick（微秒级，无视觉延迟）。
            // 事件循环尚未启动时会返回 Err——那种情况下用户还无法交互，
            // 其后的搜索自然会拿到全量结果，故安全忽略。
            let _ = slint::invoke_from_event_loop(|| {
                crate::app::on_index_ready();
            });
        })
        .ok();
}

/// 是否系统盘（Windows 所在盘）。取 `%SystemDrive%`，缺省 `C:`。
fn is_system_drive(root: &Path) -> bool {
    let sys = std::env::var("SystemDrive")
        .or_else(|_| std::env::var("SYSTEMDRIVE"))
        .unwrap_or_else(|_| "C:".to_string());
    // 归一化：去尾部斜杠 + 统一大写，避免 "C:\" vs "c:" 误判。
    let norm = |s: &str| s.trim_end_matches(['\\', '/']).to_ascii_uppercase();
    norm(&root.to_string_lossy()) == norm(&sys)
}

/// 枚举固定驱动器根目录。
///
/// 顺序：**非系统盘优先，系统盘（C:）排最后**。索引有总量上限，若先扫 C 盘，
/// 海量系统文件会在触及上限前挤占掉全部名额，导致数据盘根本无法入库。
pub(crate) fn list_roots() -> Vec<PathBuf> {
    let drives: Vec<PathBuf> = crate::fs::list_drives()
        .into_iter()
        .filter(|d| matches!(d.drive_type, crate::fs::drives::DriveType::Fixed))
        .map(|d| PathBuf::from(format!("{}:\\", d.letter)))
        .collect();
    // 稳定分区：数据盘保持原相对顺序沉在前面，系统盘移到末尾。
    let (system, data): (Vec<PathBuf>, Vec<PathBuf>) =
        drives.into_iter().partition(|p| is_system_drive(p));
    data.into_iter().chain(system).collect()
}

/// 跳过的目录名(大小写不敏感比较)。
/// 系统噪声目录名（小写形式）。命中即整棵剪枝，避免索引被系统文件占满。
const NOISE_DIR_NAMES: &[&str] = &[
    "windows",
    "program files",
    "program files (x86)",
    "$recycle.bin",
    "system volume information",
    "appdata",
    "node_modules",
    ".git",
];

/// 是否系统噪声目录（大小写不敏感）。
///
/// 用 `eq_ignore_ascii_case` 原地比较，**不做 `to_lowercase()` 堆分配**：
/// 索引构建时每个目录条目都会调一次（百万级条目），分配累积起来相当可观。
/// `eq_ignore_ascii_case` 内部先比长度再逐字节比，多数候选一次长度比较即淘汰。
///
/// 语义与旧版 `to_lowercase()` 等价——候选名全是 ASCII，唯一差异是
/// `"windowſ"`（含长 s U+017F）这类 Unicode 怪字符：旧版会被误判为
/// `"windows"` 而剪掉，新版不会。而那种名字本就不该被当作系统目录。
pub(crate) fn is_skipped(name: &str) -> bool {
    NOISE_DIR_NAMES.iter().any(|n| name.eq_ignore_ascii_case(n))
}

/// 递归扫描；返回新增条目数。
fn walk(dir: &Path, idx: &SharedIndex, count: usize, stop: &Arc<AtomicBool>) -> usize {
    if count >= MAX_ENTRIES || stop.load(Ordering::Relaxed) {
        return 0;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    let mut added = 0usize;
    let mut subdirs: Vec<PathBuf> = Vec::new();
    for entry in entries.flatten() {
        let Ok(ft) = entry.file_type() else {
            continue;
        };
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') && name.len() > 1 && ft.is_dir() {
            continue;
        }
        if ft.is_dir() {
            if is_skipped(&name) {
                continue;
            }
            subdirs.push(entry.path());
        }
        if count + added >= MAX_ENTRIES {
            break;
        }
        let fields = pinyin_fields(&name);
        if let Ok(mut w) = idx.write() {
            w.entries.push(IndexEntry {
                path: entry.path(),
                name: name.clone(),
                fields,
                is_dir: ft.is_dir(),
            });
        }
        added += 1;
        if added % 2000 == 0 {
            std::thread::sleep(std::time::Duration::from_millis(1)); // 让出 CPU
        }
    }
    for sub in subdirs {
        if count + added >= MAX_ENTRIES {
            break;
        }
        added += walk(&sub, idx, count + added, stop);
    }
    added
}

// logln 由 crate 根导出。
use crate::logln;

// ---------------------------------------------------------------------------
// 深度搜索(文件夹视图):在当前目录子树中递归查找匹配项。
// ---------------------------------------------------------------------------

/// 深度搜索单条结果:条目 + 预计算拼音(UI 端高亮复用,避免二次计算)。
pub struct DeepItem {
    pub entry: crate::fs::FileEntry,
    pub fields: crate::search::PinyinFields,
}

/// 深度扫描条目上限(防失控;配合 worker 无中断,上限要保守)。
const DEEP_MAX_ENTRIES: usize = 20_000;
/// 深度搜索结果上限。
pub const DEEP_MAX_RESULTS: usize = 300;
/// 递归深度上限。
const DEEP_MAX_DEPTH: usize = 10;

/// 在 `root` 子树中递归查找名称匹配 `query_lower` 的条目。
/// 直接子项(depth 0)不返回——它们已在当前视图列表中,由本地过滤负责。
struct DeepScan {
    out: Vec<DeepItem>,
    scanned: usize,
}

pub fn deep_scan(root: &Path, query_lower: &str) -> Vec<DeepItem> {
    let mut matcher = nucleo_matcher::Matcher::new(nucleo_matcher::Config::DEFAULT);
    let mut bufs = (Vec::new(), Vec::new(), Vec::new());
    let mut st = DeepScan {
        out: Vec::with_capacity(64),
        scanned: 0,
    };
    deep_walk(root, 0, query_lower, &mut matcher, &mut bufs, &mut st);
    st.out
}

fn deep_walk(
    dir: &Path,
    depth: usize,
    query_lower: &str,
    matcher: &mut nucleo_matcher::Matcher,
    bufs: &mut (Vec<char>, Vec<char>, Vec<char>),
    st: &mut DeepScan,
) {
    if depth > DEEP_MAX_DEPTH
        || st.scanned >= DEEP_MAX_ENTRIES
        || st.out.len() >= DEEP_MAX_RESULTS
    {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut subdirs: Vec<PathBuf> = Vec::new();
    for entry in entries.flatten() {
        if st.scanned >= DEEP_MAX_ENTRIES || st.out.len() >= DEEP_MAX_RESULTS {
            break;
        }
        let Ok(ft) = entry.file_type() else {
            continue;
        };
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if ft.is_dir() {
            // 隐藏目录与系统噪声目录不进入(与全局索引一致)。
            if name.starts_with('.') && name.len() > 1 {
                continue;
            }
            if is_skipped(&name) {
                continue;
            }
            subdirs.push(path.clone());
        }
        st.scanned += 1;
        // depth 0 = 当前视图的直接子项,本地过滤已覆盖,不重复返回。
        if depth == 0 {
            continue;
        }
        let fields = pinyin_fields(&name);
        if crate::search::matches_query(&name, &fields, query_lower, matcher, bufs) {
            let meta = std::fs::symlink_metadata(&path).ok();
            let size = if ft.is_dir() {
                0
            } else {
                meta.as_ref().map(|m| m.len()).unwrap_or(0)
            };
            let modified = meta.and_then(|m| m.modified().ok());
            st.out.push(DeepItem {
                entry: crate::fs::FileEntry::from_path(&path, ft.is_dir(), size, modified, false),
                fields,
            });
        }
    }
    for sub in subdirs {
        if st.scanned >= DEEP_MAX_ENTRIES || st.out.len() >= DEEP_MAX_RESULTS {
            break;
        }
        deep_walk(&sub, depth + 1, query_lower, matcher, bufs, st);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str) -> IndexEntry {
        IndexEntry {
            path: PathBuf::from(format!("D:\\data\\{name}")),
            name: name.to_string(),
            fields: crate::search::pinyin_fields(name),
            is_dir: false,
        }
    }

    #[test]
    fn noise_dir_names_match_case_insensitively() {
        // 各种大小写形式都应命中（这是剪枝的前提）。
        for n in [
            "windows",
            "WINDOWS",
            "Windows",
            "WiNdOwS",
            "Program Files",
            "PROGRAM FILES (X86)",
            "$Recycle.Bin",
            "System Volume Information",
            "AppData",
            "NODE_MODULES",
            ".git",
            ".GIT",
        ] {
            assert!(is_skipped(n), "{n} 应被判为噪音目录");
        }
        // 仅子串相同或前缀相同不应命中（旧版是全等匹配，必须保持）。
        for n in [
            "MyWindows",
            "windows.old",
            "windowsx",
            "Program",
            "ProgramFiles",
            "git",
            "appdata.local",
            "System",
            "",
        ] {
            assert!(!is_skipped(n), "{n} 不应被判为噪音目录");
        }
    }

    #[test]
    fn system_drive_detection_is_normalized() {
        // 自洽构造：按宿主环境变量（缺省 C:）推导系统盘符。
        let sys = std::env::var("SystemDrive")
            .or_else(|_| std::env::var("SYSTEMDRIVE"))
            .unwrap_or_else(|_| "C:".to_string());
        let letter = sys
            .trim_end_matches(['\\', '/'])
            .chars()
            .next()
            .unwrap_or('C');
        // 带/不带尾斜杠、大小写差异都应识别为同一系统盘。
        assert!(is_system_drive(Path::new(&format!("{letter}:\\"))));
        assert!(is_system_drive(Path::new(&format!("{letter}:"))));
        assert!(is_system_drive(Path::new(&format!(
            "{}:\\",
            letter.to_ascii_lowercase()
        ))));
        // 另一盘符不应误判。
        let other = if letter.eq_ignore_ascii_case(&'C') { 'D' } else { 'C' };
        assert!(!is_system_drive(Path::new(&format!("{other}:\\"))));
    }

    #[test]
    fn search_reports_untruncated_total() {
        let mut idx = GlobalIndex::default();
        for i in 0..10 {
            idx.entries.push(entry(&format!("file{i}.txt")));
        }
        // 上限宽于命中数：不截断，total 等于命中数。
        let r = idx.search("file", 100);
        assert_eq!(r.total, 10);
        assert_eq!(r.hits.len(), 10);
        // 上限窄于命中数：结果截断，但 total 仍回报全量命中数。
        let r = idx.search("file", 3);
        assert_eq!(r.total, 10, "截断后 total 必须仍是全量命中数");
        assert_eq!(r.hits.len(), 3);
        // 空查询：零命中。
        let r = idx.search("   ", 10);
        assert_eq!(r.total, 0);
        assert!(r.hits.is_empty());
    }
}
