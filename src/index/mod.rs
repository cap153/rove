//! 索引引擎门面：MFT 全量扫描 + USN 增量监听 + 拼音搜索。
//!
//! ```text
//!  ┌─ 每固定 NTFS 卷 ─┐   ┌─ 查询线程 ─┐
//!  │ MFT 扫描线程      │   │ 收 query   │──► 遍历各卷树匹配 ──► 结果回传 UI
//!  │  └─ USN 监听循环  │   └────────────┘
//!  └──────────────────┘
//! ```

#[cfg(windows)]
pub mod mft;
pub mod tree;
#[cfg(windows)]
pub mod usn;

use crate::matcher::{self, MatchEntry};

#[cfg(windows)]
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;

use tree::Tree;

/// 索引统计（状态栏展示）
#[derive(Debug, Clone, Default)]
#[allow(dead_code)]
pub struct IndexStats {
    pub volumes: usize,
    pub nodes: usize,
    pub scanning: bool,
    pub last_usn: i64,
}

/// 枚举模式条目（MFT 不可用时的回退索引）
#[derive(Debug, Clone)]
pub struct EnumEntry {
    pub name: String,
    pub path: String,
    pub is_dir: bool,
    pub size: u64,
}

/// 单卷索引：MFT 优先，失败回退目录枚举
pub struct Volume {
    pub drive: String,
    /// MFT 模式（带 USN 增量）
    pub tree: Option<Arc<Mutex<Tree>>>,
    /// 枚举模式（静态快照）
    pub entries: Option<Vec<EnumEntry>>,
}

/// 一条搜索结果
#[derive(Debug, Clone)]
pub struct SearchHit {
    pub name: String,
    pub path: String,
    pub is_dir: bool,
    pub size: u64,
}

/// 全局结果通道（主线程定时器轮询用）
static RESULTS: OnceLock<Mutex<Receiver<Vec<SearchHit>>>> = OnceLock::new();

/// 取回最新一批搜索结果（主线程调用）
pub fn poll_results() -> Option<Vec<SearchHit>> {
    let receiver = RESULTS.get()?;
    let guard = receiver.lock().unwrap_or_else(|e| e.into_inner());
    let mut latest = None;
    while let Ok(hits) = guard.try_recv() {
        latest = Some(hits);
    }
    latest
}

/// 索引服务（UI 线程持有）
pub struct IndexService {
    query_tx: Sender<String>,
    #[allow(dead_code)]
    stats_rx: Receiver<IndexStats>,
}

impl IndexService {
    /// 启动索引：枚举固定 NTFS 卷，逐卷扫描 MFT 并监听 USN
    pub fn start() -> Self {
        let (query_tx, query_rx) = channel::<String>();
        let (result_tx, result_rx) = channel::<Vec<SearchHit>>();
        let (stats_tx, stats_rx) = channel::<IndexStats>();
        #[cfg(not(windows))]
        let _ = &stats_tx;

        let query_rx = Arc::new(Mutex::new(query_rx));
        let result_tx = Arc::new(Mutex::new(result_tx));

        // 枚举固定磁盘
        let drives = fixed_drives();
        let volumes: Vec<Arc<Mutex<Volume>>> = drives
            .iter()
            .map(|drive| {
                Arc::new(Mutex::new(Volume {
                    drive: drive.clone(),
                    tree: Some(Arc::new(Mutex::new(Tree::new(drive)))),
                    entries: None,
                }))
            })
            .collect();
        let volumes = Arc::new(volumes);

        // 每卷一个扫描 + USN 监听线程
        #[cfg(windows)]
        for volume in volumes.iter() {
            let volume = Arc::clone(volume);
            let stats_tx = stats_tx.clone();
            thread::spawn(move || {
                run_volume(volume, stats_tx);
            });
        }

        // 查询线程
        {
            let volumes = Arc::clone(&volumes);
            thread::spawn(move || {
                let query_rx = query_rx.lock().unwrap_or_else(|e| e.into_inner());
                let result_tx = result_tx.lock().unwrap_or_else(|e| e.into_inner());
                while let Ok(query) = query_rx.recv() {
                    let hits = search(&volumes, &query);
                    let _ = result_tx.send(hits);
                }
            });
        }

        let _ = RESULTS.set(Mutex::new(result_rx));

        Self {
            query_tx,
            stats_rx,
        }
    }

    /// 发起搜索（异步）
    pub fn search(&self, query: &str) {
        let _ = self.query_tx.send(query.to_string());
    }

    /// 主线程取回搜索结果（已改为全局 `poll_results()`，此方法保留供兼容）
    #[allow(dead_code)]
    pub fn poll(&self) -> Vec<SearchHit> {
        Vec::new()
    }

    /// 主线程取回统计
    #[allow(dead_code)]
    pub fn poll_stats(&self) -> Option<IndexStats> {
        self.stats_rx.try_recv().ok()
    }
}

/// 单卷：MFT 全量扫描 -> USN 事件循环；MFT 不可用（未提权）时回退目录枚举
#[cfg(windows)]
fn run_volume(volume: Arc<Mutex<Volume>>, stats_tx: std::sync::mpsc::Sender<IndexStats>) {
    let drive = volume
        .lock()
        .map(|v| v.drive.clone())
        .unwrap_or_else(|_| "?:".to_string());
    let _ = stats_tx.send(IndexStats {
        volumes: 1,
        nodes: 0,
        scanning: true,
        last_usn: 0,
    });

    // 管理员登录时该权限存在但默认禁用，启用后无需重启即可读裸卷
    mft::enable_manage_volume_privilege();

    let handle = match mft::open_volume(&drive) {
        Ok(handle) => handle,
        Err(e) => {
            eprintln!("[index] {drive} 裸卷不可用（{e}），回退目录枚举索引");
            let entries = enumerate_fallback(&drive);
            let count = entries.len();
            {
                let mut volume = volume.lock().unwrap_or_else(|e| e.into_inner());
                volume.tree = None;
                volume.entries = Some(entries);
            }
            let _ = stats_tx.send(IndexStats {
                volumes: 1,
                nodes: count,
                scanning: false,
                last_usn: 0,
            });
            return;
        }
    };

    let Ok(info) = mft::read_volume_info(handle, &drive) else {
        eprintln!("[index] 无法读取卷参数 {drive}");
        mft::close_volume(handle);
        return;
    };

    let tree = volume
        .lock()
        .ok()
        .and_then(|v| v.tree.clone())
        .unwrap_or_else(|| Arc::new(Mutex::new(Tree::new(&drive))));
    {
        let mut tree = tree.lock().unwrap_or_else(|e| e.into_inner());
        match mft::scan_mft(handle, &info, &mut tree) {
            Ok(count) => eprintln!("[index] {drive} MFT 扫描完成：{count} 条记录"),
            Err(e) => eprintln!("[index] {drive} MFT 扫描失败：{e}"),
        }
    }
    let _ = stats_tx.send(IndexStats {
        volumes: 1,
        nodes: tree.lock().map(|t| t.len()).unwrap_or(0),
        scanning: false,
        last_usn: 0,
    });

    // USN 事件循环
    let Ok(mut journal) = usn::open_journal(handle) else {
        eprintln!("[index] {drive} USN Journal 不可用");
        mft::close_volume(handle);
        return;
    };
    eprintln!("[index] {drive} USN 监听已启动 (journal {})", journal.journal_id);

    loop {
        {
            let mut tree = tree.lock().unwrap_or_else(|e| e.into_inner());
            if usn::read_and_apply(handle, &mut journal, &mut tree).is_err() {
                // journal 失效：重新打开并继续
                match usn::open_journal(handle) {
                    Ok(new_journal) => journal = new_journal,
                    Err(_) => std::thread::sleep(std::time::Duration::from_secs(5)),
                }
            }
        }
        let _ = stats_tx.send(IndexStats {
            volumes: 1,
            nodes: tree.lock().map(|t| t.len()).unwrap_or(0),
            scanning: false,
            last_usn: journal.next_usn,
        });
    }
}

/// 回退索引：递归枚举目录（限深、限量、跳过噪声目录）
#[cfg(windows)]
fn enumerate_fallback(drive: &str) -> Vec<EnumEntry> {
    const MAX_ENTRIES: usize = 200_000;
    const MAX_DEPTH: u32 = 8;
    const SKIP: [&str; 16] = [
        "Windows",
        "WinSxS",
        "System Volume Information",
        "$Recycle.Bin",
        "node_modules",
        "WindowsApps",
        "Program Files",
        "Program Files (x86)",
        "ProgramData",
        "$WinREAgent",
        "AppData",
        ".git",
        "Temp",
        "Packages",
        ".cache",
        "Cache",
    ];

    let mut entries = Vec::new();
    let root = PathBuf::from(format!("{drive}\\"));
    let mut stack: Vec<(PathBuf, u32)> = vec![(root, 0)];

    while let Some((dir, depth)) = stack.pop() {
        if entries.len() >= MAX_ENTRIES {
            break;
        }
        let Ok(read) = std::fs::read_dir(&dir) else {
            continue;
        };
        for item in read.flatten() {
            if entries.len() >= MAX_ENTRIES {
                break;
            }
            let path = item.path();
            let Ok(metadata) = item.metadata() else {
                continue;
            };
            let name = item.file_name().to_string_lossy().to_string();
            if name.starts_with('$') {
                continue;
            }
            let is_dir = metadata.is_dir();
            entries.push(EnumEntry {
                name: name.clone(),
                path: path.display().to_string(),
                is_dir,
                size: metadata.len(),
            });
            if is_dir && depth < MAX_DEPTH && !SKIP.contains(&name.as_str()) {
                stack.push((path, depth + 1));
            }
        }
    }
    entries
}

/// 在所有卷的索引中搜索
fn search(volumes: &[Arc<Mutex<Volume>>], query: &str) -> Vec<SearchHit> {
    let query = query.trim();
    if query.is_empty() {
        return Vec::new();
    }
    let limit = 2000usize;
    let mut hits: Vec<SearchHit> = Vec::new();
    // 每次查询一个匹配器（避免逐条分配）
    let mut matcher = matcher::new_matcher();
    let mut scratch = matcher::MatcherScratch::default();

    for volume in volumes {
        let volume = volume.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(tree) = &volume.tree {
            let mut tree = tree.lock().unwrap_or_else(|e| e.into_inner());
            // 先快照命中的节点，避免与 path_of 的可变借用冲突
            let matched: Vec<(u64, String, bool, u64)> = tree
                .iter()
                .filter(|(_, node)| !node.hidden && !node.name.is_empty())
                .filter(|(_, node)| {
                    let entry = MatchEntry::new(&node.name);
                    matcher::matches(&entry, query, &mut matcher, &mut scratch)
                })
                .map(|(frn, node)| (frn, node.name.clone(), node.is_dir, node.size))
                .collect();
            for (frn, name, is_dir, size) in matched {
                let Some(path) = tree.path_of(frn) else {
                    continue;
                };
                hits.push(SearchHit {
                    name,
                    path,
                    is_dir,
                    size,
                });
                if hits.len() >= limit {
                    return hits;
                }
            }
        } else if let Some(entries) = &volume.entries {
            for entry in entries {
                let candidate = MatchEntry::new(&entry.name);
                if !matcher::matches(&candidate, query, &mut matcher, &mut scratch) {
                    continue;
                }
                hits.push(SearchHit {
                    name: entry.name.clone(),
                    path: entry.path.clone(),
                    is_dir: entry.is_dir,
                    size: entry.size,
                });
                if hits.len() >= limit {
                    return hits;
                }
            }
        }
    }
    hits
}

/// 枚举固定磁盘，返回 "C:" 形式
#[cfg(windows)]
fn fixed_drives() -> Vec<String> {
    use windows::Win32::Storage::FileSystem::{GetDriveTypeW, GetLogicalDrives};

    let mask = unsafe { GetLogicalDrives() };
    let mut drives = Vec::new();
    for index in 0..26u32 {
        if mask & (1 << index) == 0 {
            continue;
        }
        let letter = (b'A' + index as u8) as char;
        let root = format!("{letter}:");
        let root_wide = crate::wstr::wide(&format!("{root}\\"));
        // DRIVE_FIXED = 3
        if unsafe { GetDriveTypeW(crate::wstr::pcw(&root_wide)) } == 3 {
            drives.push(root);
        }
    }
    if drives.is_empty() {
        drives.push("C:".to_string());
    }
    drives
}

/// 非 Windows 平台无磁盘可枚举
#[cfg(not(windows))]
fn fixed_drives() -> Vec<String> {
    Vec::new()
}
