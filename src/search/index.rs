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

/// 索引条目上限（防失控）。落盘后每条约 60–90 B（名字 + 拼音各一份，路径靠
/// 父指针重建），100 万条约 60–90 MB 磁盘、常驻只占其中被查询碰到的那几页。
pub const MAX_ENTRIES: usize = 1_000_000;

/// 一条全局索引记录（**构建期**形态：带完整路径，MFT / walker 直接往里填）。
///
/// 落盘时会被压成「父指针 + 名字池」（见 [`encode`]）：路径不再逐条存，
/// 对外查询的那一份永远是映射文件，内存里不留全路径 —— 这是把常驻内存
/// 从"每条一份完整路径"降下来的关键。
#[derive(serde::Serialize, serde::Deserialize)]
pub struct IndexEntry {
    pub path: PathBuf,
    pub name: String,
    pub fields: crate::search::PinyinFields,
    pub is_dir: bool,
}

/// 一条定长记录占用的字节数（布局见 [`encode`]）。
const REC_SIZE: usize = 20;
/// `parent` 取这个值表示「名字槽里装的是完整路径」：盘符根与孤儿条目。
const REC_ROOT: u32 = u32::MAX;
/// 缓存文件头长度。
const HEADER_LEN: usize = 40;

/// 映射好的只读索引视图。
struct View {
    map: crate::search::mapfile::Mapped,
    /// 记录区起点（字节偏移）。
    records: usize,
    /// 字符串池起点（字节偏移）。
    pool: usize,
    count: u32,
    /// 构建时刻（Unix 秒）。
    built_unix: u64,
}

impl View {
    fn bytes(&self) -> &[u8] {
        self.map.bytes()
    }

    /// 第 `i` 条记录的定长字节。
    fn rec(&self, i: u32) -> &[u8] {
        let at = self.records + i as usize * REC_SIZE;
        &self.bytes()[at..at + REC_SIZE]
    }

    /// 记录里的字符串槽：池子里的 `u16 长度 + UTF-8`。
    fn slot(&self, off: u32) -> &str {
        let at = self.pool + off as usize;
        let b = self.bytes();
        if at + 2 > b.len() {
            return "";
        }
        let len = u16::from_le_bytes([b[at], b[at + 1]]) as usize;
        let from = at + 2;
        match b.get(from..from + len) {
            // 建库写入的都是按字符边界切好的 UTF-8，这里再校验一次更稳。
            Some(bytes) => std::str::from_utf8(bytes).unwrap_or(""),
            None => "",
        }
    }

    /// 一条记录的 (父下标, 名字或完整路径)。
    fn parts(&self, i: u32) -> (u32, &str) {
        let r = self.rec(i);
        (
            u32::from_le_bytes([r[0], r[1], r[2], r[3]]),
            self.slot(u32::from_le_bytes([r[4], r[5], r[6], r[7]])),
        )
    }

    /// 按父指针拼出完整路径。只对进入结果集的条目调用（≤ `limit` 条）。
    fn path_of(&self, mut id: u32) -> PathBuf {
        let mut parts: Vec<&str> = Vec::new();
        let mut base: Option<&str> = None;
        // 深度上限防脏数据成环。取 256：`C:\a\a\a\…` 这种极端深链仍能在
        // 260 字符的路径上限内凑满 100 多层，上限给小了会拼出**没有盘符的相对
        // 路径**（静默错数据）——所以超限时宁可不给路径。
        for _ in 0..256 {
            let (parent, text) = self.parts(id);
            if parent == REC_ROOT {
                base = Some(text);
                break;
            }
            parts.push(text);
            id = parent;
        }
        let Some(base) = base else {
            return PathBuf::new();
        };
        let mut out = String::with_capacity(base.len() + parts.iter().map(|p| p.len() + 1).sum::<usize>());
        out.push_str(base);
        for part in parts.iter().rev() {
            if !out.ends_with('\\') {
                out.push('\\');
            }
            out.push_str(part);
        }
        PathBuf::from(out)
    }
}

/// 查询期看到的一行：字段全是借用切片，零拷贝、零分配。
struct Row<'a> {
    id: u32,
    name: &'a str,
    full: &'a str,
    abbr: &'a str,
    is_dir: bool,
    /// 盘符 'A'..'Z'；0 = 未知。
    vol: u8,
}

/// 全局索引（后台构建，读多写少）。
#[derive(Default)]
pub struct GlobalIndex {
    /// 构建期条目（MFT / walker 往这里填）。提交后清空。
    pub entries: Vec<IndexEntry>,
    /// 已映射的只读索引：对外查询走这个。
    view: Option<View>,
    /// 索引是否已构建完成（false = 构建中 / 未加载）。
    pub ready: bool,
    /// 最近一次构建完成的时刻（用于设置页展示「上次同步」）。
    pub built_at: Option<std::time::SystemTime>,
    /// 索引占用 = 映射文件大小。真正驻留多少由系统按需分页决定（远小于此）。
    pub heap_bytes: usize,
}

impl GlobalIndex {
    /// 条目数（映射优先，其次构建期条目）。
    pub fn len(&self) -> usize {
        match &self.view {
            Some(v) => v.count as usize,
            None => self.entries.len(),
        }
    }

    /// 遍历所有条目（映射区直接读，或退化为内存条目）。
    fn for_each_row(&self, mut f: impl FnMut(Row<'_>)) {
        if let Some(v) = &self.view {
            for i in 0..v.count {
                let r = v.rec(i);
                f(Row {
                    id: i,
                    name: v.slot(u32::from_le_bytes([r[4], r[5], r[6], r[7]])),
                    full: v.slot(u32::from_le_bytes([r[8], r[9], r[10], r[11]])),
                    abbr: v.slot(u32::from_le_bytes([r[12], r[13], r[14], r[15]])),
                    is_dir: u16::from_le_bytes([r[16], r[17]]) & 1 != 0,
                    vol: r[18],
                });
            }
            return;
        }
        for (i, e) in self.entries.iter().enumerate() {
            f(Row {
                id: i as u32,
                name: &e.name,
                full: &e.fields.full,
                // 空简拼表示「与全拼相同」（纯 ASCII 名的快路径）。
                abbr: if e.fields.abbr.is_empty() { &e.fields.full } else { &e.fields.abbr },
                is_dir: e.is_dir,
                vol: drive_letter_of(&e.path),
            });
        }
    }

    /// 取一条命中条目的展示字段（只在收集结果时调用，条数 ≤ `limit`）。
    fn describe(&self, id: u32) -> (String, String, String, bool) {
        if let Some(v) = &self.view {
            let (parent, text) = v.parts(id);
            let r = v.rec(id);
            // 盘符根 / 孤儿条目的名字槽装的是完整路径，展示时只取末段。
            let name = if parent == REC_ROOT {
                text.rsplit('\\').find(|p| !p.is_empty()).unwrap_or(text).to_string()
            } else {
                text.to_string()
            };
            return (
                name,
                v.slot(u32::from_le_bytes([r[8], r[9], r[10], r[11]])).to_string(),
                v.slot(u32::from_le_bytes([r[12], r[13], r[14], r[15]])).to_string(),
                u16::from_le_bytes([r[16], r[17]]) & 1 != 0,
            );
        }
        match self.entries.get(id as usize) {
            Some(e) => (
                e.name.clone(),
                e.fields.full.to_string(),
                e.fields.abbr.to_string(),
                e.is_dir,
            ),
            None => (String::new(), String::new(), String::new(), false),
        }
    }

    fn path_of(&self, id: u32) -> PathBuf {
        match &self.view {
            Some(v) => v.path_of(id),
            None => self
                .entries
                .get(id as usize)
                .map(|e| e.path.clone())
                .unwrap_or_default(),
        }
    }

    /// 拼音/字面查询，按得分排序取前 `limit` 条，并回报未截断的命中总数。
    /// 支持盘符限定前缀（如 `c: videos`）：非目标盘一票否决；仅输入盘符时返回该盘全部条目。
    pub fn search(&self, query: &str, limit: usize) -> SearchHits {
        self.search_filtered(query, limit, false)
    }

    /// 只返回文件夹的查询（吸附窗专用）。
    ///
    /// 关键点：**过滤必须发生在截断之前**。吸附窗只列文件夹，先取前 `limit`
    /// 名再筛 `is_dir` 会让同名文件占满名额，界面表现为「输入 power 一条
    /// 文件夹都没有，输全名 powershell 才有」。
    pub fn search_dirs(&self, query: &str, limit: usize) -> SearchHits {
        self.search_filtered(query, limit, true)
    }

    fn search_filtered(&self, query: &str, limit: usize, dirs_only: bool) -> SearchHits {
        let (drive_filter, clean_query) = crate::search::parse_drive_prefix(query);
        let query_lower = clean_query.to_lowercase();

        if drive_filter.is_none() && query_lower.is_empty() {
            return SearchHits { hits: Vec::new(), total: 0 };
        }

        let want_drive = drive_filter.map(|c| c.to_ascii_uppercase() as u8);
        let mut matcher = nucleo_matcher::Matcher::new(nucleo_matcher::Config::DEFAULT);
        let mut bufs = (Vec::new(), Vec::new(), Vec::new());
        let mut scored: Vec<(u32, u16)> = Vec::new();
        self.for_each_row(|row| {
            // 0. 只要文件夹时先筛掉文件：过滤必须在收集阶段完成（见上文）。
            if dirs_only && !row.is_dir {
                return;
            }
            // 1. 指定了盘符时，非目标盘一票否决。
            if let Some(want) = want_drive {
                if row.vol != want {
                    return;
                }
            }
            let hit = if query_lower.is_empty() {
                Some(100u16)
            } else {
                crate::search::score_parts_ranked(
                    row.name,
                    row.abbr,
                    row.full,
                    &query_lower,
                    &mut matcher,
                    &mut bufs,
                    row.is_dir,
                )
            };
            if let Some(score) = hit {
                scored.push((row.id, score));
            }
        });
        scored.sort_by(|a, b| b.1.cmp(&a.1));
        let total = scored.len();
        scored.truncate(limit);
        let hits = scored
            .into_iter()
            .map(|(id, score)| {
                let (name, full, abbr, is_dir) = self.describe(id);
                Hit {
                    path: self.path_of(id),
                    name,
                    fields: crate::search::PinyinFields {
                        full: full.into(),
                        abbr: abbr.into(),
                    },
                    is_dir,
                    score,
                }
            })
            .collect();
        SearchHits { hits, total }
    }
}

/// 一条命中（路径已按需拼好，拼音字段是拷贝出来的，条数 ≤ `limit`）。
pub struct Hit {
    pub path: PathBuf,
    pub name: String,
    pub fields: crate::search::PinyinFields,
    pub is_dir: bool,
    pub score: u16,
}

/// 一次全局索引查询的结果。
pub struct SearchHits {
    /// 按得分降序的前 `limit` 条命中。
    pub hits: Vec<Hit>,
    /// 命中总数（**未截断**）。`hits.len() < total` 表示结果被 `limit` 截断，
    /// 调用方应据此在状态栏提示用户。
    pub total: usize,
}

/// 路径的盘符（大写）。非盘符路径返回 0。
fn drive_letter_of(path: &Path) -> u8 {
    let s = path.to_string_lossy();
    let mut it = s.chars();
    match (it.next(), it.next()) {
        (Some(c), Some(':')) => c.to_ascii_uppercase() as u8,
        _ => 0,
    }
}

/// 把字符串写进池子：`u16 长度 + UTF-8`，返回偏移。
fn push_slot(pool: &mut Vec<u8>, text: &str) -> u32 {
    let bytes = text.as_bytes();
    // 槽长度用 u16；Windows 单个文件名/路径远达不到 64 KB，极端情况截断。
    // **必须在字符边界上截断**：截在多字节字符中间会让整个池子不再是合法
    // UTF-8，加载时被判为损坏从而每次启动都重建（真机复现过）。
    let mut len = bytes.len().min(u16::MAX as usize);
    while len > 0 && !text.is_char_boundary(len) {
        len -= 1;
    }
    let off = pool.len() as u32;
    pool.extend_from_slice(&(len as u16).to_le_bytes());
    pool.extend_from_slice(&bytes[..len]);
    off
}

/// 内存条目 → 定长记录 + 字符串池。
///
/// 每条记录 20 字节（小端）：
///
/// ```text
/// 0..4   parent   父记录下标；u32::MAX = 名字槽里装的是完整路径
/// 4..8   name     名字（或完整路径）槽偏移
/// 8..12  full     全拼槽偏移
/// 12..16 abbr     简拼槽偏移（空串 = 与全拼相同）
/// 16..18 flags    bit0 = 目录
/// 18     vol      盘符 'A'..'Z'；0 = 未知
/// 19     pad
/// ```
///
/// 父指针让"每条都存一份完整路径"变成"每个目录存一次名字"：15 万个文件
/// 都挂在 `C:\Users\...\Videos\` 下时，那串前缀只存在于父记录里一次。
fn encode(entries: &[IndexEntry]) -> (Vec<u8>, Vec<u8>, u32) {
    use std::collections::HashMap;
    let mut dirs: HashMap<&Path, u32> = HashMap::with_capacity(entries.len() / 8 + 16);
    for (i, e) in entries.iter().enumerate() {
        if e.is_dir {
            dirs.insert(e.path.as_path(), i as u32);
        }
    }
    let mut recs: Vec<u8> = Vec::with_capacity(entries.len() * REC_SIZE);
    let mut pool: Vec<u8> = Vec::with_capacity(entries.len() * 24);
    for (i, e) in entries.iter().enumerate() {
        // 父目录不在索引里（被排除 / 没扫到）就当孤儿：整条存自己的完整路径。
        let parent = e
            .path
            .parent()
            .and_then(|p| dirs.get(p).copied())
            .filter(|&p| p != i as u32)
            .unwrap_or(REC_ROOT);
        let name = if parent == REC_ROOT {
            e.path.to_string_lossy()
        } else {
            e.name.as_str().into()
        };
        let name_off = push_slot(&mut pool, &name);
        let full_off = push_slot(&mut pool, &e.fields.full);
        let abbr_off = push_slot(&mut pool, &e.fields.abbr);
        recs.extend_from_slice(&parent.to_le_bytes());
        recs.extend_from_slice(&name_off.to_le_bytes());
        recs.extend_from_slice(&full_off.to_le_bytes());
        recs.extend_from_slice(&abbr_off.to_le_bytes());
        recs.extend_from_slice(&(e.is_dir as u16).to_le_bytes());
        recs.push(drive_letter_of(&e.path));
        recs.push(0);
    }
    (recs, pool, entries.len() as u32)
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
const CACHE_VERSION: u32 = 4;

/// 全局索引单例（首次调用初始化，后台线程异步填充）。
static INDEX: std::sync::OnceLock<SharedIndex> = std::sync::OnceLock::new();
/// 索引里是否已经有可用数据（加载完成 / 构建完成 / 明确是空范围）。
///
/// 与 `ready` 的区别：`ready` 表示「构建完了、结果可信」，卸载后也会置假；
/// 这个标记专门用来回答「要不要为一次搜索去读盘」，卸载与空范围两种情况
/// 都不会误判成「需要重新加载」。
static LOADED: AtomicBool = AtomicBool::new(false);
/// 是否有构建线程在跑（用于区分「构建中，直接查部分结果」和「没加载，去读盘」）。
static BUILDING: AtomicBool = AtomicBool::new(false);

/// 构建线程退出时清标记。
struct ClearBuilding;
impl Drop for ClearBuilding {
    fn drop(&mut self) {
        BUILDING.store(false, Ordering::Relaxed);
    }
}

/// 索引是否已有可用数据（见 [`LOADED`]）。
pub fn has_data() -> bool {
    LOADED.load(Ordering::Relaxed)
}

/// 是否有线程正在装载缓存（防止并发重复读盘）。
static LOADING: AtomicBool = AtomicBool::new(false);

/// 是否有构建线程正在跑。
pub fn is_building() -> bool {
    BUILDING.load(Ordering::Relaxed)
}

/// 构建轮次：新轮次启动会令旧轮次在下次检查时自行退出并丢弃结果。
static BUILD_EPOCH: AtomicU64 = AtomicU64::new(0);
/// 当前构建线程的停止开关（重新扫描时先置位旧线程）。
static CURRENT_STOP: Mutex<Option<Arc<AtomicBool>>> = Mutex::new(None);

/// 获取（必要时懒初始化）全局索引。首次调用会拉起后台构建线程。
pub fn shared() -> SharedIndex {
    INDEX
        .get_or_init(|| {
            let idx: SharedIndex = Arc::new(RwLock::new(GlobalIndex::default()));
            // 磁盘索引范围被关空（设置里把每个盘的开关都关了）：**不读缓存、也不扫描**。
            //
            // 以前这里无条件 `load_cache`，于是「明明关掉了所有盘的索引」进程
            // 启动后照样把几十上百兆的旧索引读进内存 —— 一条都用不上，纯白占。
            // 真机反馈过这一条（关掉索引仍近 200 MB），确实是我们没处理好。
            if list_roots().is_empty() {
                if let Ok(mut w) = idx.write() {
                    w.ready = true;
                    w.heap_bytes = 0;
                    w.built_at = Some(std::time::SystemTime::now());
                }
                LOADED.store(true, Ordering::Relaxed);
                logln!("index: no drive in scope, skip cache load (0 entries)");
                let _ = slint::invoke_from_event_loop(|| {
                    crate::app::on_index_ready();
                });
                return idx;
            }
            let cached = load_cache(&idx);
            if cached {
                // 缓存命中：立即向 UI 发送就绪通知，第一帧即亮绿灯！
                let _ = slint::invoke_from_event_loop(|| {
                    crate::app::on_index_ready();
                });
                // 校验时效：未超过空闲重扫周期则直接复用，不无端扫盘
                if !cache_is_fresh(&idx) {
                    spawn_builder(idx.clone());
                }
            } else {
                spawn_builder(idx.clone());
            }
            idx
        })
        .clone()
}

/// 按需加载：第一次真正要用索引时才把缓存读进内存（后台线程）。
///
/// 启动不再预热。全盘搜索大多不常用，而 14.8 万条的索引常驻就是三十多兆，
/// 上百万条时两百多兆 —— 用不上的人不该替它付这笔钱。什么时候要：
///
/// * 用户第一次做全盘搜索（`run_global_search`）；
/// * 用户打开设置 →「搜索与索引」看板（那里要显示真实条目数）。
///
/// 读盘期间界面照常响应，读完经 `on_index_ready` 通知，正在输入的那次全盘
/// 搜索会被自动补查一次（`refresh_after_index_ready`），用户感觉不到断档。
pub fn warm_up_async() {
    if has_data() || is_building() || LOADING.load(Ordering::Relaxed) {
        return;
    }
    let spawned = std::thread::Builder::new()
        .name("rove-index-load".into())
        .spawn(ensure_loaded);
    if let Err(e) = spawned {
        logln!("!! index: lazy load thread SPAWN FAILED: {e}");
        // 兜底：线程起不来就同步加载，慢一点也比搜不出东西强。
        ensure_loaded();
    }
}

/// 确保索引已经装进内存：缓存命中就把缓存读进来并通知界面，否则拉起后台构建。
///
/// 幂等。**不能只靠 `shared()`**：`OnceLock` 初始化过一次之后，即使后来
/// 被 [`unload`] 清空，`shared()` 也只会把那个空索引原样还回来 —— 那样
/// 「闲置卸载后再次搜索」就会搜出 0 条。这里每次都按真实状态判断。
pub fn ensure_loaded() {
    if has_data() || is_building() {
        return;
    }
    // 同一时刻只允许一个线程在装载：多按几下搜索不该把几十兆读两遍。
    if LOADING.swap(true, Ordering::SeqCst) {
        return;
    }
    let _guard = ClearLoading;
    let idx = shared();
    if has_data() || is_building() {
        return;
    }
    if load_cache(&idx) {
        notify_ready();
        // 与首次初始化一致：缓存过期了就顺手重扫，别让用户一直吃旧索引。
        if !cache_is_fresh(&idx) {
            spawn_builder(idx);
        }
    } else {
        spawn_builder(idx);
    }
}

/// 磁盘缓存是否还在「空闲重扫周期」之内（周期为 0 表示永不因过期重扫）。
fn cache_is_fresh(idx: &SharedIndex) -> bool {
    let config = crate::config::Config::load();
    if config.search_idle_rescan_minutes == 0 {
        return true;
    }
    idx.read()
        .ok()
        .and_then(|g| g.built_at)
        .and_then(|t| std::time::SystemTime::now().duration_since(t).ok())
        .is_some_and(|elapsed| {
            elapsed < std::time::Duration::from_secs(config.search_idle_rescan_minutes as u64 * 60)
        })
}

/// 通知界面「索引可用了」。
fn notify_ready() {
    let _ = slint::invoke_from_event_loop(|| {
        crate::app::on_index_ready();
    });
}

/// 装载线程退出时清标记。
struct ClearLoading;
impl Drop for ClearLoading {
    fn drop(&mut self) {
        LOADING.store(false, Ordering::SeqCst);
    }
}

/// 索引最近一次被真正使用（全盘搜索 / 吸附窗检索）的时刻。
///
/// 空闲卸载只看这一个时间戳：它比「谁来装、谁来卸」更重要 —— 主窗口和
/// 吸附窗（`--background` 常驻时可能只有它）共用同一份判定。
static LAST_TOUCH: std::sync::OnceLock<std::sync::Mutex<std::time::Instant>> =
    std::sync::OnceLock::new();

/// 记一次「索引被用过」。
pub fn touch() {
    let slot = LAST_TOUCH.get_or_init(|| std::sync::Mutex::new(std::time::Instant::now()));
    if let Ok(mut t) = slot.lock() {
        *t = std::time::Instant::now();
    }
}

/// 距离上一次使用过了多久；从没被用过返回 `None`。
pub fn idle_for() -> Option<std::time::Duration> {
    LAST_TOUCH.get()?.lock().ok().map(|t| t.elapsed())
}

/// 把内存里的索引还给系统（磁盘缓存 `index.bin` 原样保留，下次用再读）。
///
/// 只在空闲足够久之后调用。多数人一天也用不上一次全盘搜索，而映射着的文件
/// 会让系统一直替它保留一部分页缓存。再次搜索时重新映射是瞬间完成的
/// （不解析、不复制），所以这里不需要保守。
///
/// 正在构建时（`ready == false`）不动：那会儿条目还是一半的，清了反而添乱。
pub fn unload() {
    let Some(idx) = INDEX.get() else {
        return;
    };
    let Ok(mut w) = idx.write() else {
        return;
    };
    if !w.ready || w.len() == 0 {
        return;
    }
    let freed = w.heap_bytes;
    w.view = None;
    w.entries = Vec::new();
    w.heap_bytes = 0;
    w.ready = false;
    w.built_at = None;
    LOADED.store(false, Ordering::Relaxed);
    drop(w);
    logln!(
        "index: unloaded, released {:.1} MB mapping (disk cache kept)",
        freed as f64 / (1024.0 * 1024.0)
    );
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

/// 把索引写成「定长记录 + 字符串池」并原子落盘（`index.bin.tmp` → `index.bin`）。
///
/// 头 40 字节：魔数 8 + 版本 4 + 条数 4 + 记录长度 4 + 保留 4 + 池偏移 8 + 构建时刻 8。
fn save_cache(entries: &[IndexEntry]) -> std::io::Result<()> {
    let (recs, pool, count) = encode(entries);
    let path = cache_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let tmp_path = path.with_extension("bin.tmp");
    let mut writer = std::io::BufWriter::new(std::fs::File::create(&tmp_path)?);
    let built_unix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    writer.write_all(&CACHE_MAGIC)?;
    writer.write_all(&CACHE_VERSION.to_le_bytes())?;
    writer.write_all(&count.to_le_bytes())?;
    writer.write_all(&(REC_SIZE as u32).to_le_bytes())?;
    writer.write_all(&0u32.to_le_bytes())?;
    writer.write_all(&((HEADER_LEN + recs.len()) as u64).to_le_bytes())?;
    writer.write_all(&built_unix.to_le_bytes())?;
    writer.write_all(&recs)?;
    writer.write_all(&pool)?;
    writer.flush()?;
    drop(writer);
    std::fs::rename(&tmp_path, &path)?;
    Ok(())
}

fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

fn u64_at(b: &[u8], at: usize) -> u64 {
    let mut v = [0u8; 8];
    v.copy_from_slice(&b[at..at + 8]);
    u64::from_le_bytes(v)
}

/// 打开并校验映射（不解析、不复制）。
///
/// 任何一项对不上（旧版本 / 半截文件 / 被别的程序改写）都返回 `None`，
/// 调用方据此重建 —— 与缓存头魔数的初衷一致。
fn open_view(path: &Path) -> Option<View> {
    let map = crate::search::mapfile::Mapped::open(path).ok().flatten()?;
    let len = map.bytes().len();
    if len < HEADER_LEN {
        logln!("index: cache too short ({len} B), rebuild");
        return None;
    }
    // 校验只在借用期内做，随后把 `map` 移进 `View`（借用结束才能 move）。
    let (count, pool, built_unix) = {
    let bytes = map.bytes();
    if bytes[..8] != CACHE_MAGIC {
        logln!("index: cache has no rove header (old format), rebuild");
        return None;
    }
    let ver = u32_at(bytes, 8);
    if ver != CACHE_VERSION {
        logln!("index: cache format v{ver} != v{CACHE_VERSION}, rebuild");
        return None;
    }
    let rec_size = u32_at(bytes, 16) as usize;
    if rec_size != REC_SIZE {
        logln!("index: cache record size {rec_size} != {REC_SIZE}, rebuild");
        return None;
    }
    let mut count = u32_at(bytes, 12);
    let pool = u64_at(bytes, 24) as usize;
    let records_end = HEADER_LEN.checked_add(count as usize * REC_SIZE)?;
    if pool < records_end || pool > len {
        logln!("index: cache layout invalid (pool {pool}, records end {records_end}, len {len}), rebuild");
        return None;
    }
    if count as usize > MAX_ENTRIES {
        logln!("index: cache has {count} entries > cap, truncating view");
        count = MAX_ENTRIES as u32;
    }
    // 注意：**不能**把整池当成一个字符串校验 —— 池是「u16 长度 + 文本」的
    // 拼接，长度前缀本身就是任意字节，整池必然含非法 UTF-8 序列（踩过这个坑）。
    // 每个槽在读取时各自校验（见 `View::slot`）。
    (count, pool, u64_at(bytes, 32))
    };
    Some(View {
        map,
        records: HEADER_LEN,
        pool,
        count,
        built_unix,
    })
}

/// 磁盘缓存文件里的统计信息（只读文件头，不映射、不占内存）。
pub struct CacheMeta {
    /// 缓存里有多少条记录。
    pub count: usize,
    /// 上次构建的时刻（缓存头里带的 unix 秒）。
    pub built_at: Option<std::time::SystemTime>,
    /// 缓存文件大小。
    pub bytes: u64,
}

/// 读缓存文件的头，拿「有多少条 / 上次什么时候建 / 文件多大」。
///
/// 给设置页看板用：那里的数字是**展示**用的，不该为了让数字好看就把几十兆
/// 索引映射进内存（更不该因此阻止空闲释放）。文件不存在 / 版本不对 → `None`。
pub fn cache_meta() -> Option<CacheMeta> {
    use std::io::Read;
    let path = cache_path();
    let bytes = std::fs::metadata(&path).ok()?.len();
    let mut header = [0u8; HEADER_LEN];
    std::fs::File::open(&path).ok()?.read_exact(&mut header).ok()?;
    parse_cache_header(&header, bytes)
}

/// 校验缓存头并取出统计数据（拆出来是为了能不碰磁盘做单元测试）。
fn parse_cache_header(header: &[u8], bytes: u64) -> Option<CacheMeta> {
    if header.len() < HEADER_LEN || bytes < HEADER_LEN as u64 {
        return None;
    }
    if header[..8] != CACHE_MAGIC || u32_at(header, 8) != CACHE_VERSION {
        return None;
    }
    if u32_at(header, 16) as usize != REC_SIZE {
        return None;
    }
    let mut count = u32_at(header, 12) as usize;
    if count > MAX_ENTRIES {
        count = MAX_ENTRIES as usize;
    }
    let built_unix = u64_at(header, 32);
    let built_at = std::time::UNIX_EPOCH.checked_add(std::time::Duration::from_secs(built_unix));
    Some(CacheMeta {
        count,
        built_at,
        bytes,
    })
}

/// 启动 / 按需加载：把磁盘缓存**映射**进来（不读进内存）。
pub fn load_cache(idx: &SharedIndex) -> bool {
    let path = cache_path();
    let Some(view) = open_view(&path) else {
        return false;
    };
    let count = view.count;
    let bytes = view.map.bytes().len();
    let mtime = std::fs::metadata(&path).ok().and_then(|m| m.modified().ok());
    let built = std::time::UNIX_EPOCH
        .checked_add(std::time::Duration::from_secs(view.built_unix))
        .or(mtime)
        .or_else(|| Some(std::time::SystemTime::now()));
    if let Ok(mut w) = idx.write() {
        w.view = Some(view);
        w.ready = true;
        w.built_at = built;
        w.heap_bytes = bytes;
    }
    LOADED.store(true, Ordering::Relaxed);
    logln!(
        "index: mapped {count} cached entries from disk ({:.1} MB file, resident pages on demand)",
        bytes as f64 / (1024.0 * 1024.0)
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
    // 检索范围为空（设置里把所有磁盘都关了）：什么都不做。
    // 以前这里会把内存索引清空、再让构建线程写出一个 40 字节的空缓存 ——
    // 用户只是临时关掉开关点了一次「重新扫描」，之前扫好的索引就被覆盖没了，
    // 重新勾上磁盘还得从头全盘扫一遍。现在直接保持现状（映射和缓存都不动）。
    if list_roots().is_empty() {
        logln!("index: rebuild skipped, no drive in scope (cache kept)");
        return idx;
    }
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
        w.view = None;
        w.heap_bytes = 0;
        w.ready = false;
        w.built_at = None;
    }
    spawn_builder(idx.clone());
    idx
}

fn spawn_builder(idx: SharedIndex) {
    // 没有盘在检索范围内就不构建：不拆旧映射、不写空缓存（见 `rebuild`）。
    if list_roots().is_empty() {
        logln!("index: build skipped, no drive in scope (cache kept)");
        return;
    }
    let epoch = BUILD_EPOCH.fetch_add(1, Ordering::SeqCst) + 1;
    BUILDING.store(true, Ordering::Relaxed);
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
            let _clear_building = ClearBuilding;
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

            // 覆盖 index.bin 之前必须先丢掉旧映射：Windows 不允许替换一个还有
            // 活动映射的文件（改名前一次 rebuild 就白干了）。这里有约 0.2 s
            // 的空窗期，但 `BUILDING` 仍为真，搜索只会看到"构建中"。
            if let Ok(mut w) = idx.write() {
                w.view = None;
                w.entries = Vec::new();
            }

            // 原子落盘：写 index.bin.tmp -> 原地 rename 到 index.bin
            let saved = match save_cache(&entries) {
                Ok(()) => {
                    crate::logv!("index: saved {} entries to disk cache", entries.len());
                    true
                }
                Err(e) => {
                    logln!("!! index: save cache failed: {e}");
                    false
                }
            };

            // 提交。正常路径：把刚写好的文件映射进来（不读进内存），构建期的
            // 条目随即丢掉。落盘或映射任一步失败都**不能**提交一个空索引 ——
            // 那会让之后每次搜索都返回 0 条，而状态栏还显示「已就绪」。这种
            // 情况退回构建期条目，代价只是常驻内存。
            let now = std::time::SystemTime::now();
            let view = if saved { open_view(&cache_path()) } else { None };
            if view.is_none() && saved {
                logln!("!! index: mapping the fresh cache failed, keeping in-memory entries");
            }
            let mapped = view.as_ref().map(|v| v.bytes().len()).unwrap_or(0);
            let have_view = view.is_some();
            if let Ok(mut w) = idx.write() {
                w.view = view;
                if have_view {
                    w.entries = Vec::new();
                    w.heap_bytes = mapped;
                } else {
                    w.entries = entries;
                    w.heap_bytes = 0;
                }
                w.ready = true;
                w.built_at = Some(now);
            }
            LOADED.store(true, Ordering::Relaxed);
            logln!(
                "global index built and committed: {} entries ({:.1} MB on disk, mapped={})",
                count,
                mapped as f64 / (1024.0 * 1024.0),
                have_view
            );
            // 通知 UI 索引就绪。用 Slint 原生跨线程投递，而非定时器轮询：
            // 零空转开销，且就绪与刷新落在同一事件循环 tick（微秒级，无视觉延迟）。
            // 事件循环尚未启动时会返回 Err——那种情况下用户还无法交互，
            // 其后的搜索自然会拿到全量结果，故安全忽略。
            let _ = slint::invoke_from_event_loop(|| {
                crate::app::on_index_ready();
            });
        })
        .map_err(|e| {
            // 线程起不来 = 这一轮构建根本不存在。不留着 `BUILDING`，
            // 否则之后每次搜索都会被当成「构建中，先查部分结果」，永远是空的。
            BUILDING.store(false, Ordering::Relaxed);
            logln!("!! index: builder thread SPAWN FAILED: {e}");
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
    // 系统目录
    "windows",
    "program files",
    "program files (x86)",
    "$recycle.bin",
    "system volume information",
    "appdata",
    // 构建产物 / 依赖目录
    "node_modules",
    "target",
    "my_program",
    // 注：**以 '.' 开头的目录不用在这里列举** —— `is_skipped` 里有统一规则，
    // `.git` / `.cargo` / `.rustup` / `.cache` / `.npm` / `.local` 这些一次覆盖。
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
    // 以 '.' 开头的目录**一律**当噪声：开发工具缓存（.cargo / .rustup / .npm /
    // .deno / .cache / .local / .codebuddy…）与版本库元数据（.git/.hg/.svn）全落在
    // 这一类，逐个维护名单既维护不完，也总会漏。用户真要看这些目录，直接进目录
    // 看或者用当前目录的筛选就行 —— 受影响的只有「全盘索引搜得到搜不到」。
    // 点开头的**文件**不受影响（剪枝只看目录名）。
    if name.starts_with('.') {
        return true;
    }
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

/// 一次回传给 UI 的条数。
///
/// 注意：这**不是**「往下滚一页再来 300 条」的分页，后台线程会一批接一批地
/// 一直送到 `is_done()`（上限或扫完）为止。分这么多批只为了两件事：
/// 第一批几百毫秒内就能上屏、且每批之间让 UI 线程喘口气（反思：以前注释
/// 写「滚到底再放出 300 条」，跟实际行为不符，读者会以为结果能无限续下去）。
pub const DEEP_BATCH: usize = 300;
/// 单批最多扫多少个条目：保证第一批够快、按键盘时不断流。
const DEEP_BATCH_SCAN: usize = 20_000;

/// 一批最多干这么久。**这不是性能调优，是响应性问题**：网络盘 / 机械盘上
/// 光是 `read_dir` 翻两万个条目就可能要几十秒，期间工作线程不回到主循环，
/// 新请求（用户接着打下一个字）和取消都排不上队 —— 真机表现是输入框已经
/// 写着 `ziyan`，列表却停在上一轮的结果上二十多秒不动。
///
/// 切成 60ms 一片之后，无论盘多慢，工作线程至少每 60ms 回一次主循环，
/// 请求最多晚 60ms 被接手，进度也每 60ms 刷新一次。
const DEEP_SLICE: std::time::Duration = std::time::Duration::from_millis(60);
/// 一个查询累计最多扫多少条目（在大盘根目录上搜索不至于跑十几分钟）。
const DEEP_MAX_ENTRIES: usize = 500_000;
/// 「低档命中」的配额：子串/简拼/全拼这些只是**沾边**的结果，
/// 一千条足够翻了（Pulse 的全局搜索干脆只给 200 条）。
///
/// 高档命中（整串相等 / 名字以查询开头）**不占这个配额**：用户按前缀搜自己
/// 命名的一批视频，那是「把这一堆找齐」的正经用法，不该被当成噪音掐掉。
pub const DEEP_MAX_RESULTS: usize = 1_000;
/// 绝对上限（高档命中也算）：防的是搜单个 `a` 时整棵树几乎全命中、把内存
/// 和列表重建拖垮。1 万条已经远超一次能处理的量。
pub const DEEP_MAX_TOTAL: usize = 10_000;
/// 递归深度上限（防 junction / 挂载点绕成环）。
const DEEP_MAX_DEPTH: usize = 16;

/// 可中断、可续跑的深度搜索（文件夹视图的子树递归）。
///
/// 以前是全有全无：一次走完（超过 20k 条目或攒到 300 条就停），一趟就完。
/// 现在拆成状态机：队列里存**待扫描的目录**，每调一次 `next_batch` 就往前推
/// 一批，扫完（或到上限）才 `done`。这样批次之间可以中断——用户改词时不
/// 必等整棵树扫完，旧扫描直接丢掉。
///
/// 遍历用**深度优先**（后进先出）：名字在很深的子目录里（`合集\2024\03\bengugu`）
/// 也只要扫过同一条路径上的几层就能碰到，不会被同一层里的大目录挡在后面。
/// 顺序本身不影响排序 —— 结果按得分重排，精确同名的那条一出现就跳到第一行。
pub struct DeepScan {
    query: String,
    /// 名字本身命中查询的目录：**优先深入**。用户要找的东西常常就在同名/近名
    /// 文件夹里，先进这些目录就能早点把高分结果放出来，而不是等整盘扫完
    /// （真机：整盘搜 `ziyan` 头一条要等 10 秒、扫过 6.4 万项）。
    hot: std::collections::VecDeque<(PathBuf, usize)>,
    /// 其余目录（原来的深度优先队列）。
    cold: std::collections::VecDeque<(PathBuf, usize)>,
    /// 正在读的那个目录（+ 它的深度）。批次在目录中间用完时**保留这个迭代器**，
    /// 下一批从断点继续——否则重新 `read_dir` 会把已经返回过的条目再报一遍。
    cur: Option<(std::fs::ReadDir, usize)>,
    /// 累计扫过的条目数（跨批次）。
    scanned: usize,
    /// 累计回传过的条数（跨批次，含高档命中）。
    emitted: usize,
    /// 其中「低档命中」的条数：只有这部分受 `DEEP_MAX_RESULTS` 约束。
    weak: usize,
    done: bool,
    matcher: nucleo_matcher::Matcher,
    bufs: (Vec<char>, Vec<char>, Vec<char>),
}

impl DeepScan {
    /// `root` 的直接子项不返回（它们就在当前视图里，由本地过滤负责），
    /// 只把 root 的子目录排进队列。
    pub fn new(root: &Path, query_lower: &str) -> Self {
        let mut hot = std::collections::VecDeque::new();
        let mut cold = std::collections::VecDeque::new();
        let mut matcher = nucleo_matcher::Matcher::new(nucleo_matcher::Config::DEFAULT);
        let mut bufs = (Vec::new(), Vec::new(), Vec::new());
        if let Ok(entries) = std::fs::read_dir(root) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                if !is_walkable_dir(&entry, &name) {
                    continue;
                }
                let bucket = if dir_name_matches(&name, query_lower, &mut matcher, &mut bufs) {
                    &mut hot
                } else {
                    &mut cold
                };
                bucket.push_back((entry.path(), 1));
            }
        }
        Self {
            query: query_lower.to_string(),
            done: hot.is_empty() && cold.is_empty(),
            hot,
            cold,
            cur: None,
            scanned: 0,
            emitted: 0,
            weak: 0,
            matcher,
            bufs,
        }
    }

    /// 还要不要再扫：队列空了**且**手上没有读一半的目录，或到了累计上限。
    ///
    /// 「队列空」不等于「扫完」：批次可能正好在一个目录中间用完，那个目录的
    /// 迭代器还在 `cur` 里等着继续。
    pub fn is_done(&self) -> bool {
        self.done || self.capped() || (self.hot.is_empty() && self.cold.is_empty() && self.cur.is_none())
    }

    /// 撞到上限了（扫的条目太多 / 低档结果太多 / 总量太多）。
    fn capped(&self) -> bool {
        self.scanned >= DEEP_MAX_ENTRIES
            || self.weak >= DEEP_MAX_RESULTS
            || self.emitted >= DEEP_MAX_TOTAL
    }

    /// 已经扫过多少个条目（跨批次）。界面拿它显示「搜到哪了」。
    pub fn scanned(&self) -> usize {
        self.scanned
    }

    /// 是不是被上限掐停的（而不是真的扫完了）。状态栏要据此说明结果不完整。
    pub fn hit_cap(&self) -> bool {
        self.capped()
    }

    /// 续跑一批：最多返回 `want` 条，最多扫 `DEEP_BATCH_SCAN` 个条目。
    ///
    /// 扫到 `want` 条就停，剩下的目录 / 目录里剩下的条目留到下一次调用 ——
    /// 这也是「改词立刻换扫描」能生效的原因：被中断时丢掉的只是队列，
    /// 已经回传的结果不受影响。
    pub fn next_batch(&mut self, want: usize) -> Vec<DeepItem> {
        let started = std::time::Instant::now();
        let mut out: Vec<DeepItem> = Vec::new();
        let mut scanned_here = 0usize;
        // 本批里「低档命中」的条数。
        let mut weak_here = 0usize;
        loop {
            if scanned_here >= DEEP_BATCH_SCAN || self.capped() || started.elapsed() >= DEEP_SLICE {
                break;
            }
            if self.cur.is_none() {
                // 名字命中查询的目录先走完，再回到普通目录。
                let next = self.hot.pop_front().or_else(|| self.cold.pop_front());
                let Some((dir, depth)) = next else {
                    self.done = true;
                    break;
                };
                match std::fs::read_dir(&dir) {
                    Ok(iter) => self.cur = Some((iter, depth)),
                    Err(_) => continue,
                }
            }
            let Some((iter, depth)) = self.cur.as_mut() else {
                break;
            };
            let mut exhausted = false;
            let mut quota_hit = false;
            loop {
                if scanned_here >= DEEP_BATCH_SCAN
                    || self.scanned >= DEEP_MAX_ENTRIES
                    || started.elapsed() >= DEEP_SLICE
                {
                    break;
                }
                let Some(Ok(entry)) = iter.next() else {
                    exhausted = true;
                    break;
                };
                let name = entry.file_name().to_string_lossy().into_owned();
                let Ok(ft) = entry.file_type() else {
                    continue;
                };
                let path = entry.path();
                let depth = *depth;
                self.scanned += 1;
                scanned_here += 1;
                // 中文/日文查询只走字面匹配，拼音串一点用不上：这里就别为每个
                // 条目（含子目录）现算一遍拼音了。整盘遍历时这是最贵的一步。
                let fields = if self.query.is_ascii() {
                    crate::search::pinyin_fields(&name)
                } else {
                    crate::search::PinyinFields::default()
                };
                if ft.is_dir() && is_walkable_dir(&entry, &name) && depth < DEEP_MAX_DEPTH {
                    // 目录名本身就命中查询（`紫颜` 之于 `ziyan`）→ 丢进热队列，
                    // 下一个就进它；否则照旧深度优先（插队头，先把子树走完）。
                    let hot = crate::search::matches_query(
                        &name,
                        &fields,
                        &self.query,
                        &mut self.matcher,
                        &mut self.bufs,
                    );
                    let bucket = if hot { &mut self.hot } else { &mut self.cold };
                    bucket.push_front((path.clone(), depth + 1));
                }
                // 打分（而不是只问「匹配吗」）：要区分高档命中（整串相等 / 名字
                // 以查询开头）和只是沾边的低档命中，两者的配额不同。
                let Some(score) = crate::search::score_ranked(
                    &name,
                    &fields,
                    &self.query,
                    &mut self.matcher,
                    &mut self.bufs,
                    ft.is_dir(),
                ) else {
                    if out.len() >= want {
                        break;
                    }
                    continue;
                };
                let strong = score >= crate::search::SCORE_PREFIX;
                if !strong && self.weak + weak_here >= DEEP_MAX_RESULTS {
                    // 低档配额用完：连这一批也不用再扫了（否则还要白翻两万个
                    // 条目、把后面的边角料全过滤一遍才返回）。
                    quota_hit = true;
                    break;
                }
                let meta = std::fs::symlink_metadata(&path).ok();
                let size = if ft.is_dir() {
                    0
                } else {
                    meta.as_ref().map(|m| m.len()).unwrap_or(0)
                };
                let modified = meta.and_then(|m| m.modified().ok());
                // 绝对上限卡在**条**上，不是卡在批次边界上：否则最后一批会整批
                // 塞进来（上限 1 万时会跑到 10 200 条）。
                if self.emitted + out.len() >= DEEP_MAX_TOTAL {
                    break;
                }
                if !strong {
                    weak_here += 1;
                }
                out.push(DeepItem {
                    entry: crate::fs::FileEntry::from_path(&path, ft.is_dir(), size, modified, false),
                    fields,
                });
                if out.len() >= want {
                    break;
                }
            }
            if exhausted {
                self.cur = None;
            }
            if out.len() >= want || quota_hit {
                break;
            }
        }
        if self.cur.is_none() && self.hot.is_empty() && self.cold.is_empty() {
            self.done = true;
        }
        if self.capped() {
            self.done = true;
        }
        self.emitted += out.len();
        self.weak += weak_here;
        out
    }
}

/// 目录名本身是不是命中查询（命中就往热队列放，优先深入）。
fn dir_name_matches(
    name: &str,
    query_lower: &str,
    matcher: &mut nucleo_matcher::Matcher,
    bufs: &mut (Vec<char>, Vec<char>, Vec<char>),
) -> bool {
    let fields = crate::search::pinyin_fields(name);
    crate::search::matches_query(name, &fields, query_lower, matcher, bufs)
}

/// 目录是否值得继续往下走：隐藏目录与系统噪声目录不进入（与全局索引一致）。
fn is_walkable_dir(entry: &std::fs::DirEntry, name: &str) -> bool {
    if entry.file_type().map(|t| !t.is_dir()).unwrap_or(true) {
        return false;
    }
    // 点开头与系统噪声名单都在 `is_skipped` 里，这里不必重复判断。
    !is_skipped(name)
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

    fn named(name: &str, is_dir: bool) -> IndexEntry {
        IndexEntry {
            is_dir,
            ..entry(name)
        }
    }

    /// 吸附窗只要文件夹：过滤必须在截断之前，否则文件占满名额会让文件夹消失。
    /// 深度搜索必须能「分批续跑」：第一批只给 `want` 条，剩下的还在队列里，
    /// 再调一次能继续拿到，扫完才 `is_done`（批次由后台线程连续拉，
    /// 不是等用户滚动）。
    #[test]
    fn deep_scan_resumes_between_batches() {
        let root = std::env::temp_dir().join(format!("rove-deep-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("a")).unwrap();
        std::fs::create_dir_all(root.join("b")).unwrap();
        std::fs::create_dir_all(root.join("a").join("nest")).unwrap();
        // 直接子项不算深度结果（它们就在当前视图里）。
        std::fs::write(root.join("bengugu.txt"), b"x").unwrap();
        // 深度结果 3 条：一级 2 条 + 二级 1 条。
        std::fs::write(root.join("a").join("bengugu-1.mp4"), b"x").unwrap();
        std::fs::write(root.join("a").join("nest").join("bengugu-2.mp4"), b"x").unwrap();
        std::fs::write(root.join("b").join("bengugu-3.mp4"), b"x").unwrap();

        let mut scan = DeepScan::new(&root, "bengugu");
        assert!(!scan.is_done(), "还没扫完就先报完了");
        let first = scan.next_batch(2);
        assert_eq!(first.len(), 2, "第一批只该给 2 条");
        let rest = scan.next_batch(10);
        assert_eq!(rest.len(), 1, "第二批该给剩下那 1 条");
        assert!(scan.is_done());
        assert!(scan.next_batch(10).is_empty());

        let _ = std::fs::remove_dir_all(&root);
    }

    /// 名字埋在很深的子目录里也必须能在**第一批**就碰到。
    ///
    /// 造一棵「每层都有几十个无关文件」的树，把 `bengugu` 放在第 5 层：
    /// 深度优先走同一条路径上的几层就到，不会被同层的大目录挡在后面。
    #[test]
    fn deep_scan_reaches_deeply_nested_name() {
        let root = std::env::temp_dir().join(format!("rove-deep-nest-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let mut level = root.clone();
        for d in 0..5 {
            for i in 0..40 {
                let _ = std::fs::write(
                    level.join(format!("其他素材-{d}-{i}.mp4")),
                    b"x",
                );
            }
            level = level.join(format!("合集{d}"));
            std::fs::create_dir_all(&level).unwrap();
        }
        std::fs::create_dir_all(level.join("bengugu")).unwrap();
        // 根目录再挂 20 个各 40 个文件的旁支：广度优先得先扫完这 800 条才轮到
        // 第 5 层，深度优先只要顺着链走 5 层。
        for i in 0..20 {
            let side = root.join(format!("旁支{i}"));
            std::fs::create_dir_all(&side).unwrap();
            for j in 0..40 {
                let _ = std::fs::write(side.join(format!("旁支{i}-{j}.mp4")), b"x");
            }
        }

        // 遍历顺序取决于 `read_dir` 给的顺序，所以这里不赌「第几个扫到」。
        // 深度优先真正保证的是**子树扫完才回到兄弟**：同一个旁支的条目必须
        // 连成一段，不会和别的旁支交替出现 —— 否则深层命中会被上层的大目录
        // 无限推后（用户看到的「怎么半天只有一个」）。
        let mut scan = DeepScan::new(&root, ".mp4");
        let mut order: Vec<String> = Vec::new();
        while !scan.is_done() {
            for item in scan.next_batch(DEEP_BATCH) {
                order.push(item.entry.name.clone());
            }
        }
        assert!(!order.is_empty(), "一条都没扫到，fixture 有问题");
        for i in 0..20 {
            let at: Vec<usize> = order
                .iter()
                .enumerate()
                .filter(|(_, n)| n.starts_with(&format!("旁支{i}-")))
                .map(|(idx, _)| idx)
                .collect();
            assert!(!at.is_empty(), "旁支{i} 的文件一个都没扫到");
            assert_eq!(
                at.last().unwrap() - at.first().unwrap() + 1,
                at.len(),
                "旁支{i} 的文件被别的目录打断了（深度优先没生效）"
            );
        }

        // 再确认深层可命中：直系链上的 bengugu 也能扫到。
        let mut scan = DeepScan::new(&root, "bengugu");
        let mut found = false;
        while !scan.is_done() && !found {
            found = scan
                .next_batch(DEEP_BATCH)
                .iter()
                .any(|i| i.entry.is_dir && i.entry.name == "bengugu");
        }
        assert!(found, "第 5 层的 bengugu 一直没被扫到");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 攒一批文件：`n` 个，名字由闭包给。
    fn fixture_files(tag: &str, n: usize, name_of: impl Fn(usize) -> String) -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!("rove-deep-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        // 根目录的直接子项由本地过滤负责，深搜只扫子目录，所以文件放进一层子目录。
        let sub = root.join("合集");
        std::fs::create_dir_all(&sub).unwrap();
        for i in 0..n {
            let _ = std::fs::write(sub.join(name_of(i)), b"x");
        }
        root
    }

    /// 跑完整个扫描，返回 (总条数, 批次数)。
    fn drain(scan: &mut DeepScan) -> (usize, usize) {
        let mut total = 0usize;
        let mut batches = 0usize;
        while !scan.is_done() {
            batches += 1;
            total += scan.next_batch(DEEP_BATCH).len();
            if batches > 200 {
                panic!("批次失控：{total}");
            }
        }
        (total, batches)
    }

    /// 低档命中（只是沾边）必须卡在配额上，而且卡在**条**上不是批次边界上：
    /// 以前每批 300 条整批塞进来再看上限，1 万的上限实测能跑到 10 200 条。
    #[test]
    fn deep_scan_stops_exactly_at_weak_cap() {
        let root = fixture_files("weak-cap", DEEP_MAX_RESULTS + 137, |i| {
            format!("zz-beam-{i}.mp4")
        });
        let mut scan = DeepScan::new(&root, "beam");
        let (total, _) = drain(&mut scan);
        assert_eq!(total, DEEP_MAX_RESULTS, "低档命中必须正好停在配额上");
        assert!(scan.hit_cap(), "被上限掐停的要能问出来（状态栏要说话）");
        // 掐停之后不能再吐命中。**不要**按「批次数 ≤ 配额 / 批大小」来断言：
        // 一批何时结束有两把尺子 —— 攒满 `want` 条，或者一片 `DEEP_SLICE`（60ms）
        // 时间用完。机器忙的时候（跑全量测试、机械盘）时间先到，一批就不满，
        // 批次数自然多于「理想值」，按批次数断言等于在赌测试机的负载。
        assert!(
            scan.next_batch(DEEP_BATCH).is_empty(),
            "到了配额就不该再有命中"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 高档命中（名字以查询开头）不占配额：用户按前缀搜自己命名的一批视频，
    /// 要的是「全找齐」，不是前 1000 个。900 多条必须一条不少地全回来。
    #[test]
    fn deep_scan_prefix_hits_are_not_capped() {
        let n = DEEP_MAX_RESULTS + 437;
        let root = fixture_files("prefix-cap", n, |i| format!("beam-{i}.mp4"));
        let mut scan = DeepScan::new(&root, "beam");
        let (total, _) = drain(&mut scan);
        assert_eq!(total, n, "前缀命中不该被配额掐掉");
        assert!(!scan.hit_cap(), "这不是被上限掐停的");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 看板的「磁盘缓存统计」不能读错：魔数 / 版本 / 记录大小任一不符都当没缓存，
    /// 否则会对着一个旧格式文件报出错误的条目数。
    #[test]
    fn cache_header_is_parsed_and_validated() {
        let mut header = [0u8; HEADER_LEN];
        header[..8].copy_from_slice(b"ROVEIDX1");
        header[8..12].copy_from_slice(&CACHE_VERSION.to_le_bytes());
        header[12..16].copy_from_slice(&1234u32.to_le_bytes());
        header[16..20].copy_from_slice(&(REC_SIZE as u32).to_le_bytes());
        header[32..40].copy_from_slice(&1_700_000_000u64.to_le_bytes());

        let meta = parse_cache_header(&header, 99_999).expect("合法头应当能解析");
        assert_eq!(meta.count, 1234);
        assert_eq!(meta.bytes, 99_999);
        assert!(meta.built_at.is_some());

        let mut bad_magic = header;
        bad_magic[0] = b'X';
        assert!(parse_cache_header(&bad_magic, 99_999).is_none(), "魔数不符");

        let mut bad_version = header;
        bad_version[8..12].copy_from_slice(&(CACHE_VERSION + 1).to_le_bytes());
        assert!(
            parse_cache_header(&bad_version, 99_999).is_none(),
            "版本不符"
        );

        let mut bad_rec = header;
        bad_rec[16..20].copy_from_slice(&7u32.to_le_bytes());
        assert!(
            parse_cache_header(&bad_rec, 99_999).is_none(),
            "记录大小不符"
        );

        assert!(parse_cache_header(&header[..16], 99_999).is_none(), "头太短");
        assert!(parse_cache_header(&header, 20).is_none(), "文件比头还短");
    }

    /// 但绝对上限还在：搜「沾边」的词最多到 `DEEP_MAX_TOTAL` 条就收手。
    #[test]
    fn deep_scan_hard_cap_bounds_memory() {
        // 前缀命中不受配额限制，靠绝对上限兜住。
        let root = fixture_files("hard-cap", DEEP_MAX_TOTAL + 200, |i| {
            format!("beam-{i}.mp4")
        });
        let mut scan = DeepScan::new(&root, "beam");
        let (total, _) = drain(&mut scan);
        assert!(total <= DEEP_MAX_TOTAL, "绝对上限没兜住：{total}");
        assert!(scan.hit_cap());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn search_dirs_filters_before_truncating() {
        let mut idx = GlobalIndex::default();
        // 完全同名的一堆文件稳居榜首（整串相等 > 前缀），把名额占满。
        for _ in 0..60 {
            idx.entries.push(entry("power"));
        }
        idx.entries.push(named("PowerShell", true));
        // 前提：只取 1 条时被文件占住。
        let all = idx.search("power", 1);
        assert_eq!(all.hits.len(), 1);
        assert!(!all.hits[0].is_dir, "前提：文件应占满前几名");
        // 只要文件夹：同名文件夹必须还在。
        let dirs = idx.search_dirs("power", 1);
        assert_eq!(dirs.hits.len(), 1, "文件夹被文件挤出结果了");
        assert_eq!(dirs.hits[0].name, "PowerShell");
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

    /// 点开头的目录一律剪掉：开发工具缓存与版本库元数据都落这一类，
    /// 逐个维护名单维护不完。点开头的**文件**不受影响（剪枝只看目录名）。
    #[test]
    fn dot_directories_are_always_skipped() {
        for n in [
            ".git",
            ".cargo",
            ".rustup",
            ".cache",
            ".npm",
            ".deno",
            ".local",
            ".zcompcache",
            ".codebuddy",
            ".codebuddy-server-cn",
            ".vscode",
            ".某个新工具的缓存",
        ] {
            assert!(is_skipped(n), "{n} 应被判为噪音目录");
        }
        for n in ["git", "cargo", "rustup", "cache", "vscode"] {
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
