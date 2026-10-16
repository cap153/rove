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

/// 索引条目上限（防失控）。
pub const MAX_ENTRIES: usize = 300_000;

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

impl GlobalIndex {
    /// 拼音/模糊查询，按得分排序取前 `limit` 条。
    pub fn search(&self, query: &str, limit: usize) -> Vec<(&IndexEntry, u16)> {
        let query_lower = query.to_lowercase();
        if query_lower.is_empty() {
            return Vec::new();
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
        scored.truncate(limit);
        scored
    }
}

/// 共享句柄。
pub type SharedIndex = Arc<RwLock<GlobalIndex>>;

/// 全局索引单例（首次调用初始化，后台线程异步填充）。
pub fn shared() -> SharedIndex {
    static INDEX: std::sync::OnceLock<SharedIndex> = std::sync::OnceLock::new();
    INDEX
        .get_or_init(|| {
            let idx: SharedIndex = Arc::new(RwLock::new(GlobalIndex::default()));
            spawn_builder(idx.clone());
            idx
        })
        .clone()
}

fn spawn_builder(idx: SharedIndex) {
    let stop = Arc::new(AtomicBool::new(false));
    std::thread::Builder::new()
        .name("rove-indexer".into())
        .spawn(move || {
            let mut count = 0usize;
            let roots = list_roots();
            for root in roots {
                if count >= MAX_ENTRIES {
                    break;
                }
                count += walk(&root, &idx, count, &stop);
            }
            if let Ok(mut w) = idx.write() {
                w.ready = true;
            }
            logln!("global index built: {} entries", count);
        })
        .ok();
}

/// 枚举固定驱动器根目录。
fn list_roots() -> Vec<PathBuf> {
    crate::fs::list_drives()
        .into_iter()
        .filter(|d| matches!(d.drive_type, crate::fs::drives::DriveType::Fixed))
        .map(|d| PathBuf::from(format!("{}:\\", d.letter)))
        .collect()
}

/// 跳过的目录名(大小写不敏感比较)。
pub(crate) fn is_skipped(name: &str) -> bool {
    let lower = name.to_lowercase();
    matches!(
        lower.as_str(),
        "windows" | "program files" | "program files (x86)" | "$recycle.bin"
            | "system volume information" | "appdata" | "node_modules" | ".git"
    )
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
                entry: crate::fs::FileEntry::from_path(&path, ft.is_dir(), size, modified),
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
