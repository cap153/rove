//! 全盘文件索引（Phase 5）：后台线程递归扫描固定驱动器，
//! 构建拼音预计算的内存索引；查询走 RwLock 读锁，毫秒级返回。
//!
//! 范围控制：
//! - 跳过系统目录（Windows / Program Files* / $Recycle.Bin / System Volume
//!   Information / AppData）避免噪声与超长扫描；
//! - 总量上限 [`MAX_ENTRIES`]（达上限即停止，防止极端磁盘拖垮内存）。

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use super::pinyin_fields;

/// 索引条目上限（防失控）。MFT 路径下每条约 80B（路径 + 名称 + 拼音字段），
/// 100 万条约 80MB，足以覆盖绝大多数家用电脑的全盘文件量。
pub const MAX_ENTRIES: usize = 1_000_000;

/// 一条全局索引记录。
#[derive(serde::Serialize, serde::Deserialize)]
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
    /// 最近一次构建完成的时刻（用于设置页展示「上次同步」）。
    pub built_at: Option<std::time::SystemTime>,
    /// 常驻内存估算（含堆上字符串）。设置页的「索引大小」与 `ROVE_LOG=1`
    /// 的内存明细都用它——以前这里按 80 B/条估算，实测约 220 B/条，
    /// 显示出来的数字比真实占用小 3 倍，看着安心其实是假的。
    pub heap_bytes: usize,
}

/// 估算索引常驻内存：结构体内联 + 四条堆字符串（路径 / 名字 / 全拼 / 简拼）。
pub fn estimate_heap(entries: &[IndexEntry]) -> usize {
    let inline = std::mem::size_of::<IndexEntry>();
    entries
        .iter()
        .map(|e| {
            inline
                + e.path.as_os_str().len()
                + e.name.len()
                + e.fields.full.len()
                + e.fields.abbr.len()
        })
        .sum()
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
    /// 支持盘符限定前缀（如 `c: videos`）：非目标盘一票否决；仅输入盘符（如 `c:`）时返回该盘全部条目。
    pub fn search(&self, query: &str, limit: usize) -> SearchHits<'_> {
        self.search_filtered(query, limit, false)
    }

    /// 只返回文件夹的查询（吸附窗专用）。
    ///
    /// 关键点：**过滤必须发生在截断之前**。吸附窗只列文件夹，旧写法是先取
    /// 前 `limit` 名再筛 `is_dir`，于是同一个词命中一堆文件时（如 `power`
    /// 命中几十个 `power*.py`），这些文件会把分数相近的文件夹挤出前 `limit`
    /// 名，界面表现为「输入 power 一条文件夹都没有，输全名 powershell 才有」。
    pub fn search_dirs(&self, query: &str, limit: usize) -> SearchHits<'_> {
        self.search_filtered(query, limit, true)
    }

    fn search_filtered(&self, query: &str, limit: usize, dirs_only: bool) -> SearchHits<'_> {
        let (drive_filter, clean_query) = crate::search::parse_drive_prefix(query);
        let query_lower = clean_query.to_lowercase();

        if drive_filter.is_none() && query_lower.is_empty() {
            return SearchHits { hits: Vec::new(), total: 0 };
        }

        let mut matcher = nucleo_matcher::Matcher::new(nucleo_matcher::Config::DEFAULT);
        let mut bufs = (Vec::new(), Vec::new(), Vec::new());
        let mut scored: Vec<(&IndexEntry, u16)> = self
            .entries
            .iter()
            .filter_map(|e| {
                // 0. 只要文件夹时先筛掉文件：截断在排序之后，这里的过滤必须在
                //    收集阶段就完成，否则文件会占满 `limit` 个名额。
                if dirs_only && !e.is_dir {
                    return None;
                }
                // 1. 若指定了盘符，非目标盘一票否决
                if let Some(target_drive) = drive_filter {
                    let path_str = e.path.to_string_lossy();
                    let matches_drive = path_str.chars().next().is_some_and(|c| c.eq_ignore_ascii_case(&target_drive))
                        && path_str.chars().nth(1) == Some(':');
                    if !matches_drive {
                        return None;
                    }
                }

                // 2. 若仅输入了盘符（如 "c:"），直接返回该盘条目
                if query_lower.is_empty() {
                    Some((e, 100u16))
                } else {
                    crate::search::score(&e.name, &e.fields, &query_lower, &mut matcher, &mut bufs)
                        .map(|s| (e, s))
                }
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

/// 索引缓存文件头魔数（8 字节，后面紧跟 4 字节小端格式版本）。
///
/// **必须有**。以前 `index.bin` 就是裸露的 bincode 数据，而 bincode 不带字段名：
/// 只要 `IndexEntry` 的布局改一次（比如去掉反向定位表），旧数据就会被按新布局
/// 「错位解析」——上一条的 `indices` 长度被当成 `is_dir`，后面的字节再被当成
/// 下一条的 Vec 长度，于是启动时试图一次性分配几个 GB，界面永远出不来
/// （真机复现过两次，日志停在 `request_listing` 之后再无输出）。
/// 带上魔数与版本号后，旧文件在读任何条目之前就被判为过期，正常走重建。
const CACHE_MAGIC: [u8; 8] = *b"ROVEIDX1";
/// 缓存格式版本。**`IndexEntry` 布局有任何变化都必须 +1**。
const CACHE_VERSION: u32 = 2;

/// 全局索引单例（首次调用初始化，后台线程异步填充）。
static INDEX: std::sync::OnceLock<SharedIndex> = std::sync::OnceLock::new();
/// 构建轮次：新轮次启动会令旧轮次在下次检查时自行退出并丢弃结果。
static BUILD_EPOCH: AtomicU64 = AtomicU64::new(0);
/// 当前构建线程的停止开关（重新扫描时先置位旧线程）。
static CURRENT_STOP: Mutex<Option<Arc<AtomicBool>>> = Mutex::new(None);

/// 获取（必要时懒初始化）全局索引。首次调用会拉起后台构建线程。
pub fn shared() -> SharedIndex {
    INDEX
        .get_or_init(|| {
            let idx: SharedIndex = Arc::new(RwLock::new(GlobalIndex::default()));
            let cached = load_cache(&idx);
            if cached {
                // 缓存命中：立即向 UI 发送就绪通知，第一帧即亮绿灯！
                let _ = slint::invoke_from_event_loop(|| {
                    crate::app::on_index_ready();
                });
                // 校验时效：未超过空闲重扫周期则直接复用，不无端扫盘
                let config = crate::config::Config::load();
                let is_fresh = if config.search_idle_rescan_minutes > 0 {
                    idx.read()
                        .ok()
                        .and_then(|g| g.built_at)
                        .and_then(|t| std::time::SystemTime::now().duration_since(t).ok())
                        .is_some_and(|elapsed| {
                            elapsed < std::time::Duration::from_secs(config.search_idle_rescan_minutes as u64 * 60)
                        })
                } else {
                    true
                };
                if !is_fresh {
                    spawn_builder(idx.clone());
                }
            } else {
                spawn_builder(idx.clone());
            }
            idx
        })
        .clone()
}

/// 持久化缓存文件路径：%LOCALAPPDATA%\Rove\index.bin
pub fn cache_path() -> PathBuf {
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        PathBuf::from(local).join("Rove").join("index.bin")
    } else {
        std::env::temp_dir().join("Rove").join("index.bin")
    }
}

/// 获取磁盘上持久化索引文件的物理字节大小。
pub fn cache_file_size() -> Option<u64> {
    std::fs::metadata(cache_path()).ok().map(|m| m.len())
}

/// 将条目序列化写入暂存文件 index.bin.tmp，并原子重命名为 index.bin。
fn save_cache(entries: &[IndexEntry]) -> std::io::Result<()> {
    let path = cache_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let tmp_path = path.with_extension("bin.tmp");
    let file = std::fs::File::create(&tmp_path)?;
    let mut writer = std::io::BufWriter::new(file);
    writer.write_all(&CACHE_MAGIC)?;
    writer.write_all(&CACHE_VERSION.to_le_bytes())?;
    bincode::serialize_into(&mut writer, entries)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
    writer.flush()?;
    std::fs::rename(&tmp_path, &path)?;
    Ok(())
}

/// 启动时读取磁盘缓存，喂饱内存索引。
///
/// **整文件读进内存再从切片解析**，不用 `bincode::deserialize_from(reader)`：
/// 流式解析遇到格式错位的垃圾数据时，会把错误的长度前缀当成真实长度去预分配，
/// 启动线程就卡在那儿（真机复现过：日志停在 `request_listing` 之后再无输出，
/// 进程活着但窗口永远不出现）。切片解析的语义是「恰好读完全部字节」，
/// 任何错位都会立刻返回错误。
pub fn load_cache(idx: &SharedIndex) -> bool {
    let path = cache_path();
    let Ok(bytes) = std::fs::read(&path) else {
        return false;
    };
    let mtime = std::fs::metadata(&path).ok().and_then(|m| m.modified().ok());
    if bytes.len() < 12 {
        logln!("index: cache too short ({} B), rebuild", bytes.len());
        return false;
    }
    // 先验文件头：旧版本 / 别的程序写的文件一律不解析（见 CACHE_MAGIC 注释）。
    if bytes[..8] != CACHE_MAGIC {
        logln!("index: cache has no rove header (old format), rebuild");
        return false;
    }
    let ver = u32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]);
    if ver != CACHE_VERSION {
        logln!("index: cache format v{ver} != v{CACHE_VERSION}, rebuild");
        return false;
    }
    let Ok(mut entries): Result<Vec<IndexEntry>, _> = bincode::deserialize(&bytes[12..]) else {
        logln!("index: cache deserialize failed, rebuild");
        return false;
    };
    drop(bytes);
    // 兜底：解析成功也不该超过上限（截断而不是整个丢掉）。
    if entries.len() > MAX_ENTRIES {
        logln!("index: cache has {} entries > cap, truncating", entries.len());
        entries.truncate(MAX_ENTRIES);
        entries.shrink_to_fit();
    }
    let count = entries.len();
    let heap = estimate_heap(&entries);
    if let Ok(mut w) = idx.write() {
        w.entries = entries;
        w.ready = true;
        w.built_at = mtime.or_else(|| Some(std::time::SystemTime::now()));
        w.heap_bytes = heap;
    }
    logln!(
        "index: loaded {count} cached entries from disk (resident ~{} MB)",
        heap / (1024 * 1024)
    );
    true
}

/// 已初始化的索引句柄；尚未初始化时返回 `None`（**不会**触发构建）。
///
/// 供 UI 轮询「索引是否就绪」：不能为了显示一个状态提示就把整个
/// 全盘扫描拉起来，那会让从不搜索的用户也白白扫一遍盘。
pub fn try_shared() -> Option<SharedIndex> {
    INDEX.get().cloned()
}

/// 手动重建全局索引（设置页「重新扫描驱动器」）。
///
/// 先停掉当前构建线程并清空内存索引，再拉起新一轮后台构建；旧轮次即使在
/// 途也会因轮次过期而丢弃结果，不会把半成品写回 `ready`。
pub fn rebuild() -> SharedIndex {
    let idx = shared();
    // 置位旧线程并等待其释放写锁：随后的 clear 不会被旧轮次覆盖。
    if let Ok(mut cur) = CURRENT_STOP.lock() {
        if let Some(old) = cur.take() {
            old.store(true, Ordering::Relaxed);
        }
    }
    if let Ok(mut w) = idx.write() {
        // `clear()` 只把长度归零，旧的百万级容量数组会一直挂着：
        // 重建期等于新旧两份同时驻留。换成空 Vec 立刻把容量还给分配器。
        w.entries = Vec::new();
        w.heap_bytes = 0;
        w.ready = false;
        w.built_at = None;
    }
    spawn_builder(idx.clone());
    idx
}

fn spawn_builder(idx: SharedIndex) {
    let epoch = BUILD_EPOCH.fetch_add(1, Ordering::SeqCst) + 1;
    let stop = Arc::new(AtomicBool::new(false));
    // 登记新开关并停掉上一轮（若重建时未显式停止）。
    if let Ok(mut cur) = CURRENT_STOP.lock() {
        if let Some(old) = cur.replace(stop.clone()) {
            old.store(true, Ordering::Relaxed);
        }
    }
    let stop_thread = stop.clone();
    std::thread::Builder::new()
        .name("rove-indexer".into())
        .spawn(move || {
            let mut count = 0usize;
            let roots = list_roots();
            let config = crate::config::Config::load();
            let filter = std::sync::Arc::new(super::filter::IndexFilter::new(&config.search_excluded_paths));

            // 核心隔离：创建临时 staging 索引容器，构建过程完全不抢 live 读写锁
            let staging_idx: SharedIndex = Arc::new(RwLock::new(GlobalIndex::default()));

            // 任务 3.2：优先用 NTFS $MFT 加速全盘索引；无管理员权限或全盘
            // 失败时回退到原有 std::fs 递归 walker。
            // 两种路径都记录条目数与耗时，便于实机对比直读收益。
            let t0 = std::time::Instant::now();
            match crate::search::mft::build_via_mft(&staging_idx, &stop_thread, &filter) {
                Some(_) => {
                    // MFT 路径：重新统计条目数（build_via_mft 已写入）。
                    if let Ok(w) = staging_idx.read() {
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
                        if count >= MAX_ENTRIES || stop_thread.load(Ordering::Relaxed) {
                            break;
                        }
                        let n = walk(&root, &staging_idx, count, &stop_thread, &filter);
                        // 每个根目录单独记一行：某块盘没进索引时能立刻看出来。
                        logln!(
                            "index: walk {root:?} -> {n} entries (total {})",
                            count + n
                        );
                        count += n;
                    }
                    logln!(
                        "index: std::fs walk -> {} entries in {:.1} ms",
                        count,
                        t0.elapsed().as_secs_f64() * 1000.0
                    );
                }
            }

            if BUILD_EPOCH.load(Ordering::SeqCst) != epoch || stop_thread.load(Ordering::Relaxed) {
                logln!("index: build epoch {epoch} superseded or stopped, discard result");
                return;
            }

            // 提取构建完成的条目
            let Ok(mut staging_guard) = staging_idx.write() else { return; };
            let entries = std::mem::take(&mut staging_guard.entries);
            drop(staging_guard);

            // 原子落盘：写 index.bin.tmp -> 原地 rename 到 index.bin
            if let Err(e) = save_cache(&entries) {
                logln!("index: save cache failed: {e}");
            } else {
                crate::logv!("index: saved {} entries to disk cache", entries.len());
            }

            // 1 微秒原子提交：写入主索引 live 槽
            let now = std::time::SystemTime::now();
            let heap = estimate_heap(&entries);
            if let Ok(mut w) = idx.write() {
                w.entries = entries;
                w.ready = true;
                w.built_at = Some(now);
                w.heap_bytes = heap;
            }
            logln!(
                "global index built and committed: {} entries (~{} MB resident)",
                count,
                heap / (1024 * 1024)
            );
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
    let excluded = crate::config::Config::load().search_excluded_drives;
    let drives: Vec<PathBuf> = crate::fs::list_drives()
        .into_iter()
        .filter(|d| matches!(d.drive_type, crate::fs::drives::DriveType::Fixed))
        .filter(|d| {
            !excluded
                .iter()
                .any(|e| e.eq_ignore_ascii_case(&d.letter.to_string()))
        })
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
    "target",
    "my_program",
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

/// 归一化用户排除路径：去尾部斜杠、统一小写，便于大小写不敏感前缀比对。
///
/// 在构建开始时算一次，后续 `walk` / MFT 每条目只做纯字符串比较，
/// 避免百万级条目反复 `to_lowercase()` 用户配置。
/// 递归扫描；返回新增条目数。
///
/// 剪枝：隐藏目录、系统噪声目录（`is_skipped`）与用户/内置过滤规则
/// （`filter` 已预编译为 GlobSet）命中的子树一律跳过；文件额外按默认
/// 垃圾后缀与通配符拦截。
fn walk(
    dir: &Path,
    idx: &SharedIndex,
    count: usize,
    stop: &Arc<AtomicBool>,
    filter: &super::filter::IndexFilter,
) -> usize {
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
        let sub_path = entry.path();
        if ft.is_dir() {
            if is_skipped(&name) || filter.is_skipped_dir(&sub_path) {
                continue;
            }
            subdirs.push(sub_path);
        } else if filter.is_skipped_file(&sub_path) {
            // 核心新增：文件按默认垃圾后缀 + 用户通配符拦截。
            continue;
        }
        if count + added >= MAX_ENTRIES {
            break;
        }
        if stop.load(Ordering::Relaxed) {
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
        if count + added >= MAX_ENTRIES || stop.load(Ordering::Relaxed) {
            break;
        }
        added += walk(&sub, idx, count + added, stop, filter);
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

    /// 吸附窗只要文件夹：过滤必须在截断之前，否则文件占满名额会让文件夹消失。
    #[test]
    fn search_dirs_filters_before_truncating() {
        let mut idx = GlobalIndex::default();
        // 分数相同的一堆文件排在前面（sort_by 稳定，同分保持插入顺序）。
        for i in 0..60 {
            idx.entries.push(entry(&format!("power{i:02}")));
        }
        idx.entries.push(IndexEntry {
            path: PathBuf::from("D:\\docs\\PowerShell"),
            name: "PowerShell".to_string(),
            fields: crate::search::pinyin_fields("PowerShell"),
            is_dir: true,
        });
        // 前提：只取 1 条时被文件占住。
        let all = idx.search("power", 1);
        assert_eq!(all.hits.len(), 1);
        assert!(!all.hits[0].0.is_dir, "前提：文件应占满前几名");
        // 只要文件夹：同名文件夹必须还在。
        let dirs = idx.search_dirs("power", 1);
        assert_eq!(dirs.hits.len(), 1, "文件夹被文件挤出结果了");
        assert_eq!(dirs.hits[0].0.name, "PowerShell");
        assert_eq!(dirs.total, 1, "total 应是过滤后的文件夹总数");
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
            "target",
            "my_program",
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
    fn user_exclusions_align_on_separators() {
        // 空规则：不误伤任何路径。
        let empty = super::super::filter::IndexFilter::new(&[]);
        assert!(!empty.is_skipped_dir(Path::new(r"D:\other")));
        assert!(!empty.is_skipped_dir(Path::new(r"D:\Cache\x")));

        // 目录前缀（无通配符）自动追加 /** 匹配整棵子树，且大小写/分隔符不敏感。
        let f = super::super::filter::IndexFilter::new(&[
            "D:\\Cache".to_string(),
            "E:/Temp Files".to_string(),
        ]);
        assert!(f.is_skipped_dir(Path::new(r"d:\cache\a\b")));
        assert!(f.is_skipped_dir(Path::new(r"D:\Cache")));
        assert!(f.is_skipped_dir(Path::new(r"e:/temp files/x.txt")));
        assert!(!f.is_skipped_dir(Path::new(r"d:\cache-old\a")));
        assert!(!f.is_skipped_dir(Path::new(r"d:\other")));
    }

    #[test]
    fn filter_blocks_garbage_extensions() {
        // 默认垃圾后缀无论大小写均被拦截。
        let f = super::super::filter::IndexFilter::new(&[]);
        assert!(f.is_skipped_file(Path::new(r"D:\\a.LOG")));
        assert!(f.is_skipped_file(Path::new(r"D:\\b.tmp")));
        assert!(f.is_skipped_file(Path::new(r"D:\\c.crdownload")));
        assert!(!f.is_skipped_file(Path::new(r"D:\\src\\main.rs")));
        assert!(!f.is_skipped_file(Path::new(r"D:\\报告.docx")));
    }

    #[test]
    fn filter_builtin_system_dirs() {
        // 内置系统/缓存目录通配符对任意盘符、任意层级生效。
        let f = super::super::filter::IndexFilter::new(&[]);
        assert!(f.is_skipped_dir(Path::new(r"C:\Windows\System32\x.dll")));
        assert!(f.is_skipped_dir(Path::new(r"c:\windows\winsxs\y.dll")));
        assert!(f.is_skipped_dir(Path::new(r"D:\$Recycle.Bin\S-1-5-21\z.ini")));
        assert!(f.is_skipped_dir(Path::new(r"C:\System Volume Information\t.log")));
        assert!(f.is_skipped_dir(Path::new(r"C:\Users\me\AppData\Local\Temp\s.tmp")));
        assert!(f.is_skipped_dir(Path::new(r"C:\Program Files\Git\cmd\git.exe")));
        // 仅子串相同不应命中。
        assert!(!f.is_skipped_dir(Path::new(r"D:\MyWindows\backup.iso")));
        assert!(!f.is_skipped_dir(Path::new(r"D:\Projects\rove\src\main.rs")));
    }

    #[test]
    fn filter_user_glob_matches() {
        // 用户通配符（含 */?）按原样匹配。
        let f = super::super::filter::IndexFilter::new(&[
            "**/*.log".to_string(),
            r"D:\cache".to_string(),
        ]);
        assert!(f.is_skipped_file(Path::new(r"D:\x\y.log")));
        assert!(f.is_skipped_dir(Path::new(r"D:\cache\sub\a.bin")));
        assert!(!f.is_skipped_dir(Path::new(r"D:\cache-old\a.bin")));
        assert!(!f.is_skipped_dir(Path::new(r"D:\Projects\cache\a.bin")));
    }

    // appended-marker


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
