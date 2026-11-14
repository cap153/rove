//! src/app.rs
//! 应用装配：状态机 + UI 回调 + 后台任务。
//!
//! 线程模型：
//! - UI 线程：Slint 事件循环；所有回调与状态变更都在这里发生。
//! - 目录列举线程：`list_directory` / 驱动器枚举在此执行，结果经通道
//!   回传，由轮询定时器合并（代号 generation 过期即丢弃）。
//! - 文件操作线程：复制/移动/删除，进度经共享状态暴露。

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime};

use slint::language::{PointerEvent, PointerEventButton, PointerEventKind};
use slint::{
    Color, ComponentHandle, LogicalPosition, LogicalSize, Model, ModelRc, SharedString, Timer,
    TimerMode, VecModel, WindowPosition, WindowSize,
};

use crate::config::{Config, QuickItem, ThemeChoice};
use crate::fs::{
    self, DriveInfo, FileEntry, SortKey, SortState, icon_color, icon_text_for, list_drives,
    list_network_locations, list_network_places, quick_access_dirs, recycle_bin_stats,
    sort_entries,
};
use crate::ops::{
    self, clipboard, jobs::{JobKind, JobManager, JobSummary, status_text},
};
use crate::state::AppState;

slint::include_modules!();
#[path = "i18n.rs"]
mod i18n;

/// 日志等级判定：`ROVE_LOG` 存在即启用普通级；`ROVE_LOG=2|verbose|debug` 才启用详细级。
pub(crate) fn log_enabled(level: u8) -> bool {
    if level < 2 {
        return std::env::var_os("ROVE_LOG").is_some();
    }
    match std::env::var("ROVE_LOG") {
        Ok(v) => {
            let v = v.trim();
            v == "2" || v.eq_ignore_ascii_case("verbose") || v.eq_ignore_ascii_case("debug")
        }
        Err(_) => false,
    }
}

/// 诊断日志时间戳：本地时间 `YYYY-MM-DD HH:MM:SS.mmm`。
/// Windows 取本机本地时间；其他平台无时区信息可用，退化为 UTC（后缀 `Z`）。
pub(crate) fn log_stamp() -> String {
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::SystemInformation::GetLocalTime;
        let mut st: windows_sys::Win32::Foundation::SYSTEMTIME = unsafe { std::mem::zeroed() };
        unsafe { GetLocalTime(&mut st) };
        format!(
            "{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:03}",
            st.wYear, st.wMonth, st.wDay, st.wHour, st.wMinute, st.wSecond, st.wMilliseconds
        )
    }
    #[cfg(not(windows))]
    {
        let ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        let days = ms.div_euclid(86_400_000);
        let sod = ms.rem_euclid(86_400_000);
        let (y, m, d) = civil_from_days(days);
        format!(
            "{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:03}Z",
            y,
            m,
            d,
            sod / 3_600_000,
            (sod / 60_000) % 60,
            (sod / 1000) % 60,
            sod % 1000
        )
    }
}

/// 相对时间文案：索引构建完成时刻 -> "刚刚 / N 分钟前 / N 小时前 / N 天前"。
fn format_ago(t: SystemTime, en: bool) -> String {
    let secs = SystemTime::now()
        .duration_since(t)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    if en {
        match secs {
            0..=59 => "just now".to_string(),
            60..=3599 => format!("{} min ago", secs / 60),
            3600..=86399 => format!("{} h ago", secs / 3600),
            _ => format!("{} d ago", secs / 86400),
        }
    } else {
        match secs {
            0..=59 => "刚刚".to_string(),
            60..=3599 => format!("{} 分钟前", secs / 60),
            3600..=86399 => format!("{} 小时前", secs / 3600),
            _ => format!("{} 天前", secs / 86400),
        }
    }
}

/// 配置变更后，若索引已启用则后台重建（未启用则维持懒加载，不无端扫盘）。
/// 索引重建的合并窗口：设置页里连续改多项时，只真正重建一次。
/// 不合并的话，每改一项（磁盘开关 / 增删/重置排除路径）都会触发一次全量重扫，
/// 实测日志里就出现过 1.5 秒内两次完整 walk（342ms + 386ms）并落盘两次。
const REINDEX_DEBOUNCE_MS: u64 = 600;

/// 待执行的索引重建截止时刻（None = 无待办）。
static REINDEX_DEADLINE: OnceLock<Mutex<Option<std::time::Instant>>> = OnceLock::new();

/// 安排一次索引重建（合并窗口内的多次调用只算一次）。
/// 实际执行在 40ms 主轮询里检查截止时刻，到点才调 `index::rebuild()`。
fn reindex_if_active() {
    if crate::search::index::try_shared().is_none() {
        return;
    }
    let slot = REINDEX_DEADLINE.get_or_init(|| Mutex::new(None));
    if let Ok(mut g) = slot.lock() {
        *g = Some(std::time::Instant::now() + Duration::from_millis(REINDEX_DEBOUNCE_MS));
    }
}

/// 主轮询定期调用：合并窗口结束则真正触发重建。
/// 重建本身是异步的（spawn_builder 另开线程），不会阻塞 UI。
fn maybe_run_pending_reindex() {
    let Some(slot) = REINDEX_DEADLINE.get() else { return };
    let due = match slot.lock() {
        Ok(mut g) => match *g {
            Some(deadline) if std::time::Instant::now() >= deadline => {
                *g = None; // 先取出，避免重建期间重复触发
                true
            }
            _ => false,
        },
        Err(_) => false,
    };
    if due && crate::search::index::try_shared().is_some() {
        crate::logln!("index: rebuild (debounced {REINDEX_DEBOUNCE_MS}ms)");
        crate::search::index::rebuild();
    }
}

/// Unix 天数 → 公历 `(年, 月, 日)`（Howard Hinnant `civil_from_days` 算法）。
#[cfg(not(windows))]
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// 把一条诊断日志追加写入 exe 同目录的 `rove.log`（自动带本地时间戳）。
/// GUI 子系统进程的 stderr 不可见，文件日志是 WSL 下排查的唯一渠道；
/// 写在 exe 同目录（WSL 文件系统上 Windows 进程可写），避免依赖 %TEMP% 解析。
pub(crate) fn log_write(msg: &str) {
    use std::io::Write as _;
    let path = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("rove.log")))
        .unwrap_or_else(|| std::env::temp_dir().join("rove.log"));
    // 必须先拼成整行、再一次 write_all：`write_fmt` 会按格式片段分多次
    // write_all，多个后台线程并发写日志时会把两行交错成一团
    // （表现为时间戳重复、两行粘成一行）。
    let line = format!("[rove] {} {}\n", log_stamp(), msg);
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = f.write_all(line.as_bytes());
    }
}

/// 诊断日志：`ROVE_LOG` 存在时写入 `rove.log`（带本地时间戳）。
#[macro_export]
macro_rules! logln {
    ($($arg:tt)*) => {{
        if $crate::app::log_enabled(1) {
            $crate::app::log_write(&format!($($arg)*));
        }
    }};
}

/// 详细级诊断日志：仅 `ROVE_LOG=2|verbose|debug` 时写入。
/// 用于高频、仅在排查布局/虚拟窗口问题时才有价值的噪音日志。
#[macro_export]
macro_rules! logv {
    ($($arg:tt)*) => {{
        if $crate::app::log_enabled(2) {
            $crate::app::log_write(&format!($($arg)*));
        }
    }};
}


/// 虚拟列表行高（与 ui/file_list.slint 的 row-h 保持一致）。
const ROW_HEIGHT: f32 = 30.0;
/// 视口上下过扫描行数。
const OVERSCAN: usize = 4;
/// 单条索引记录的粗估内存占用（路径 + 名称 + 拼音字段）。
const INDEX_ENTRY_BYTES: usize = 80;

/// 缩略图缓存最大项数（宽松限制，主要受字节预算约束）。
const THUMB_CACHE_MAX_ITEMS: usize = 4096;
/// 缩略图缓存字节预算（192 MB）。Rove 的 Image 常驻 CPU 侧 RGBA，
/// 按最大尺寸(132px*2.0=264px)每张约 278 KB，192 MB 可存约 690 张。
const THUMB_CACHE_MAX_BYTES: usize = 192 * 1024 * 1024;
/// 缩略图磁盘缓存预算（512 MB，对标 Pulse）。超限按最后修改时间淘汰到 80%。
const THUMB_DISK_MAX_BYTES: u64 = 512 * 1024 * 1024;

// ---- 右键菜单动作 ID ----
const CMD_PASTE: &str = "paste";
const CMD_TERMINAL: &str = "terminal";
const CMD_NEW_FOLDER: &str = "new_folder";
const CMD_REFRESH: &str = "refresh";
const CMD_SELECT_ALL: &str = "select_all";
const CMD_MANAGE_ACTIONS: &str = "manage_actions";
/// 搜索结果里右键打开路径：在 Rove 内跳到所在目录，或交给资源管理器定位。
const CMD_OPEN_LOCATION: &str = "open_location";
#[cfg(windows)]
const CMD_REVEAL_EXPLORER: &str = "reveal_explorer";

/// 右键菜单作用范围（决定挂哪些项、以及结果要作用于谁）。
#[derive(Clone, Debug)]
enum MenuScope {
    /// 选中的项（非空）。
    Selection(Vec<PathBuf>),
    /// 列表空白处（作用于当前目录）。
    Background,
}

/// 目录列举请求。
struct ListRequest {
    tab_id: u32,
    /// 目标分屏面板下标（0 / 1）：分屏时两栏各自请求、各自回填。
    pane: usize,
    generation: u64,
    /// None = "此电脑"视图（列举驱动器）。
    path: Option<PathBuf>,
    sort: SortState,
}

/// 目录列举结果。
struct ListResult {
    tab_id: u32,
    pane: usize,
    generation: u64,
    entries: Vec<FileEntry>,
}

/// 深度搜索请求：在目录子树中递归查找匹配项（文件夹视图搜索）。
struct DeepRequest {
    tab_id: u32,
    pane: usize,
    generation: u64,
    deep_generation: u64,
    root: PathBuf,
    query: String,
}

/// 深度搜索结果。
struct DeepResult {
    tab_id: u32,
    pane: usize,
    generation: u64,
    deep_generation: u64,
    query: String,
    items: Vec<crate::search::index::DeepItem>,
}

/// 文件夹体积统计请求（一批 = 当前列表中的全部子文件夹 + 各自的修改时间）。
struct DirSizeRequest {
    tab_id: u32,
    generation: u64,
    /// 待计算目录及其列举时的修改时间（用于缓存命中校验）。
    dirs: Vec<(PathBuf, Option<SystemTime>)>,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

/// 文件夹体积统计结果（算完一个回传一个，不攒批）。
struct DirSizeResult {
    tab_id: u32,
    generation: u64,
    path: PathBuf,
    size: u64,
    /// 是否为部分统计/下界估值（跳过 Junction/云占位/无权限子树）。
    partial: bool,
    /// 计算时记录的目录修改时间（用于写入带时效的缓存）。
    mtime: Option<SystemTime>,
}

/// 文件夹容量内存缓存上限（参考 Pulse folder_size_store 的 4096）。
const DIR_SIZE_CACHE_CAP: usize = 4096;

/// 调试日志：经项目既有的 `log_write` 写入 exe 同目录的 `rove.log`（前缀 `[rove:dirsize]`），
/// 与项目其它诊断日志同源、同一 `ROVE_LOG` 开关（存在即启用），不再走 stderr。
fn dirsize_trace(msg: String) {
    if log_enabled(2) {
        log_write(&format!("[rove:dirsize] {msg}"));
    }
}

/// 判定是否为跳过自动扫描的昂贵系统目录（显示 "—"，需显式点击才计算）。
/// 注意：`$Recycle.Bin` 用户明确要求统计，不应列入推迟名单。
fn is_expensive_system_dir(path: &Path) -> bool {
    let s = path.to_string_lossy();
    let norm = s.trim_end_matches(['\\', '/']);
    // 系统根目录（动态读 SystemRoot，回退 C:\Windows）
    if let Ok(sys_root) = std::env::var("SystemRoot") {
        if norm.eq_ignore_ascii_case(sys_root.trim_end_matches(['\\', '/'])) {
            return true;
        }
    } else if norm.eq_ignore_ascii_case(r"C:\Windows") {
        return true;
    }
    // 卷根下的系统保留目录（无权限/不可枚举）
    if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
        if name.eq_ignore_ascii_case("System Volume Information") {
            return true;
        }
    }
    false
}

/// 磁盘持久化条目格式（落盘到系统 Temp/rove_dir_sizes.json）。
#[derive(serde::Serialize, serde::Deserialize)]
struct PersistentDirSize {
    path: PathBuf,
    size: u64,
    partial: bool,
    mtime_ms: u64,
}

/// 文件夹体积容量缓存（内存 + 落盘双用）。全局单例，进程级生命周期。
struct DirSizeStore {
    cache: HashMap<PathBuf, (u64, bool, SystemTime)>,
    order: Vec<PathBuf>,
    /// 自上次落盘以来是否被修改（避免无变化时频繁写盘）。
    dirty: bool,
    /// 上次落盘时刻，用于节流（≥5s 才真正写一次）。
    last_save: std::time::Instant,
}

impl DirSizeStore {
    fn new() -> Self {
        let mut store = Self {
            cache: HashMap::new(),
            order: Vec::new(),
            dirty: false,
            last_save: std::time::Instant::now(),
        };
        store.load_from_temp();
        store
    }

    fn temp_file_path() -> PathBuf {
        // 核心修复：迁移到 %LOCALAPPDATA%\Rove\，避免被系统 %tmp% 清理逻辑误删
        // （误删会导致重启后全部文件夹大小重新计算，造成短暂卡顿）。
        rove_data_dir().join("dir_sizes.json")
    }

    fn load_from_temp(&mut self) {
        let path = Self::temp_file_path();
        let Ok(file) = std::fs::File::open(&path) else {
            return;
        };
        let reader = std::io::BufReader::new(file);
        let items: Vec<PersistentDirSize> = serde_json::from_reader(reader).unwrap_or_default();
        let count = items.len();
        for item in items {
            let mtime = std::time::UNIX_EPOCH + std::time::Duration::from_millis(item.mtime_ms);
            self.cache.insert(item.path.clone(), (item.size, item.partial, mtime));
            self.order.push(item.path);
        }
        dirsize_trace(format!("cache LOAD from {}: {} entries", path.display(), count));
    }

    fn save_to_temp(&mut self) {
        let path = Self::temp_file_path();
        let items: Vec<PersistentDirSize> = self
            .order
            .iter()
            .filter_map(|p| {
                self.cache.get(p).map(|&(size, partial, mtime)| {
                    let mtime_ms = mtime
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_millis() as u64;
                    PersistentDirSize {
                        path: p.clone(),
                        size,
                        partial,
                        mtime_ms,
                    }
                })
            })
            .collect();
        let count = items.len();
        if let Ok(file) = std::fs::File::create(&path) {
            let writer = std::io::BufWriter::new(file);
            if serde_json::to_writer(writer, &items).is_ok() {
                dirsize_trace(format!("cache SAVE to {}: {} entries", path.display(), count));
            }
        }
        self.dirty = false;
        self.last_save = std::time::Instant::now();
    }

    /// 标记已修改，并按节流间隔（≥5s）落盘，避免高频写文件。
    fn mark_modified(&mut self) {
        self.dirty = true;
        if self.last_save.elapsed() >= std::time::Duration::from_secs(5) {
            self.save_to_temp();
        }
    }
}

/// 缩略图缓存 Key：包含 mtime/size，彻底解决「文件被替换后显示旧缩略图」的 Bug。
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct ThumbKey {
    path: PathBuf,
    /// 修改时间（毫秒时间戳），保证文件变动后不命中旧图。
    mtime_ms: u64,
    /// 文件大小。
    file_size: u64,
    /// 请求的像素尺寸。
    px_size: u32,
}

/// 缓存项：原图 + 估算字节数（用于字节预算淘汰）。
struct CachedThumb {
    img: slint::Image,
    bytes: usize,
}

/// 全局常驻的体积缓存单例（进程级生命周期，窗口关闭不销毁）。
static GLOBAL_DIR_SIZES: OnceLock<Mutex<DirSizeStore>> = OnceLock::new();

fn global_dir_sizes() -> &'static Mutex<DirSizeStore> {
    GLOBAL_DIR_SIZES.get_or_init(|| Mutex::new(DirSizeStore::new()))
}

/// 图标视图浮层签名：(选中项索引, 框宽, 行哈希, 名字哈希,
/// 悬停项索引, 悬停框宽, 悬停行哈希)。
type ExpandSig = (i32, u32, u64, u64, i32, u32, u64);

/// 面板所在的「视图种类」：决定它用哪一套排序状态。
#[derive(Clone, Copy, PartialEq, Eq)]
enum ViewSort {
    /// 「此电脑」（路径为 None）：主键是盘符，没有修改时间列。
    ThisPc,
    /// 回收站虚拟目录。
    Recycle,
    /// 普通目录：用全局排序。
    Normal,
}

impl ViewSort {
    fn of(path: Option<&std::path::Path>) -> Self {
        match path {
            None => Self::ThisPc,
            Some(p) if p == crate::fs::drives::recycle_virtual_path().as_path() => Self::Recycle,
            _ => Self::Normal,
        }
    }

    /// 这个视图到底有哪几个列可排。
    /// 「此电脑」没有名称列，也没有修改时间列；它的主键是「盘符」。
    fn allows(self, key: SortKey) -> bool {
        match (self, key) {
            (Self::ThisPc, SortKey::Drive | SortKey::Type | SortKey::Size) => true,
            (Self::ThisPc, _) => false,
            (_, SortKey::Drive) => false,
            _ => true,
        }
    }

    /// 排序键在不可用时退回的兼底值。
    fn fallback(self, key: SortKey) -> SortKey {
        if self.allows(key) {
            key
        } else if self == Self::ThisPc {
            SortKey::Drive
        } else {
            SortKey::Name
        }
    }
}

/// SortKey ↔ 配置文件里的字符串。
fn sort_key_id(key: SortKey) -> &'static str {
    match key {
        SortKey::Name => "name",
        SortKey::Size => "size",
        SortKey::Modified => "modified",
        SortKey::Type => "type",
        SortKey::Path => "path",
        SortKey::Drive => "drive",
    }
}

fn parse_sort_key(id: &str, fallback: SortKey) -> SortKey {
    match id {
        "name" => SortKey::Name,
        "size" => SortKey::Size,
        "modified" => SortKey::Modified,
        "type" => SortKey::Type,
        "path" => SortKey::Path,
        "drive" => SortKey::Drive,
        _ => fallback,
    }
}

/// 应用主体。
pub struct App {
    ui: MainWindow,
    state: AppState,
    config: Config,
    jobs: JobManager,
    drives: Vec<DriveInfo>,
    /// 侧边栏数据指纹，避免无变化时重建模型打断滚动。
    sidebar_fingerprint: u64,
    /// 虚拟列表当前窗口 (first, count) 的失效标志（各处置 None 表示需重建）。
    visible_cache: Option<(usize, usize)>,
    /// 上次真正写入行模型时的签名 (generation, first, count, total)。
    /// 仅用于「重命名期间」抑制无意义重建：缩略图/体积陆续到达会不停触发刷新，
    /// 若每次都换 VecModel，行内 TextInput 会被销毁重建 → 输入丢失、焦点被抢。
    ///
    /// 与 `visible_cache` 分开存放：后者会被各处清成 None 作为“失效”信号
    /// （如 40ms 轮询里缩略图到达就置 None），若共用会在这里反复失效导致重建。
    ///
    /// 必须包含 generation：列举结果到达（导航/刷新/外部变更）时 generation 递增，
    /// 必须重建；否则重命名期间列表会冻住在旧数据上（完全看不到目录刷新）。
    /// 必须包含 total：新建/删除条目即使可见窗口不变，内容已变。
    rows_sig: Option<(u64, usize, usize, usize)>,
    /// 右分屏同名指纹：右栏重命名期间也要抑制无谓的行模型重建。
    rows_sig_right: Option<(u64, usize, usize, usize)>,
    /// 图标视图展开浮层的签名（索引 / 框宽 / 行内容哈希，左右各一份）。
    /// 未变时跳过重建嵌套 ModelRc 与 set —— 本函数每帧都会走到。
    /// 图标视图浮层签名：(选中项索引, 框宽, 行哈希, 名字哈希,
    /// 悬停项索引, 悬停框宽, 悬停行哈希)。悬停单独占一格，因为它与选中项
    /// 是两层独立浮层，不能共用一个索引。
    expand_sig: Option<[ExpandSig; 2]>,
    /// 面包屑各段的窗口逻辑矩形（由 Slint 上报，拖动落点判定用）。
    ///
    /// 用独立的 `Rc<RefCell<_>>` 而不是普通字段：几何是在 Slint 的 `changed`
    /// 回调里同步上报的，可能落在「App 已被可变借用」的路径上（例如
    /// `sync_breadcrumbs` 在 `&mut self` 里重建模型后布局求值），直接写字段
    /// 会 RefCell panic。独立容器只借用自己，任何触发时机都安全，也不会丢几何。
    #[cfg(windows)]
    crumb_rects: std::rc::Rc<std::cell::RefCell<Vec<(f32, f32, f32, f32)>>>,
    /// 侧边栏条目行的窗口逻辑矩形（key = `SidebarItem.id`）。
    /// 用 id 而不是下标：分区顺序可配置重排、展开态会改变条目数量，索引不稳定。
    #[cfg(windows)]
    sidebar_rects: std::rc::Rc<
        std::cell::RefCell<std::collections::HashMap<String, (f32, f32, f32, f32)>>,
    >,
    /// 当前右键菜单的作用范围（结果回投时用它决定作用于谁）。
    menu_scope: Option<MenuScope>,
    /// 本次右键按下所在行的路径（多选时「打开文件所在位置」/「在资源管理器中显示」
    /// 优先作用于它，而不是选中集的第一项）。
    menu_anchor_path: Option<PathBuf>,
    /// 多选状态下按下某个「已选中行」时，先保留多选以便批量拖出；
    /// 若直到 Up 都没发生拖动，才把选择集塌缩成该行单选（资源管理器语义）。
    pending_single_click: Option<PathBuf>,
    /// Shell 背景菜单（含系统「新建 → 文本文档」）弹出前的目录快照：
    /// Shell 把「新建」命令直接交回系统执行，Rove 拿不到新建项路径（zsdesk
    /// 也只对自己的 NewFolder 命令用 IFileOperation::PostNewItem 回传）。
    /// 这里在菜单弹出时记下当前目录已有条目，等 Shell 操作结束后的列举结果
    /// 回来做 diff：恰好新增一个文件时选中它并进入重命名。
    shell_new_watch: Option<(
        PathBuf,
        std::collections::HashSet<PathBuf>,
        std::time::Instant,
    )>,
    /// diff 命中后待触发的重命名路径（等列表/滚动就绪再执行，避免抢焦点）。
    pending_shell_new_rename: Option<PathBuf>,
    /// 图标视图鼠标悬停的格子显示索引（分屏各一份，-1 = 无）。
    hover_display_index: [i32; 2],
    /// 鼠标是否压在展开的完整名浮层上（分屏各一份）。
    /// 浮层盖住名称区：鼠标下移到文件名上时格子自己的 hover 会变 false，
    /// 若不拦住就会「浮层消失 → 鼠标落回格子 → 浮层重现」来回闪。
    overlay_hovered: [bool; 2],
    /// 格子自己的 hover 上报值（-1 = 指针不在任何格子上）。
    /// 与 overlay_hovered 取或，决定最终悬停索引。
    cell_hovered: [i32; 2],
    /// 名称浮层诊断日志的节流签名（视图 / 分屏 / 截断行数）。
    tip_log_sig: Option<(String, usize, usize)>,
    /// 双击检测：上次左键按下的 (路径, 时刻)。
    last_click: Option<(PathBuf, std::time::Instant)>,
    /// 侧栏分区拖拽：正在拖动的分区 header 行索引。
    sidebar_drag: Option<usize>,
    /// 侧栏网络位置拖拽：(行索引, 按下时的指针 y, 是否已移动)。
    network_drag: Option<(usize, f32, bool)>,
    /// 侧栏「快速访问」拖拽，字段含义同 network_drag。
    quick_drag: Option<(usize, f32, bool)>,
    /// 顶部标签拖动交换：(起始索引, 按下时的内容 x, 是否已移动)。
    tab_drag: Option<(usize, f32, bool)>,
    /// 标签拖动松手后那次补发的 clicked 要丢弃，避免误切标签。
    suppress_tab_click: Option<std::time::Instant>,
    /// 拖拽结束后的那次 clicked 要丢弃（否则拖完会顺带导航）。带时间上限，不会误吐下一次点击。
    suppress_sidebar_click: Option<std::time::Instant>,
    /// 正在编辑的网络位置（旧地址）；None 表示“添加”。
    network_edit_target: Option<String>,
    /// 外部拖入缓冲：winit 对一次投放逐文件发事件，聚合后统一处理。
    drop_buffer: Vec<PathBuf>,
    /// 任务 3（刷新）：各打开目录的 mtime 快照，轮询检测外部/内部变更。
    dir_mtimes: HashMap<PathBuf, SystemTime>,
    /// 拖入聚合定时器（最后一个文件事件后短暂延迟处理）。
    drop_timer: Timer,
    /// 拖出待定手势：(按下时行局部 x, y, 待拖路径)。
    drag_pending: Option<(f32, f32, Vec<PathBuf>)>,
    /// 框选基线：按下时若按住 Ctrl/Shift，记录当时的选择集作为累加底本；
    /// None 表示普通替换式框选。预览与结束都基于它重算，矩形缩回时高亮也跟着缩回。
    rubber_band_base: Option<HashSet<PathBuf>>,
    /// 已经做过「列宽自适应」的目录键，按栏记录（左 0 / 右 1）。
    /// 每个目录只在打开时算一次，避免滚动 / 刷新时反复重算导致列宽跳动。
    /// 必须按栏分：两栏可能在不同目录，共用一个键会让切焦点时用另一栏的内容
    /// 重新拟合，表现为「顶栏列宽自己变」。
    autofit_key: [Option<String>; 2],
    /// 界面上当前实际生效的四列宽度（名称/修改时间/类型/大小），按栏一份。
    /// 拖动某列时以它为基准，避免把没动过的列弹回默认值。
    cur_col_widths: [[f32; 4]; 2],
    /// 后台大小变化时置位，等待主线程空闲时静默重排。
    pub size_sort_pending: bool,
    /// 切换标签时置位，屏蔽切换过程中 Slint 因内容高度变化自动 Clamp 而
    /// 产生的残留 scroll-changed 事件污染新标签数据（否则表现为切标签白屏）。
    syncing_tab: bool,
    /// 首次启动的 75% 居中：show() 后 scale_factor() 仍可能是 1.0（窗口尚未真正
    /// 落到显示器），必须等到首帧 RedrawRequested 窗口已 realize、scale 正确时再居中，
    /// 否则尺寸会按物理像素当逻辑像素计算而偏大、偏离中心。
    first_run_pending: bool,
    /// 首帧居中重试计数：HWND 未就绪时每次 redraw 重试一次，超过上限就放弃居中
    /// （避免极端情况下 HWND 永远取不到导致每帧 request_redraw 死循环、CPU 打满）。
    geom_retries: u32,
    /// 切换视图时，暂存待恢复的“逻辑条目索引”，等布局更新后再换算成像素。
    /// （视图切换会改变 cell 布局，旧像素偏移无法直接复用，必须先换回条目序号。）
    pending_scroll_index: Option<usize>,
    /// 重命名期间的“真滚动锁”：(pane0 位置, pane1 位置, 失效时刻)。
    /// Slint 在焦点元素获得焦点时会自动把它滚进视野（window.rs:1531
    /// `item.try_scroll_into_visible()`，无 API 可关），表现为“开始重命名滚动条
    /// 自己往上跳”，并连带把行模型推出可见窗口 → 重建 → 行内输入框被销毁 →
    /// 焦点/全选丢失。这里记住预期位置，把无用户输入迹象的滚动事件丢弃并推回。
    rename_scroll_guard: Option<(f32, f32, Instant)>,
    /// 重命名后“钉住原位置”：资源管理器同款行为 —— 改名不引起排序跳变，
    /// 只有用户显式重排 / 换目录 / 窗口过期（3 秒）后才归位。
    /// (目录, 条目新路径, 原显示位置, 失效时刻)
    rename_pin: Option<(PathBuf, PathBuf, usize, Instant)>,
    /// 待滚到可视区的条目路径（新建文件夹 / 粘贴完成后）。
    /// 列举是异步的，所以先存路径，数据就位后再换算成显示索引。
    scroll_to_paths: Vec<PathBuf>,
    /// 显现后是否把它们选中（粘贴/还原 = true；新建文件夹 = false）。
    scroll_select: bool,
    /// 是否有原生 Shell 文件操作正在进行（复制/移动/删除）。
    /// 这类操作会弹 Shell 自己的确认/进度对话框，期间必须拒绝文件类按键。
    shell_op_running: bool,
    /// 本轮操作里是否已经记过“按键被拦”的日志（避免长按回车刷屏）。
    shell_key_suppressed_logged: bool,
    /// 回收站下暂存盘点「移动」：等删除任务成功后才清空暂存盘。
    clear_staging_on_delete_success: bool,
    /// 待滚到可视区的条目索引（由 scroll_to_path 解析而来，在 refresh_visible_rows 里消费）。
    scroll_to_index: Option<usize>,
    /// 上次重建可见行时的列表几何 (list_height, cells_per_row, row_h, 图标视图)。
    /// 启动首帧布局未完成时这几个值还不可用（list_height=0 / cells_per_row=1），
    /// 此时算出的可见窗口偏小，必须等布局落定后重算一次，否则只渲染前几行
    /// （表现为「此电脑」部分盘符空白，直到切标签/重开窗口才恢复）。
    list_geom: (f32, usize, f32, bool, f32, f32),
    /// actions.toml 驱动的右键动作（内置动作开关 + 自定义）。
    actions: Vec<crate::ops::actions::ActionEntry>,
    /// 缩略图 LRU 缓存（Key 包含 mtime/size，防止旧图）。
    thumb_cache: HashMap<ThumbKey, CachedThumb>,
    /// LRU 顺序（队首最久未用，命中时移到队尾）。
    thumb_order: Vec<ThumbKey>,
    /// 当前缓存的估算总字节数（用于字节预算淘汰）。
    thumb_cache_bytes: usize,
    /// 已请求未完成的缩略图（防重复提交）。
    thumb_pending: HashSet<ThumbKey>,
    /// 普通池请求的排队时间（按发出先后）。用于队列积压时把最早、
    /// 已滑出可视区域的请求作废——否则滚过的每个位置都会往里塞请求，
    /// 越滚越长，用户突然滚到中间时要排在一大堆已作废请求后面。
    thumb_queued: Vec<ThumbKey>,
    /// 失败负缓存（本会话内不再重试）。
    thumb_failed: HashSet<ThumbKey>,
    /// 批量刷新标记：后台有缩略图到达时置位，由 poll_timer 统一刷新。
    thumb_needs_refresh: bool,
    #[allow(dead_code)]
    thumb_tx: Sender<ThumbKey>,
    /// 优先通道：可见窗口内的缩略图请求。
    /// 与 `thumb_tx` 分开，避免「先慢后快」渲染——轮转到的预览区把 4096 个文件
    /// 全部占入 worker 队列后，用户突然滚到中间，新发出的请求会排在几千个
    /// 已作废的请求后面。优先通道有自己的 worker（`PRIORITY_THREADS` 个）。
    thumb_prio_tx: Sender<ThumbKey>,
    list_tx: Sender<ListRequest>,
    list_rx: Receiver<ListResult>,
    /// 深度搜索通道（文件夹视图子树搜索，独立线程执行）。
    deep_tx: Sender<DeepRequest>,
    deep_rx: Receiver<DeepResult>,
    /// 文件夹体积异步统计通道（独立 worker 线程逐个递归计算并回传）。
    dir_size_tx: Sender<DirSizeRequest>,
    dir_size_rx: Receiver<DirSizeResult>,
    /// 当前一批统计任务的取消开关：切换目录/刷新时置位即可中断前序任务。
    dir_size_stop: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    /// 当前活动目录的文件系统递归监听器（随当前目录切换重建）。
    active_watcher: Option<crate::fs::DirWatcher>,
    /// 文件变更事件通道：worker 线程 -> 主线程 poll_timer 消费。
    watch_tx: Sender<PathBuf>,
    watch_rx: Receiver<PathBuf>,
    /// 变动防抖收集池：记录需要被重算的当前层直接子文件夹。
    pending_invalidated_subdirs: HashSet<PathBuf>,
    /// 最近一次变动发生时刻，用于 400ms 防抖。
    last_watch_event: Option<std::time::Instant>,
    /// 单目录重算冷却时间戳：避免大量文件变动时密集轰炸磁盘（最低 2.5s 间隔）。
    last_rescan_times: HashMap<PathBuf, std::time::Instant>,
    /// 声明式快捷键映射（差量合并后）。
    keymap: crate::ops::keymap::Keymap,
    /// 正在录制的 (Action ID, 槽位序号)（非空时优先捕获物理键）。
    recording_shortcut: Option<(String, usize)>,
    poll_timer: Timer,
    sidebar_timer: Timer,
    /// 侧栏周期刷新（驱动器容量/回收站统计/网络位置）。
    /// 必须挂在 App 字段上：局部 Timer 会在函数返回时停止。
    refresh_timer: Timer,
    /// 侧栏与空闲重扫的周期定时器（15s）。
    /// 不能与 `refresh_timer`（1s 目录轮询）共用：Slint Timer 重复 start
    /// 会以后一次为准，共用会把 1s 轮询错降为 15s。
    sidebar_periodic_timer: Timer,
    /// 文件对话框跳转吸附窗。
    jump_window: DialogJumpWindow,
    /// 当前跳转目标：(对话框 HWND, 当前目录)。
    jump_target: Option<(usize, Option<PathBuf>)>,
    /// 最近一次吸附几何 (x, y, w, placement)，供结果数 / 焦点变化时重新锚定。
    #[cfg(windows)]
    jump_geom: Option<(i32, i32, i32, String)>,
    /// 文件对话框吸附轮询定时器（仅 Windows 生效，见 wire_callbacks）。
    /// 必须挂在 App 字段上：局部 Timer 会在函数返回时停止。
    jump_poll_timer: Timer,
    /// 单击文件名重命名的延迟定时器：按下先不起重命名，留出双击窗口；
    /// 窗口内再来一次按下即判定为双击打开（取消本定时器），否则到点才重命名。
    rename_defer_timer: Timer,
    /// 自身弱引用：供 rename_defer_timer 等定时器回调里升级回 App。
    self_weak: Option<Weak<RefCell<App>>>,
    }

// ---------------------------------------------------------------------------
// 构造与运行
// ---------------------------------------------------------------------------

impl App {
    fn new(
        ui: MainWindow,
        config: Config,
        jobs: JobManager,
        list_tx: Sender<ListRequest>,
        list_rx: Receiver<ListResult>,
        deep_tx: Sender<DeepRequest>,
        deep_rx: Receiver<DeepResult>,
        thumb_tx: Sender<ThumbKey>,
        thumb_prio_tx: Sender<ThumbKey>,
        dir_size_tx: Sender<DirSizeRequest>,
        dir_size_rx: Receiver<DirSizeResult>,
        watch_tx: Sender<PathBuf>,
        watch_rx: Receiver<PathBuf>,
    ) -> Self {
        let sort = SortState {
            key: match config.sort_key.as_str() {
                "modified" => SortKey::Modified,
                "type" => SortKey::Type,
                "size" => SortKey::Size,
                "path" => SortKey::Path,
                _ => SortKey::Name,
            },
            ascending: config.sort_ascending,
        };
        let state = AppState::new(None, sort);
        let keymap = crate::ops::keymap::Keymap::new(&config.shortcuts);
        let jump_window = DialogJumpWindow::new().unwrap();
        Self {
            ui,
            state,
            config,
            keymap,
            jobs,
            drives: Vec::new(),
            sidebar_fingerprint: 0,
            visible_cache: None,
            rows_sig: None,
            rows_sig_right: None,
            expand_sig: None,
            #[cfg(windows)]
            crumb_rects: std::rc::Rc::new(std::cell::RefCell::new(Vec::new())),
            #[cfg(windows)]
            sidebar_rects: std::rc::Rc::new(std::cell::RefCell::new(
                std::collections::HashMap::new(),
            )),
            menu_scope: None,
            menu_anchor_path: None,
            pending_single_click: None,
            shell_new_watch: None,
            pending_shell_new_rename: None,
            hover_display_index: [-1, -1],
            overlay_hovered: [false, false],
            cell_hovered: [-1, -1],
            tip_log_sig: None,
            last_click: None,
            sidebar_drag: None,
            network_drag: None,
            quick_drag: None,
            tab_drag: None,
            suppress_tab_click: None,
            suppress_sidebar_click: None,
            network_edit_target: None,
            drop_buffer: Vec::new(),
            dir_mtimes: HashMap::new(),
            drop_timer: Timer::default(),
            drag_pending: None,
            rubber_band_base: None,
            autofit_key: [None, None],
            cur_col_widths: [
                [
                    crate::config::DEFAULT_COLUMN_WIDTHS[0],
                    crate::config::DEFAULT_COLUMN_WIDTHS[1],
                    crate::config::DEFAULT_COLUMN_WIDTHS[2],
                    crate::config::DEFAULT_COLUMN_WIDTHS[3],
                ],
                [
                    crate::config::DEFAULT_COLUMN_WIDTHS[0],
                    crate::config::DEFAULT_COLUMN_WIDTHS[1],
                    crate::config::DEFAULT_COLUMN_WIDTHS[2],
                    crate::config::DEFAULT_COLUMN_WIDTHS[3],
                ],
            ],
            size_sort_pending: false,
            list_geom: (-1.0, 0, -1.0, false, -1.0, -1.0),
            recording_shortcut: None,
            dir_size_tx,
            dir_size_rx,
            dir_size_stop: None,
            active_watcher: None,
            watch_tx,
            watch_rx,
            pending_invalidated_subdirs: HashSet::new(),
            last_watch_event: None,
            last_rescan_times: HashMap::new(),
            actions: Vec::new(),
            thumb_cache: HashMap::new(),
            thumb_order: Vec::new(),
            thumb_cache_bytes: 0,
            thumb_pending: HashSet::new(),
            thumb_queued: Vec::new(),
            thumb_failed: HashSet::new(),
            thumb_needs_refresh: false,
            syncing_tab: false,
            first_run_pending: false,
            geom_retries: 0,
            pending_scroll_index: None,
            rename_scroll_guard: None,
            rename_pin: None,
            scroll_to_paths: Vec::new(),
            scroll_select: false,
            shell_op_running: false,
            shell_key_suppressed_logged: false,
            clear_staging_on_delete_success: false,
            scroll_to_index: None,
            thumb_tx,
            thumb_prio_tx,
            list_tx,
            list_rx,
            deep_tx,
            deep_rx,
            poll_timer: Timer::default(),
            sidebar_timer: Timer::default(),
            refresh_timer: Timer::default(),
            sidebar_periodic_timer: Timer::default(),
            jump_window,
            jump_target: None,
            #[cfg(windows)]
            jump_geom: None,
            jump_poll_timer: Timer::default(),
            rename_defer_timer: Timer::default(),
            self_weak: None,
        }
    }

    /// 请求列举目录（工作线程执行）。
    fn request_listing(
        &self,
        tab_id: u32,
        pane: usize,
        generation: u64,
        path: Option<PathBuf>,
        sort: SortState,
    ) {
        logv!("request_listing tab={tab_id} pane={pane} gen={generation} path={path:?}");
        let _ = self.list_tx.send(ListRequest {
            tab_id,
            pane,
            generation,
            path,
            sort,
        });
    }

    /// 重新列举指定标签的当前聚焦面板。
    fn reload_tab(&self, tab_index: usize) {
        let Some(tab) = self.state.tabs.get(tab_index) else {
            return;
        };
        tab_loading_flag(&self.ui, tab_index == self.state.active_tab, true);
        let pane = tab.active_pane;
        self.request_listing(
            tab.id,
            pane,
            tab.panes[pane].generation,
            tab.panes[pane].path.clone(),
            tab.panes[pane].sort,
        );
    }

    /// 任务 3：轮询各打开目录的 mtime，检测外部/内部变更后实时刷新对应标签。
    fn poll_dir_changes(&mut self) {
        if self.ui.get_settings_open() {
            return;
        }
        let n = self.state.tabs.len();
        let mut to_reload: Vec<usize> = Vec::new();
        for i in 0..n {
            let path = match self.state.tabs[i].active().path.clone() {
                Some(p) => p,
                None => continue,
            };

            // 核心修复 1：跳过网络路径的 mtime 轮询，防止网络 IO 阻塞主线程导致 UI 假死
            if is_network_path(&path) {
                continue;
            }

            let mtime = match std::fs::symlink_metadata(&path).and_then(|m| m.modified()) {
                Ok(m) => m,
                Err(_) => continue,
            };
            match self.dir_mtimes.get(&path) {
                Some(prev) if *prev == mtime => {}
                _ => {
                    self.dir_mtimes.insert(path.clone(), mtime);
                    to_reload.push(i);
                }
            }
        }
        for i in to_reload {
            self.reload_tab(i);
        }
    }

    /// 重新列举当前标签（常规操作复用：新建/删除/拖放/还原等，不清体积缓存）。
    fn reload_active(&mut self) {
        // 列举请求会带上 pane.sort：新建标签 / 添加网络位置 / 切换到设置页再回来这类
        // 不经过 navigate 的路径，pane.sort 可能是从上一个视图（比如此电脑的盘符）
        // 继承来的，直接发出去会用错排序。这里统一对齐到当前视图自己的状态。
        self.sync_view_sort();
        self.reload_tab(self.state.active_tab);
    }

    /// 用户主动刷新（F5 / 工具栏 / 右键菜单）：清理「当前目录下各子文件夹」的
    /// 历史体积缓存，强制穿透重算（子目录 mtime 可能已变而父目录 mtime 未变）。
    fn user_refresh_active(&mut self) {
        if let Some(current_dir) = self.state.active().active().path.clone() {
            let mut store = global_dir_sizes().lock().unwrap();
            let before = store.cache.len();
            store
                .cache
                .retain(|path, _| path.parent() != Some(current_dir.as_path()));
            store
                .order
                .retain(|path| path.parent() != Some(current_dir.as_path()));
            let cleared = before - store.cache.len();
            if cleared > 0 {
                dirsize_trace(format!(
                    "F5/invalidate: cleared {} cached subdir sizes under {} ({} left)",
                    cleared,
                    current_dir.display(),
                    store.cache.len()
                ));
            }
            store.mark_modified();
        }
        self.reload_active();
    }

    /// 列举结果到达：代号匹配才应用。
    fn apply_list_result(&mut self, result: ListResult) {
        logv!("list_result tab={} gen={} n={}", result.tab_id, result.generation, result.entries.len());
        let Some(tab_index) = self.state.tab_by_id(result.tab_id) else {
            logln!("apply_list_result: tab not found");
            return;
        };
        let tab = &mut self.state.tabs[tab_index];
        if tab.panes[result.pane].generation != result.generation {
            return; // 过期结果
        }
        // 按「显示隐藏文件」开关过滤：关闭时剔除具有隐藏属性的条目。
        let show_hidden = self.config.show_hidden;
        let listed_is_active = result.pane == tab.active_pane;

        // 1. 过滤隐藏文件
        let mut entries: Vec<FileEntry> = if show_hidden {
            result.entries
        } else {
            result.entries.into_iter().filter(|e| !e.is_hidden).collect()
        };

        // 2. 核心修复：在排序前，先查一遍缓存，把已知的体积回填进去。
        //    （排序只看 size，若先排后填，缓存命中的目录也会排错。）
        {
            let store = global_dir_sizes().lock().unwrap();
            // 毫秒级一致性比对：NTFS 的 100ns 精度在 JSON 落盘时被截断为 ms，
            // 若用全等于 == 比对，重启后 loaded 的 mtime 会被误判为"已修改" -> 100% MISS。
            let mtime_equal = |t1: SystemTime, t2: SystemTime| {
                let ms1 = t1
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis();
                let ms2 = t2
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis();
                ms1 == ms2
            };
            for entry in entries.iter_mut().filter(|e| e.is_dir) {
                // 昂贵系统目录：不入队扫描，直接呈现 "—"
                if is_expensive_system_dir(&entry.path) {
                    entry.dir_size_loaded = true;
                    entry.dir_size_deferred = true;
                    continue;
                }
                if let Some(&(cached_sz, cached_partial, cached_mtime)) =
                    store.cache.get(&entry.path)
                {
                    if entry.modified.is_some_and(|m| mtime_equal(m, cached_mtime)) {
                        entry.size = cached_sz;
                        entry.dir_size_partial = cached_partial;
                        entry.dir_size_loaded = true;
                    }
                }
            }
        }

        // 3. 回填后再排序，写入 pane。
        // 先清掉已过期的重命名“钉位”（见 rename_pin）。
        if self
            .rename_pin
            .as_ref()
            .is_some_and(|(_, _, _, until)| Instant::now() >= *until)
        {
            self.rename_pin = None;
        }
        let rename_pin = self.rename_pin.clone();
        {
            let pane = &mut tab.panes[result.pane];
            crate::fs::sort_entries(&mut entries, pane.sort);
            // 重命名后不重排：把刚改名的条目钉回它原来的显示位置，
            // 其余条目的相对顺序完全不变。（资源管理器同款：改名不引起跳变。）
            if let Some((dir, pin_path, pin_index, _)) = rename_pin {
                if pane.path.as_deref() == Some(dir.as_path()) {
                    if let Some((from, to)) = pin_entry_at(&mut entries, &pin_path, pin_index) {
                        crate::logv!(
                            "[rename] keep position: {:?} {} -> {}",
                            pin_path, from, to
                        );
                    }
                }
            }
            // 「此电脑」视图：记录本次真正拿到的驱动器，用于区分
            // “行缺失”还是“行在但图标空”。
            if pane.path.is_none() {
                let names: Vec<String> = entries.iter().map(|e| e.name.clone()).collect();
                crate::logv!("[list] this-pc drives n={} {:?}", names.len(), names);
            }
            pane.entries = entries;
            // 拼音缓存随 entries 失效（惰性重建）。
            pane.pinyin_cache.clear();
            // 新列表到达即失效全局搜索的截断提示。
            pane.search_total = None;
            // 「此电脑」重载 = 回到驱动器列表，残留的全局搜索词会误过滤空表。
            if pane.path.is_none() {
                pane.filter.clear();
            }
            pane.loading = false;
            pane.rebuild_display();
            // Shell 系统菜单「新建 → 文本文档」：菜单前的快照 diff 出唯一新增文件。
            // 只在 10 秒内、同一目录、恰好新增 1 个「文件」时生效，避免把粘贴 /
            // 批量新建 / 无关的后台创建误判成一次新建。zsdesk 用 IFileOperation
            // 的 PostNewItem 回调拿路径，Rove 走 Shell 菜单没有该回调，故用列举 diff。
            if let Some((dir, before, at)) = self.shell_new_watch.take() {
                if at.elapsed().as_secs() < 10
                    && pane.path.as_deref() == Some(dir.as_path())
                    && result.pane == tab.active_pane
                {
                    let added: Vec<PathBuf> = pane
                        .entries
                        .iter()
                        .filter(|e| !e.is_dir && !before.contains(&e.path))
                        .map(|e| e.path.clone())
                        .collect();
                    if added.len() == 1 {
                        logln!(
                            "[shell-new] detected new file {} (dir={})",
                            added[0].display(),
                            dir.display()
                        );
                        self.pending_shell_new_rename = Some(added[0].clone());
                        self.scroll_to_paths = vec![added[0].clone()];
                        self.scroll_select = true;
                        self.visible_cache = None;
                    } else if !added.is_empty() {
                        logln!("[shell-new] ignore diff: {} new entries", added.len());
                    }
                }
            }
            // 新建文件夹 / 粘贴完成：数据就位后把新条目换算成显示索引，交给
            // refresh_visible_rows 做“最小位移滚进可视区”（否则它们按排序沉到底部，看不到）。
            // 多条时取最靠前的一条，保证粘贴块的起始处可见。
            if !self.scroll_to_paths.is_empty() && result.pane == tab.active_pane {
                let mut matched: Vec<(usize, PathBuf)> = Vec::new();
                for (pos, &ei) in pane.display.iter().enumerate() {
                    if let Some(e) = pane.entries.get(ei) {
                        if self.scroll_to_paths.iter().any(|p| *p == e.path) {
                            matched.push((pos, e.path.clone()));
                        }
                    }
                }
                if let Some((pos, first)) = matched.first() {
                    self.scroll_to_index = Some(*pos);
                    // 粘贴/还原后选中它们（资源管理器同款）；新建文件夹不选（正在重命名）。
                    if self.scroll_select {
                        pane.anchor = Some(first.clone());
                        pane.selection = matched.iter().map(|(_, p)| p.clone()).collect();
                        self.visible_cache = None;
                    }
                    logln!(
                        "[new-folder] list matched: pos={} first={} scroll_select={} rename_path={}",
                        pos,
                        first.display(),
                        self.scroll_select,
                        self.ui.get_rename_path()
                    );
                    self.scroll_to_paths.clear();
                    self.scroll_select = false;
                } else {
                    // 列举结果里还没出现目标（异步竞态）→ 保留 pending，等下一批。
                    logln!(
                        "[new-folder] list MISS: waiting={} total={} rename_path={}",
                        self.scroll_to_paths.len(),
                        pane.display.len(),
                        self.ui.get_rename_path()
                    );
                }
            }
        }
        let listed_path = tab.panes[result.pane].path.clone();

        // 核心修复：列举完成后立即记下目录 mtime，供 poll_dir_changes 比对。
        // 否则该目录在 dir_mtimes 里没有条目，下一次轮询（约几十 ms 后）会判定“已变化”，
        // 立即再全量列举一遍 —— 表现日志里就是每次导航/重命名后都出现两条同样的
        // request_listing + list_result。网络路径不参与轮询，无需记录。
        if let Some(p) = listed_path.as_ref() {
            if !is_network_path(p) {
                if let Ok(m) = std::fs::symlink_metadata(p).and_then(|m| m.modified()) {
                    self.dir_mtimes.insert(p.clone(), m);
                }
            }
        }
        // 记录最近目录（仅当前聚焦面板才写入）。
        if listed_is_active {
            if let Some(path) = listed_path {
                self.state.push_recent(&path);
                self.config.recent = self.state.recent.clone();
                self.config.save();
            }
        }
        if tab_index == self.state.active_tab {
            self.visible_cache = None;
            self.sync_file_list();
            if listed_is_active {
                self.sync_status();
                self.sync_nav();
            }
            // 核心修复：如果是导航（后退/前进/地址栏/面包屑均可）产生的加载，列表数据
            // 就位后恢复滚动条。navigate/jump_to 已按路径从 path_scroll_memo 取出 scroll_y
            // 并置 pending_scroll_restore；此时数据才真正就绪，clamp 已无意义（下面直接用恢复值推 UI）。
            let mut is_restore = false;
            if self.state.tabs[tab_index].panes[result.pane].pending_scroll_restore {
                self.state.tabs[tab_index].panes[result.pane].pending_scroll_restore = false;
                is_restore = true;
            }

            let (scroll0, is_split, scroll1) = {
                let tab = &self.state.tabs[tab_index];
                (
                    tab.panes.first().map(|p| p.scroll_y).unwrap_or(0.0),
                    tab.is_split,
                    tab.panes.get(1).map(|p| p.scroll_y).unwrap_or(0.0),
                )
            };

            if is_restore {
                crate::logv!(
                    "[nav] restore scroll for pane {} to {:.1} (target was history pos)",
                    result.pane,
                    if result.pane == 0 { scroll0 } else { scroll1 }
                );
                push_scroll(&self.ui, result.pane, if result.pane == 0 { scroll0 } else { scroll1 });
            } else {
                push_scroll(&self.ui, 0, scroll0);
                if is_split {
                    push_scroll(&self.ui, 1, scroll1);
                }
            }

            // 安全网：本次列举已完成，本标签下所有面板都解除“恢复中”标记。
            // 否则若某 pane 的列举因故没走到上面那条分支，标记会永久残留，
            // 导致该面板的滚动回调被一直忽略（表现为列表永远不跟随滚动）。
            for pane in &mut self.state.tabs[tab_index].panes {
                pane.pending_scroll_restore = false;
            }
        }
        // 标签标题可能变化（"此电脑" 不变，目录名会变）。
        self.sync_tabs();

        // ---- 异步统计子文件夹体积 ----
        // 命中缓存则瞬时填入（mtime 严格一致才算），仅未命中/已变动的目录提交后台。
        if let Some(current_path) = self.state.tabs[tab_index].panes[result.pane].path.as_ref() {
            // ⑥ 网络路径防护：UNC 路径（\\server\share）与映射网络驱动器（Z:\）
            // 都不自动递归，避免拖垮内网 IO。
            let is_net = is_network_path(current_path);
            if is_net {
                dirsize_trace(format!(
                    "Network path -> skip auto scan: {}",
                    current_path.display()
                ));
            }

            if !is_net {
                // 停掉上一批未完成的计算任务，避免旧结果回灌当前视图
                if let Some(old_stop) = self.dir_size_stop.take() {
                    old_stop.store(true, std::sync::atomic::Ordering::Relaxed);
                }

                // 收集未命中缓存的目录（第 2 步回填时已标记 dir_size_loaded）。
                let mut missing_dirs: Vec<(PathBuf, Option<SystemTime>)> = Vec::new();
                {
                    let tab = &mut self.state.tabs[tab_index];
                    for entry in tab.active_mut().entries.iter_mut().filter(|e| e.is_dir) {
                        if !entry.dir_size_loaded {
                            missing_dirs.push((entry.path.clone(), entry.modified));
                        }
                    }
                }

                // 有缓存命中且是当前标签时，先刷一遍 UI 让旧值立即显现
                if tab_index == self.state.active_tab {
                    self.visible_cache = None;
                    self.refresh_visible_rows();
                }

                // 2. 仅对未命中或 mtime 已变动的目录派发后台统计
                if !missing_dirs.is_empty() {
                    dirsize_trace(format!(
                        "cache MISS x{} -> dispatch background scan",
                        missing_dirs.len()
                    ));
                    let new_stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
                    self.dir_size_stop = Some(new_stop.clone());

                    let tab = &self.state.tabs[tab_index];
                    let _ = self.dir_size_tx.send(DirSizeRequest {
                        tab_id: tab.id,
                        generation: tab.active().generation,
                        dirs: missing_dirs,
                        stop: new_stop,
                    });
                }

                // ---- 文件变更监听：对当前活动目录递归监听整棵子树 ----
                if let Some(path) = self.state.tabs[tab_index].active().path.clone() {
                    let is_net = is_network_path(&path);
                    if tab_index == self.state.active_tab && !is_net {
                        // 若当前已有监听器且监听的正是该路径，直接复用，不重复重建
                        let already_watching = self
                            .active_watcher
                            .as_ref()
                            .map(|w| w.path == path)
                            .unwrap_or(false);

                        if !already_watching {
                            // 重建监听器：旧 watcher 在 drop 时自动关闭句柄，杀掉旧线程
                            self.active_watcher =
                                crate::fs::DirWatcher::start(&path, self.watch_tx.clone());
                            self.pending_invalidated_subdirs.clear();
                            self.last_watch_event = None;
                            dirsize_trace(format!("watcher started on {}", path.display()));
                        }
                    }
                }
            }
        }

        // Shell 系统菜单「新建 → 文本文档」：到这里列表数据、行模型、滚动都已
        // 就位，再进入重命名（在 diff 当场调用会抢在行模型重建之前，输入框拿不到行）。
        if let Some(path) = self.pending_shell_new_rename.take() {
            logln!("[shell-new] entering rename for {}", path.display());
            self.begin_rename(&path);
        }
    }

    /// 统一应用搜索词（搜索框编辑 / 外部唤起预填共用）：
    /// 「此电脑」= 全局索引秒搜；目录视图 = 本地过滤 + 异步深度搜索。
    fn apply_search_query(&mut self, text: &str) {
        logv!("search query: {text:?}");
        let is_root = self.state.active().active().path.is_none();
        // 记录搜索页面（可前进/后退）以及搜索前的滚动位置。
        self.state
            .active_mut()
            .active_mut()
            .set_filter_from_search(text);
        let clearing = text.trim().is_empty();
        // 「此电脑」视图：搜索框 = 全局拼音秒搜（Phase 5 索引）。
        if is_root {
            let query = text.trim().to_string();
            self.run_global_search(&query);
        } else {
            // 深度搜索（子树递归）：异步执行，结果经 poll timer 合并。
            self.start_deep_search();
            self.state.active_mut().active_mut().rebuild_display();
        }
        self.visible_cache = None;
        self.sync_file_list();
        self.sync_status();
        // 搜索会改动历史（set_filter_from_search 压入/弹出搜索页），必须同步前进/后退
        // 按钮状态；否则刚搜出来的页面在 UI 上仍是「不可后退」（与分屏/切换面板时
        // 搜索状态串台是同一类「搜索框 ↔ 活动面板导航态」一致性问题）。
        self.sync_nav();
        if clearing {
            // 清空搜索框：回到开始搜索前的滚动位置，而不是强制滚回顶部。
            let (pane_index, scroll) = {
                let tab = self.state.active();
                (tab.active_pane, tab.active().scroll_y)
            };
            push_scroll(&self.ui, pane_index, scroll);
        } else {
            self.ui.invoke_scroll_list_to_top();
        }
    }

    /// 启动/重置当前标签的深度搜索（filter 变化时调用）。
    /// 空查询仅清空残留结果；有效查询在代号递增后发往工作线程。
    fn start_deep_search(&mut self) {
        let tab = self.state.active_mut();
        tab.active_mut().clear_deep();
        let (tab_id, pane, generation, deep_generation, root, query) = (
            tab.id,
            tab.active_pane,
            tab.active_mut().generation,
            tab.active_mut().deep_generation,
            tab.active_mut().path.clone(),
            tab.active_mut().filter.trim().to_string(),
        );
        // 「此电脑」/ 回收站虚拟目录没有子树搜索。
        let Some(root) = root else {
            logv!("[deep] skip: root=none filter={:?}", query);
            return;
        };
        if root == fs::drives::recycle_virtual_path() || query.is_empty() {
            logv!("[deep] skip: root={:?} q={:?}", root, query);
            return;
        }
        let _ = self.deep_tx.send(DeepRequest {
            tab_id,
            pane,
            generation,
            deep_generation,
            root,
            query,
        });
        self.state.active_mut().active_mut().deep_searching = true;
    }

    /// 深度搜索结果到达：标签存在、代号与查询都匹配才应用。
    fn apply_deep_result(&mut self, result: DeepResult) {
        logv!(
            "deep result tab={} gen={} dgen={} n={}",
            result.tab_id,
            result.generation,
            result.deep_generation,
            result.items.len()
        );
        let Some(tab_index) = self.state.tab_by_id(result.tab_id) else {
            return;
        };
        let tab = &mut self.state.tabs[tab_index];
        let deep_is_active = result.pane == tab.active_pane;
        let pane = &mut tab.panes[result.pane];
        if pane.generation != result.generation || pane.deep_generation != result.deep_generation {
            logv!(
                "[deep] drop stale gen pane={}/{} dgen={}/{} n={}",
                pane.generation,
                result.generation,
                pane.deep_generation,
                result.deep_generation,
                result.items.len()
            );
            return; // 过期结果
        }
        if pane.filter.trim().to_lowercase() != result.query.trim().to_lowercase() {
            logln!(
                "[deep] !! drop mismatched filter: pane={:?} result={:?}",
                pane.filter,
                result.query
            );
            return;
        }
        logv!(
            "[deep] apply pane={} n={} filter={:?}",
            result.pane,
            result.items.len(),
            result.query
        );
        pane.deep_entries = result.items.iter().map(|i| i.entry.clone()).collect();
        pane.deep_pinyin = result.items.into_iter().map(|i| i.fields).collect();
        pane.deep_searching = false;
        pane.rebuild_display();
        if tab_index == self.state.active_tab {
            self.visible_cache = None;
            self.sync_file_list();
            if deep_is_active {
                self.sync_status();
            }
        }
    }

    /// 文件夹体积统计结果到达：标签与代号都匹配才写入条目。
    /// 命中后先写入带 mtime 的 LRU 缓存，再刷新当前视图。
    fn apply_dir_size_result(&mut self, result: DirSizeResult) {
        // 写入 LRU 缓存（FIFO 淘汰，达到上限移除最旧项）。
        if let Some(mtime) = result.mtime {
            let mut store = global_dir_sizes().lock().unwrap();
            if !store.cache.contains_key(&result.path) {
                if store.cache.len() >= DIR_SIZE_CACHE_CAP {
                    if let Some(oldest) = store.order.first().cloned() {
                        store.order.remove(0);
                        store.cache.remove(&oldest);
                        dirsize_trace(format!(
                            "cache EVICT (cap={}): {}",
                            DIR_SIZE_CACHE_CAP,
                            oldest.display()
                        ));
                    }
                }
                store.order.push(result.path.clone());
            }
            store
                .cache
                .insert(result.path.clone(), (result.size, result.partial, mtime));
            dirsize_trace(format!(
                "cache STORE: {} = {} bytes (mtime ok, total {} entries)",
                result.path.display(),
                result.size,
                store.cache.len()
            ));
            store.mark_modified();
        } else {
            dirsize_trace(format!(
                "result without mtime, NOT cached: {}",
                result.path.display()
            ));
        }

        let Some(tab_index) = self.state.tab_by_id(result.tab_id) else {
            return;
        };
        // 局部借用块：写入条目并顺带取出选中态，块结束即释放借用，
        // 避免与下方 self.* 重绘调用冲突。
        let is_selected = {
            let tab = &mut self.state.tabs[tab_index];
            if tab.active_mut().generation != result.generation {
                return;
            }
            if let Some(entry) = tab.active_mut().entries.iter_mut().find(|e| e.path == result.path) {
                entry.size = result.size;
                entry.dir_size_partial = result.partial;
                entry.dir_size_loaded = true;
            }
            tab.active_mut().selection.contains(&result.path)
        };

        if tab_index == self.state.active_tab {
            // 让可见缓存失效，就地重新渲染当前视口中的行
            self.visible_cache = None;
            self.refresh_visible_rows();
            if is_selected {
                self.sync_status();
            }

            // 核心修复：不立刻重排，打个标记，等用户空闲时再排。
            // （体积结果按标签回传、可能属于任一面板，故只要该标签有按大小排序的面板就置位）
            if self.state.tabs[tab_index]
                .panes
                .iter()
                .any(|p| p.sort.key == SortKey::Size)
            {
                self.size_sort_pending = true;
            }
        }
    }
}

/// 设置标签栏加载态（仅影响激活标签的 spinner）。
fn tab_loading_flag(ui: &MainWindow, is_active: bool, loading: bool) {
    if is_active {
        ui.set_tab_loading(loading);
    }
}

// ---------------------------------------------------------------------------
// 后台线程
// ---------------------------------------------------------------------------

/// 判断是否为网络路径（包含 UNC 路径和映射网络驱动器）。
/// 零系统调用开销：UNC 走前缀字符串判断；映射盘则查 DriveInfo 类型。
/// 是否为本地固定磁盘（排除 U 盘/光驱/网络盘）。
/// 缩略图磁盘缓存只对固定盘生效：可移动介质拔掉后缓存会变成垃圾，
/// 网络盘重读代价本来就高且缓存命中率低（对标 Pulse 的 `Eligible()`）。
#[cfg(windows)]
fn is_fixed_drive(path: &Path) -> bool {
    use windows::core::PCWSTR;
    use windows::Win32::Storage::FileSystem::GetDriveTypeW;

    let Some(std::path::Component::Prefix(prefix)) = path.components().next() else {
        return false;
    };
    let std::path::Prefix::Disk(drive) = prefix.kind() else {
        return false;
    };
    let root: Vec<u16> = format!("{}:\\\0", (drive as char).to_ascii_uppercase())
        .encode_utf16()
        .collect();
    // DRIVE_FIXED = 3
    unsafe { GetDriveTypeW(PCWSTR(root.as_ptr())) == 3 }
}

#[cfg(not(windows))]
fn is_fixed_drive(_path: &Path) -> bool {
    false
}

/// 判断是否为「必须永远落盘」的格式（无视 30ms 门槛）。
///
/// 对标 Pulse 的 `AlwaysKept()`：这些格式一旦 Windows 自带缩略图缓存被清理，
/// 重新冷抽帧的代价极大（本次实测 max 1369ms），所以即使本次取得很快也要存一份。
///
/// 注：**目录不在白名单内**。目录解析只有 30–106ms，且绝大多数是同一张“文件夹
/// 图形”；落 96px 小图会在 icons-xl（显示 264px）下放大 2.75× 发糊，
/// 存原尺寸又要 19.4KB/个。所以目录干脆不落盘，只靠内存 LRU。
fn is_always_kept(path: &Path) -> bool {
    let ext = path
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    matches!(
        ext.as_str(),
        "mp4" | "m4v"
            | "mkv"
            | "mov"
            | "avi"
            | "wmv"
            | "webm"
            | "flv"
            | "mpg"
            | "mpeg"
            | "vob"
            | "ts"
            | "mts"
            | "m2ts"
            | "3gp"
            | "3g2"
            | "ogv"
            | "asf"
            | "rm"
            | "rmvb"
            | "heic"
            | "heif"
            | "hif"
            | "avif"
            | "jxl"
            | "psd"
            | "psb"
            | "pdf"
            | "tif"
            | "tiff"
            | "cr2"
            | "cr3"
            | "nef"
            | "arw"
            | "dng"
            | "orf"
            | "rw2"
            | "raf"
            | "pef"
            | "srw"
            | "rwl"
            | "srf"
            | "sr2"
    )
}

fn is_network_path(path: &Path) -> bool {
    let s = path.to_string_lossy();
    if s.starts_with(r"\\") {
        return true;
    }
    // 检查是否为映射网络驱动器（如 Z:\）
    if let Some(std::path::Component::Prefix(prefix)) = path.components().next() {
        if let std::path::Prefix::Disk(drive) = prefix.kind() {
            let letter = (drive as char).to_ascii_uppercase();
            if let Some(info) = crate::fs::list_drives().iter().find(|d| d.letter == letter) {
                if info.drive_type == crate::fs::drives::DriveType::Network {
                    return true;
                }
            }
        }
    }
    false
}

/// 获取 Rove 在 %LOCALAPPDATA% 下的专属数据目录（不存在则创建）。
/// 缩略图 / 索引 / 容量缓存都放在这里，避免被系统 %tmp% 清理逻辑误删。
fn rove_data_dir() -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let dir = base.join("Rove");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// 缩略图磁盘缓存路径，使用两级哈希子目录避免单目录文件过多。
fn thumb_disk_path(key: &ThumbKey) -> PathBuf {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    key.hash(&mut hasher);
    let hash = format!("{:016x}", hasher.finish());
    rove_data_dir()
        .join("thumbs")
        .join(&hash[0..2])
        .join(format!("{}.bin", hash))
}

/// 扫描并清理缩略图磁盘缓存：总大小超过预算时，按最后修改时间删除最旧文件到 80%。
/// 磁盘缓存当前总字节数（原子）。启动时扫描一次填充，此后每次写入 `fetch_add`，
/// 越过预算就异步 trim。避免每次写盘都递归扫描目录（Pulse 的做法）。
static THUMB_DISK_BYTES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// 触发一次异步磁盘缓存清理（不阻塞调用线程）。
fn trigger_thumb_disk_trim() {
    std::thread::spawn(|| {
        let root = rove_data_dir().join("thumbs");
        let mut files: Vec<(PathBuf, u64, std::time::SystemTime)> = Vec::new();
        if let Ok(read_dir) = std::fs::read_dir(&root) {
            for entry in read_dir.flatten() {
                if let Ok(sub_dir) = std::fs::read_dir(entry.path()) {
                    for sub_entry in sub_dir.flatten() {
                        if let Ok(meta) = sub_entry.metadata() {
                            if meta.is_file() {
                                let mtime = meta.modified().unwrap_or(std::time::UNIX_EPOCH);
                                files.push((sub_entry.path(), meta.len(), mtime));
                            }
                        }
                    }
                }
            }
        }
        files.sort_by_key(|(_, _, mtime)| *mtime);
        let mut current = THUMB_DISK_BYTES.load(std::sync::atomic::Ordering::Relaxed);
        let target = (THUMB_DISK_MAX_BYTES as f64 * 0.8) as u64;
        for (path, size, _) in files {
            if current <= target {
                break;
            }
            if std::fs::remove_file(&path).is_ok() {
                current = current.saturating_sub(size);
            }
        }
        THUMB_DISK_BYTES.store(current, std::sync::atomic::Ordering::Relaxed);
    });
}

/// 启动时初始化磁盘缓存统计：只计数，不做全量删除（写入时按需淘汰）。
fn init_thumb_disk_cache() {
    let root = rove_data_dir().join("thumbs");
    let mut total = 0u64;
    if let Ok(read_dir) = std::fs::read_dir(&root) {
        for entry in read_dir.flatten() {
            if let Ok(sub_dir) = std::fs::read_dir(entry.path()) {
                for sub_entry in sub_dir.flatten() {
                    if let Ok(meta) = sub_entry.metadata() {
                        if meta.is_file() {
                            total += meta.len();
                        }
                    }
                }
            }
        }
    }
    THUMB_DISK_BYTES.store(total, std::sync::atomic::Ordering::Relaxed);
    crate::logln!("[thumb] disk cache {} KB", total / 1024);
    if total > THUMB_DISK_MAX_BYTES {
        trigger_thumb_disk_trim();
    }
}

/// 是否位于 Rove 自身的数据目录（`%LOCALAPPDATA%\Rove`）下。
///
/// 这类文件（`dir_sizes.json` / `index.bin` / `thumbs\`）每次运行都会被重写，
/// mtime/size 变化 → `ThumbKey` 变化 → 每次会话多生成一份新缩略图，白占预算。
///
/// 注意：Windows 路径大小写不敏感，而 `Path::starts_with` 是字节级比较。
/// 磁盘上目录名可能是早期版本建的 `rove`（小写），而 `rove_data_dir()` 拼的是
/// `Rove`，直接用 `starts_with` 永远不会匹配，所以统一转小写再比。
fn is_rove_data_path(path: &Path) -> bool {
    static ROOT: OnceLock<String> = OnceLock::new();
    let root = ROOT.get_or_init(|| rove_data_dir().to_string_lossy().to_lowercase());
    path.to_string_lossy().to_lowercase().starts_with(root.as_str())
}

/// 强制把面板的 scroll_y 推送到 UI，让视图与虚拟列表的计算同步。
///
/// 使用 `invoke_set_file_scroll_y` 直接调用 Slint 的公开函数，
/// 绕过属性 `changed` 事件的合并延迟，确保切换标签的一瞬间布局立刻复位。
fn push_scroll(ui: &MainWindow, pane: usize, scroll_y: f32) {
    // 排查：重命名期间的任何程序推值都记下来（正常路径不会推，除非 clamp/导航）。
    if !ui.get_rename_path().is_empty() {
        crate::logv!("[push] !! pane={} -> {:.1}", pane, scroll_y.max(0.0));
    }
    ui.invoke_set_file_scroll_y(pane as i32, scroll_y.max(0.0));
}

/// 启动目录列举工作线程。
fn spawn_list_worker(rx: Receiver<ListRequest>, tx: Sender<ListResult>) {
    // 核心修复 2：改为分发器模式。每个请求独立开启线程，避免网络 I/O 阻塞本地目录列举。
    std::thread::Builder::new()
        .name("rove-lister-dispatcher".into())
        .spawn(move || {
            logln!("lister dispatcher started");
            while let Ok(req) = rx.recv() {
                let tx = tx.clone();
                std::thread::spawn(move || {
                    logv!("worker req tab={} path={:?}", req.tab_id, req.path);
                    let entries = match &req.path {
                        None => list_drives().iter().map(FileEntry::drive).collect(),
                        Some(path) if *path == fs::drives::recycle_virtual_path() => {
                            // 回收站站内浏览：虚拟目录，条目来自系统回收站。
                            let mut entries = fs::drives::list_recycle_entries();
                            sort_entries(&mut entries, req.sort);
                            entries
                        }
                        Some(path) => {
                            let mut entries = fs::list_directory(path);
                            sort_entries(&mut entries, req.sort);
                            entries
                        }
                    };
                    let _ = tx.send(ListResult {
                        tab_id: req.tab_id,
                        pane: req.pane,
                        generation: req.generation,
                        entries,
                    });
                });
            }
        })
        .expect("spawn rove-lister dispatcher");
}

/// 启动深度搜索工作线程（文件夹视图子树递归查找）。
/// 会话私有；扫描有上限（20k 条目 / 300 结果），完成即回传，
/// 过期结果由 UI 端按代号丢弃，无需中断机制。
fn spawn_deep_worker(rx: Receiver<DeepRequest>, tx: Sender<DeepResult>) {
    std::thread::Builder::new()
        .name("rove-deepsearch".into())
        .spawn(move || {
            logln!("deepsearch worker started");
            while let Ok(req) = rx.recv() {
                let query = req.query.trim().to_lowercase();
                let items = if query.is_empty() {
                    Vec::new()
                } else {
                    logln!("deep req tab={} root={:?} q={query}", req.tab_id, req.root);
                    crate::search::index::deep_scan(&req.root, &query)
                };
                let sent = tx.send(DeepResult {
                    tab_id: req.tab_id,
                    pane: req.pane,
                    generation: req.generation,
                    deep_generation: req.deep_generation,
                    query: req.query,
                    items,
                });
                if sent.is_err() {
                    break;
                }
            }
        })
        .expect("spawn rove-deepsearch worker");
}

/// 启动文件夹体积统计工作线程。
/// 一批请求内的目录逐个递归计算，算完一个立即回传一个；
/// `req.stop` 被置位（切换目录/刷新）后尽快放弃剩余目录。
fn spawn_dir_size_worker(rx: Receiver<DirSizeRequest>, tx: Sender<DirSizeResult>) {
    std::thread::Builder::new()
        .name("rove-dirsize".into())
        .spawn(move || {
            while let Ok(req) = rx.recv() {
                for (dir, mtime) in req.dirs {
                    if req.stop.load(std::sync::atomic::Ordering::Relaxed) {
                        break;
                    }
                    let (sz, partial) = crate::fs::dir::calculate_dir_size(&dir, &req.stop);
                    if req.stop.load(std::sync::atomic::Ordering::Relaxed) {
                        break;
                    }
                    let sent = tx.send(DirSizeResult {
                        tab_id: req.tab_id,
                        generation: req.generation,
                        path: dir,
                        size: sz,
                        partial,
                        mtime,
                    });
                    if sent.is_err() {
                        return;
                    }
                }
            }
        })
        .expect("spawn rove-dirsize worker");
}

/// 缩略图工作线程：COM 初始化一次，循环取路径生成缩略图，
/// 经事件循环回写缓存（线程安全：Image 可跨线程发送）。
///
/// 缩略图工作线程分发器。两个池：
/// - 主池（`主池（自适应，见下）`）：当前目录的全量缩略图，轮询分发；
/// - 优先池（`PRIORITY_THREADS`）：只装**当前可见窗口**的请求，
///   抢在主池积压任务之前完成，解决“滚到中段却从顶部一个个加载”。
#[cfg(windows)]
fn spawn_thumb_worker(rx: mpsc::Receiver<ThumbKey>, prio_rx: mpsc::Receiver<ThumbKey>) {
    // 启动时初始化磁盘字节统计（仅计数，超预算才异步裁剪）。
    std::thread::spawn(init_thumb_disk_cache);

    std::thread::Builder::new()
        .name("rove-thumbs-dispatcher".into())
        .spawn(move || {
            // 主池线程数随 CPU 核数自适应：留 2 核给 UI + 系统，
            // 夹在 [2, 6] 之间。下限 2 = 至少 1 条图片专用 + 1 条视频；
            // 上限 6 避免抢光多核机的算力。优先池固定 2 条，
            // 故总线程 = num_threads + 2，范围 [4, 8]。
            let cores = std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(4);
            let num_threads = (cores.saturating_sub(2)).clamp(2, 6);
            const PRIORITY_THREADS: usize = 2;
            crate::logln!(
                "[thumb] pool: cores={} main={} (1 fast + {} slow) prio={} total={}",
                cores,
                num_threads,
                num_threads - 1,
                PRIORITY_THREADS,
                num_threads + PRIORITY_THREADS
            );
            let mut worker_txs = Vec::with_capacity(num_threads);

            // 0. 优先池：只服务当前可见窗口。独立线程，不抢主池队列。
            {
                let mut prio_txs = Vec::with_capacity(PRIORITY_THREADS);
                for i in 0..PRIORITY_THREADS {
                    let (tx, worker_rx) = mpsc::channel::<ThumbKey>();
                    prio_txs.push(tx);
                    let spawned = std::thread::Builder::new()
                        .name(format!("rove-thumb-prio-{i}"))
                        .spawn(move || {
                            crate::logv!("[thumb] prio-{i} started");
                            while let Ok(key) = worker_rx.recv() {
                                thumb_decode_one("prio", i, key);
                            }
                            crate::logv!("[thumb] prio-{i} channel closed");
                        });
                    match spawned {
                        Ok(_) => crate::logv!("[thumb] prio-{i} spawned"),
                        Err(e) => crate::logln!("[thumb] prio-{i} SPAWN FAILED: {e}"),
                    }
                }
                std::thread::Builder::new()
                    .name("rove-thumbs-prio-dispatch".into())
                    .spawn(move || {
                        let n = prio_txs.len().max(1);
                        let mut rr = 0usize;
                        let mut logged = 0usize;
                        while let Ok(req) = prio_rx.recv() {
                            if logged < 64 {
                                crate::logv!(
                                    "[thumb] prio dispatch {} size={}",
                                    req.path.display(),
                                    req.px_size
                                );
                                logged += 1;
                            }
                            let lane = rr % n;
                            if let Err(e) = prio_txs[lane].send(req) {
                                crate::logln!("[thumb] !! prio lane={lane} send FAILED: {e}");
                            }
                            rr += 1;
                        }
                        crate::logv!("[thumb] prio dispatcher loop ended");
                    })
                    .ok();
            }

            // 1. 启动后台工作线程（数量自适应，见上方 num_threads 计算）。
            // 慢格式隔离（抄 Pulse 的 slow_cap=lanes-1）：
            //   lane 0 永远只接「快格式」（图片/文件夹），作为图片的专属通道；
            //   lane 1..n 承接「慢格式」（视频海报等），且慢格式在途数上限
            //   slow_cap = num_threads-1，永远给图片留 1 条空闲线程，
            //   避免滚到「图片+视频混排」文件夹时图片被视频卡死。
            const FAST_LANE: usize = 0;
            let slow_cap = num_threads - 1; // 始终 ≥ 1
            let (slow_done_tx, slow_done_rx) = mpsc::channel::<()>();
            for i in 0..num_threads {
                let (tx, worker_rx) = mpsc::channel::<ThumbKey>();
                worker_txs.push(tx);
                let slow_done_tx = slow_done_tx.clone();
                let spawned = std::thread::Builder::new()
                    .name(format!("rove-thumb-worker-{i}"))
                    .spawn(move || {
                        crate::logv!("[thumb] worker-{i} started");
                        while let Ok(key) = worker_rx.recv() {
                            let slow = crate::ops::thumbs::is_slow_thumbnail(&key.path);
                            thumb_decode_one("worker", i, key);
                            if slow {
                                let _ = slow_done_tx.send(());
                            }
                        }
                        crate::logv!("[thumb] worker-{i} channel closed");
                    });
                match spawned {
                    Ok(_) => crate::logv!("[thumb] worker-{i} spawned"),
                    Err(e) => crate::logln!("[thumb] worker-{i} SPAWN FAILED: {e} (requests on this lane will be LOST)"),
                }
            }

            // 2. 主分发循环：慢格式隔离 + 快格式优先 lane 0。
            let mut rr = 0;
            let mut slow_in_flight = 0usize;
            // 慢格式在途已满时的暂存区（避免阻塞分发线程、饿死快格式）。
            let mut slow_stash: std::collections::VecDeque<ThumbKey> = Default::default();
            loop {
                // 回收已完成的慢请求，腾出慢格式名额。
                while let Ok(()) = slow_done_rx.try_recv() {
                    slow_in_flight = slow_in_flight.saturating_sub(1);
                }
                // 有空位就排空暂存。
                while slow_in_flight < slow_cap {
                    let Some(k) = slow_stash.pop_front() else { break };
                    let lane = FAST_LANE + 1 + (rr % (num_threads - 1));
                    rr += 1;
                    let _ = worker_txs[lane].send(k);
                    slow_in_flight += 1;
                }
                let req = match rx.recv_timeout(std::time::Duration::from_millis(50)) {
                    Ok(r) => r,
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                };
                let slow = crate::ops::thumbs::is_slow_thumbnail(&req.path);
                if slow {
                    if slow_in_flight < slow_cap {
                        let lane = FAST_LANE + 1 + (rr % (num_threads - 1));
                        rr += 1;
                        if let Err(e) = worker_txs[lane].send(req) {
                            crate::logln!("[thumb] !! slow lane={lane} send FAILED: {e}");
                        }
                        slow_in_flight += 1;
                    } else {
                        slow_stash.push_back(req);
                    }
                } else {
                    // 快格式：优先 lane 0（图片专属通道）。
                    if let Err(e) = worker_txs[FAST_LANE].send(req) {
                        crate::logln!("[thumb] !! fast lane send FAILED: {e}");
                    }
                }
            }
            crate::logv!("[thumb] dispatcher loop ended");
        })
        .ok();
}

/// 单个缩略图任务：解码 → （可选）落盘缓存 → 回主线程写内存 LRU。
///
/// `tag` / `idx` 只用于日志区分是主池还是优先池的哪个线程。
#[cfg(windows)]
fn thumb_decode_one(tag: &str, idx: usize, key: ThumbKey) {
    // 每个工作线程只需在启动时初始化一次 COM (STA)，CoInitializeEx 是幂等的。
    unsafe {
        use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
    }
    crate::logv!(
        "[thumb] {tag}-{idx} recv {} size={}",
        key.path.display(),
        key.px_size
    );
    match crate::ops::thumbs::get_thumbnail_timed(&key.path, key.px_size) {
        Ok((w, h, rgba, pure_decode_ms)) => {
            const MIN_DECODE_MS: u128 = 30;
            let force_keep = is_always_kept(&key.path);

            // 目录不落盘：解析仅 30–106ms，且图标几乎都是同一张“文件夹图形”；
            // 落小图会在 icons-xl（显示 264px）下放大发糊，落原图又要 19.4KB/个。
            // 文件：视频/RAW/PDF 等白名单无视耗时门槛，其他格式仍要求 >= 30ms。
            // 且只写到本地固定盘（U 盘/光驱/网络盘不写）。
            // Rove 自有数据文件（dir_sizes.json / index.bin）每次重写，一律排除。
            if !key.path.is_dir()
                && (pure_decode_ms >= MIN_DECODE_MS || force_keep)
                && is_fixed_drive(&key.path)
                && !is_rove_data_path(&key.path)
            {
                let disk_path = thumb_disk_path(&key);
                if let Some(parent) = disk_path.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                // WIC 编码：不透明→JPEG q90，带 alpha→PNG（原分辨率）。
                if crate::ops::thumbs::encode_wic_to_file(&disk_path, w, h, &rgba, None).is_ok() {
                    if let Ok(meta) = std::fs::metadata(&disk_path) {
                        let total = THUMB_DISK_BYTES
                            .fetch_add(meta.len(), std::sync::atomic::Ordering::Relaxed)
                            + meta.len();
                        if total > THUMB_DISK_MAX_BYTES {
                            trigger_thumb_disk_trim();
                        }
                    }
                }
            }

            // 2. 交还给主线程构建 Image 并写入内存 LRU。
            let _ = slint::invoke_from_event_loop(move || {
                let Some(img) = to_slint_image(w, h, &rgba) else {
                    // 静默丢弃会让 thumb_pending 永远留着该 key →
                    // 该行永久空白且不再重试。必须记日志。
                    crate::logln!(
                        "[thumb] !! to_slint_image FAILED {} {}x{} bytes={} (row stays blank)",
                        key.path.display(),
                        w,
                        h,
                        rgba.len()
                    );
                    if let Some(app) = SESSION.with(|slot| slot.borrow().clone()) {
                        app.borrow_mut().fail_thumbnail(key);
                    }
                    return;
                };
                let Some(app) = SESSION.with(|slot| slot.borrow().clone()) else {
                    crate::logln!(
                        "[thumb] !! SESSION None when storing {} (row stays blank)",
                        key.path.display()
                    );
                    return;
                };
                app.borrow_mut().store_thumbnail(key, img);
            });
        }
        Err(_e) => {
            crate::logln!("[thumb] {tag}-{idx} failed {}: {_e}", key.path.display());
            let _ = slint::invoke_from_event_loop(move || {
                let Some(app) = SESSION.with(|slot| slot.borrow().clone()) else {
                    return;
                };
                app.borrow_mut().fail_thumbnail(key);
            });
        }
    }
}

/// BGRA→RGBA 已由 thumbs 模块完成；这里包成 Slint 图像。
#[allow(dead_code)]
fn to_slint_image(w: u32, h: u32, rgba: &[u8]) -> Option<slint::Image> {
    if w == 0 || h == 0 || rgba.len() != (w * h * 4) as usize {
        return None;
    }
    let buffer = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(rgba, w, h);
    Some(slint::Image::from_rgba8(buffer))
}

/// 拖出手势判定（纯函数，便于测试）：给定按下点与当前指针位置，
/// 若位移超过阈值则返回待拖路径（调用方应随后清除 drag_pending 并启动拖出）。
/// 关键：未越过阈值时必须返回 None 且不消耗 drag_pending，否则首次微小移动
/// 就会清空待定手势，导致永远无法拖出。
fn drag_gesture_paths(
    drag_pending: &Option<(f32, f32, Vec<PathBuf>)>,
    x: f32,
    y: f32,
) -> Option<Vec<PathBuf>> {
    let (sx, sy, paths) = drag_pending.as_ref()?;
    let dx = x - sx;
    let dy = y - sy;
    if dx * dx + dy * dy > crate::sys::dnd::DRAG_THRESHOLD * crate::sys::dnd::DRAG_THRESHOLD {
        Some(paths.clone())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 三种视图必须能区分开：普通目录、此电脑（路径 None）、回收站。
    #[test]
    fn view_sort_classifies_paths() {
        assert!(matches!(ViewSort::of(None), ViewSort::ThisPc));
        let recycle = crate::fs::drives::recycle_virtual_path();
        assert!(matches!(ViewSort::of(Some(recycle.as_path())), ViewSort::Recycle));
        let normal = std::path::PathBuf::from("D:\\重");
        assert!(matches!(ViewSort::of(Some(normal.as_path())), ViewSort::Normal));
    }

    /// 每个视图只能按自己真实存在的列排序：否则会变成「按一列看不见的东西排」，
    /// 用户既发现不了也改不回来。
    #[test]
    fn views_only_allow_their_own_columns() {
        // 此电脑：盘符/类型/大小，没有名称列、也没有修改时间列。
        assert!(ViewSort::ThisPc.allows(SortKey::Drive));
        assert!(ViewSort::ThisPc.allows(SortKey::Type));
        assert!(ViewSort::ThisPc.allows(SortKey::Size));
        assert!(!ViewSort::ThisPc.allows(SortKey::Modified));
        assert!(!ViewSort::ThisPc.allows(SortKey::Name));

        // 回收站与普通目录：有名称/修改时间/类型/大小，没有「盘符」。
        for view in [ViewSort::Recycle, ViewSort::Normal] {
            for key in [
                SortKey::Name,
                SortKey::Modified,
                SortKey::Type,
                SortKey::Size,
                SortKey::Path,
            ] {
                assert!(view.allows(key), "{key:?} 应该允许");
            }
            assert!(!view.allows(SortKey::Drive));
        }

        // 兼底：不可用时退回该视图的主键，可用时原样返回。
        assert_eq!(ViewSort::ThisPc.fallback(SortKey::Modified), SortKey::Drive);
        assert_eq!(ViewSort::Recycle.fallback(SortKey::Drive), SortKey::Name);
        assert_eq!(ViewSort::Normal.fallback(SortKey::Size), SortKey::Size);
    }

    /// 排序字段的字符串标识必须能双向转换：落盘后再读回来不能变成别的字段。
    #[test]
    fn sort_key_ids_roundtrip() {
        for key in [
            SortKey::Name,
            SortKey::Size,
            SortKey::Modified,
            SortKey::Type,
            SortKey::Path,
            SortKey::Drive,
        ] {
            assert_eq!(parse_sort_key(sort_key_id(key), SortKey::Name), key);
        }
        // 配置文件里出现未知/残缺值时不惊动现状，走调用方给的兑底值。
        assert_eq!(parse_sort_key("", SortKey::Drive), SortKey::Drive);
        assert_eq!(parse_sort_key("nonsense", SortKey::Modified), SortKey::Modified);
    }

    /// 超长名字截断时必须保留扩展名（否则文件失去关联类型）。
    #[test]
    fn clamp_file_name_keeps_extension() {
        let long_stem = "文".repeat(300);
        let name = format!("{}.mp4", long_stem);
        let out = clamp_file_name(&name);
        assert!(out.ends_with(".mp4"), "扩展名丢了: {:?}", &out[out.len() - 8..]);
        assert_eq!(utf16_len(&out), MAX_FILE_NAME_UNITS);

        // 短名字不动
        assert_eq!(clamp_file_name("a.mp4"), "a.mp4");

        // 无扩展名 → 直接截到上限
        let no_ext = "文".repeat(300);
        assert_eq!(utf16_len(&clamp_file_name(&no_ext)), MAX_FILE_NAME_UNITS);

        // 开头点（.gitignore 风格）不算扩展名
        let hidden = format!(".{}", "文".repeat(300));
        assert_eq!(utf16_len(&clamp_file_name(&hidden)), MAX_FILE_NAME_UNITS);

        // 多后缀只保留最后一段
        let multi = format!("{}.tar.gz", "文".repeat(300));
        assert!(clamp_file_name(&multi).ends_with(".gz"));
    }

    /// 截断不能把一个 emoji（代理对）切成半个。
    #[test]
    fn clamp_file_name_does_not_split_surrogates() {
        let name = "🦀".repeat(200); // 每个 2 个 UTF-16 码元
        let out = clamp_file_name(&name);
        assert!(utf16_len(&out) <= MAX_FILE_NAME_UNITS);
        assert_eq!(out.chars().count(), MAX_FILE_NAME_UNITS / 2);
        assert!(out.chars().all(|c| c == '🦀'));
    }

    #[test]
    fn pulse_size_format_matches_pulse_sidebar() {
        let kib = 1024u64;
        let mib = kib * 1024;
        let gib = mib * 1024;
        // GB 档：取整到整数 G。注意 Rust 的 `{:.0}` 是银行家舍入（半数取偶），
        // 6.5 → "6"、7.5 → "8"；故用 6.6 这类无歧义值验证进位。
        assert_eq!(format_pulse_size(268 * gib), "268G");
        assert_eq!(format_pulse_size(831 * gib), "831G");
        assert_eq!(format_pulse_size(6 * gib + 6 * gib / 10), "7G"); // 6.6G → 7G
        assert_eq!(format_pulse_size(6 * gib + gib / 2), "6G"); // 6.5G → 6G（半数取偶）
        assert_eq!(format_pulse_size(0), "0G");
        // TB 档：≥1000GB 才折算；≥10TB 取整，否则一位小数并去掉多余的 .0。
        assert_eq!(format_pulse_size(1024 * gib), "1TB");
        assert_eq!(format_pulse_size(6_200 * gib / 10), "620G"); // 620G 仍在 GB 档
        assert_eq!(format_pulse_size(2 * 1024 * gib + 512 * gib), "2.5TB");
        assert_eq!(format_pulse_size(12 * 1024 * gib), "12TB");
    }

    /// 重命名后不重排：被改名的条目回到原位置，其余条目相对顺序不变。
    #[test]
    fn rename_pin_keeps_position_without_disturbing_others() {
        let mut entries: Vec<crate::fs::entry::FileEntry> = ["a", "b", "c", "d"]
            .iter()
            .map(|n| crate::fs::dir::test_entry(n, true, 0))
            .collect();
        // 模拟“按名称排序后 d 改名为 "aa"”：重排后 aa 会跑到最前。
        let mut sorted: Vec<_> = entries.iter().map(|e| e.name.clone()).collect();
        sorted.sort();
        entries.sort_by(|x, y| x.name.cmp(&y.name));
        assert_eq!(
            entries.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(),
            vec!["a", "b", "c", "d"]
        );

        // 改名为 "aa" 后再排序 → 位置变成 0。
        let mut renamed = crate::fs::dir::test_entry("aa", true, 0);
        renamed.path = std::path::Path::new("d").to_path_buf();
        entries[3] = renamed;
        entries.sort_by(|x, y| x.name.cmp(&y.name));
        assert_eq!(
            entries.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(),
            vec!["a", "aa", "b", "c"]
        );

        // 钉回原显示位置 3：条目自己回到下标 3，其余顺序不变。
        let moved = pin_entry_at(&mut entries, std::path::Path::new("d"), 3);
        assert_eq!(moved, Some((1, 3)));
        assert_eq!(
            entries.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(),
            vec!["a", "b", "c", "aa"]
        );
    }

    /// 越界的目标下标要夹到末尾，且找不到路径时不应改动列表。
    #[test]
    fn rename_pin_clamps_and_tolerates_missing_entry() {
        let mut entries: Vec<crate::fs::entry::FileEntry> = ["a", "b"]
            .iter()
            .map(|n| crate::fs::dir::test_entry(n, false, 0))
            .collect();
        assert_eq!(pin_entry_at(&mut entries, std::path::Path::new("a"), 99), Some((0, 1)));
        assert_eq!(
            entries.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(),
            vec!["b", "a"]
        );
        assert_eq!(pin_entry_at(&mut entries, std::path::Path::new("zzz"), 0), None);
        assert_eq!(
            entries.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(),
            vec!["b", "a"]
        );
    }

    #[test]
    fn pulse_drive_palette_rotates_and_warns_on_full() {
        let drive = |used_gib: u64, total_gib: u64| DriveInfo {
            letter: 'C',
            label: String::new(),
            drive_type: fs::drives::DriveType::Fixed,
            total_bytes: total_gib * 1024 * 1024 * 1024,
            free_bytes: (total_gib - used_gib) * 1024 * 1024 * 1024,
        };
        // 调色盘按序号轮替：蓝 → 薄荷绿 → 琥珀黄 → 紫 → 回到蓝。
        let expected = [
            (0x3B, 0x82, 0xF6),
            (0x10, 0xB9, 0x81),
            (0xF5, 0x9E, 0x0B),
            (0xA8, 0x55, 0xF7),
            (0x3B, 0x82, 0xF6),
        ];
        for (i, (r, g, b)) in expected.iter().enumerate() {
            let c = drive_pulse_color(i, &drive(200, 1000));
            assert_eq!((c.red(), c.green(), c.blue()), (*r, *g, *b), "index {i}");
        }
        // 占用 >90% 一律告警红，覆盖轮替色。
        let full = drive_pulse_color(0, &drive(950, 1000));
        assert_eq!((full.red(), full.green(), full.blue()), (0xE0, 0x52, 0x52));
        // 恰好 90% 不算告急，仍用调色盘。
        let edge = drive_pulse_color(0, &drive(900, 1000));
        assert_eq!((edge.red(), edge.green(), edge.blue()), (0x3B, 0x82, 0xF6));
    }

    #[test]
    fn drag_gesture_accumulates_until_threshold() {
        let paths = vec![PathBuf::from("C:\\a.txt")];
        let dp = Some((0.0f32, 0.0f32, paths.clone()));

        // 首次微小移动（1px）：未越过 4px 阈值，不应触发，且手势保留。
        assert!(drag_gesture_paths(&dp, 1.0, 1.0).is_none());
        assert!(dp.is_some(), "手势不应被微小移动清空");

        // 第二次累计到 5px：越过阈值，触发并返回路径。
        let fired = drag_gesture_paths(&dp, 3.0, 4.0);
        assert!(fired.is_some());
        assert_eq!(fired.unwrap(), paths);
    }

    #[test]
    fn drag_gesture_none_when_no_pending() {
        assert!(drag_gesture_paths(&None, 10.0, 10.0).is_none());
    }

    /// 图标网格预切行：首行必须保留、最多 3 行、超长时末行带省略号。
    /// 回归用例来自实测样本（文件名含 ❀ 会让 Slint 的 elide 吞掉首行）。
    #[test]
    fn grid_display_name_keeps_first_line_and_ellipsis() {
        assert_eq!(grid_display_name("abc.mp4", 116.0, 3), "abc.mp4");

        let name = "-菇菇哩--【竖屏】❀江南夜❀温婉旗袍，国风小调丨P2仙气汉元素不要错过(1)-气质温婉旗袍.mp4";
        let out = grid_display_name(name, 116.0, 3);
        let lines: Vec<&str> = out.split('\n').collect();
        assert_eq!(lines.len(), 3, "应恰好 3 行: {out:?}");
        assert!(lines[0].starts_with("-菇菇哩--"), "首行被吞: {out:?}");
        assert!(lines[2].ends_with('…'), "末行应有省略号: {out:?}");

        // 未超过 3 行时不加省略号
        let fits = "文".repeat(24);
        let out2 = grid_display_name(&fits, 116.0, 3);
        assert!(!out2.contains('…'), "未超出行数不该有省略号: {out2:?}");
    }

    /// 英文按单词边界换行：单词不能被劈开，中文仍逐字断。
    #[test]
    fn grid_display_name_wraps_on_word_boundary() {
        let out = grid_display_name("Quarterly Financial Report Summary.mp4", 116.0, 3);
        for w in ["Quarterly", "Financial", "Report", "Summary.mp4"] {
            assert!(
                out.split('\n').any(|l| l.contains(w)),
                "单词 {w} 被换行拆开: {out:?}"
            );
        }

        // 单个超长单词放不下一行时只能硬断，但仍要压到 3 行且末行带省略号。
        let long = format!("{}.mp4", "Supercalifragilistic".repeat(3));
        let out2 = grid_display_name(&long, 116.0, 3);
        let lines: Vec<&str> = out2.split('\n').collect();
        assert_eq!(lines.len(), 3, "超长单词仍应压到 3 行: {out2:?}");
        assert!(!lines[0].is_empty(), "首行不能为空: {out2:?}");
        assert!(lines[2].ends_with('…'), "末行应有省略号: {out2:?}");
    }

    /// 切行要保留高亮分段：搜索命中片段跨行后仍带 hl 标记。
    #[test]
    fn grid_display_lines_keep_highlight_parts() {
        let parts = vec![
            ("Fire".to_string(), true),
            ("foxSetupWithAVeryLongNameForWrapping.exe".to_string(), false),
        ];
        let lines = grid_display_lines(&parts, 116.0, 3);
        let joined: String = lines
            .iter()
            .map(|l| l.iter().map(|(t, _)| t.as_str()).collect::<String>())
            .collect::<Vec<_>>()
            .join("");
        assert!(joined.starts_with("Fire"), "命中段不在行首: {joined}");
        assert!(
            lines[0].iter().any(|(t, hl)| *hl && t.contains("Fire")),
            "高亮标记丢了: {lines:?}"
        );
    }

    /// 状态栏危险提示的判定：失败/超限/删除类应为红色，普通提示不是。
    #[test]
    fn status_danger_markers() {
        assert!(status_is_danger("操作失败：Shell 操作失败，错误代码: 123"));
        assert!(status_is_danger("名称超过 255 个字符上限，已退出重命名"));
        assert!(status_is_danger("删除失败：拒绝访问"));
        // 删除成功 / 取消都不是危险提示（裸「删除」已故意从标记里移除）。
        assert!(!status_is_danger("已删除 3 个项目到回收站"));
        assert!(!status_is_danger("删除已取消"));
        assert!(!status_is_danger("已复制 3 个项目"));
        assert!(!status_is_danger("已重命名"));
        assert!(!status_is_danger(""));
    }

    /// 连续装饰符号会回退到符号字体，按 14px 的保守值切行，
    /// 否则多个符号累积超宽会让整行被水平裁掉。
    #[test]
    fn grid_display_name_budgets_fallback_symbols() {
        let name = format!("{}.mp4", "❀".repeat(12));
        let out = grid_display_name(&name, 116.0, 3);
        // 预算 = 116 - 11.5 = 104.5；符号按 14px 估 → 一行最多 7 个。
        let first = out.split('\n').next().unwrap();
        assert_eq!(
            first.chars().filter(|c| *c == '❀').count(),
            7,
            "按 11.5px 估会塞进 9 个而超宽: {out:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// UI 同步
// ---------------------------------------------------------------------------

/// 全盘索引就绪的跨线程通知入口。
///
/// 由后台索引线程（`search::index::spawn_builder`）经
/// `slint::invoke_from_event_loop` 投递，**必然运行在 UI 线程**，
/// 因此可以安全地借用 `SESSION` 中的 `App`。
///
/// 相比定时器轮询：零空转开销，且就绪与刷新落在同一事件循环 tick。
pub(crate) fn on_index_ready() {
    let Some(app) = SESSION.with(|slot| slot.borrow().clone()) else {
        return;
    };
    let mut app = app.borrow_mut();
    app.refresh_after_index_ready();
    // 无论是否重查都同步一次状态栏：把「全盘索引构建中...」摘掉。
    app.sync_status();
    // 设置页「搜索与索引」正在前台：立即刷新看板，无需等下一次轮询。
    let on_search_settings = {
        let tab = app.state.active();
        tab.kind == crate::state::TabKind::Settings
            && tab.settings_subpage == crate::state::SettingsSubpage::SearchAndIndex
    };
    if on_search_settings {
        app.sync_search_stats(false);
    }
}

impl App {
    /// Rust 侧生成的界面文案也走同一份字典。
    fn tr(&self, zh: &str) -> SharedString {
        i18n::text(&self.config.language, zh)
    }

    /// 带 `{}` 占位符的文案：按顺序替换（中英共用同一模板）。
    fn trf(&self, zh: &str, args: &[String]) -> String {
        let mut s = self.tr(zh).to_string();
        for a in args {
            s = s.replace("{}", a);
        }
        s
    }

    /// 类型徽标文案：字典没有的「XXX 文件」兜底成英文 "XXX file"。
    fn tr_type(&self, text: &str) -> SharedString {
        let t = i18n::text(&self.config.language, text);
        if t.as_str() != text || !text.ends_with(" 文件") {
            return t;
        }
        SharedString::from(format!("{} file", text.trim_end_matches(" 文件")))
    }

    /// 驱动器显示名（「本地磁盘 (C:)」）：类型标签部分走字典，卷标保持原样。
    fn tr_drive_label(&self, s: &str) -> SharedString {
        if let Some(pos) = s.rfind(" (") {
            let tail = &s[pos..];
            if tail.len() == 5 && tail.starts_with(" (") && tail.ends_with(":)") {
                return SharedString::from(format!("{}{}", self.tr_type(&s[..pos]), tail));
            }
        }
        SharedString::from(s)
    }

    fn sync_tabs(&self) {
        let tabs: Vec<TabInfo> = self
            .state
            .tabs
            .iter()
            .map(|t| {
                // 标签图标：设置页齿轮 / 此电脑 / 回收站 / 盘符字母（大写）。
                // 取路径首组件（如 "C:"）的首字符，非字母（网络路径等）回落文件夹符号。
                let icon_text = if t.kind == crate::state::TabKind::Settings {
                    "⚙".to_string()
                } else {
                    match &t.active().path {
                        None => self.tr("此").to_string(),
                        Some(p) if *p == fs::drives::recycle_virtual_path() => self.tr("回").to_string(),
                        Some(p) => p
                            .components()
                            .next()
                            .and_then(|c| c.as_os_str().to_str())
                            .and_then(|s| s.chars().next())
                            .filter(|c| c.is_ascii_alphabetic())
                            .map(|c| c.to_ascii_uppercase().to_string())
                            .unwrap_or_else(|| "📁".to_string()),
                    }
                };
                TabInfo {
                    id: t.id as i32,
                    title: i18n::text(&self.config.language, &t.title()),
                    active: t.id == self.state.active().id,
                    is_settings: t.kind == crate::state::TabKind::Settings,
                    icon_text: icon_text.into(),
                }
            })
            .collect();
        self.ui.set_tabs(ModelRc::new(VecModel::from(tabs)));
        // 标签数量/宽度变化会改变标题栏空白拖拽区，同步给命中测试钩子。
        #[cfg(windows)]
        sync_drag_bounds(self, "tabs");
        self.ui.set_active_tab(self.state.active_tab as i32);
        self.ui.set_tab_loading(self.state.active().active().loading);
        self.ui.set_settings_open(self.state.active().kind == crate::state::TabKind::Settings);
        // 标题栏实时显示当前聚焦标签的名称（切换标签、导航、设置子页都会走这里）。
        let window_title = format!(
            "{} - Rove",
            i18n::text(&self.config.language, &self.state.active().title())
        );
        self.ui.set_window_title(window_title.into());
    }

    fn sync_nav(&self) {
        let tab = self.state.active();
        self.ui.set_can_go_back(tab.active().can_go_back());
        self.ui.set_can_go_forward(tab.active().can_go_forward());
        self.ui.set_can_go_up(tab.active().parent_path().is_some());
    }

    fn sync_breadcrumbs(&self) {
        let tab = self.state.active();
        let mut segments: Vec<BreadcrumbSegment> = Vec::new();
        segments.push(BreadcrumbSegment {
            label: self.tr("此电脑"),
            path: SharedString::default(),
        });
        if let Some(path) = &tab.active().path {
            if *path == fs::drives::recycle_virtual_path() {
                // 回收站视图：单个虚拟段。
                segments.push(BreadcrumbSegment {
                    label: self.tr("回收站"),
                    path: fs::drives::recycle_virtual_path().to_string_lossy().into_owned().into(),
                });
                self.ui.set_breadcrumbs(ModelRc::new(VecModel::from(segments)));
                return;
            }
            // 驱动器段使用卷标（"Win11 (C:)"），其余为目录名。
            //
            // 网络路径必须特殊处理：`Path::components()` 把整个 UNC 前缀
            // （\\服务器\共享）当成**一段**，直接用会把
            // `\\192.168.1.5\Video` 粘成一个不可分的新户，
            // 而服务器根与共享目录其实都是可浏览路径，必须拆成两段。
            // 路径字符串**全程用 String 拼**，不用 `PathBuf`。
            // `PathBuf::new().to_string_lossy()` 在 "\\\\" 上得到空串（反斜杠被当成路径分隔符规范化掉），
            // 于是服务器根的 path 丢了前导反斜杠，地址栏编辑时变成 `192.168.1.5`。
            let mut cumulative = String::new();
            for comp in path.components() {
                match comp {
                    std::path::Component::Prefix(prefix) => match prefix.kind() {
                        std::path::Prefix::UNC(server, share)
                        | std::path::Prefix::VerbatimUNC(server, share) => {
                            let server = server.to_string_lossy().into_owned();
                            let server_root = format!("\\\\{server}");
                            segments.push(BreadcrumbSegment {
                                label: server.into(),
                                path: server_root.clone().into(),
                            });
                            cumulative = if share.is_empty() {
                                server_root
                            } else {
                                let share = share.to_string_lossy().into_owned();
                                let share_path = format!("{server_root}\\{share}");
                                segments.push(BreadcrumbSegment {
                                    label: share.into(),
                                    path: share_path.clone().into(),
                                });
                                share_path
                            };
                        }
                        _ => {
                            let text = comp.as_os_str().to_string_lossy().into_owned();
                            // "C:" → 补上根
                            cumulative = format!("{text}\\");
                            let label = self
                                .drives
                                .iter()
                                .find(|d| cumulative.starts_with(format!("{}:", d.letter).as_str()))
                                .map(|d| self.tr_drive_label(&d.display_label()).to_string())
                                .unwrap_or_else(|| cumulative.clone());
                            segments.push(BreadcrumbSegment {
                                label: label.into(),
                                path: cumulative.clone().into(),
                            });
                        }
                    },
                    // 根分隔符已由前缀段覆盖，不单独出段。
                    std::path::Component::RootDir => {}
                    std::path::Component::Normal(name) => {
                        let name = name.to_string_lossy().into_owned();
                        if !cumulative.ends_with('\\') {
                            cumulative.push('\\');
                        }
                        cumulative.push_str(&name);
                        segments.push(BreadcrumbSegment {
                            label: name.into(),
                            path: cumulative.clone().into(),
                        });
                    }
                    _ => {}
                }
            }
        }
        self.ui.set_breadcrumbs(ModelRc::new(VecModel::from(segments)));
    }

    /// 切换视图模式并持久化。
    /// Ctrl+滚轮切换视图。向上依次：详细信息 → 列表 → 小图标 → 中等图标
    /// → 大图标 → 超大图标；向下反过来；到达两端停住，不循环。
    fn cycle_view_mode(&mut self, up: bool) {
        const ORDER: [&str; 6] = [
            "details",
            "list",
            "icons-small",
            "icons-medium",
            "icons-large",
            "icons-xl",
        ];
        let Some(cur) = ORDER.iter().position(|m| *m == self.config.view_mode) else {
            return;
        };
        let next = if up {
            (cur + 1).min(ORDER.len() - 1)
        } else {
            cur.saturating_sub(1)
        };
        if next != cur {
            self.set_view_mode(ORDER[next]);
        }
    }

    fn set_view_mode(&mut self, mode: &str) {
        if self.config.view_mode == mode {
            return;
        }

        // 1. 记录当前视口顶部是第几个条目（index）：视图切换会改变 cell 布局，
        //    旧的像素偏移不能直接复用到新视图，必须先换回逻辑条目序号。
        //    行高统一从 UI 读（图标视图的行高按字体行高算出来，不再在 Rust 写死）。
        let old_row_h = self.ui.get_row_height_out() as f32;
        let old_cells = self.ui.get_cells_per_row().max(1) as usize;
        let scroll_y = self.state.active().active().scroll_y;
        let first_row = (scroll_y / old_row_h).floor();
        let first_index = (first_row * old_cells as f32) as usize;

        // 2. 记录到待定状态，交给 refresh_visible_rows 去换算成新像素位置。
        self.pending_scroll_index = Some(first_index);

        // 3. 更新视图配置（注意：不再这里清 scroll_y / 滚到顶部）
        self.config.view_mode = mode.to_string();
        self.config.save();
        self.ui.set_view_mode(mode.into());
        self.visible_cache = None;
        self.sync_file_list();
    }

    /// 全局应用排序并持久化。同时刷新所有已打开的面板，确保多标签/分屏统一。
    /// 「此电脑」「回收站」各有独立排序状态：在这两个视图里点排序只改它们自己，
    /// 不碰全局；在普通目录里点排序也只作用于普通目录，不会带走其余两者。
    fn apply_sort(&mut self, key: SortKey, ascending: bool) {
        // 显式重排优先：丢掉重命名后临时钉住的位置。
        self.rename_pin = None;
        let view = ViewSort::of(self.state.active().active().path.as_deref());
        // 此电脑没有「修改时间」列：要是这个键还落进去，状态就变成了
        // 「按一列不显示的东西排」，用户看不出发生了什么，也改不回来。
        if !view.allows(key) {
            return;
        }
        match view {
            ViewSort::ThisPc => {
                self.config.this_pc_sort_key = sort_key_id(key).to_string();
                self.config.this_pc_sort_ascending = ascending;
            }
            ViewSort::Recycle => {
                self.config.recycle_sort_key = sort_key_id(key).to_string();
                self.config.recycle_sort_ascending = ascending;
            }
            ViewSort::Normal => {
                self.config.sort_key = sort_key_id(key).to_string();
                self.config.sort_ascending = ascending;
            }
        }
        self.config.save();

        for tab in &mut self.state.tabs {
            if tab.kind == crate::state::TabKind::Settings { continue; }
            for pane in &mut tab.panes {
                // 别的视图的面板保持自己的排序，不被本次操作带走。
                if ViewSort::of(pane.path.as_deref()) != view {
                    continue;
                }
                pane.sort.key = key;
                pane.sort.ascending = ascending;
                crate::fs::sort_entries(&mut pane.entries, pane.sort);
                pane.rebuild_display();
            }
        }

        self.visible_cache = None;
        self.sync_file_list();
        self.sync_status();
    }

    /// 把每个面板的排序对齐到它所在视图的保存值。
    /// 切目录（前进/后退/跳转/启动）后必须调用：`pane.sort` 是从上一个目录
    /// 继承来的，进出「此电脑」/「回收站」时要替换成该视图自己的状态。
    fn sync_view_sort(&mut self) {
        // 配置文件可能被手改或来自旧版本：用不存在的列排序就退回兼底值。
        let (this_pc, recycle, global) = (
            SortState {
                key: ViewSort::ThisPc
                    .fallback(parse_sort_key(&self.config.this_pc_sort_key, SortKey::Drive)),
                ascending: self.config.this_pc_sort_ascending,
            },
            SortState {
                key: ViewSort::Recycle
                    .fallback(parse_sort_key(&self.config.recycle_sort_key, SortKey::Modified)),
                ascending: self.config.recycle_sort_ascending,
            },
            SortState {
                key: ViewSort::Normal
                    .fallback(parse_sort_key(&self.config.sort_key, SortKey::Name)),
                ascending: self.config.sort_ascending,
            },
        );
        let want = |path: Option<&std::path::Path>| match ViewSort::of(path) {
            ViewSort::ThisPc => this_pc,
            ViewSort::Recycle => recycle,
            ViewSort::Normal => global,
        };
        let mut changed = false;
        for tab in &mut self.state.tabs {
            if tab.kind == crate::state::TabKind::Settings { continue; }
            for pane in &mut tab.panes {
                let s = want(pane.path.as_deref());
                if pane.sort.key == s.key && pane.sort.ascending == s.ascending {
                    continue;
                }
                pane.sort = s;
                // 列表结果还没到（loading 中）时不动旧数据：worker 会用新排序重算。
                if !pane.loading && !pane.entries.is_empty() {
                    crate::fs::sort_entries(&mut pane.entries, pane.sort);
                    pane.rebuild_display();
                }
                changed = true;
            }
        }
        if changed {
            self.visible_cache = None;
        }
    }

    /// 全局拼音秒搜：查询后台索引，结果写入当前标签（此电脑视图）。
    /// 结果 = FileEntry（文件/文件夹），双击沿用 open_path 行为。
    /// 查询写入 tab.filter：驱动 rebuild_display 排序，并为行高亮提供查询词。
    fn run_global_search(&mut self, query: &str) {
        use crate::search::index::shared;
        // 空查询：恢复「此电脑」的驱动器列表。
        if query.trim().is_empty() {
            let drives = list_drives();
            let entries: Vec<FileEntry> =
                drives.iter().map(FileEntry::drive).collect();
            let n = entries.len();
            let tab = self.state.active_mut();
            tab.active_mut().entries = entries;
            tab.active_mut().pinyin_cache.clear();
            tab.active_mut().filter.clear();
            tab.active_mut().search_total = None;
            tab.active_mut().display = (0..n).collect();
            return;
        }
        let index = shared();
        let Ok(guard) = index.read() else {
            return;
        };
        let limit = self.config.search_max_results.clamp(50, 10000);
        let result = guard.search(query, limit);
        let total = result.total;
        let entries: Vec<FileEntry> = result
            .hits
            .into_iter()
            .map(|(e, _)| FileEntry::from_path(&e.path, e.is_dir, 0, None, false))
            .collect();
        let shown = entries.len();
        let tab = self.state.active_mut();
        tab.active_mut().entries = entries;
        tab.active_mut().pinyin_cache.clear();
        tab.active_mut().filter = query.to_string();
        // 命中数超过展示上限时记录总数，供状态栏提示截断。
        tab.active_mut().search_total = (shown < total).then_some(total);
        tab.active_mut().rebuild_display();
    }

    /// 全盘索引刚刚就绪：若当前正处于「此电脑」全局搜索且搜索框非空，
    /// 静默重查一次，把构建期的部分结果补全为完整结果。
    ///
    /// 背景：索引是后台增量填充的，构建中搜索只会命中已扫完的磁盘
    /// （实测 275 → 302 条）。没有这一步，用户必须手动清空再输入才能拿到全量。
    fn refresh_after_index_ready(&mut self) {
        let (is_root, query) = {
            let tab = self.state.active();
            (tab.active().path.is_none(), tab.active().filter.trim().to_string())
        };
        // 仅全局搜索需要补全：目录视图走的是本地过滤 + 深度搜索，与索引无关。
        if !is_root || query.is_empty() {
            return;
        }
        logln!("index ready: re-query global search {query:?}");
        self.run_global_search(&query);
        // 必须清虚拟列表缓存，否则列表命中旧窗口不重渲染（只有状态栏会变）。
        self.visible_cache = None;
        self.sync_file_list();
        self.sync_status();
    }

    /// 全盘索引构建状态：`Some(true)` 已就绪 / `Some(false)` 正在构建 /
    /// `None` 索引尚未启动，或读锁被索引线程占用（本次拿不到）。
    ///
    /// 直接读真实状态而非缓存字段：状态栏每次同步都准确，不存在
    /// 「定时器还没跑到所以显示陈旧状态」的空窗；也无需轮询定时器。
    /// 用 `try_read` 而非 `read`：此函数运行在 UI 线程，而索引线程正高频
    /// 持写锁推条目，阻塞等待会把卡顿带到界面上。取不到锁时退化为「不提示」，
    /// 代价仅是偶尔少一行状态文案，远优于卡死主线程。
    fn index_build_state() -> Option<bool> {
        crate::search::index::try_shared().and_then(|i| i.try_read().ok().map(|g| g.ready))
    }

    fn sync_status(&self) {
        // 工具栏粘贴按钮与右键菜单同源：实时读系统剪贴板。
        let (paste_paths, _) = self.paste_state();
        self.ui.set_can_paste(!paste_paths.is_empty());
        let tab = self.state.active();
        let (dirs, files, selected_size) = tab.active().stats();
        let total = tab.active().display.len();
        let en = !i18n::is_chinese(&self.config.language);
        let mut left = if en {
            format!("{total} items ({dirs} folders, {files} files)")
        } else {
            format!("{total} 个项目（{dirs} 个文件夹，{files} 个文件）")
        };
        // 全盘索引构建中：此电脑视图的全局搜索结果可能只是已扫完那部分磁盘的
        // 未完成结果（会随构建推进而增多），必须显式告知，否则用户会以为搜漏了。
        // 判定绑定真实构建状态：仅「已启动但未就绪」才提示，索引未启动不误报。
        if tab.active().path.is_none() && matches!(Self::index_build_state(), Some(false)) {
            left.push_str(if en { " · Building global index..." } else { " · 全盘索引构建中..." });
        }
        // 全局索引搜索命中数超过展示上限：明确告知结果被截断。
        if let Some(found) = tab.active().search_total.filter(|&n| total < n) {
            left.push_str(&if en {
                format!(" · Showing top {total} of {found} matches")
            } else {
                format!(" · 已展示前 {total} 项（共找到 {found} 项）")
            });
        }
        if tab.active().deep_searching {
            left.push_str(if en { " · Searching subfolders…" } else { " · 正在搜索子文件夹…" });
        }
        self.ui.set_status_left(left.into());
        let selected = tab.active().selection_count();
        let right = if selected == 0 {
            String::new()
        } else if selected_size > 0 {
            if en {
                format!("{selected} selected · {}", fs::format_size_bytes(selected_size))
            } else {
                format!("已选 {selected} 项 · {}", fs::format_size_bytes(selected_size))
            }
        } else {
            if en { format!("{selected} selected") } else { format!("已选 {selected} 项") }
        };
        self.ui.set_status_right(right.into());
    }

    fn sync_sidebar(&mut self) {
        self.drives = list_drives();
        let stats = recycle_bin_stats();
        // 用户自己添加的（`网络位置` 分区，显示别名）。
        let network: Vec<(String, String)> = self
            .config
            .network_locations
            .iter()
            .map(|n| (n.label.clone(), n.path.clone()))
            .collect();
        // 资源管理器里本来就有、但用户没加到 Rove 的（独立 `网络位置(系统)` 分区）。
        let mut detected: Vec<(String, String)> = Vec::new();
        for remote in list_network_locations() {
            if network
                .iter()
                .any(|(_, path)| path.eq_ignore_ascii_case(&remote))
                || detected
                    .iter()
                    .any(|(_, path)| path.eq_ignore_ascii_case(&remote))
            {
                continue;
            }
            detected.push((remote.clone(), remote));
        }
        // 「添加网络位置」向导的产物只是快捷方式，没有网络连接，WNet 枚举不到。
        for (label, target) in list_network_places() {
            if network
                .iter()
                .any(|(_, path)| path.eq_ignore_ascii_case(&target))
                || detected
                    .iter()
                    .any(|(_, path)| path.eq_ignore_ascii_case(&target))
            {
                continue;
            }
            detected.push((label, target));
        }

        // 指纹：驱动器容量（按 MB 取整）+ 回收站条目数 + 网络位置集合。
        let mut fingerprint = 0u64;
        for d in &self.drives {
            fingerprint = fingerprint
                .wrapping_mul(31)
                .wrapping_add(d.letter as u64)
                .wrapping_add(d.free_bytes / (1024 * 1024))
                .wrapping_add(d.total_bytes / (1024 * 1024) * 7);
        }
        fingerprint = fingerprint
            .wrapping_mul(31)
            .wrapping_add(stats.items as u64)
            .wrapping_add(stats.bytes as u64 / (1024 * 1024));
        for n in network.iter().chain(detected.iter()) {
            fingerprint = fingerprint
                .wrapping_mul(31)
                .wrapping_add(n.0.len() as u64)
                .wrapping_add(n.1.len() as u64 * 3);
        }
        // 快速访问的顺序本身就是要持久化的数据，纳入指纹避免重绘被短路。
        for q in &self.config.quick_items {
            fingerprint = fingerprint
                .wrapping_mul(31)
                .wrapping_add(q.id.bytes().map(|b| b as u64).sum::<u64>())
                .wrapping_add(q.path.len() as u64);
        }
        // 纳入 5 个分区的展开状态（任一状态切换均触发侧栏重绘）
        for exp in [
            self.config.quick_expanded,
            self.config.recent_expanded,
            self.config.computer_expanded,
            self.config.network_expanded,
            self.config.explorer_net_expanded,
        ] {
            fingerprint = fingerprint.wrapping_mul(31).wrapping_add(exp as u64);
        }
        if fingerprint == self.sidebar_fingerprint {
            return;
        }
        self.sidebar_fingerprint = fingerprint;

        let mut items: Vec<SidebarItem> = Vec::new();

        // ---- 各分区内容 ----
        // key → 区块（header + 条目），按 config.sidebar_order 组装。
        let arrow_quick = if self.config.quick_expanded { "▼" } else { "▶" };
        let mut h = header("quick", self.tr("快速访问").as_str());
        h.icon_text = arrow_quick.into();
        let mut quick_items: Vec<SidebarItem> = vec![h];

        // 侧边栏图标统一采用 Windows 标准 32px 规格：
        // 彻底解决启动阶段 scale 由 1.0 变 1.5 导致请求 key(20) 与判定 key(30) 脱节的 Bug！
        #[cfg(windows)]
        const SIDEBAR_ICON_PX: u32 = 32;

        if self.config.quick_expanded {
            // 顺序与条目全部来自持久化配置；系统项按 id 从 quick_access_dirs()
            // 解析真实路径（含 OneDrive 回退），用户固定项直接用存下来的绝对路径。
            let dirs = quick_access_dirs();
            // 先克隆出局部列表：下面取图标的 thumb_for 需要 &mut self。
            for q in self.config.quick_items.clone() {
                let _path: PathBuf = if q.path.is_empty() {
                    match dirs
                        .iter()
                        .find(|(_, key, _)| *key == q.id.as_str())
                        .and_then(|(_, _, d)| d.clone())
                    {
                        Some(d) => d,
                        // 系统目录不存在（或换了机器）：先不显示，但保留在配置里。
                        None => continue,
                    }
                } else {
                    PathBuf::from(&q.path)
                };
                let is_recycle = q.is_special();
                #[cfg(windows)]
                let icon_img = if is_recycle {
                    self.thumb_for(Path::new("shell:RecycleBinFolder"), None, 0, SIDEBAR_ICON_PX, true)
                } else {
                    self.thumb_for(&_path, None, 0, SIDEBAR_ICON_PX, false)
                };
                #[cfg(not(windows))]
                let icon_img = slint::Image::default();

                let (color, icon) = match q.id.as_str() {
                    "desktop" => (Color::from_rgb_u8(0xE8, 0xB3, 0x3C), self.tr("桌")),
                    "downloads" => (Color::from_rgb_u8(0x3E, 0x7B, 0xC4), self.tr("下")),
                    "pictures" => (Color::from_rgb_u8(0x4C, 0x9F, 0x70), self.tr("图")),
                    "documents" => (Color::from_rgb_u8(0x3E, 0x7B, 0xC4), self.tr("文")),
                    "music" => (Color::from_rgb_u8(0xD9, 0x69, 0x4E), self.tr("乐")),
                    "videos" => (Color::from_rgb_u8(0x8E, 0x6C, 0xC9), self.tr("视")),
                    "recycle" => (Color::from_rgb_u8(0x8A, 0x8F, 0x98), self.tr("回")),
                    _ => (Color::from_rgb_u8(0x3E, 0x7B, 0xC4), self.tr("夹")),
                };
                let detail = if is_recycle {
                    if !i18n::is_chinese(&self.config.language) {
                        format!(
                            "{} items · {}",
                            stats.items,
                            fs::format_size_bytes(stats.bytes.max(0) as u64)
                        )
                    } else {
                        format!(
                            "{} 项 · {}",
                            stats.items,
                            fs::format_size_bytes(stats.bytes.max(0) as u64)
                        )
                    }
                } else {
                    String::new()
                };
                quick_items.push(SidebarItem {
                    id: format!("quick:{}", q.id).into(),
                    label: self.tr(&q.label).into(),
                    detail: detail.into(),
                    kind: "quick".into(),
                    icon_color: color,
                    icon_text: icon.into(),
                    usage: -1.0,
                    icon: icon_img,
                });
            }
        }

        let mut recent_items: Vec<SidebarItem> = Vec::new();
        if !self.state.recent.is_empty() {
            // 展开与收起箭头
            let arrow = if self.config.recent_expanded { "▼" } else { "▶" };
            let mut h = header("recent", self.tr("最近使用").as_str());
            h.icon_text = arrow.into();
            recent_items.push(h);

            // 仅在展开状态下才推入子条目；收起时仅保留标题栏
            if self.config.recent_expanded {
                // 复制路径快照：避免在遍历 self.state.recent（不可变借用）期间
                // 又对 self 做可变借用（thumb_for），触发借用冲突。
                let recent_paths: Vec<PathBuf> = self.state.recent.iter().take(8).cloned().collect();
                for path in recent_paths {
                    if !path.exists() && !crate::fs::dir::is_unc_server_root(&path) {
                        continue;
                    }
                    #[cfg(windows)]
                    let icon_img = self.thumb_for(&path, None, 0, SIDEBAR_ICON_PX, false);
                    #[cfg(not(windows))]
                    let icon_img = slint::Image::default();

                    let label = path
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_else(|| path.to_string_lossy().into_owned());
                    recent_items.push(SidebarItem {
                        id: format!("recent:{}", path.to_string_lossy()).into(),
                        label: label.into(),
                        detail: String::new().into(),
                        kind: "recent".into(),
                        icon_color: Color::from_rgb_u8(0xE8, 0xB3, 0x3C),
                        icon_text: String::new().into(),
                        usage: -1.0,
                        icon: icon_img, // <-- 注入原生文件夹图标
                    });
                }
            }
        }

        let arrow_computer = if self.config.computer_expanded { "▼" } else { "▶" };
        let mut h = header("computer", self.tr("此电脑").as_str());
        h.icon_text = arrow_computer.into();
        let mut computer_items: Vec<SidebarItem> = vec![h];
        if self.config.computer_expanded {
            for (idx, drive) in self.drives.iter().enumerate() {
                // 紧凑格式改为：剩余容量 / 总容量（如 "32G / 100G"）
                computer_items.push(SidebarItem {
                    id: format!("drive:{}", drive.letter).into(),
                    label: self.tr_drive_label(&drive.display_label()).into(),
                    detail: format!(
                        "{} / {}",
                        format_pulse_size(drive.free_bytes),
                        format_pulse_size(drive.total_bytes)
                    )
                    .into(),
                    kind: "drive".into(),
                    icon_color: drive_pulse_color(idx, drive),
                    icon_text: drive.letter.to_string().into(),
                    usage: drive.usage() as f32,
                    icon: slint::Image::default(),
                });
            }
        }

        let arrow_network = if self.config.network_expanded { "▼" } else { "▶" };
        let mut h = header("network", self.tr("网络位置").as_str());
        h.icon_text = arrow_network.into();
        let mut network_items: Vec<SidebarItem> = vec![h];
        if self.config.network_expanded {
            if network.is_empty() {
                network_items.push(SidebarItem {
                    id: "net:none".into(),
                    label: self.tr("暂无，右键标题添加").into(),
                    detail: String::new().into(),
                    kind: "hint".into(),
                    icon_color: Color::from_rgb_u8(0x8A, 0x8F, 0x98),
                    icon_text: self.tr("网").into(),
                    usage: -1.0,
                    icon: slint::Image::default(),
                });
            } else {
                for (label, target) in &network {
                    network_items.push(SidebarItem {
                        id: format!("net:{target}").into(),
                        label: label.clone().into(),
                        detail: String::new().into(),
                        kind: "network".into(),
                        icon_color: Color::from_rgb_u8(0x3A, 0xA6, 0xA6),
                        icon_text: "N".into(),
                        usage: -1.0,
                        icon: slint::Image::default(),
                    });
                }
            }
        }

        // 资源里已有的网络位置：整组只在**真的检测到**时才出现。
        let arrow_explorer_net = if self.config.explorer_net_expanded {
            "▼"
        } else {
            "▶"
        };
        let mut explorer_net_items: Vec<SidebarItem> = Vec::new();
        if !detected.is_empty() {
            let mut h = header("explorer-net", self.tr("网络位置(系统)").as_str());
            h.icon_text = arrow_explorer_net.into();
            explorer_net_items.push(h);
            if self.config.explorer_net_expanded {
                for (label, target) in &detected {
                    explorer_net_items.push(SidebarItem {
                        id: format!("net:{target}").into(),
                        label: label.clone().into(),
                        detail: String::new().into(),
                        kind: "network".into(),
                        icon_color: Color::from_rgb_u8(0x3A, 0xA6, 0xA6),
                        icon_text: "N".into(),
                        usage: -1.0,
                        icon: slint::Image::default(),
                    });
                }
            }
        }

        // ---- 按 config.sidebar_order 组装 ----
        for key in &self.config.sidebar_order {
            let block = match key.as_str() {
                "quick" => &quick_items,
                "recent" => &recent_items,
                "computer" => &computer_items,
                "network" => &network_items,
                "explorer-net" => &explorer_net_items,
                _ => continue,
            };
            items.extend(block.iter().cloned());
        }

        // 当前激活的侧边栏项。
        let active_id = match self.state.active().active().path.as_ref() {
            None => String::new(),
            Some(path) if *path == fs::drives::recycle_virtual_path() => "quick:recycle".to_string(),
            Some(path) => {
                let s = path.to_string_lossy().into_owned();
                if let Some(drive) = self.drives.iter().find(|d| s.starts_with(format!("{}:", d.letter).as_str())) {
                    format!("drive:{}", drive.letter)
                } else if s.starts_with("\\\\") {
                    format!("net:{s}")
                } else {
                    String::new()
                }
            }
        };
        self.ui.set_sidebar_active_id(active_id.into());
        self.ui
            .set_sidebar_items(ModelRc::new(VecModel::from(items)));
    }

    fn sync_staging(&self) {
        let items: Vec<StagingItem> = self
            .state
            .staging
            .iter()
            .map(|s| StagingItem {
                path: s.path.to_string_lossy().into_owned().into(),
                name: s
                    .path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| s.path.to_string_lossy().into_owned())
                    .into(),
                is_dir: s.is_dir,
                icon_color: if s.is_dir {
                    Color::from_rgb_u8(0xE8, 0xB3, 0x3C)
                } else {
                    Color::from_rgb_u8(0x8A, 0x8F, 0x98)
                },
                icon_text: if s.is_dir {
                    SharedString::default()
                } else {
                    s.path
                        .extension()
                        .map(|e| e.to_string_lossy().chars().take(4).collect::<String>())
                        .unwrap_or_default()
                        .into()
                },
            })
            .collect();
        self.ui
            .set_staging_items(ModelRc::new(VecModel::from(items)));
    }

    /// 全量同步文件列表（标题、排序态、总行数、可见窗口）。
    fn sync_file_list(&mut self) {
        let (
            sort_key,
            sort_asc,
            sort_key_right,
            sort_asc_right,
            loading0,
            total0,
            is_recycle0,
            is_computer0,
            is_split,
            loading1,
            total1,
            is_recycle1,
            is_computer1,
            single0,
            single1,
        ) = {
            let tab = self.state.active();
            let panes = &tab.panes;
            // 「路径」列仅回收站显示：站内条目来源各异，路径列是唯一来源线索；
            // 普通目录的路径与地址栏一致，隐藏以让出宽度给名称列。
            // 左右两栏各自判定：分屏时两栏可能一个是回收站一个是普通目录。
            let is_recycle = |p: &crate::state::PaneState| {
                p.path.as_deref() == Some(fs::drives::recycle_virtual_path().as_path())
            };
            // 排序标记按栏下发：表头箭头要看自己那一栏的排序，
            // 不能拿「当前聚焦的那一栏」去反映给两栏。
            let key_id = |k: SortKey| match k {
                SortKey::Name => "name",
                SortKey::Size => "size",
                SortKey::Modified => "modified",
                SortKey::Type => "type",
                SortKey::Path => "path",
                SortKey::Drive => "drive",
            };
            (
                key_id(panes[0].sort.key),
                panes[0].sort.ascending,
                // 右栏自己的排序标记（未分屏时与左栏一致）。
                panes.get(1).map(|p| key_id(p.sort.key)),
                panes.get(1).map(|p| p.sort.ascending),
                panes[0].loading,
                panes[0].display.len() as i32,
                is_recycle(&panes[0]),
                panes[0].path.is_none(),
                tab.is_split,
                panes.get(1).map(|p| p.loading).unwrap_or(false),
                panes.get(1).map(|p| p.display.len()).unwrap_or(0) as i32,
                panes.get(1).is_some_and(is_recycle),
                panes.get(1).is_some_and(|p| p.path.is_none()),
                // 「非多选」即可：无选中时鼠标悬停也允许显示截断浮层；
                // 多选（>=2）时一律禁用，避免满屏浮层。
                panes[0].selection.len() <= 1,
                panes.get(1).map(|p| p.selection.len() <= 1).unwrap_or(false),
            )
        };
        // 多选时不弹名称浮层（左右栏各自判定）。
        self.ui.set_file_single_selection(single0);
        self.ui.set_file_single_selection_right(single1);
        self.ui.set_sort_key(sort_key.into());
        self.ui.set_sort_ascending(sort_asc);
        self.ui.set_sort_key_right(sort_key_right.unwrap_or(sort_key).into());
        self.ui
            .set_sort_ascending_right(sort_asc_right.unwrap_or(sort_asc));
        self.ui.set_file_loading(loading0);
        self.ui.set_file_total(total0);
        self.ui.set_show_path_column(is_recycle0);
        self.ui.set_show_path_column_right(is_recycle1);
        // 视图形态（此电脑无修改时间列、大小列表头叫剩余）也按栏判定，
        // 否则分屏时「此电脑 + 普通目录」会把普通目录的修改时间列一起藏掉。
        self.ui.set_file_is_computer_view(is_computer0);
        self.ui.set_file_is_computer_view_right(is_computer1);
        self.ui.set_is_split(is_split);
        self.ui.set_file_loading_right(loading1);
        self.ui.set_file_total_right(total1);
        self.refresh_visible_rows();
    }

    /// 重建虚拟列表可见窗口（滚动/选择/数据变化时调用）。
    /// 取某路径的缩略图：命中缓存直接返回；否则对候选路径发起后台加载
    /// （去重），返回空图（UI 显示占位色块，加载完成后经事件循环刷新）。
    /// 当前视图对应的缩略图边长。
    #[cfg(windows)]
    fn thumb_size_for_view(&self) -> u32 {
        // 获取当前窗口的真实物理缩放比例（如 125% -> 1.25, 150% -> 1.5）
        let scale = self.ui.window().scale_factor() as f32;

        let logical_size = match self.config.view_mode.as_str() {
            "icons-xl" => 132.0,
            "icons-large" => 88.0,
            "icons-medium" => 60.0,
            _ => crate::ops::thumbs::THUMB_SIZE as f32,
        };

        // 关键修复：向 Windows Shell 索要乘上缩放系数的真实物理分辨率图像，
        // 杜绝位图被强行放大发虚（高 DPI 屏下发虚的根因）。
        let px = (logical_size * scale).ceil() as u32;
        px
    }

    /// OLE 拖拽图像的边长（物理像素）：与当前视图里图标的屏幕尺寸一致。
    /// 与 `thumb_size_for_view` 的区别是 details/list —— 取图会超采样到 44，
    /// 但列表里只显示 24 逻辑像素，拖拽图应跟显示尺寸而不是取图尺寸。
    #[cfg(windows)]
    fn drag_icon_px(&self) -> u32 {
        let scale = self.ui.window().scale_factor() as f32;
        let logical = match self.config.view_mode.as_str() {
            "details" | "list" => 24.0,
            "icons-xl" => 132.0,
            "icons-large" => 88.0,
            "icons-medium" => 60.0,
            _ => crate::ops::thumbs::THUMB_SIZE as f32, // icons-small
        };
        (logical * scale).ceil() as u32
    }

    #[cfg(windows)]
    fn thumb_for(
        &mut self,
        path: &Path,
        mtime: Option<SystemTime>,
        file_size: u64,
        size: u32,
        priority: bool,
    ) -> slint::Image {
        let mtime_ms = mtime
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let key = ThumbKey {
            path: path.to_path_buf(),
            mtime_ms,
            file_size,
            px_size: size,
        };

        // 1. 内存 LRU 命中：提升至队尾（真 LRU，非 FIFO）。
        if let Some(entry) = self.thumb_cache.get(&key) {
            let img = entry.img.clone();
            self.thumb_order.retain(|k| k != &key);
            self.thumb_order.push(key);
            return img;
        }

        // 2. 负缓存命中：本会话内该图确实取不到，直接空白。
        if self.thumb_failed.contains(&key) {
            crate::logv!(
                "[thumb] negative-cached (will stay blank) {} size={}",
                path.display(),
                size
            );
            return slint::Image::default();
        }

        // 3. 未在 pending 中：先查磁盘缓存，查不到再交给后台 Shell 解码。
        if !self.thumb_pending.contains(&key) {
            self.thumb_pending.insert(key.clone());

            // 磁盘缓存优先：视频海报等慢解码项落盘后，回滚是读盘(毫秒级)
            // 而非 Shell 解码(几十~几百 ms)，这就是「永不重新渲染」的关键。
            // 格式为 WIC 编码的 JPEG/PNG（非旧版 bincode 裸 RGBA）。
            let disk_path = thumb_disk_path(&key);
            if disk_path.exists() {
                match crate::ops::thumbs::decode_wic_from_file(&disk_path) {
                    Ok((w, h, rgba)) => {
                        if let Some(img) = to_slint_image(w, h, &rgba) {
                            crate::logv!(
                                "[thumb] disk-cache HIT {} size={} {}x{}",
                                path.display(),
                                size,
                                w,
                                h
                            );
                            self.thumb_pending.remove(&key);
                            self.insert_thumb_cache(key, img.clone());
                            return img;
                        }
                    }
                    Err(e) => {
                        // 旧版 bincode 裸 RGBA 条目 / 损坏文件：删掉重新解码。
                        crate::logv!(
                            "[thumb] disk-cache STALE (removing) {}: {}",
                            disk_path.display(),
                            e
                        );
                        let _ = std::fs::remove_file(&disk_path);
                    }
                }
            }

            // 磁盘未命中，发送给后台去解码。
            // `priority` = 该行在**当前可见窗口**内，走独立优先池，
            // 不排在主池几千个“已滚走”的任务后面。
            if priority {
                if self.thumb_prio_tx.send(key.clone()).is_err() {
                    self.thumb_pending.remove(&key);
                    self.thumb_failed.insert(key);
                    crate::logln!("[thumb] !! prio channel closed");
                }
            } else {
                // 普通池：登记排队时间，队列过长时把最早的（已滑出屏幕的）作废。
                self.thumb_queued.push(key.clone());
                if self.thumb_tx.send(key.clone()).is_err() {
                    // 通道断开：不能把 key 永远留在 pending 里，否则该行永久空白。
                    crate::logln!(
                        "[thumb] !! send to worker channel FAILED {} size={}",
                        path.display(),
                        size
                    );
                    self.thumb_pending.remove(&key);
                    self.thumb_failed.insert(key);
                }
            }
        }
        // 命中 pending 分支不记日志：此处在热路径上（每行每次刷新都会调用），
        // 1538 张图加载期间可达 6 万行、占日志 79%。
        slint::Image::default()
    }

    /// 写入 LRU 缓存（内存），按字节预算 + 项数双上限淘汰（队首最久未用）。
    fn insert_thumb_cache(&mut self, key: ThumbKey, img: slint::Image) {
        let sz = img.size();
        // RGBA8 估算：每像素 4 字节。
        let bytes = (sz.width * sz.height * 4) as usize;

        if let Some(old) = self.thumb_cache.remove(&key) {
            self.thumb_cache_bytes = self.thumb_cache_bytes.saturating_sub(old.bytes);
            self.thumb_order.retain(|k| k != &key);
        }

        self.thumb_cache.insert(key.clone(), CachedThumb { img, bytes });
        self.thumb_order.push(key);
        self.thumb_cache_bytes += bytes;

        // 淘汰：从头部（最久未用）移除，直到符合双上限。
        while (self.thumb_cache.len() > THUMB_CACHE_MAX_ITEMS
            || self.thumb_cache_bytes > THUMB_CACHE_MAX_BYTES)
            && !self.thumb_order.is_empty()
        {
            let oldest = self.thumb_order.remove(0);
            if let Some(evicted) = self.thumb_cache.remove(&oldest) {
                self.thumb_cache_bytes =
                    self.thumb_cache_bytes.saturating_sub(evicted.bytes);
            }
        }
    }

    #[cfg(not(windows))]
    fn thumb_for(
        &mut self,
        _path: &Path,
        _mtime: Option<SystemTime>,
        _file_size: u64,
        _size: u32,
        _priority: bool,
    ) -> slint::Image {
        slint::Image::default()
    }

    #[cfg(not(windows))]
    fn thumb_size_for_view(&self) -> u32 {
        44
    }

    /// 标记缩略图失败（清理 pending + 负缓存，本会话不再重试）。
    #[allow(dead_code)]
    fn fail_thumbnail(&mut self, key: ThumbKey) {
        crate::logv!(
            "[thumb] FAILED -> negative cache {} size={} (row stays blank)",
            key.path.display(),
            key.px_size
        );
        self.thumb_pending.remove(&key);
        self.thumb_queued.retain(|k| k != &key);
        self.thumb_failed.insert(key);
    }

    /// 写入缩略图缓存（事件循环线程）：只打批量刷新标记，由 poll_timer 统一刷新。
    #[allow(dead_code)]
    fn store_thumbnail(&mut self, key: ThumbKey, img: slint::Image) {
        self.thumb_pending.remove(&key);
        self.thumb_queued.retain(|k| k != &key);
        self.insert_thumb_cache(key, img);
        // 核心修复：不再每次 store 都全量 refresh，只置位，由主线程 40ms 批量刷新。
        self.thumb_needs_refresh = true;
    }

    /// 丢弃普通池里排得太老的请求（从队首开始，即最早发出的）。
    ///
    /// 不丢的话，每滑过一个位置就塞 60 个新请求，队列只增不减；用户突然
    /// 滑到中间时，他要的那一屏就排在上百个已经滑走的请求之后。
    /// 最多保留 `keep` 个；被丢弃的不算失败，滑回去会重新请求。
    ///
    /// 返回实际丢弃的个数。
    fn drop_stale_thumb_requests(&mut self, keep: usize) -> usize {
        let mut dropped = 0;
        while self.thumb_queued.len() > keep {
            let key = self.thumb_queued.remove(0);
            // 已经在解的（缓存里已有图）不能动，只删还没出图的。
            if self.thumb_cache.contains_key(&key) {
                continue;
            }
            self.thumb_pending.remove(&key);
            dropped += 1;
        }
        dropped
    }

    /// 当前列表几何（用于检测布局落定 / 窗口尺寸变化）。
    /// 末两项是左右分屏的名称列宽：列宽变化（拖分隔符 / 自适应）不会改变
    /// 列表高度等几何，若不纳入签名，行模型里的 `name-truncated`（浮层弹出的
    /// 依据）会一直用拖动前的旧预算判定，浮层表现为"按之前的列宽截断"。
    fn current_list_geom(&self) -> (f32, usize, f32, bool, f32, f32) {
        (
            self.ui.get_list_height(),
            self.ui.get_cells_per_row().max(1) as usize,
            self.ui.get_row_height_out(),
            self.ui.get_is_icons_view_out(),
            self.ui.get_file_name_area_w(),
            self.ui.get_file_name_area_w_right(),
        )
    }

    fn refresh_visible_rows(&mut self) {
        // 布局未落定时的容错（启动首帧）：此时 list_height 为 0，
        // 算出的 rows_visible 只剩 2*OVERSCAN，只能渲染前几行
        // （表现为「此电脑」部分盘符空白）。用典型视口高度占位，
        // 布局落定后几何监听会立即重算。
        let raw_list_height = self.ui.get_list_height();
        let list_height = if raw_list_height > 1.0 { raw_list_height } else { 600.0 };
        let cells_per_row = (self.ui.get_cells_per_row().max(1)) as usize;
        let row_h = if self.ui.get_row_height_out() > 0.0 {
            self.ui.get_row_height_out()
        } else {
            ROW_HEIGHT
        };
        let rows_visible = (list_height / row_h).ceil() as usize + OVERSCAN * 2;

        // 「此电脑」视图（pane 没有路径）：详细信息要隐藏修改时间列，
        // 大小列改为只显示剩余容量、表头文字改成「剩余」。
        // 左右两栏各自判定，不能只看当前聚焦的那一栏（分屏时两边可能不同）。
        {
            let tab = self.state.active();
            self.ui
                .set_file_is_computer_view(tab.panes[0].path.is_none());
            self.ui.set_file_is_computer_view_right(
                tab.panes.get(1).is_some_and(|p| p.path.is_none()),
            );
        }

        // 名称浮层只在「非多选」时出现。放在这里而不是 sync_file_list：
        // 点击 / 框选 / 键盘改选择时只会走 refresh_visible_rows，不走 sync_file_list，
        // 单选标记若不在这里同步，多选后仍停在 true → 满屏浮层。
        {
            let tab = self.state.active();
            self.ui.set_file_single_selection(
                tab.panes.first().map(|p| p.selection.len() <= 1).unwrap_or(true),
            );
            self.ui.set_file_single_selection_right(
                tab.panes.get(1).map(|p| p.selection.len() <= 1).unwrap_or(true),
            );
        }

        // 核心修复：若是切换视图触发的刷新，且拿到了待恢复的条目索引，则换算成
        // 新的像素位置。必须放在下方的 clamp 块“之前”——否则残留的旧像素大偏移
        // 会先被 clamp 到内容底部，再恢复就已经晚了。
        if let Some(first_index) = self.pending_scroll_index.take() {
            let new_row = first_index / cells_per_row;
            let new_scroll_y = (new_row as f32) * row_h;
            for pane in &mut self.state.active_mut().panes {
                pane.scroll_y = new_scroll_y;
            }
            push_scroll(&self.ui, 0, new_scroll_y);
            if self.state.active().is_split {
                push_scroll(&self.ui, 1, new_scroll_y);
            }
        }

        // 新建文件夹后把对应条目滚进可视区（最小位移：已在视口内则不动）。
        // 必须在下面计算 first0/count0 之前做，否则新行不在本帧渲染窗口里。
        if let Some(index) = self.scroll_to_index.take() {
            let row = index / cells_per_row;
            let item_top = row as f32 * row_h;
            let item_bottom = item_top + row_h;
            let pane_idx = {
                let tab = self.state.active();
                tab.active_pane.min(tab.panes.len().saturating_sub(1))
            };
            let current = self.state.active().panes[pane_idx].scroll_y.max(0.0);
            let target = if item_top < current {
                item_top
            } else if item_bottom > current + list_height {
                item_bottom - list_height
            } else {
                current
            };
            if (target - current).abs() > 0.5 {
                self.state.active_mut().panes[pane_idx].scroll_y = target;
                push_scroll(&self.ui, pane_idx, target);
                // 这次改位置是我们自己的意图：把重命名“假滚动锁”的预期值同步到新位置，
                // 否则紧接着的回显会被当成 spurious 又推回旧位置（新建文件夹仍看不到）。
                if let Some((k0, k1, until)) = self.rename_scroll_guard {
                    self.rename_scroll_guard = Some(if pane_idx == 0 {
                        (target, k1, until)
                    } else {
                        (k0, target, until)
                    });
                }
                crate::logv!(
                    "[scroll] reveal index={} row={} {:.1} -> {:.1} (view_h={})",
                    index, row, current, target, list_height
                );
            }
        }

        // 防御性兜底：把每个面板的 scroll_y 夹到内容真实范围。
        //
        // Slint 的 Flickable 在 content-height 变小时不会主动重新 clamp 真实的
        // content-y，所以一旦残留一个巨大偏移（切标签 / 进入目录），
        // pane_view_range 会把它 clamp 到最后一行，只渲染 1 行 → 白屏/盘符空白。
        // 这里在每次重建可视窗口前先修正状态并回推 UI，保证视图与内容一致。
        {
            let max_scroll_for = |total: usize| -> f32 {
                let total_rows = total.div_ceil(cells_per_row);
                (total_rows as f32 * row_h - list_height).max(0.0)
            };
            let pane_count = self.state.active().panes.len();
            for idx in 0..pane_count {
                let (total, scroll, pending) = {
                    let pane = &self.state.active().panes[idx];
                    (pane.display.len(), pane.scroll_y.max(0.0), pane.pending_scroll_restore)
                };
                // 核心修复：如果正在等待加载历史记录（后退/前进），跳过 clamp。
                // 否则此时 display 还是空的（旧目录已卸载/新目录未到），scroll_y 会被夹到 0 → 跳顶。
                if pending {
                    continue;
                }
                let clamped = scroll.min(max_scroll_for(total));
                if (clamped - scroll).abs() > 0.5 {
                    // 排查：重命名期间任何“程序改滚动位置”都是嫌疑犯（会把行模型推出
                    // 可见窗口 → 重建 → 行内 TextInput 被销毁 → 焦点丢失）。
                    if !self.ui.get_rename_path().is_empty() {
                        logln!(
                            "[clamp] !! pane={} scroll {:.1} -> {:.1} (max={:.1} total={} list_h={} row_h={})",
                            idx,
                            scroll,
                            clamped,
                            max_scroll_for(total),
                            total,
                            list_height,
                            row_h
                        );
                    }
                    self.state.active_mut().panes[idx].scroll_y = clamped;
                    push_scroll(&self.ui, idx, clamped);
                }
            }
        }

        let (is_split, total0, scroll0) = {
            let tab = self.state.active();
            (
                tab.is_split,
                tab.panes[0].display.len(),
                tab.panes[0].scroll_y.max(0.0),
            )
        };

        let (first0, count0) = pane_view_range(total0, cells_per_row, row_h, rows_visible, scroll0);

        // 丢弃排队太久、可能已滑出屏幕的缩略图请求，避免队列无限增长：
        // 每滑过一个位置就塞 60 个新请求，不丢的话越滑越多，用户突然滑到
        // 中间时就只能排在上百个已滑走的请求之后（表现：依然从顶部慢慢出图）。
        // 保留量 = 当前可视窗口的条目数（含 overscan）的 2 倍，留一屏余量。
        let stale_dropped = self.drop_stale_thumb_requests(count0.max(60) * 2);
        if stale_dropped > 0 {
            crate::logv!(
                "[thumb] dropped {stale_dropped} stale requests (queued={})",
                self.thumb_queued.len()
            );
        }

        // 左栏始终绑定 pane 0
        let rows0 =
            self.build_rows_for_pane(self.state.active_tab, 0, first0, count0, list_height);
        // 启动期“盘符空白”排查：可视窗口内有图片为空的行就是病灶。
        // 只在真的有空图时输出，正常滚动不吵。
        // 启动期“盘符空白”排查：可视窗口内有图片为空的行就是病灶。
        // 只输出计数，不列文件名（104 个长文件名会让单行达到 ~800 字节）。
        // 正常滚动时如果一直没有空图，此处不会产生任何日志。
        let empty_thumbs = rows0
            .iter()
            .filter(|r| r.thumb.size().width == 0)
            .count();
        if empty_thumbs > 0 {
            crate::logv!(
                "[rows] EMPTY THUMBS {}/{} | geom list_h={} (raw={}) cells={} row_h={} rows_visible={} | total={} first={} count={} scroll={:.1} mode={} is_split={} expect_px={} cache_len={} pending={} failed={}",
                empty_thumbs,
                rows0.len(),
                list_height,
                raw_list_height,
                cells_per_row,
                row_h,
                rows_visible,
                total0,
                first0,
                count0,
                scroll0,
                self.config.view_mode,
                is_split,
                self.thumb_size_for_view(),
                self.thumb_cache.len(),
                self.thumb_pending.len(),
                self.thumb_failed.len()
            );
        }
        // 核心修复：内联重命名期间，若可见窗口（首行/行数）未变，就不要重建行模型。
        // set_file_rows 换整个 VecModel 会让 Slint 重建每一行 → 行内的 TextInput
        // 被销毁重建 → 已输入的字丢失、焦点被抢走（表现为“无法重命名”）。
        // 40ms 轮询里的 thumb_needs_refresh 会频繁触发本函数，因此必须抑制这种无效重建。
        // 注：滚动会改变 (first0, count0) → 依然正常重建，不影响滚动。
        let sig = (
            self.state
                .active()
                .panes
                .first()
                .map(|p| p.generation)
                .unwrap_or(0),
            first0,
            count0,
            total0,
        );
        let renaming = !self.ui.get_rename_path().is_empty();
        let rename_pane = self.ui.get_rename_pane();
        // 左栏匹配诊断只对左栏重命名有意义；右栏重命名时 rows0 里本就找不到
        // 目标路径，不做会刷出无意义的 "no visible row matches" 误报。
        if renaming && rename_pane == 0 {
            // 只在异常时输出（重命名中却没有任何可见行匹配）——这是“输入框不出现 / 键盘全吞”的病灶。
            let rp = self.ui.get_rename_path();
            // 排查：重命名期间把影响滚动/虚拟窗口的关键几何一次性打出来。
            // 低频（仅重命名中），但能同时回答三个问题：视口高度倒底是多少、
            // 滚动位置是否被动过、行模型是否被重建（重建=行内 TextInput 被销毁=失焦）。
            logv!(
                "[rename] refresh list_h={} raw={} cells={} row_h={} rows_vis={} scroll={:.1} first={} count={} total={} icons={}",
                list_height,
                raw_list_height,
                cells_per_row,
                row_h,
                rows_visible,
                scroll0,
                first0,
                count0,
                total0,
                self.ui.get_is_icons_view_out()
            );
            let hit = rows0.iter().any(|r| r.path.as_str() == rp.as_str());
            if !hit {
                logln!(
                    "[rename] !! no visible row matches rename-path={} rows={} first={} count={} total={}",
                    rp, rows0.len(), first0, count0, total0
                );
            } else {
                // 行模型里确实有该行；下面 reuse/重建 二选一决定输入框是否被销毁。
                // 重命名期间本函数会被 40ms 轮询反复调用，故放详细级避免刷屏。
                logv!(
                    "[rename] visible row HIT rename-path={} first={} count={}",
                    rp, first0, count0
                );
            }
        }
        let reuse_rows = renaming && self.rows_sig == Some(sig);
        if reuse_rows {
            logv!("[rename] keep row model (sig={:?})", sig);
        } else {
            if renaming {
                // 异常：重命名期间还在换行模型。换模型会销毁行内 TextInput，
                // 造成失焦/输入丢失。看到这条就说明 rows_sig 没拦住。
                logln!(
                    "[rename] !! row model rebuilt during rename sig={:?} prev={:?}",
                    sig, self.rows_sig
                );
            }
            // 重建可见行（重命名中重建会得到新输入框实例，必须重新抢焦点）。
            self.ui.set_file_rows(ModelRc::new(VecModel::from(rows0)));
            self.rows_sig = Some(sig);
            if renaming {
                // 新实例获得焦点时 Slint 又会自动 reveal 一次，重新锁住当前位置。
                self.arm_rename_scroll_guard();
            }
        }

        // 右栏（如果开启分屏）绑定 pane 1
        if is_split {
            let (total1, scroll1) = {
                let tab = self.state.active();
                if let Some(pane) = tab.panes.get(1) {
                    (pane.display.len(), pane.scroll_y.max(0.0))
                } else {
                    (0, 0.0)
                }
            };
            let (first1, count1) = pane_view_range(total1, cells_per_row, row_h, rows_visible, scroll1);
            let rows1 =
                self.build_rows_for_pane(self.state.active_tab, 1, first1, count1, list_height);
            self.ui.set_file_total_right(total1 as i32);
            // 与左栏同理：右栏重命名期间，可见窗口没变就不要重建行模型，
            // 否则 40ms 轮询会不断销毁行内 TextInput，右栏根本没法输入。
            let sig_right = (
                self.state
                    .active()
                    .panes
                    .get(1)
                    .map(|p| p.generation)
                    .unwrap_or(0),
                first1,
                count1,
                total1,
            );
            let reuse_right =
                renaming && rename_pane == 1 && self.rows_sig_right == Some(sig_right);
            if !reuse_right {
                self.ui
                    .set_file_rows_right(ModelRc::new(VecModel::from(rows1)));
                self.rows_sig_right = Some(sig_right);
                if renaming && rename_pane == 1 {
                    self.arm_rename_scroll_guard();
                }
            }
        } else {
            self.ui.set_file_total_right(0);
            self.ui
                .set_file_rows_right(ModelRc::new(VecModel::from(Vec::<FileRow>::new())));
            self.rows_sig_right = None;
        }

        // 行已经就绪：没有手动列宽记录的目录，这里按最长条目自适应一次。
        #[cfg(windows)]
        autofit_column_widths_if_needed(self);

        self.sync_expand_targets();
    }

    /// 算出图标视图“展开名称层”的目标：正在重命名的条目，或本栏只选中一个条目时。
    ///
    /// 给出的是**绝对显示索引**（与 `FileRow.index` 同一坐标系）。
    /// 只在中/大/超大图标视图生效——小图标视图是横排单行，保持原样。
    fn sync_expand_targets(&mut self) {
        let icons_big = matches!(
            self.config.view_mode.as_str(),
            "icons-medium" | "icons-large" | "icons-xl"
        );
        if !icons_big {
            self.ui.set_file_expand_index(-1);
            self.ui.set_file_expand_name(SharedString::default());
            self.ui.set_file_expand_box_w(0.0);
            self.ui
                .set_file_expand_lines(ModelRc::new(VecModel::from(Vec::<GridLine>::new())));
            self.ui.set_file_expand_index_right(-1);
            self.ui.set_file_expand_name_right(SharedString::default());
            self.ui.set_file_expand_box_w_right(0.0);
            self.ui.set_file_expand_lines_right(ModelRc::new(VecModel::from(
                Vec::<GridLine>::new(),
            )));
            // 悬停层同样要清掉：`hover_display_index` 只在图标视图的行
            // TouchArea 上更新，切走视图时不会自动归零。虽非图标视图下
            // 两层浮层都不绘制，但旧值留着会在切回图标视图的第一帧里
            // 拿一个早已不在鼠标下的格子画出悬停浮层。
            self.hover_display_index = [-1, -1];
            self.cell_hovered = [-1, -1];
            self.overlay_hovered = [false, false];
            self.ui.set_file_hover_expand_index(-1);
            self.ui
                .set_file_hover_expand_lines(ModelRc::new(VecModel::from(Vec::<GridLine>::new())));
            self.ui.set_file_hover_expand_box_w(0.0);
            self.ui.set_file_hover_expand_index_right(-1);
            self.ui.set_file_hover_expand_lines_right(ModelRc::new(VecModel::from(
                Vec::<GridLine>::new(),
            )));
            self.ui.set_file_hover_expand_box_w_right(0.0);
            // 切回图标视图时需要能重新下发，签名必须清空。
            self.expand_sig = None;
            return;
        }

        let rename = self.ui.get_rename_path().to_string();
        let rename_pane = self.ui.get_rename_pane() as usize;
        let is_split = self.state.active().is_split;
        // 先拷出悬停索引，避免与 self.state 的不可变借用纠缠。
        let hovers = self.hover_display_index;
        // 每栏两份目标：选中/重命名（expand）与鼠标悬停（hover）。
        // 必须分开：只有一个索引时两层浮层会轮流顶掉对方（选中一个再悬停
        // 另一个长名文件，两个浮层就会交替闪烁）。
        let mut targets: [(i32, String, Vec<(String, bool)>); 2] = [
            (-1, String::new(), Vec::new()),
            (-1, String::new(), Vec::new()),
        ];
        let mut hover_targets: [(i32, String, Vec<(String, bool)>); 2] = [
            (-1, String::new(), Vec::new()),
            (-1, String::new(), Vec::new()),
        ];
        for pane in 0..2 {
            if pane == 1 && !is_split {
                break;
            }
            let tab = self.state.active();
            let Some(p) = tab.panes.get(pane) else { continue };
            // 选中/重命名层：重命名 > 单选。悬停层单独算，互不占用。
            let target: Option<PathBuf> = if pane == rename_pane && !rename.is_empty() {
                Some(PathBuf::from(&rename))
            } else if p.selection.len() == 1 {
                p.selection.iter().next().cloned()
            } else {
                None
            };
            // 悬停层：指针下的格子。与选中项相同时下面会跳过重复绘制。
            let hover_target: Option<PathBuf> = if hovers[pane] >= 0 {
                p.display_entry(hovers[pane] as usize)
                    .map(|e| e.path.clone())
            } else {
                None
            };
            let rows = if pane == 1 {
                self.ui.get_file_rows_right()
            } else {
                self.ui.get_file_rows()
            };
            let resolve = |want: Option<PathBuf>| -> Option<(i32, String, Vec<(String, bool)>)> {
                let target = want?;
                let hit = (0..p.display.len()).find_map(|di| {
                    let e = p.display_entry(di)?;
                    (e.path == target).then(|| (di as i32, e.name.clone()))
                })?;
                let (di, name) = hit;
                // 该条目的完整高亮分段：行模型里存的就是未截断的原始分段，
                // 直接复用，避免在这里重跑一遍搜索命中计算。
                let key = p
                    .display_entry(di as usize)
                    .map(|e| e.path.to_string_lossy().into_owned())
                    .unwrap_or_default();
                let mut parts: Vec<(String, bool)> = Vec::new();
                for i in 0..rows.row_count() {
                    let Some(r) = rows.row_data(i) else { continue };
                    if r.path.as_str() == key {
                        parts = (0..r.name_parts.row_count())
                            .filter_map(|k| r.name_parts.row_data(k))
                            .map(|x| (x.text.to_string(), x.hl))
                            .collect();
                        break;
                    }
                }
                if parts.is_empty() {
                    parts = vec![(name.clone(), false)];
                }
                Some((di, name, parts))
            };
            if let Some(t) = resolve(target) {
                targets[pane] = t;
            }
            if let Some(t) = resolve(hover_target) {
                hover_targets[pane] = t;
            }
        }
        // 是否需要浮层（名字超过 3 行）改在 Rust 侧判定：只有需要时才给
        // expand-index，Slint 侧因此不再依赖文本测量决定弹不弹。
        //
        // 本函数每帧都会走到（滚动 / 几何变化 / 数据刷新），但选中项通常没变。
        // 用「索引 + 框宽 + 行内容哈希」做签名，未变就整段跳过：不重建嵌套
        // ModelRc，也不 set（set 会触发 Slint 侧重排）。
        let hash_lines = |lines: &GridLines| -> u64 {
            let mut h = lines.len() as u64;
            for line in lines {
                h = h.wrapping_mul(31).wrapping_add(line.len() as u64);
                for (t, hl) in line {
                    for ch in t.chars() {
                        h = h.wrapping_mul(31).wrapping_add(ch as u64);
                    }
                    h = h.wrapping_mul(31).wrapping_add(*hl as u64);
                }
            }
            h
        };
        let hash_str = |s: &str| -> u64 {
            let mut h = 0u64;
            for ch in s.chars() {
                h = h.wrapping_mul(31).wrapping_add(ch as u64);
            }
            h
        };

        let mut sigs: [ExpandSig; 2] = [(-1, 0, 0, 0, -1, 0, 0); 2];
        let mut plans: [Option<(f32, GridLines, SharedString, f32, GridLines)>; 2] = [None, None];
        for (pane, t) in targets.iter().enumerate() {
            let ht = &hover_targets[pane];
            // 悬停项与选中/重命名项是同一个格子：不重复画（Slint 侧也会跳过）。
            let hover_same = ht.0 < 0 || ht.0 == t.0;
            let mut hover_sig = (-1i32, 0u32, 0u64);
            let mut hover_plan = (0.0f32, GridLines::new());
            if !hover_same {
                let (needs, box_w, lines) = expand_layout(&self.ui, &ht.2);
                let index = if needs { ht.0 } else { -1 };
                hover_sig = (index, box_w.to_bits(), hash_lines(&lines));
                hover_plan = (box_w, lines);
            }
            if t.0 < 0 {
                sigs[pane] = (-1, 0, 0, 0, hover_sig.0, hover_sig.1, hover_sig.2);
                plans[pane] = Some((0.0, Vec::new(), SharedString::default(), hover_plan.0, hover_plan.1));
                continue;
            }
            // 重命名中输入框的实时文本才是浮层该显示的内容；否则每帧都会用
            // 磁盘上的旧名字，把用户正在输入的行覆盖回去。
            let renaming_here = rename_pane == pane && !rename.is_empty();
            let parts: Vec<(String, bool)> = if renaming_here {
                vec![(self.ui.get_rename_text().to_string(), false)]
            } else {
                t.2.clone()
            };
            let (needs, box_w, lines) = expand_layout(&self.ui, &parts);
            // 重命名必须弹浮层（输入框就在浮层里），与「名字是否超过 3 行」无关：
            // 超大图标视图格子宽，名字基本都在 3 行内，之前会因 needs=false
            // 拿不到 expand-index，于是完全无法重命名。
            let index = if needs || renaming_here { t.0 } else { -1 };
            sigs[pane] = (
                index,
                box_w.to_bits(),
                hash_lines(&lines),
                hash_str(&t.1),
                hover_sig.0,
                hover_sig.1,
                hover_sig.2,
            );
            plans[pane] = Some((box_w, lines, t.1.as_str().into(), hover_plan.0, hover_plan.1));
        }
        if self.expand_sig.as_ref() == Some(&sigs) {
            return;
        }
        self.expand_sig = Some(sigs);

        for (pane, plan) in plans.into_iter().enumerate() {
            let (box_w, lines, name, hv_box_w, hv_lines) = match plan {
                Some(v) => v,
                None => (0.0f32, Vec::new(), SharedString::default(), 0.0f32, Vec::new()),
            };
            let index = sigs[pane].0;
            let hv_index = sigs[pane].4;
            let model = ModelRc::new(VecModel::from(
                lines
                    .into_iter()
                    .map(|line| GridLine {
                        parts: ModelRc::new(VecModel::from(
                            line.into_iter()
                                .map(|(text, hl)| NamePart {
                                    text: text.into(),
                                    hl,
                                })
                                .collect::<Vec<_>>(),
                        )),
                    })
                    .collect::<Vec<_>>(),
            ));
            let hv_model = ModelRc::new(VecModel::from(
                hv_lines
                    .into_iter()
                    .map(|line| GridLine {
                        parts: ModelRc::new(VecModel::from(
                            line.into_iter()
                                .map(|(text, hl)| NamePart {
                                    text: text.into(),
                                    hl,
                                })
                                .collect::<Vec<_>>(),
                        )),
                    })
                    .collect::<Vec<_>>(),
            ));
            if pane == 0 {
                self.ui.set_file_expand_index(index);
                self.ui.set_file_expand_name(name);
                self.ui.set_file_expand_box_w(box_w);
                self.ui.set_file_expand_lines(model);
                self.ui.set_file_hover_expand_index(hv_index);
                self.ui.set_file_hover_expand_box_w(hv_box_w);
                self.ui.set_file_hover_expand_lines(hv_model);
            } else {
                self.ui.set_file_expand_index_right(index);
                self.ui.set_file_expand_name_right(name);
                self.ui.set_file_expand_box_w_right(box_w);
                self.ui.set_file_expand_lines_right(model);
                self.ui.set_file_hover_expand_index_right(hv_index);
                self.ui.set_file_hover_expand_box_w_right(hv_box_w);
                self.ui.set_file_hover_expand_lines_right(hv_model);
            }
        }
    }

    /// 为指定面板构建可视窗口的行模型（左栏 pane 0 / 右栏 pane 1）。
    ///
    /// 两遍构建：第一遍在 `self.state` 的不可变借用下收集条目/选择/高亮，
    /// 第二遍释放借用后再调 `self.thumb_for` 填缩略图（可能需要 &mut self）。
    fn build_rows_for_pane(
        &mut self,
        tab_index: usize,
        pane_index: usize,
        first: usize,
        count: usize,
        list_height: f32,
    ) -> Vec<FileRow> {
        let (collected, computer_view) = {
            let tab = &self.state.tabs[tab_index];
            let Some(pane) = tab.panes.get(pane_index) else {
                return Vec::new();
            };
            let computer_view = pane.path.is_none();
            // 小图标视图（横排单行）名称区宽 = item-w - 50px，item-w = cell-w - 8px。
            // 搜索态的高亮分段走 HighlightName：它把前几段宽度钉死、最后一段
            // 被压到 0 宽，overflow:elide 画不出省略号。这里按格宽预算做一次
            // 单行切分，超宽时末段腾位补 `…`（保留命中高亮），与竖排 grid-lines 同源。
            let view_mode = self.config.view_mode.clone();
            let small_icons = view_mode == "icons-small";
            // 名称区真实宽度直接取 UI 导出的值（含自适应压缩结果），
            // 左右分屏各用自己那一栏，避免用下发列宽粗估导致漏判截断。
            let name_area_w = if pane_index == 0 {
                self.ui.get_file_name_area_w()
            } else {
                self.ui.get_file_name_area_w_right()
            } as f32;
            let name_area_w = name_area_w.max(24.0);
            // 小图标视图：HorizontalLayout 里的名称 Text 宽度 = item-w - 58px
            // （导出值已按视图算好，这里只在导出为 0 时退回按格宽估算）。
            let small_name_w = if name_area_w > 24.0 {
                name_area_w
            } else {
                (self.ui.get_cell_w_out() as f32 - 66.0).max(24.0)
            };
            let details_name_w = name_area_w;
            let grid_name_w = name_area_w;
            // 剥离盘符前缀（如 c:），避免在文件名中误匹配 c: 里的无关字符
            let (_, clean_filter) = crate::search::parse_drive_prefix(&pane.filter);
            let hl_query = clean_filter.trim().to_lowercase();
            let cut = &self.state.cut_paths;
            let mut hl_matcher = nucleo_matcher::Matcher::new(nucleo_matcher::Config::DEFAULT);
            let mut hl_bufs = (Vec::new(), Vec::new(), Vec::new());
            // (显示索引, 条目, 选中, 剪切, 行内分段, 浮层完整分段, 名称被截断)
            let collected: Vec<(
                i32,
                FileEntry,
                bool,
                bool,
                Vec<(String, bool)>,
                Vec<(String, bool)>,
                bool,
            )> = (first..first + count)
                    .filter_map(|di| {
                        let entry = pane.display_entry(di)?.clone();
                        let selected = pane.selection.contains(&entry.path);
                        let cut = cut.contains(&entry.path);
                        // 完整高亮分段（不截断）：浮层专用，保证搜索态也能显示全名。
                        let tip_parts: Vec<(String, bool)> = if hl_query.is_empty() {
                            Vec::new()
                        } else {
                            pane.display_parts(di)
                                .map(|(e, fields)| {
                                    crate::search::highlight_parts(
                                        &e.name,
                                        fields,
                                        &hl_query,
                                        &mut hl_matcher,
                                        &mut hl_bufs,
                                    )
                                })
                                .unwrap_or_default()
                        };
                        // 名称是否被当前视图的名称区宽度截断。
                        // 用与渲染同源的字符宽度估算（grid_display_lines 的预算规则），
                        // 比 Slint 的 preferred-width 猜阈值可靠：只要**存在截断**就弹浮层。
                        let (budget, max_lines) = if small_icons {
                            (small_name_w, 1)
                        } else if view_mode == "details" || view_mode == "list" {
                            (details_name_w, 1)
                        } else {
                            // 中/大/超大图标：名称最多 3 行
                            (grid_name_w, 3)
                        };
                        let full: Vec<(String, bool)> = if tip_parts.is_empty() {
                            vec![(entry.name.clone(), false)]
                        } else {
                            tip_parts.clone()
                        };
                        let layout = grid_display_lines(&full, budget, max_lines);
                        // 未截断时切行结果拼起来 == 原名；超宽时末行补 `…` 并丢掉尾部。
                        // 用长度比较而不是「是否含 …」，避免文件名本身带省略号时误判。
                        let original_len: usize = entry.name.chars().count();
                        let kept_len: usize = layout
                            .iter()
                            .flat_map(|line| line.iter())
                            .map(|(t, _)| t.chars().count())
                            .sum();
                        let name_truncated = kept_len < original_len;
                        let name_parts = if !small_icons || tip_parts.is_empty() {
                            tip_parts.clone()
                        } else {
                            // 小图标行内：按格宽预算切成单行、末段补省略号（保留高亮）。
                            grid_display_lines(&tip_parts, small_name_w, 1)
                                .into_iter()
                                .next()
                                .unwrap_or_default()
                        };
                        Some((di as i32, entry, selected, cut, name_parts, tip_parts, name_truncated))
                    })
                    .collect();
            (collected, computer_view)
        };
        // 缩略图请求：**按“离视口中心的距离”从近到远发出**，两步解决“滚到中段
        // 依然从顶部一个个慢慢加载”：
        //
        // 病灶：可视窗口 = 可视行 + 2*OVERSCAN（icons-xl 下 5 列，约 100 个条目）。
        // 按索引顺序发时，先发的是窗口**顶部**那一批；而旧窗口已经往主池排过
        // 成千上万个已滚走的任务，新请求只能排在它们后面，于是看起来像
        // 从顶部线性加载。
        //
        // 1. 按“距视口中心”排序，中心先出图，观感是整屏同时出现；
        // 2. 屏幕上正在显示的格子（不含 overscan）走独立优先池。
        //
        // 注意：可见区间必须用**条目下标**算，不能用 `scroll / row_h`。
        // `scroll_y` 不是按行高换算的像素值（图标视图里两者不同步），
        // 除出来会得到一个偏离真实位置的行号，结果优先池永远收不到任务。
        // `first` 是本帧窗口的**首个条目下标**，但它包含了上方 OVERSCAN 行缓冲
        // （见 pane_view_range：`first_row = scroll/row_h - OVERSCAN`）。
        // 真实屏幕的首项要跳过「实际存在的」上方缓冲行。
        //
        // 关键修复：窗口在顶部时 pane_view_range 会把 first_row clamp 到 0，
        // 此时 first 已经是屏幕首行、**没有任何上方缓冲**。以前这里无条件
        // 跳过 OVERSCAN(=4) 行，等于把屏幕最前 4 行排除出优先池，只能排普通
        // 池（还会被 drop_stale_thumb_requests 优先丢掉）——表现就是「滚动条在
        // 开头，缩略图却从第 4 行才开始渲染」。改为按 clamp 前的行数算实际缓冲。
        let row_h = if self.ui.get_row_height_out() > 0.0 {
            self.ui.get_row_height_out()
        } else {
            ROW_HEIGHT
        };
        let cells_per_row = (self.ui.get_cells_per_row().max(1)) as usize;
        let visible_rows = (list_height / row_h).ceil().max(1.0) as usize;
        let scroll = self.state.tabs[tab_index].panes[pane_index]
            .scroll_y
            .max(0.0);
        let raw_first_row = (scroll / row_h).floor() as isize - OVERSCAN as isize;
        let effective_above = if raw_first_row < 0 {
            (OVERSCAN as isize + raw_first_row).max(0) as usize
        } else {
            OVERSCAN
        };
        // 真实屏幕的条目下标区间（不含上下缓冲）。
        let screen_first = (first + effective_above * cells_per_row).min(first + count);
        let screen_last = (screen_first + visible_rows * cells_per_row).min(first + count);
        let core_first = screen_first;
        let core_last = screen_last;
        // 视口中心用真实屏幕首行表示，供“距中心排序”使用。
        let first_row = first / cells_per_row;
        let viewport_center_row =
            (first_row + effective_above) as f32 + visible_rows as f32 / 2.0;
        if effective_above != OVERSCAN {
            // 详细级：顶部钳制是常态（每次窗口刷新都会命中），
            // 当初只为排查「滚动条在开头、缩略图却从第4行开始渲染」而加，
            // 那个 bug 已修，保留能力但不再刷 1 级日志。
            logv!(
                "[thumb] top-clamped window: first={} count={} scroll={:.1} above={} screen_first={} core=[{},{})",
                first, count, scroll, effective_above, screen_first, core_first, core_last
            );
        }

        let thumb_px = self.thumb_size_for_view();
        // 网格名称的可换宽度 = cell-w - 16px（与 file_list.slint 内层文字宽一致）。
        let grid_name_w = (self.ui.get_cell_w_out() as f32 - 16.0).max(24.0);
        // 关键诊断（ROVE_LOG=1）：验收「存在截断就弹浮层」时记录一次汇总。
        // 按签名节流，避免滚动/40ms 轮询每次都刷日志。
        let truncated_count = collected.iter().filter(|t| t.6).count();
        let single = self.state.tabs[tab_index].panes[pane_index].selection.len() <= 1;
        let mode = self.config.view_mode.clone();
        let budget = if pane_index == 0 {
            self.ui.get_file_name_area_w()
        } else {
            self.ui.get_file_name_area_w_right()
        } as f32;
        let sig = (mode.clone(), pane_index, truncated_count);
        if self.tip_log_sig.as_ref() != Some(&sig) {
            self.tip_log_sig = Some(sig);
            // 详细级：每次导航/栏宽变化都会重算一次，属布局诊断噪音。
            logv!(
                "[tip] mode={} budget={:.1} pane={} rows={} truncated={} single={}",
                mode,
                budget,
                pane_index,
                collected.len(),
                truncated_count,
                single,
            );
        }
        let mut ordered = collected;
        ordered.sort_by_key(|(index, _, _, _, _, _, _)| {
            let row = (*index as usize) / cells_per_row;
            let center = row as f32 + 0.5;
            (center - viewport_center_row).abs() as i64
        });
        ordered
            .into_iter()
            .map(|(index, entry, selected, cut, name_parts, tip_parts, name_truncated)| {
                let in_core = index >= core_first as i32 && (index as usize) < core_last;
                let thumb = self.thumb_for(
                    &entry.path,
                    entry.modified,
                    entry.size,
                    thumb_px,
                    in_core,
                );
                // 此电脑视图的驱动器条目：显示名里的类型标签（"本地磁盘"）走字典翻译，
                // 卷标保持原样——与侧边栏 / 面包屑的 tr_drive_label 行为一致。
                // 只改显示名；排序、打开、重命名等仍用 entry.name / entry.path。
                let display_name = if computer_view {
                    self.tr_drive_label(&entry.name).to_string()
                } else {
                    entry.name.clone()
                };
                FileRow {
                    index,
                    name: display_name.clone().into(),
                    grid_lines: {
                        // 名称分成「高亮分段」两路输入：搜索态直接用命中分段，
                        // 常态退化为单个不高亮段，渲染路径因此只有一条。
                        let parts: Vec<(String, bool)> = if name_parts.is_empty() {
                            vec![(display_name, false)]
                        } else {
                            name_parts.clone()
                        };
                        ModelRc::new(VecModel::from(
                            grid_display_lines(&parts, grid_name_w, 3)
                                .into_iter()
                                .map(|line| GridLine {
                                    parts: ModelRc::new(VecModel::from(
                                        line.into_iter()
                                            .map(|(text, hl)| NamePart {
                                                text: text.into(),
                                                hl,
                                            })
                                            .collect::<Vec<_>>(),
                                    )),
                                })
                                .collect::<Vec<_>>(),
                        ))
                    },
                    // 「路径」列显示删除前的原始路径，而不是内部唯一 Shell id。
                    path: entry
                        .original_path
                        .as_ref()
                        .unwrap_or(&entry.path)
                        .to_string_lossy()
                        .into_owned()
                        .into(),
                    is_dir: entry.is_dir,
                    is_hidden: entry.is_hidden,
                    // 此电脑视图：extra 形如「77.5 GB / 851 GB」，只留剩余容量。
                    size_text: if computer_view {
                        entry
                            .extra
                            .split('/')
                            .next()
                            .unwrap_or("")
                            .trim()
                            .to_string()
                    } else {
                        fs::format_size(&entry)
                    }
                    .into(),
                    modified_text: fs::format_time(entry.modified).into(),
                    type_text: self.tr_type(&entry.type_text).into(),
                    icon_color: icon_color(&entry),
                    icon_text: icon_text_for(&entry).into(),
                    selected,
                    cut,
                    thumb,
                    name_parts: ModelRc::new(VecModel::from(
                        name_parts
                            .into_iter()
                            .map(|(text, hl)| NamePart { text: text.into(), hl })
                            .collect::<Vec<_>>(),
                    )),
                    tip_parts: ModelRc::new(VecModel::from(
                        tip_parts
                            .into_iter()
                            .map(|(text, hl)| NamePart { text: text.into(), hl })
                            .collect::<Vec<_>>(),
                    )),
                    name_truncated,
                }
            })
            .collect()
    }

    /// 切换标签后的完整同步。
    fn sync_active_tab(&mut self) {
        // 打开保护伞：切换标签时，Slint 会因内容高度变化对 Flickable 自动 Clamp，
        // 触发残留的 scroll-changed 事件；此刻置位可避免其把旧标签的巨大坐标
        // 写进新标签的 scroll_y（否则表现为切标签白屏）。
        self.syncing_tab = true;

        // 切换标签时取消挂起的延迟重命名：否则切走再切回原标签，原标签选择集可能
        // 仍满足「唯一选中该路径」，导致 400ms 后跨标签冒出重命名输入框。
        self.cancel_rename_defer();

        // 切换标签后退出地址栏编辑态。
        self.ui.set_path_editing(false);
        self.sync_tabs();
        // 每个标签/分屏各有一份搜索词，切换后搜索框显示新页面的内容。
        let pane_filter = self.state.active().active().filter.clone();
        self.ui.set_filter_text(pane_filter.into());
        self.sync_nav();
        self.sync_breadcrumbs();
        self.sync_status();
        self.sync_split_state();
        let ratio = self.state.active().split_ratio;
        self.ui.set_split_ratio_want(ratio);

        // 核心修复：先推送正确的滚动条位置，再刷新列表数据。
        // 否则 Slint 在内容高度重置时先 Clamp（污染），再被我们覆盖。
        let (scroll0, is_split, scroll1) = {
            let tab = self.state.active();
            (
                tab.panes.first().map(|p| p.scroll_y).unwrap_or(0.0),
                tab.is_split,
                tab.panes.get(1).map(|p| p.scroll_y).unwrap_or(0.0),
            )
        };
        push_scroll(&self.ui, 0, scroll0);
        if is_split {
            push_scroll(&self.ui, 1, scroll1);
        } else {
            push_scroll(&self.ui, 1, 0.0);
        }

        // 再更新数据和列表
        self.visible_cache = None;
        self.sync_file_list();
        self.sync_sidebar_active();

        // 关闭保护伞
        self.syncing_tab = false;

        // 设置标签的导航/标题/分组由 sync_settings 下发：从快照恢复或切标签
        // 回来时也必须补一次，否则左侧子导航是空的。
        if self.state.active().kind == crate::state::TabKind::Settings {
            self.sync_settings();
        }
    }

    /// 切换当前标签的分屏状态。
    ///
    /// - 开启：以当前聚焦面板为蓝本克隆第二个面板（同路径 / 历史 / 排序），
    ///   聚焦切到新面板并立刻发起列举；
    /// - 关闭：丢弃第二个面板，聚焦回到面板 0。
    fn toggle_split(&mut self) {
        let mut request = None;
        {
            let tab = self.state.active_mut();
            if tab.kind != crate::state::TabKind::Explorer {
                return; // 设置页不分屏
            }
            if !tab.is_split {
                let src = tab.active();
                // 右栏从干净状态新建：同路径 + 同排序，但不继承左栏的搜索词与历史，
                // 否则左栏正在进行的搜索会随分屏混入右栏（含前进/后退导航里的历史搜索词）。
                let mut pane = crate::state::PaneState::new(src.path.clone(), src.sort);
                pane.loading = true;
                tab.panes.push(pane);
                tab.is_split = true;
                tab.active_pane = 1;
                let p = tab.active_pane;
                request = Some((
                    tab.id,
                    p,
                    tab.panes[p].generation,
                    tab.panes[p].path.clone(),
                    tab.panes[p].sort,
                ));
            } else {
                tab.panes.truncate(1);
                tab.is_split = false;
                tab.active_pane = 0;
            }
        }
        if let Some((id, pane, generation, path, sort)) = request {
            self.request_listing(id, pane, generation, path, sort);
        }
        // 分屏后焦点切到新面板：搜索框同步为当前聚焦面板（右栏干净、应为空）。
        self.ui.set_filter_text(self.state.active().active().filter.clone().into());
        let ratio = self.state.active().split_ratio;
        self.visible_cache = None;
        self.sync_split_state();
        self.ui.set_split_ratio_want(ratio);
        self.sync_nav();
        self.sync_breadcrumbs();
        self.sync_status();
        self.sync_file_list();
        self.sync_tabs();
        // 打开/关闭分屏后左栏视口宽度变了：立刻按左栏内容重新自适应一次列宽，
        // 否则名称列仍按分屏前的宽视口估算，右栏挤进来后会出现横向滚动条。
        #[cfg(windows)]
        if !self.state.active().panes.first().map(|p| p.loading).unwrap_or(true) {
            refit_columns_now(self, 0);
        }
    }

    /// 聚焦某一分屏面板（0 / 1），顶部地址栏与工具栏随之切换数据源。
    fn focus_pane(&mut self, index: usize) {
        let active_tab = self.state.active_tab;
        let mut request = None;
        {
            let Some(tab) = self.state.tabs.get_mut(active_tab) else {
                return;
            };
            if index >= tab.panes.len() || tab.active_pane == index {
                return;
            }
            tab.active_pane = index;
            // 该面板尚未列举过（例如刚克隆出来）则补一次请求。
            let need_reload = tab.panes[index].entries.is_empty()
                && tab.panes[index].path.is_some()
                && !tab.panes[index].loading;
            if need_reload {
                tab.panes[index].loading = true;
                request = Some((
                    tab.id,
                    index,
                    tab.panes[index].generation,
                    tab.panes[index].path.clone(),
                    tab.panes[index].sort,
                ));
            }
        }
        if let Some((id, pane, generation, path, sort)) = request {
            self.request_listing(id, pane, generation, path, sort);
        }
        self.visible_cache = None;
        self.sync_split_state();
        self.sync_nav();
        self.sync_breadcrumbs();
        self.sync_status();
        self.sync_file_list();
        // 每个分屏各有一份搜索词：切换焦点时把搜索框换成新面板的内容。
        let pane_filter = self.state.active().active().filter.clone();
        self.ui.set_filter_text(pane_filter.into());
        // 同步标签栏标题、图标和 loading：标签作用于当前聚焦面板。
        self.sync_tabs();
    }

    /// 把分屏状态同步到 UI（开关 / 当前聚焦面板）。
    fn sync_split_state(&self) {
        let tab = self.state.active();
        self.ui.set_is_split(tab.is_split);
        self.ui.set_active_pane(tab.active_pane as i32);
    }

    fn sync_sidebar_active(&mut self) {
        // 仅更新激活项（避免整表重建）。
        let active_id = match self.state.active().active().path.as_ref() {
            None => SharedString::default(),
            Some(path) => {
                let s = path.to_string_lossy().into_owned();
                if let Some(drive) = self
                    .drives
                    .iter()
                    .find(|d| s.starts_with(format!("{}:", d.letter).as_str()))
                {
                    format!("drive:{}", drive.letter).into()
                } else if s.starts_with("\\\\") {
                    format!("net:{s}").into()
                } else {
                    SharedString::default()
                }
            }
        };
        self.ui.set_sidebar_active_id(active_id);
        // 切目录后按该目录记录还原列宽（含双击自适应所需的估算宽度）。
        #[cfg(windows)]
        apply_column_widths(self);
    }
}

/// 把 `path` 对应的条目移动到 `index`（重命名后保持原位置）。
/// 其余条目的相对顺序完全不变。返回 `(原位置, 新位置)`；找不到该路径则返回 `None`。
fn pin_entry_at(
    entries: &mut Vec<crate::fs::entry::FileEntry>,
    path: &Path,
    index: usize,
) -> Option<(usize, usize)> {
    let from = entries.iter().position(|e| e.path == path)?;
    let to = index.min(entries.len().saturating_sub(1));
    if from != to {
        let pinned = entries.remove(from);
        entries.insert(to, pinned);
    }
    Some((from, to))
}

/// 图标视图格子尺寸从 UI 读（UI 里按真实字体行高算，保证 3 行名字装得下）。
/// Windows 文件名长度上限（UTF-16 码元）。超过这个名字的条目系统会拒绝创建，
/// 所以重命名输入框提前截断，而不是等系统报错。
const MAX_FILE_NAME_UNITS: usize = 255;

/// 按 Windows 文件名上限截断，**保留扩展名**（超长时只截主名）。
/// 不切断代理对（emoji 不会被截成半个）。
fn clamp_file_name(name: &str) -> String {
    let total = utf16_len(name);
    if total <= MAX_FILE_NAME_UNITS {
        return name.to_string();
    }
    // 先切出扩展名（含点）：“.gitignore” 这种开头点的、末尾是点的都算无扩展名。
    let (stem, ext) = match name.rfind('.') {
        Some(i) if i > 0 && i + 1 < name.len() => (&name[..i], &name[i..]),
        _ => (name, ""),
    };
    let ext_units = utf16_len(ext);
    if ext_units == 0 || ext_units >= MAX_FILE_NAME_UNITS {
        return clamp_units(name, MAX_FILE_NAME_UNITS).to_string();
    }
    let keep = MAX_FILE_NAME_UNITS - ext_units;
    // 截断后主名可能以空格结尾（“abc .mp4”），去掉更好看。
    let stem = clamp_units(stem, keep).trim_end_matches(' ');
    format!("{}{}", stem, ext)
}

/// UTF-16 码元个数（Windows 的“文件名长度”就是数这个）。
fn utf16_len(s: &str) -> usize {
    s.encode_utf16().count()
}

/// 截到 `max` 个 UTF-16 码元，不切断代理对。
fn clamp_units(s: &str, max: usize) -> &str {
    let mut units = 0usize;
    for (idx, ch) in s.char_indices() {
        let w = ch.len_utf16();
        if units + w > max {
            return &s[..idx];
        }
        units += w;
    }
    s
}

/// CJK / 全角字符：11.5px 字号下 advance 基本等于 1em，与 11.5px 估值一致。
/// 其余非 ASCII（`❀` `♡` `❤` `▶` 等装饰符号、emoji）会回退到符号字体，
/// 实际宽度往往大于字号，由调用方按更大值估算。
fn is_wide_cjk(c: char) -> bool {
    matches!(
        c as u32,
        0x1100..=0x115F       // 谚文字母
            | 0x2E80..=0x303E // CJK 部首 / 标点（含全角空格）
            | 0x3041..=0x33FF // 假名 / 注音 / CJK 兼容
            | 0x3400..=0x4DBF // CJK 扩展 A
            | 0x4E00..=0x9FFF // CJK 统一表意文字
            | 0xA000..=0xA4CF // 彝文
            | 0xAC00..=0xD7A3 // 谚文音节
            | 0xF900..=0xFAFF // CJK 兼容表意
            | 0xFE30..=0xFE4F // CJK 兼容形式
            | 0xFF00..=0xFF60 // 全角 ASCII
            | 0xFFE0..=0xFFE6 // 全角符号
            | 0x20000..=0x3FFFD // CJK 扩展 B 及以后
    )
}

/// 名字切行结果：每行若干 `(文本, 是否高亮)` 片段。
type GridLines = Vec<Vec<(String, bool)>>;

/// 把「带高亮分段的名字」按可用宽度预切成最多 `max_lines` 行，超出时末行以
/// `…` 收尾，返回逐行的片段列表。
///
/// 为什么按行拆成片段而不是交给 Slint：`Text` 不支持富文本，搜索命中高亮必须
/// 拆成多个 `Text`；而多行 `Text` 用 `overflow:elide` 时，文件名里的 `❀` 等
/// 装饰符号会触发字体回退多断一行，elide 按完整文本高度定位就会把首行整行
/// 裁掉。行数由 Rust 定死后，多行、首行完整、末行省略号、命中高亮可以同时成立。
fn grid_display_lines(parts: &[(String, bool)], width_px: f32, max_lines: usize) -> GridLines {
    const WIDE_W: f32 = 11.5; // 与 file_list.slint 的 font-size 一致
    // 符号/emoji 按更保守的 14px 估：连续多个装饰字符（❀♡❤▶）各自回退到
    // 符号字体时，若仍按 11.5px 计就会累积超宽，整行被水平裁掉两端。
    const SYMBOL_W: f32 = 14.0;
    // 留整整一个宽字符余量：估算与真实 shaping 总有偏差，留一格缓冲。
    let budget = (width_px - WIDE_W).max(WIDE_W * 2.0);
    // ASCII 不能再统一按 6px 估：大写与数字明显更宽（「EXID-Up&Down.mp4」这类
    // 名字会被低估，行宽超出容器后 HorizontalLayout 会压缩末段，把 `.mp4` 的
    // 尾部裁掉）。这里按字宽细分，宁可估宽一点也不裁字。
    let width_of = |c: char| -> f32 {
        if c.is_ascii() {
            match c {
                ' ' | '\t' => 3.6,
                'a'..='z' => 5.9,
                'A'..='Z' => 7.4,
                '0'..='9' => 6.6,
                '.' | ',' | ':' | ';' | '\'' | '|' | '!' => 3.6,
                _ => 6.6,
            }
        } else if is_wide_cjk(c) {
            WIDE_W
        } else {
            SYMBOL_W
        }
    };

    // 可在该字符**之后**换行的位置：空白与常见分隔符；非 ASCII（CJK / 全角
    // 符号）本身逐字可断，与资源管理器的中文换行一致。
    // 刻意不含 '.'：避免把扩展名拆成单独一行（"xxx." / "mp4"）。
    let breakable_after = |c: char| -> bool {
        if c.is_ascii() {
            matches!(
                c,
                ' ' | '\t'
                    | '-'
                    | '_'
                    | '/'
                    | '\\'
                    | ','
                    | ';'
                    | ')'
                    | ']'
                    | '}'
                    | '+'
                    | '&'
                    | '='
                    | '~'
                    | '|'
            )
        } else {
            true
        }
    };

    // 展开成「字符 + 是否高亮」，切行只依赖字符宽度，分组时再还原高亮段。
    let chars: Vec<(char, bool)> = parts
        .iter()
        .flat_map(|(t, hl)| t.chars().map(move |c| (c, *hl)))
        .collect();
    if chars.is_empty() {
        return Vec::new();
    }

    let mut ranges: Vec<(usize, usize)> = Vec::new();
    let mut i = 0usize;
    while i < chars.len() {
        let mut used = 0.0f32;
        let mut j = 0usize;
        // 行内最后一个可换行位置（相对行首的字符数）。
        let mut last_break = 0usize;
        while i + j < chars.len() {
            let c = chars[i + j].0;
            let cw = width_of(c);
            if j > 0 && used + cw > budget {
                break;
            }
            used += cw;
            j += 1;
            if breakable_after(c) {
                last_break = j;
            }
        }
        let rest_fits = i + j >= chars.len();
        // 优先回退到最后一个可断点，避免把英文单词从中间劈开；
        // 整行都没有可断点（单个超长单词）时才硬断。
        let take = if rest_fits {
            j
        } else if last_break > 0 {
            last_break
        } else {
            j.max(1)
        };
        let mut end = (i + take).min(chars.len());
        while end > i && chars[end - 1].0.is_whitespace() {
            end -= 1;
        }
        if end > i {
            ranges.push((i, end));
        }
        i += take;
        // 下一行不要以空白开头。
        while i < chars.len() && chars[i].0.is_whitespace() {
            i += 1;
        }
    }

    let mut lines: GridLines = ranges
        .into_iter()
        .map(|(s, e)| group_parts(&chars[s..e]))
        .collect();
    if lines.is_empty() {
        return Vec::new();
    }
    if lines.len() <= max_lines {
        return lines;
    }

    // 超出：保留前 max_lines 行，末行腾出省略号宽度后补 `…`（不继承高亮色）。
    lines.truncate(max_lines);
    let limit = (budget - WIDE_W).max(WIDE_W);
    let last = lines.last_mut().unwrap();
    let mut kept: Vec<(String, bool)> = Vec::new();
    let mut w = 0.0f32;
    let mut full = false;
    for (text, hl) in last.iter() {
        let mut cur = String::new();
        for ch in text.chars() {
            let cw = width_of(ch);
            if w + cw > limit {
                full = true;
                break;
            }
            w += cw;
            cur.push(ch);
        }
        if !cur.is_empty() {
            kept.push((cur, *hl));
        }
        if full {
            break;
        }
    }
    // 去掉末段尾部空白，避免出现「字 …」的空隙。
    if let Some((t, _)) = kept.last_mut() {
        let len = t.trim_end().len();
        t.truncate(len);
        if t.is_empty() {
            kept.pop();
        }
    }
    kept.push(("…".to_string(), false));
    *last = kept;
    lines
}

/// 把同高亮属性的相邻字符合并成一个片段。
fn group_parts(chars: &[(char, bool)]) -> Vec<(String, bool)> {
    let mut out: Vec<(String, bool)> = Vec::new();
    for &(c, hl) in chars {
        match out.last_mut() {
            Some((t, h)) if *h == hl => t.push(c),
            _ => out.push((c.to_string(), hl)),
        }
    }
    out
}

/// 浮层布局：先按格宽判断是否需要浮层（名字超过 3 行），需要时把框放宽到
/// 1.4 倍尽量压到 4 行内，仍放不下就继续往下长。行分段与框宽由同一处算出，
/// 保证高亮分段与真实换行位置一致。
fn expand_layout(ui: &MainWindow, parts: &[(String, bool)]) -> (bool, f32, GridLines) {
    let base_box = (ui.get_cell_w_out() as f32 - 8.0).max(24.0);
    let cap = (ui.get_view_width() as f32 - 12.0).max(base_box);
    let at_cell = grid_display_lines(parts, (base_box - 8.0).max(20.0), 64);
    let needs = at_cell.len() > 3;
    // 浮层固定 1.05 倍格宽，4 行放不下就继续往下长（不再逐级试探宽度，
    // 否则宽度与切行容易对不上）。1.1 倍在超大图标视图下会被右侧滚动条挤到：
    // 滚动条会吃掉末列右侧的 net gap，实际可用边距小于 min-gap。1.05 倍时
    // 单侧只伸出 0.025 × item-w（xl 约 4.7px），滚动条占位也留得住。
    let wide = (base_box * 1.05).min(cap);
    let lines = grid_display_lines(parts, (wide - 8.0).max(20.0), 64);
    (needs, wide, lines)
}

/// 状态栏文案是否属于「危险 / 强提醒」：这类提示用红色，普通提示保持主题色。
/// 按关键词判定，避免每个调用点都要额外传一个标记。
fn status_is_danger(text: &str) -> bool {
    // 注意：「删除」不能当危险标记——"已删除 N 个项目到回收站"（成功）、
    // "删除已取消"（取消）都会被它误判成红色。删除失败本来就含「失败」，
    // 删除出错含「错误」，故移除裸「删除」后不会漏报真正的失败。
    const MARKERS: [&str; 11] = [
        "失败", "错误", "超过", "上限", "非法", "无法", "不支持", "拒绝", "failed", "cannot",
        "denied",
    ];
    let lower = text.to_lowercase();
    MARKERS.iter().any(|m| lower.contains(m))
}

/// 一个可落点的面包屑段：窗口逻辑矩形 + 目标目录（`None` = 不可落点）。
#[cfg(windows)]
#[derive(Clone)]
struct CrumbDrop {
    rect: (f32, f32, f32, f32),
    target: Option<std::path::PathBuf>,
}

/// UNC 主机段（`\\server`）：只是 IP / 域名，不是可用目录，不能作为落点。
#[cfg(windows)]
fn is_unc_host(path: &str) -> bool {
    path.strip_prefix(r"\\")
        .is_some_and(|rest| !rest.is_empty() && !rest.contains('\\'))
}

/// 命中的可落点面包屑段索引（-1 = 没命中或不可落点）。
#[cfg(windows)]
fn crumb_hit(list: &[CrumbDrop], x: f32, y: f32) -> i32 {
    list.iter()
        .position(|c| {
            c.target.is_some()
                && x >= c.rect.0
                && x <= c.rect.0 + c.rect.2
                && y >= c.rect.1
                && y <= c.rect.1 + c.rect.3
        })
        .map_or(-1, |i| i as i32)
}

/// 面包屑各段 + 可落点判定。拖动前快照：模态循环里不能再读 Slint。
#[cfg(windows)]
fn drag_crumbs(app: &App) -> Vec<CrumbDrop> {
    let crumbs = app.ui.get_breadcrumbs();
    let n = crumbs.row_count();
    let current = app.state.active().active().path.clone();
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        let Some(seg) = crumbs.row_data(i) else { continue };
        let path = seg.path.to_string();
        let rect = app
            .crumb_rects
            .borrow()
            .get(i)
            .copied()
            .unwrap_or((0.0, 0.0, 0.0, 0.0));
        // 排除：此电脑（path 为空）、当前目录（最后一段）、UNC 主机段。
        let eligible = !path.is_empty()
            && i + 1 != n
            && !is_unc_host(&path)
            && current.as_deref() != Some(std::path::Path::new(&path));
        out.push(CrumbDrop {
            rect,
            target: eligible.then(|| std::path::PathBuf::from(&path)),
        });
    }
    out
}

/// 侧边栏 id → 可落点目录。返回 `None` 的项（分区标题、网络占位、回收站、
/// UNC 主机根、当前目录）既不高亮也不会落点。
#[cfg(windows)]
fn sidebar_id_to_dir(app: &App, id: &str) -> Option<std::path::PathBuf> {
    use std::path::PathBuf;
    if id.starts_with("header:") || id == "net:none" || id == "quick:recycle" {
        return None;
    }
    let dir = if let Some(rest) = id.strip_prefix("drive:") {
        let letter = rest.chars().next()?;
        PathBuf::from(format!("{letter}:\\"))
    } else if let Some(rest) = id.strip_prefix("recent:") {
        PathBuf::from(rest)
    } else if let Some(rest) = id.strip_prefix("net:") {
        PathBuf::from(rest)
    } else if let Some(rest) = id.strip_prefix("quick:") {
        let q = app.config.quick_items.iter().find(|q| q.id == rest)?;
        if !q.path.is_empty() {
            PathBuf::from(&q.path)
        } else {
            quick_access_dirs()
                .into_iter()
                .find(|(_, k, _)| *k == q.id.as_str())
                .and_then(|(_, _, d)| d)?
        }
    } else {
        return None;
    };
    // UNC 主机根（`\\server`）不是目录；当前所在目录也不作为落点。
    if is_unc_host(&dir.to_string_lossy()) {
        return None;
    }
    if app.state.active().active().path.as_deref() == Some(dir.as_path()) {
        return None;
    }
    Some(dir)
}

/// 侧边栏可落点条目 + 几何（拖动前快照）。
#[cfg(windows)]
fn drag_sidebar_zones(app: &App) -> Vec<CrumbDrop> {
    let items = app.ui.get_sidebar_items();
    let n = items.row_count();
    let rects = app.sidebar_rects.borrow();
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        let Some(item) = items.row_data(i) else { continue };
        let id = item.id.to_string();
        let Some(target) = sidebar_id_to_dir(app, &id) else { continue };
        let Some(rect) = rects.get(&id).copied() else { continue };
        out.push(CrumbDrop {
            rect,
            target: Some(target),
        });
    }
    out
}

/// 拖到面包屑 / 侧栏条目松手：复用与内部拖放一致的传输管线。
/// 同盘用移动，跨盘（含网络路径）用复制。
#[cfg(windows)]
fn drop_into_crumb(app: &mut App, paths: &[std::path::PathBuf], target: &std::path::Path) {
    if paths.iter().all(|p| p.parent() == Some(target)) {
        return;
    }
    if paths.iter().any(|p| p == target || target.starts_with(p)) {
        app.set_status_job(app.tr("无法将文件夹移动到自身或其子文件夹中"));
        return;
    }
    app.expect_new_entries(paths, target);
    let sources = paths.to_vec();
    let dest_dir = target.to_path_buf();
    let kind = if paths.iter().all(|p| same_volume(p, target)) {
        JobKind::Move { sources, dest_dir }
    } else {
        JobKind::Copy { sources, dest_dir }
    };
    app.submit_job(kind);
}

/// 自适应去重用键：指定栏当前目录的路径字符串，虚拟目录用固定前缀。
/// 只用于避免每次刷新重复估算列宽，不再对应任何持久化记录。
#[cfg(windows)]
fn column_layout_key(app: &App, pane: usize) -> String {
    match app.state.active().panes.get(pane).and_then(|p| p.path.as_ref()) {
        Some(p) => p.to_string_lossy().into_owned(),
        None => "::root".to_string(),
    }
}

/// 按当前目录的**全部条目**估算各列「刚好放下最长内容」的宽度。
/// Slint 不暴露文本测量接口，只能按字符类型估：CJK 约一个字号宽，其余约 0.55 个。
/// 直接读数据而不是读已渲染的行，避免受虚拟滚动窗口影响。
#[cfg(windows)]
fn autofit_widths_for_pane(app: &App, pane_index: usize) -> [f32; 5] {
    fn text_w(s: &str) -> f32 {
        s.chars()
            .map(|c| if (c as u32) < 0x2E80 { 7.0 } else { 13.0 })
            .sum()
    }
    let tab = app.state.active();
    let Some(pane) = tab.panes.get(pane_index) else {
        return crate::config::DEFAULT_COLUMN_WIDTHS;
    };
    let computer = pane.path.is_none();
    let mut max = [0f32; 5];
    let mut seen = 0usize;
    for di in 0..pane.display.len() {
        // 超大目录只采样前若干条：估算够用，打开不必为它卡一下。
        if seen >= 4000 {
            break;
        }
        let Some(e) = pane.display_entry(di) else { continue };
        seen += 1;
        max[0] = max[0].max(text_w(&e.name));
        // 「此电脑」没有修改时间列，不必估算它的宽（列宽值保留占位不使用）。
        if !computer {
            max[1] = max[1].max(text_w(&crate::fs::format_time(e.modified)));
        }
        max[2] = max[2].max(text_w(&app.tr_type(&e.type_text)));
        // 此电脑视图的「剩余」列只显示 extra 的前半段。
        let size_text = if computer {
            e.extra.split('/').next().unwrap_or("").trim().to_string()
        } else {
            crate::fs::format_size(e)
        };
        max[3] = max[3].max(text_w(&size_text));
    }
    [
        (max[0] + 60.0).clamp(160.0, 900.0),
        // 此电脑视图的修改时间列整列不渲染（Slint 侧按 0 参与列排布），
        // 这个值只作占位；普通目录按下限 110px 保证时间文本不拥挤。
        (max[1] + 24.0).clamp(110.0, 320.0),
        (max[2] + 24.0).clamp(70.0, 260.0),
        (max[3] + 24.0).clamp(70.0, 200.0),
        // 第 5 项对应「路径」列：它是固定 240px，不参与自适应下发，
        // 保留占位只为与 Config 的 [f32; 5] 结构对齐。
        crate::config::DEFAULT_COLUMN_WIDTHS[4],
    ]
}

/// 下发某一栏的列宽。
///
/// 「此电脑」没有修改时间列，这只是一个占位值（Slint 侧按 0 参与列排布），
/// 不影响普通目录的时间列宽。
#[cfg(windows)]
fn push_col_widths(app: &mut App, pane: usize, w: [f32; 5]) {
    let pane = pane.min(1);
    app.cur_col_widths[pane] = [w[0], w[1], w[2], w[3]];
    app.ui
        .invoke_apply_col_widths(pane as i32, w[0], w[1], w[2], w[3]);
}

/// 切目录时重新开启「名称列窗口自适应」。
/// 注意：不动「已拟合过哪个目录」的记号——记号不变就不再重算，
/// 否则切焦点 / 切标签时两栏会用重新拟合的宽度互相覆盖（表现为顶栏列宽自己变）。
#[cfg(windows)]
fn apply_column_widths(app: &mut App) {
    let panes = if app.state.active().is_split { 2 } else { 1 };
    for pane in 0..panes {
        let key = column_layout_key(app, pane);
        if app.autofit_key[pane].as_deref() == Some(key.as_str()) {
            continue;
        }
        // 「名称列窗口自适应」开关存在 FileList 里，只能通过 Slint 侧函数切换；
        // 从 Rust 直接改主窗口属性不会传到那一栏的求值。
        app.ui.invoke_apply_auto_col_widths(pane as i32, true);
    }
}

/// 数据就绪后按目录下发列宽，每个目录只做一次（统一自适应，不落盘）。
/// Slint 侧会在 `auto-col-widths=true` 时按视口压缩名称列，
/// 保证不出现横向滚动条、「大小」列右侧留框选空白。
///
/// 两栏各自判定、各自拟合：两栏可能在不同目录（甚至一边此电脑），
/// 共用一份宽度或共用一个「已自适应」记号，会让切换焦点时用另一栏的内容
/// 重新拟合，表现为顶栏列宽自己变。
#[cfg(windows)]
fn autofit_column_widths_if_needed(app: &mut App) {
    let panes = if app.state.active().is_split { 2 } else { 1 };
    for pane in 0..panes {
        let key = column_layout_key(app, pane);
        // 导航时 display 仍是上一个目录的数据（项目特意保留旧内容避免白屏），
        // loading 期间不能拿它估算新目录的列宽，也不能就此打上「已自适应」标记。
        let ready = app
            .state
            .active()
            .panes
            .get(pane)
            .is_some_and(|p| !p.loading && !p.display.is_empty());
        if !ready {
            continue;
        }
        if app.autofit_key[pane].as_deref() == Some(key.as_str()) {
            continue;
        }
        app.autofit_key[pane] = Some(key);
        app.ui.invoke_apply_auto_col_widths(pane as i32, true);
        let w = autofit_widths_for_pane(app, pane);
        push_col_widths(app, pane, w);
    }
}

/// 强制某一栏重新自适应一次（分屏开关改变视口宽度时用）。
#[cfg(windows)]
fn refit_columns_now(app: &mut App, pane_index: usize) {
    let pane = pane_index.min(1);
    app.ui.invoke_apply_auto_col_widths(pane as i32, true);
    let w = autofit_widths_for_pane(app, pane);
    push_col_widths(app, pane, w);
    app.autofit_key[pane] = Some(column_layout_key(app, pane));
}

/// 用户拖动 / 双击列边界：只在本次会话生效，不写配置、不按目录记忆。
/// 下一次切目录会由 `apply_column_widths` 重新统一自适应。
#[cfg(windows)]
fn record_col_width(app: &mut App, pane: usize, index: i32, width: f32, name_w_hint: f32) {
    let pane = pane.min(1);
    let idx = index.clamp(0, 4) as usize;
    // 名称列宽度必须用 Slint 上报的「松手瞬间显示宽」，不能读 w-name。
    // 自适应模式下 w-name 只是上一次下发的旧值（通常是内容拟合宽或默认 320），
    // 与用户眼前看到的宽度无关；早期版本读它，导致拖一下右列就把名称列
    // 钉死在 320 / 拟合值。同理也不能读 cur_col_widths[0]。
    let name_w = if idx == 0 { width } else { name_w_hint };
    let cur = app.cur_col_widths[pane];
    let mut w = [
        name_w.clamp(crate::config::MIN_COLUMN_W, crate::config::MAX_COLUMN_W),
        cur[1],
        cur[2],
        cur[3],
        crate::config::DEFAULT_COLUMN_WIDTHS[4],
    ];
    if width < 0.0 {
        // 双击列边界（-1）：按这一栏当前目录的内容现算「刚好放下最长内容」的宽度。
        let fit = autofit_widths_for_pane(app, pane);
        w[idx] = fit[idx];
    } else {
        w[idx] = width.clamp(crate::config::MIN_COLUMN_W, crate::config::MAX_COLUMN_W);
    }
    // 拖动任意分隔符都意味着用户开始手动接管列宽：先关自适应再下发宽度，
    // 否则 auto-col-widths 仍为 true 时 col-name-w 走 auto-name-w，
    // 刚写进去的 w[0] 根本不参与显示（名称列会跟着右列反向变化）。
    // 这个开关只在 Slint 侧生效，所以必须走 invoke。切目录 / 分屏时由
    // apply_column_widths 重新置 true 恢复自适应。
    app.ui.invoke_apply_auto_col_widths(pane as i32, false);
    push_col_widths(app, pane, w);
}

/// 纯文本版本（仅供单测断言用）。
#[cfg(test)]
fn grid_display_name(name: &str, width_px: f32, max_lines: usize) -> String {
    grid_display_lines(&[(name.to_string(), false)], width_px, max_lines)
        .iter()
        .map(|line| line.iter().map(|(t, _)| t.as_str()).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n")
}

fn cell_metrics(ui: &MainWindow) -> (f32, f32) {
    (ui.get_cell_w_out() as f32, ui.get_cell_h_out() as f32)
}

/// 计算某面板的虚拟窗口条目区间（行区间 × cells-per-row）。
fn pane_view_range(
    total: usize,
    cells_per_row: usize,
    row_h: f32,
    rows_visible: usize,
    scroll: f32,
) -> (usize, usize) {
    let total_rows = total.div_ceil(cells_per_row);
    let mut first_row = (scroll / row_h).floor() as isize - OVERSCAN as isize;
    first_row = first_row.clamp(0, total_rows.saturating_sub(1) as isize);
    let first_row = first_row.max(0) as usize;
    let count_rows = rows_visible.min(total_rows.saturating_sub(first_row)).max(1);
    let first = first_row * cells_per_row;
    let count = (count_rows * cells_per_row)
        .min(total.saturating_sub(first))
        .max(1);
    (first, count)
}

fn header(key: &str, label: &str) -> SidebarItem {
    SidebarItem {
        id: format!("header:{key}").into(),
        label: label.into(),
        detail: String::new().into(),
        kind: "header".into(),
        icon_color: Color::default(),
        icon_text: String::new().into(),
        usage: -1.0,
        icon: slint::Image::default(),
    }
}

/// 复刻 Pulse 的紧凑容量格式（如 `268G`、`6.2TB`、`1TB`）。
///
/// 阈值与舍入规则对齐 Pulse 侧栏：≥1000GB 折算为 TB（≥10TB 取整、
/// 否则一位小数并去掉多余的 `.0`），否则取整到 GB。
fn format_pulse_size(bytes: u64) -> String {
    let gb = bytes as f64 / (1024.0 * 1024.0 * 1024.0);
    if gb >= 1000.0 {
        let tb = gb / 1024.0;
        if tb >= 10.0 {
            format!("{tb:.0}TB")
        } else {
            // 1.0TB → 1TB，2.5TB 保持原样。
            format!("{tb:.1}TB").replace(".0TB", "TB")
        }
    } else {
        format!("{gb:.0}G")
    }
}

/// 复刻 Pulse 的多色容量条调色盘。
///
/// 蓝 / 薄荷绿 / 琥珀黄 / 紫按驱动器序号轮替；占用 >90% 时切换为告警红，
/// 与 Pulse 的「空间告急」视觉一致。`index` 为驱动器在 `list_drives()`
/// 中的序号（数据盘优先的顺序，见 `search::index::list_roots` 的同源排序）。
fn drive_pulse_color(index: usize, drive: &DriveInfo) -> Color {
    if drive.usage() > 0.90 {
        return Color::from_rgb_u8(0xE0, 0x52, 0x52); // 空间告急：红
    }
    const PALETTE: [(u8, u8, u8); 4] = [
        (0x3B, 0x82, 0xF6), // 蓝 (win10 C:)
        (0x10, 0xB9, 0x81), // 薄荷绿 (learn D:)
        (0xF5, 0x9E, 0x0B), // 琥珀黄 (extend E:)
        (0xA8, 0x55, 0xF7), // 紫 (move_hard_disk)
    ];
    let (r, g, b) = PALETTE[index % PALETTE.len()];
    Color::from_rgb_u8(r, g, b)
}

// ---------------------------------------------------------------------------
// 主题
// ---------------------------------------------------------------------------

fn apply_theme(ui: &MainWindow, choice: ThemeChoice) {
    let theme = ui.global::<Theme>();
    match choice {
        ThemeChoice::Light => {
            theme.set_bg_main(Color::from_rgb_u8(0xF3, 0xF4, 0xF5));
            theme.set_bg_sidebar(Color::from_rgb_u8(0xEB, 0xEC, 0xEE));
            theme.set_bg_list(Color::from_rgb_u8(0xFF, 0xFF, 0xFF));
            theme.set_bg_toolbar(Color::from_rgb_u8(0xF6, 0xF7, 0xF8));
            theme.set_bg_tabstrip(Color::from_rgb_u8(0xE7, 0xE8, 0xEB));
            theme.set_bg_hover(Color::from_rgb_u8(0x00, 0x00, 0x00).with_alpha(0.043));
            theme.set_bg_selected(Color::from_rgb_u8(0x0E, 0x8A, 0x4A).with_alpha(0.14));
            theme.set_bg_active_tab(Color::from_rgb_u8(0xFF, 0xFF, 0xFF));
            theme.set_bg_statusbar(Color::from_rgb_u8(0xF6, 0xF7, 0xF8));
            theme.set_bg_popup(Color::from_rgb_u8(0xFF, 0xFF, 0xFF));
            theme.set_bg_input(Color::from_rgb_u8(0xFF, 0xFF, 0xFF));
            theme.set_bg_badge(Color::from_rgb_u8(0x00, 0x00, 0x00).with_alpha(0.05));
            theme.set_text_primary(Color::from_rgb_u8(0x19, 0x1A, 0x1C));
            theme.set_text_secondary(Color::from_rgb_u8(0x6C, 0x70, 0x78));
            theme.set_text_disabled(Color::from_rgb_u8(0xA6, 0xAA, 0xB1));
            theme.set_border(Color::from_rgb_u8(0xDC, 0xDD, 0xE1));
            theme.set_divider(Color::from_rgb_u8(0xE5, 0xE6, 0xE9));
            theme.set_accent(Color::from_rgb_u8(0x0E, 0x8A, 0x4A));
            theme.set_accent_hover(Color::from_rgb_u8(0x0B, 0x7A, 0x41));
            theme.set_accent_text(Color::from_rgb_u8(0xFF, 0xFF, 0xFF));
            theme.set_danger(Color::from_rgb_u8(0xC4, 0x2B, 0x1C));
            theme.set_dark(false);
        }
        ThemeChoice::Dark => {
            theme.set_bg_main(Color::from_rgb_u8(0x1B, 0x1B, 0x1F));
            theme.set_bg_sidebar(Color::from_rgb_u8(0x23, 0x23, 0x27));
            theme.set_bg_list(Color::from_rgb_u8(0x1E, 0x1E, 0x22));
            theme.set_bg_toolbar(Color::from_rgb_u8(0x25, 0x25, 0x29));
            theme.set_bg_tabstrip(Color::from_rgb_u8(0x17, 0x17, 0x1A));
            theme.set_bg_hover(Color::from_rgb_u8(0xFF, 0xFF, 0xFF).with_alpha(0.06));
            theme.set_bg_selected(Color::from_rgb_u8(0x3F, 0xB2, 0x7F).with_alpha(0.20));
            theme.set_bg_active_tab(Color::from_rgb_u8(0x2A, 0x2A, 0x2E));
            theme.set_bg_statusbar(Color::from_rgb_u8(0x25, 0x25, 0x29));
            theme.set_bg_popup(Color::from_rgb_u8(0x2A, 0x2A, 0x2E));
            theme.set_bg_input(Color::from_rgb_u8(0x17, 0x17, 0x1A));
            theme.set_bg_badge(Color::from_rgb_u8(0xFF, 0xFF, 0xFF).with_alpha(0.08));
            theme.set_text_primary(Color::from_rgb_u8(0xF0, 0xF0, 0xF0));
            theme.set_text_secondary(Color::from_rgb_u8(0xA0, 0xA0, 0xA6));
            theme.set_text_disabled(Color::from_rgb_u8(0x66, 0x6A, 0x70));
            theme.set_border(Color::from_rgb_u8(0x3A, 0x3A, 0x40));
            theme.set_divider(Color::from_rgb_u8(0x33, 0x33, 0x38));
            theme.set_accent(Color::from_rgb_u8(0x3F, 0xB2, 0x7F));
            theme.set_accent_hover(Color::from_rgb_u8(0x4C, 0xC2, 0x8E));
            theme.set_accent_text(Color::from_rgb_u8(0x10, 0x24, 0x1A));
            theme.set_danger(Color::from_rgb_u8(0xE0, 0x6C, 0x5E));
            theme.set_dark(true);
        }
    }
}

// ---------------------------------------------------------------------------
// 菜单构建
// ---------------------------------------------------------------------------

/// 编辑器「层级归属」候选：已有二级分组（去空去重）+ 内置的「一级菜单」。
/// `top_label` 是「一级菜单」的当前语言文案：下拉直接展示它，且必须与
/// Slint 侧识别哨兵用的 I18n.strings[5] 一致（否则英文界面会显示中文原文）。
fn group_options(actions: &[crate::ops::actions::ActionEntry], top_label: &str) -> Vec<SharedString> {
    let mut opts: Vec<SharedString> = vec![top_label.into()];
    for a in actions {
        let g = a.group.trim();
        if !g.is_empty() && !opts.iter().any(|o| o == g) {
            opts.push(g.into());
        }
    }
    opts
}

impl App {
    /// 右键菜单里 Rove 自己那一部分（Shell 项之外）——直接产出原生菜单节点。
    ///
    /// Files 视图完全由 actions.toml（含内置动作）驱动，按 `group` 字段组织
    /// 一级项与二级子菜单；空白处菜单是固定的一组。
    /// `shell_present`：本次菜单挂了 Shell 项，那些已经由系统提供的动作就不再重复输出。
    #[cfg(windows)]
    fn build_app_nodes(
        &self,
        target: &MenuScope,
        shell_present: bool,
    ) -> Vec<crate::sys::menu::MenuNode> {
        use crate::sys::menu::MenuNode;
        let item = |key: &str, label: &str, enabled: bool| MenuNode::Item {
            key: key.to_string(),
            label: label.to_string(),
            enabled,
            checked: false,
        };
        let dup = |id: &str| shell_present && Self::is_shell_duplicate(id);

        // 回收站视图：专用菜单（还原 / 剪切 / 彻底删除 / 属性 / 清空 / 视图 / 排序）。
        if self.state.active().active().path.as_deref()
            == Some(fs::drives::recycle_virtual_path().as_path())
        {
            return match target {
                MenuScope::Selection(_) => vec![
                    item("recycle-restore", self.tr("还原到原位置").as_str(), true),
                    item("recycle-cut", self.tr("剪切").as_str(), true),
                    MenuNode::Separator,
                    item("recycle-purge", self.tr("彻底删除").as_str(), true),
                    MenuNode::Separator,
                    item("recycle-properties", self.tr("属性").as_str(), true),
                ],
                MenuScope::Background => vec![
                    item("recycle-empty", self.tr("清空回收站").as_str(), true),
                    MenuNode::Separator,
                    item(CMD_REFRESH, self.tr("刷新").as_str(), true),
                    // 视图 / 排序是全局状态，回收站条目走同一套 sort_entries 与
                    // 行模型；`view:` / `sort:` 的分发也在 menu_scope 检查之前。
                    MenuNode::Submenu {
                        label: self.tr("视图").to_string(),
                        enabled: true,
                        children: self.view_menu_nodes(),
                    },
                    MenuNode::Submenu {
                        label: self.tr("排序方式").to_string(),
                        enabled: true,
                        children: self.sort_menu_nodes(),
                    },
                ],
            };
        }

        match target {
            MenuScope::Selection(paths) => {
                let mut nodes: Vec<MenuNode> = Vec::new();
                // 「打开文件所在位置」只在搜索结果里有意义：搜索页的条目来自
                // 任意目录，需要跳回所在目录；普通目录里当前路径就是所在目录，
                // 显示它纯属噪音。
                let pane = self.state.active().active();
                let is_search = !pane.filter.trim().is_empty() || !pane.deep_entries.is_empty();
                if is_search {
                    nodes.push(item(CMD_OPEN_LOCATION, self.tr("打开文件所在位置").as_str(), true));
                }
                // 「在资源管理器中显示」由设置页「一级菜单」开关控制显隐。
                #[cfg(windows)]
                {
                    let reveal_enabled = self
                        .actions
                        .iter()
                        .find(|a| a.id == "builtin.reveal_explorer")
                        .map(|a| a.enabled)
                        .unwrap_or(true);
                    if reveal_enabled {
                        nodes.push(item(
                            CMD_REVEAL_EXPLORER,
                            self.tr("在资源管理器中显示").as_str(),
                            true,
                        ));
                    }
                }
                // 只有确实有前置项时才需要分隔线，否则菜单顶部会出现一条空分隔。
                if !nodes.is_empty() {
                    nodes.push(MenuNode::Separator);
                }
                // 按 group 聚合："" 为一级项，其余按首次出现顺序输出为二级子菜单。
                let mut groups: std::collections::BTreeMap<
                    String,
                    Vec<&crate::ops::actions::ActionEntry>,
                > = std::collections::BTreeMap::new();
                let mut order: Vec<String> = Vec::new();
                for a in self
                    .actions
                    .iter()
                    // reveal_explorer 由上面的 CMD_REVEAL_EXPLORER 单独渲染，
                    // 这里排除掉，避免一级菜单里出现两个同名项。
                    .filter(|a| a.id != "builtin.reveal_explorer")
                    .filter(|a| a.enabled && a.matches_selection(paths) && !dup(&a.id))
                {
                    if !groups.contains_key(&a.group) {
                        order.push(a.group.clone());
                    }
                    groups.entry(a.group.clone()).or_default().push(a);
                }

                for a in groups.remove("").unwrap_or_default() {
                    let mut label = self.tr(&a.label).to_string();
                    // 终端：非目录时改标签。
                    if a.id == "builtin.terminal" {
                        let first_is_dir = paths.first().map(|p| p.is_dir()).unwrap_or(false);
                        label = if first_is_dir {
                            self.tr("在此处打开终端")
                        } else {
                            self.tr("在终端中打开")
                        }
                        .to_string();
                    }
                    nodes.push(MenuNode::Item {
                        key: format!("act:{}", a.id),
                        label,
                        enabled: true,
                        checked: false,
                    });
                }

                for g in order.iter().filter(|g| !g.is_empty()) {
                    let Some(children) = groups.remove(g) else { continue };
                    nodes.push(MenuNode::Submenu {
                        label: g.clone(),
                        enabled: true,
                        children: children
                            .into_iter()
                            .map(|a| MenuNode::Item {
                                key: format!("act:{}", a.id),
                                label: self.tr(&a.label).to_string(),
                                enabled: true,
                                checked: false,
                            })
                            .collect(),
                    });
                }
                nodes
            }
            MenuScope::Background => {
                // 实时读系统剪贴板：资源管理器等外部程序随时可能覆盖它，
                // 不能用进程内的陈旧缓存判断文字与可用性。
                let (paste_paths, paste_move) = self.paste_state();
                // 「属性」不在这里：真实文件夹由系统菜单提供，
                // 虚拟根（此电脑 / 回收站）本来也拿不到它。
                vec![
                    MenuNode::Item {
                        key: CMD_PASTE.to_string(),
                        label: if paste_move {
                            self.tr("粘贴（移动）")
                        } else {
                            self.tr("粘贴")
                        }
                        .to_string(),
                        enabled: !paste_paths.is_empty(),
                        checked: false,
                    },
                    item(CMD_NEW_FOLDER, self.tr("新建文件夹").as_str(), true),
                    MenuNode::Separator,
                    item(CMD_TERMINAL, self.tr("在此处打开终端").as_str(), true),
                    item(CMD_REFRESH, self.tr("刷新").as_str(), true),
                    item(CMD_SELECT_ALL, self.tr("全选").as_str(), true),
                    item(CMD_MANAGE_ACTIONS, self.tr("管理右键动作...").as_str(), true),
                    MenuNode::Submenu {
                        label: self.tr("视图").to_string(),
                        enabled: true,
                        children: self.view_menu_nodes(),
                    },
                    MenuNode::Submenu {
                        label: self.tr("排序方式").to_string(),
                        enabled: true,
                        children: self.sort_menu_nodes(),
                    },
                    MenuNode::Separator,
                ]
            }
        }
    }

    /// 「视图」子菜单：当前模式打勾。
    #[cfg(windows)]
    fn view_menu_nodes(&self) -> Vec<crate::sys::menu::MenuNode> {
        let current = self.config.view_mode.as_str();
        use crate::sys::menu::MenuNode;
        [
            ("icons-xl", "超大图标"),
            ("icons-large", "大图标"),
            ("icons-medium", "中等图标"),
            ("icons-small", "小图标"),
            ("list", "列表"),
            ("details", "详细信息"),
        ]
        .iter()
        .map(|(id, label)| MenuNode::Item {
            key: format!("view:{id}"),
            label: self.tr(label).to_string(),
            enabled: true,
            checked: current == *id,
        })
        .collect()
    }

    /// 「排序方式」子菜单：当前键与升降序打勾。
    /// 首项随视图变：此电脑的主键是「盘符」（该视图没有名称列），
    /// 回收站/普通目录是「名称」。两套状态各自记忆，互不干扰。
    #[cfg(windows)]
    fn sort_menu_nodes(&self) -> Vec<crate::sys::menu::MenuNode> {
        let sort = self.state.active().active().sort;
        let is_computer = self.state.active().active().path.is_none();
        use crate::sys::menu::MenuNode;
        let item = |key: &str, label: &str, checked: bool| MenuNode::Item {
            key: format!("sort:{key}"),
            label: self.tr(label).to_string(),
            enabled: true,
            checked,
        };
        let mut nodes = vec![if is_computer {
            // 此电脑：首项是「盘符」（与表头一致）。
            item("drive", "盘符", sort.key == SortKey::Drive)
        } else {
            item("name", "名称", sort.key == SortKey::Name)
        }];
        // 只列该视图真实存在的列（此电脑没有名称/修改时间列）。
        if !is_computer {
            nodes.push(item("modified", "修改日期", sort.key == SortKey::Modified));
        }
        nodes.push(item("type", "类型", sort.key == SortKey::Type));
        nodes.push(item("size", "大小", sort.key == SortKey::Size));
        nodes.push(MenuNode::Separator);
        nodes.push(item("asc", "递增", sort.ascending));
        nodes.push(item("desc", "递减", !sort.ascending));
        nodes
    }

    /// 打开右键菜单：Shell 菜单（系统项）+ Rove 自己的项。
    fn open_menu(&self, target: MenuScope) {
        #[cfg(windows)]
        {
            let native_target = self.shell_menu_target(&target);
            let shell_present = !matches!(native_target, crate::sys::menu::Target::AppOnly);
            let nodes = self.build_app_nodes(&target, shell_present);
            // Shift + 右键 = 扩展动词（「以管理员身份运行」这类）。
            let extended = key_down(windows_sys::Win32::UI::Input::KeyboardAndMouse::VK_SHIFT as i32);
            if !self.open_native_menu(native_target, nodes, extended) {
                logln!("[menu] 原生菜单不可用，本次右键被忽略");
            }
        }
        #[cfg(not(windows))]
        let _ = target;
    }

    /// 菜单范围 → Shell 菜单目标（哪些交给 Shell，哪些只显示应用项）。
    #[cfg(windows)]
    fn shell_menu_target(&self, target: &MenuScope) -> crate::sys::menu::Target {
        // 回收站是虚拟目录：里面的路径不是真实路径，给 Shell 也解析不出来，
        // 只显示应用自己的项。其余情况（含「此电脑」里的磁盘行、搜索结果）
        // 都是真实路径，交给 Shell。
        let is_recycle = self
            .state
            .active()
            .active()
            .path
            .as_deref()
            .map(|p| p == fs::drives::recycle_virtual_path().as_path())
            .unwrap_or(false);
        match target {
            MenuScope::Selection(paths) if !is_recycle && !paths.is_empty() => {
                crate::sys::menu::Target::Items(paths.clone())
            }
            MenuScope::Background if !is_recycle => match self.state.active().active().path.clone()
            {
                Some(dir) => crate::sys::menu::Target::Background(dir),
                None => crate::sys::menu::Target::AppOnly,
            },
            _ => crate::sys::menu::Target::AppOnly,
        }
    }


    /// 与 Shell 菜单重复的自定义动作（仅在挂了 Shell 项时跳过）。
    ///
    /// 内置动作已经不再包含标准命令；这里只是防止用户手写 actions.toml 时
    /// 用旧 id 把「打开 / 属性」之类的项重复加一遍。
    ///
    /// 复制/剪切不在其中：系统的 copy 在我们这儿不可用（写进去的 CF_HDROP 读不到），
    /// 所以这两个始终用 Rove 自己的内置动作（还能给剪切态加变淡效果）。
    #[cfg(windows)]
    fn is_shell_duplicate(id: &str) -> bool {
        crate::ops::actions::RETIRED_BUILTINS.contains(&id)
    }

    /// 弹出原生菜单。返回 false 表示原生菜单线程不可用（菜单线程起不来）。
    #[cfg(windows)]
    fn open_native_menu(
        &self,
        target: crate::sys::menu::Target,
        nodes: Vec<crate::sys::menu::MenuNode>,
        extended: bool,
    ) -> bool {
        let owner = get_window_hwnd(&self.ui).unwrap_or(0);
        let mut hide_keywords = self.config.context_menu_hide.clone();
        if matches!(target, crate::sys::menu::Target::Background(_)) {
            hide_keywords.extend(
                crate::sys::menu::VIEW_LEVEL_LABELS
                    .iter()
                    .map(|s| s.to_string()),
            );
        }
        let options = crate::sys::menu::Options {
            hide_keywords,
            extended,
        };
        let sink: crate::sys::menu::Sink = std::sync::Arc::new(|action| {
            let _ = slint::invoke_from_event_loop(move || handle_native_menu_action(action));
        });
        crate::sys::menu::open(target, nodes, options, owner, sink)
    }

}

/// 累加各分组卡片高度，得到内容总高（与 `GroupCard` 的 Slint 侧公式一致：
/// 44px 头 + 42px/行，收起仅留头部，外加 12px 卡片间距）。
///
/// 与 Slint 的 `groups-layout.preferred-height` 取 max 使用：两路独立计算，
/// 任一少报都不会让底部内容被裁在视口外又滚不到。
fn sum_group_heights<M: Model<Data = SettingsActionGroup>>(model: &M) -> f32 {
    let mut h = 0.0f32;
    for i in 0..model.row_count() {
        if let Some(g) = model.row_data(i) {
            h += if g.expanded {
                44.0 + g.items.row_count() as f32 * 42.0
            } else {
                44.0
            } + 12.0;
        }
    }
    h
}

impl App {
    /// 打开或切换到设置页标签。
    fn open_settings_tab(&mut self, subpage: crate::state::SettingsSubpage, force_subpage: bool) {
        self.state.open_settings(subpage, force_subpage);
        self.ui.set_settings_open(true);
        self.ui.set_filter_text(SharedString::default());
        self.sync_tabs();
        self.sync_nav();
        self.sync_breadcrumbs();
        self.sync_file_list();
        self.sync_status();
        self.sync_settings();
        // 窄窗口 + 多标签时，设置标签（新建在末尾或复用在中间）可能落在视口外；
        // 按当前活动标签索引滚进可视区，而不是无条件滚到最右端。
        self.ui
            .invoke_scroll_tabs_to_index(self.state.active_tab as i32);
    }

    /// 保存当前 actions 并重新加载（合并内置动作、校验自定义命令）。
    fn save_actions_and_reload(&mut self) {
        if let Err(e) = crate::ops::actions::save(&self.actions) {
            self.set_status_job(e);
            return;
        }
        let raw = crate::ops::actions::load_or_create();
        self.actions = crate::ops::actions::heal_full(&raw);
    }
    /// 搜索索引看板指标：优先 `try_read`，绝不在 UI 线程阻塞等待构建写锁。
    ///
    /// `with_drives` 为 false 时只刷新高频指标（条目数 / 大小 / 状态），
    /// 避免周期刷新重建驱动器模型打断悬停高亮与开关动画。
    fn sync_search_stats(&self, with_drives: bool) {
        let snapshot = crate::search::index::try_shared().and_then(|i| {
            i.try_read()
                .ok()
                .map(|g| (g.ready, g.entries.len(), g.built_at))
        });
        let (is_ready, entry_count, built_at) = snapshot.unwrap_or((false, 0, None));

        self.ui.set_search_ready(is_ready);
        let status_text = if is_ready {
            self.tr("索引已就绪")
        } else if snapshot.is_some() {
            self.tr("正在构建索引...")
        } else {
            self.tr("索引未构建")
        };
        self.ui.set_search_status_text(status_text);
        self.ui.set_search_items_count(format!("{entry_count}").into());
        let real_bytes = crate::search::index::cache_file_size()
            .unwrap_or_else(|| (entry_count * INDEX_ENTRY_BYTES) as u64);
        self.ui.set_search_size_text(fs::format_size_bytes(real_bytes).into());
        let en = !i18n::is_chinese(&self.config.language);
        let last_scan = if is_ready {
            built_at
                .map(|t| format_ago(t, en))
                .unwrap_or_else(|| self.tr("刚刚").to_string())
        } else if snapshot.is_some() {
            self.tr("正在扫描...").to_string()
        } else {
            self.tr("未记录").to_string()
        };
        self.ui.set_search_last_scan_time(last_scan.into());

        if !with_drives {
            return;
        }

        // 纳管驱动器列表（仅固定驱动器）。
        let mut drive_items = Vec::new();
        let mut enabled_count = 0;
        for d in fs::list_drives()
            .into_iter()
            .filter(|d| matches!(d.drive_type, fs::drives::DriveType::Fixed))
        {
            let letter = d.letter.to_string();
            let is_indexed = !self.config.search_excluded_drives.contains(&letter);
            let is_ssd = is_ssd_drive(d.letter);
            if is_indexed {
                enabled_count += 1;
            }
            drive_items.push(SettingsDriveItem {
                letter: letter.into(),
                label: d.label.into(),
                total_space: if en {
                    format!("Total {}", fs::format_size_bytes(d.total_bytes))
                } else {
                    format!("总容量 {}", fs::format_size_bytes(d.total_bytes))
                }
                .into(),
                indexed: is_indexed,
                is_ssd,
            });
        }
        self.ui.set_search_drives_count(enabled_count);
        self.ui.set_search_drives(ModelRc::new(VecModel::from(drive_items)));

        // 内置系统忽略目录（与索引器的 NOISE_DIR_NAMES 保持一致）。
        let builtin: Vec<SharedString> = vec![
            "Windows".into(),
            "Program Files".into(),
            "AppData".into(),
            "$Recycle.Bin".into(),
            "System Volume Information".into(),
            "node_modules".into(),
            ".git".into(),
            "target".into(),
            "my_program".into(),
        ];
        self.ui
            .set_search_builtin_exclusions(ModelRc::new(VecModel::from(builtin)));

        // 用户自定义忽略目录。
        let custom: Vec<SharedString> = self
            .config
            .search_excluded_paths
            .iter()
            .map(|p| p.clone().into())
            .collect();
        self.ui
            .set_search_custom_exclusions(ModelRc::new(VecModel::from(custom)));

        // 自动化策略。
        self.ui
            .set_search_idle_enabled(self.config.search_idle_rescan_minutes > 0);
        self.ui.set_search_idle_minutes(if self.config.search_idle_rescan_minutes > 0 {
            self.config.search_idle_rescan_minutes as i32
        } else {
            60
        });
        self.ui
            .set_search_max_results(self.config.search_max_results as i32);
    }

    /// 空闲自动同步：索引已就绪、无文件操作任务、且距上次构建超过配置间隔时
    /// 在后台静默重建。由 15s 周期定时器驱动，开销仅是一次 `try_read`。
    fn maybe_idle_rescan(&mut self) {
        let minutes = self.config.search_idle_rescan_minutes;
        if minutes == 0 || self.jobs.is_running() {
            return;
        }
        let Some(idx) = crate::search::index::try_shared() else {
            return; // 索引未被使用过：不因一个配置项就无端扫盘。
        };
        let Ok(guard) = idx.try_read() else {
            return; // 正在构建：本轮到点跳过。
        };
        let due = guard.ready
            && guard
                .built_at
                .and_then(|t| SystemTime::now().duration_since(t).ok())
                .is_some_and(|elapsed| elapsed >= Duration::from_secs(minutes as u64 * 60));
        drop(guard);
        if due {
            logln!("idle rescan: rebuild index after {minutes} min");
            crate::search::index::rebuild();
        }
    }

    /// 同步设置页数据到 UI。
    fn sync_settings(&self) {
        use crate::state::{SettingsSubpage, TabKind};
        let tab = self.state.active();
        let current = if tab.kind == TabKind::Settings {
            tab.settings_subpage
        } else {
            SettingsSubpage::General
        };

        let tr = |key: &str| i18n::text(&self.config.language, key);
        let nav_items = vec![
            SettingsNavItem {
                id: "general".into(),
                label: tr("通用"),
                selected: current == SettingsSubpage::General,
            },
            SettingsNavItem {
                id: "search".into(),
                label: tr("搜索与索引"),
                selected: current == SettingsSubpage::SearchAndIndex,
            },
            SettingsNavItem {
                id: "context".into(),
                label: tr("本地右键菜单"),
                selected: current == SettingsSubpage::ContextMenu,
            },
            SettingsNavItem {
                id: "shortcuts".into(),
                label: tr("快捷键"),
                selected: current == SettingsSubpage::Shortcuts,
            },
        ];
        self.ui.set_settings_nav_items(ModelRc::new(VecModel::from(nav_items)));
        // 向设置页「通用」开关灌入当前状态。
        self.ui.set_show_hidden(self.config.show_hidden);
        self.ui.set_auto_edit_path_on_new_tab(self.config.auto_edit_path_on_new_tab);

        // 子页标题 / 描述 / 新增入口。
        let (title, desc, show_add) = match current {
            SettingsSubpage::General => ("通用", "应用启动、视图与行为偏好。", false),
            SettingsSubpage::SearchAndIndex => ("搜索与索引", "拼音检索、索引范围与缓存策略。", false),
            SettingsSubpage::ContextMenu => (
                "本地右键菜单",
                "开关控制菜单项显隐，关键词隐藏系统菜单项；自定义动作可编辑或删除。",
                true,
            ),
            SettingsSubpage::Shortcuts => (
                "快捷键",
                "点击按键芯片即可录制新快捷键；修改会自动保存，支持单项或全部恢复默认。",
                false,
            ),
        };
        // 标题与导航文案走同一份字典，保证与 Slint 里 I18n.strings 的比较一致。
        self.ui.set_settings_page_title(tr(title));
        self.ui.set_settings_page_desc(tr(desc));
        self.ui.set_settings_show_add(show_add);

        // 隐藏的系统右键菜单项关键词。
        // 注意：不能放进 sync_search_stats —— 那个函数只在「搜索与索引」子页才跑，
        // 而在「本地右键菜单」页添加关键词时模型就永远不会更新（列表始终为空）。
        let hide_keywords: Vec<SharedString> = self
            .config
            .context_menu_hide
            .iter()
            .map(|k| k.clone().into())
            .collect();
        self.ui
            .set_menu_hide_keywords(ModelRc::new(VecModel::from(hide_keywords)));

        if current == SettingsSubpage::SearchAndIndex {
            self.sync_search_stats(true);
            return;
        }

        if current == SettingsSubpage::Shortcuts {
            let mut groups_map: std::collections::BTreeMap<&'static str, Vec<SettingsShortcutItem>> =
                std::collections::BTreeMap::new();
            for meta in crate::ops::keymap::DEFAULT_SHORTCUTS {
                let chords = self
                    .keymap
                    .action_to_chords
                    .get(meta.id)
                    .cloned()
                    .unwrap_or_else(|| vec![meta.default_chord.to_string()]);
                let is_custom = self.config.shortcuts.contains_key(meta.id);

                for (slot, chord) in chords.into_iter().enumerate() {
                    let item = SettingsShortcutItem {
                        id: meta.id.into(),
                        slot_index: slot as i32,
                        item_key: format!("{}:{}", meta.id, slot).into(),
                        name: self.tr(meta.name),
                        group: self.tr(meta.group),
                        chord: chord.into(),
                        default_chord: meta.default_chord.into(),
                        is_customized: is_custom,
                        is_readonly: meta.is_readonly,
                    };
                    groups_map.entry(meta.group).or_default().push(item);
                }
            }
            let groups: Vec<SettingsShortcutGroup> = ["文件操作", "导航与查看", "编辑与选择"]
                .into_iter()
                .filter_map(|g| {
                    groups_map.remove(g).map(|items| SettingsShortcutGroup {
                        name: self.tr(g),
                        items: ModelRc::new(VecModel::from(items)),
                    })
                })
                .collect();
            self.ui.set_settings_shortcut_groups(ModelRc::new(VecModel::from(groups)));
            return;
        }

        if current != SettingsSubpage::ContextMenu {
            self.ui.set_settings_action_groups(ModelRc::new(VecModel::from(Vec::<SettingsActionGroup>::new())));
            self.ui.set_settings_content_height(0.0);
            return;
        }

        let mut groups_map: std::collections::BTreeMap<String, Vec<SettingsActionItem>> =
            std::collections::BTreeMap::new();
        let mut order: Vec<String> = Vec::new();
        for a in &self.actions {
            let g = a.group.clone();
            if !groups_map.contains_key(&g) {
                order.push(g.clone());
            }
            let item = SettingsActionItem {
                id: a.id.clone().into(),
                label: self.tr(&a.label).into(),
                target: a.target.clone().into(),
                extensions: a.extensions.join(", ").into(),
                program: a.program.clone().into(),
                enabled: a.enabled,
                builtin: a.builtin,
            };
            groups_map.entry(g).or_default().push(item);
        }
        let groups: Vec<SettingsActionGroup> = order
            .into_iter()
            .map(|g| {
                let name = if g.is_empty() {
                    self.tr("一级菜单")
                } else {
                    self.tr(&g).into()
                };
                SettingsActionGroup {
                    name,
                    expanded: true,
                    items: ModelRc::new(VecModel::from(groups_map.remove(&g).unwrap_or_default())),
                }
            })
            .collect();
        let groups_model = VecModel::from(groups);
        self.ui.set_settings_content_height(sum_group_heights(&groups_model));
        self.ui.set_settings_action_groups(ModelRc::new(groups_model));
    }

    /// 工具栏 ▦ 按钮：原生下拉菜单切换视图模式。
    fn open_view_picker(&mut self) {
        // 视图切换不依赖选中项，菜单目标给个空白处兜底。
        self.menu_anchor_path = None;
        self.menu_scope = Some(MenuScope::Background);
        #[cfg(windows)]
        {
            let nodes = self.view_menu_nodes();
            if !self.open_native_menu(crate::sys::menu::Target::AppOnly, nodes, false) {
                logln!("[menu] 原生菜单不可用，视图下拉被忽略");
            }
        }
    }

    /// 解析路径字符串，展开环境变量（如 `%LOCALAPPDATA%`）与主目录（`~`）。
    fn expand_path(input: &str) -> PathBuf {
        let mut s = input.trim().trim_matches('"').to_string();

        // 1. 展开 `~` 为当前用户主目录（Windows 下通常为 C:\Users\<name>）
        if s == "~" || s.starts_with("~\\") || s.starts_with("~/") {
            let home = std::env::var("USERPROFILE")
                .or_else(|_| std::env::var("HOME"))
                .unwrap_or_else(|_| s.clone());
            if s.len() > 1 {
                s = format!("{}{}", home, &s[1..]);
            } else {
                s = home;
            }
        }

        // 2. 展开 `%VAR%` 形式的环境变量
        let mut result = String::new();
        let mut chars = s.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '%' {
                let mut var_name = String::new();
                let mut closed = false;
                while let Some(&nc) = chars.peek() {
                    if nc == '%' {
                        chars.next(); // 消耗闭合的 %
                        closed = true;
                        break;
                    } else {
                        var_name.push(nc);
                        chars.next();
                    }
                }
                // 如果找到了成对的 %，且环境变量存在，则替换
                if closed && !var_name.is_empty() {
                    if let Ok(val) = std::env::var(&var_name) {
                        result.push_str(&val);
                        continue;
                    }
                }
                // 如果没闭合或环境变量不存在，原样保留
                result.push('%');
                result.push_str(&var_name);
                if closed {
                    result.push('%');
                }
            } else {
                result.push(c);
            }
        }
        PathBuf::from(result)
    }

    /// 面包屑路径编辑提交（点击地址栏输入路径后回车）。
    fn commit_path_edit(&mut self, path: &str) {
        let trimmed = path.trim().trim_matches('"');
        if trimmed.is_empty() {
            return;
        }
        // 虚拟根：地址栏在“此电脑”下会以标签作为初始文本，回车视为回到此电脑
        if trimmed == "此电脑" || trimmed.eq_ignore_ascii_case("this pc") {
            self.navigate_active(None);
            return;
        }

        // 核心修复：使用 expand_path 展开环境变量和 ~
        let p = Self::expand_path(trimmed);

        // `\\host` 不是文件系统路径（GetFileAttributes 打不开），但能列出共享，
        // 所以不能拿 exists() 卡它。
        if !p.exists() && !crate::fs::dir::is_unc_server_root(&p) {
            self.set_status_job(self.trf("路径不存在：{}", &[trimmed.to_string()]));
            return;
        }
        self.navigate_active(Some(p));
    }

    /// 侧栏各行的 (顶 y, 行高)（内容坐标）。必须与 `ui/sidebar.slint` 完全一致：
    /// header 30 / drive 46 / 其余 32，行间距 2，顶部内边距 8。
    /// 两侧不同步时，拖拽指示线与落点判断会按驱动器数量累积偏差。
    fn sidebar_row_metrics(&self) -> Vec<(f32, f32)> {
        let model = self.ui.get_sidebar_items();
        let n = model.row_count();
        let mut metrics = Vec::with_capacity(n);
        let mut y = 8.0_f32;
        for i in 0..n {
            let h = model
                .row_data(i)
                .map(|it| match it.kind.as_str() {
                    "header" => 30.0,
                    "drive" => 46.0,
                    _ => 32.0,
                })
                .unwrap_or(32.0);
            metrics.push((y, h));
            y += h + 2.0;
        }
        metrics
    }

    /// 侧栏分区拖拽排序（phase：0 按下 / 1 拖动 / 2 松开）。
    fn sidebar_header_drag(&mut self, index: usize, phase: i32, y_in_row: f32) {
        let metrics = self.sidebar_row_metrics();
        let tops: Vec<f32> = metrics.iter().map(|(top, _)| *top).collect();
        let Some(&row_top) = tops.get(index) else { return };
        let pointer_y = row_top + y_in_row;

        // 分区列表：header 行索引 + 分区 key。
        let model = self.ui.get_sidebar_items();
        let n = model.row_count();
        let mut headers: Vec<(usize, String)> = Vec::new();
        for i in 0..n {
            let Some(it) = model.row_data(i) else { continue };
            if it.kind != "header" {
                continue;
            }
            let key = it.id.strip_prefix("header:").unwrap_or("");
            if matches!(key, "quick" | "recent" | "computer" | "network" | "explorer-net") {
                headers.push((i, key.to_string()));
            }
        }
        let Some(drag_slot) = headers.iter().position(|(i, _)| *i == index) else {
            return;
        };

        match phase {
            0 => {
                self.sidebar_drag = Some(index);
            }
            1 => {
                if self.sidebar_drag != Some(index) {
                    return;
                }
                let (_, indicator_y) = Self::header_drop_slot(
                    &tops,
                    &metrics,
                    &headers,
                    drag_slot,
                    pointer_y,
                );
                self.ui.set_sidebar_drop_y(indicator_y);
                self.ui.set_sidebar_drop_visible(true);
            }
            2 => {
                self.ui.set_sidebar_drop_visible(false);
                if self.sidebar_drag != Some(index) {
                    self.sidebar_drag = None;
                    return;
                }
                self.sidebar_drag = None;
                let (drop_slot, _) = Self::header_drop_slot(
                    &tops,
                    &metrics,
                    &headers,
                    drag_slot,
                    pointer_y,
                );
                crate::logv!(
                    "[sidebar] header drop index={index} slot={drop_slot} pointer_y={pointer_y:.1} headers={:?}",
                    headers.iter().map(|(i, k)| (*i, tops[*i], k.as_str())).collect::<Vec<_>>()
                );
                let drag_key = headers[drag_slot].1.clone();
                let mut order: Vec<String> = self
                    .config
                    .sidebar_order
                    .iter()
                    .filter(|k| **k != drag_key)
                    .cloned()
                    .collect();
                if drop_slot > order.len() {
                    order.push(drag_key.clone());
                } else {
                    order.insert(drop_slot, drag_key.clone());
                }
                if order != self.config.sidebar_order {
                    self.config.sidebar_order = order;
                    self.config.save();
                    self.sidebar_fingerprint = 0;
                    self.sync_sidebar();
                }
            }
            _ => {}
        }
    }

    /// 分区拖拽落点：参照分区中「垂直中心在指针上方」的数量即插入槽位。
    /// 分区中心按 分区顶端 → 分区末端（下一个分区顶端；最后一个分区则是
    /// 列表最后一行的底部）计算。原实现拿「列表最后一行顶端」当末分区末端，
    /// 中心会被推到很下面，导致必须把鼠标移得更低才能落到下一个分区。
    fn header_drop_slot(
        tops: &[f32],
        metrics: &[(f32, f32)],
        headers: &[(usize, String)],
        drag_slot: usize,
        pointer_y: f32,
    ) -> (usize, f32) {
        let list_end = metrics
            .last()
            .map(|(top, height)| top + height + 2.0)
            .unwrap_or(0.0);
        let section_end = |start: usize| -> f32 {
            headers
                .iter()
                .map(|(i, _)| *i)
                .find(|h| *h > start)
                .map(|next| tops[next])
                .unwrap_or(list_end)
        };
        let remaining: Vec<usize> = headers
            .iter()
            .enumerate()
            .filter(|(s, _)| *s != drag_slot)
            .map(|(_, (i, _))| *i)
            .collect();
        let slot = remaining
            .iter()
            .filter(|start| {
                let top = tops[**start];
                top + (section_end(**start) - top) / 2.0 < pointer_y
            })
            .count();
        let indicator = match remaining.get(slot) {
            Some(start) => tops[*start],
            None => remaining
                .last()
                .map(|start| section_end(*start))
                .unwrap_or(list_end),
        };
        (slot, indicator)
    }

    /// 侧边栏项激活。
    fn sidebar_activated(&mut self, id: &str) {
        // 拖拽排序结束后的那次 clicked 要丢弃，否则拖完会顺带导航。
        if let Some(at) = self.suppress_sidebar_click.take() {
            if at.elapsed() < std::time::Duration::from_millis(400) {
                return;
            }
        }
        // 分区标题点击：切换展开/收起并持久化保存
        if let Some(key) = id.strip_prefix("header:") {
            let toggled = match key {
                "quick" => { self.config.quick_expanded = !self.config.quick_expanded; true }
                "recent" => { self.config.recent_expanded = !self.config.recent_expanded; true }
                "computer" => { self.config.computer_expanded = !self.config.computer_expanded; true }
                "network" => { self.config.network_expanded = !self.config.network_expanded; true }
                "explorer-net" => {
                    self.config.explorer_net_expanded = !self.config.explorer_net_expanded;
                    true
                }
                _ => false,
            };
            if toggled {
                self.config.save();
                self.sidebar_fingerprint = 0;
                self.sync_sidebar();
                return;
            }
        }
        if id == "quick:recycle" {
            // 回收站站内浏览：虚拟目录 recycle://。
            self.navigate_active(Some(fs::drives::recycle_virtual_path()));
            return;
        }
        if id == "net:none" {
            return;
        }
        if let Some(rest) = id.strip_prefix("quick:") {
            let Some(q) = self
                .config
                .quick_items
                .iter()
                .find(|q| q.id == rest)
                .cloned()
            else {
                return;
            };
            let dir = if !q.path.is_empty() {
                Some(PathBuf::from(&q.path))
            } else {
                quick_access_dirs()
                    .into_iter()
                    .find(|(_, key, _)| *key == q.id.as_str())
                    .and_then(|(_, _, d)| d)
            };
            if let Some(dir) = dir {
                self.navigate_active(Some(dir));
            }
            return;
        }
        if let Some(rest) = id.strip_prefix("drive:") {
            let letter = rest.chars().next().unwrap_or('C');
            self.navigate_active(Some(PathBuf::from(format!("{letter}:\\"))));
            return;
        }
        if let Some(rest) = id.strip_prefix("recent:") {
            self.navigate_active(Some(PathBuf::from(rest)));
            return;
        }
        if let Some(rest) = id.strip_prefix("net:") {
            self.navigate_active(Some(PathBuf::from(rest)));
        }
    }

    /// 侧栏右键：分区标题「网络位置」弹添加框；网络位置弹编辑/删除菜单；
    /// 「快速访问」条目弹取消固定（特殊项置灰）。
    fn sidebar_right_clicked(&mut self, id: &str) {
        if id == "header:network" {
            self.open_network_dialog();
            return;
        }
        if let Some(key) = id.strip_prefix("quick:") {
            self.show_quick_item_menu(key);
            return;
        }
        let Some(path) = id.strip_prefix("net:") else {
            return;
        };
        self.show_network_item_menu(path);
    }

    /// 已添加的网络位置右键菜单（编辑 / 删除）。系统检测出的映射盘不弹。
    #[cfg(windows)]
    fn show_network_item_menu(&mut self, path: &str) {
        let Some(entry) = self
            .config
            .network_locations
            .iter()
            .find(|n| n.path.eq_ignore_ascii_case(path))
        else {
            return;
        };
        let item = |key: String, label: &str| crate::sys::menu::MenuNode::Item {
            key,
            label: label.to_string(),
            enabled: true,
            checked: false,
        };
        let nodes = vec![
            item(format!("netloc:edit:{}", entry.path), self.tr("编辑").as_str()),
            item(format!("netloc:unpin:{}", entry.path), self.tr("取消固定").as_str()),
        ];
        self.open_native_menu(crate::sys::menu::Target::AppOnly, nodes, false);
    }

    #[cfg(not(windows))]
    fn show_network_item_menu(&mut self, _path: &str) {}

    /// 「快速访问」条目右键菜单：取消固定。特殊项（回收站）置灰。
    #[cfg(windows)]
    fn show_quick_item_menu(&mut self, key: &str) {
        let Some(entry) = self
            .config
            .quick_items
            .iter()
            .find(|q| q.id == key)
            .cloned()
        else {
            return;
        };
        let nodes = vec![crate::sys::menu::MenuNode::Item {
            key: format!("quick:unpin:{key}"),
            label: self.tr("取消固定").to_string(),
            enabled: !entry.is_special(),
            checked: false,
        }];
        self.open_native_menu(crate::sys::menu::Target::AppOnly, nodes, false);
    }

    #[cfg(not(windows))]
    fn show_quick_item_menu(&mut self, _key: &str) {}

    /// 取消固定：只从侧栏移除，不动磁盘上的文件夹；特殊项不可移除。
    fn unpin_quick_item(&mut self, key: &str) {
        let before = self.config.quick_items.len();
        self.config
            .quick_items
            .retain(|q| q.id != key || q.is_special());
        if self.config.quick_items.len() == before {
            return;
        }
        self.config.save();
        self.sidebar_fingerprint = 0;
        self.sync_sidebar();
        self.set_status_job(self.tr("已取消固定"));
    }

    /// 把文件夹固定到侧栏「快速访问」；去重（含默认系统目录），返回新增条数。
    fn pin_quick_folders(&mut self, paths: &[PathBuf]) -> usize {
        let system_paths: Vec<String> = quick_access_dirs()
            .into_iter()
            .filter_map(|(_, _, d)| d)
            .map(|d| d.to_string_lossy().into_owned())
            .collect();
        let mut added = 0;
        for p in paths {
            if !p.is_dir() || *p == fs::drives::recycle_virtual_path() {
                continue;
            }
            let text = p.to_string_lossy().into_owned();
            if system_paths.iter().any(|s| s.eq_ignore_ascii_case(&text))
                || self
                    .config
                    .quick_items
                    .iter()
                    .any(|q| q.path.eq_ignore_ascii_case(&text))
            {
                continue;
            }
            let label = p
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| text.clone());
            self.config.quick_items.push(QuickItem {
                id: format!("pin:{text}"),
                label,
                path: text,
            });
            added += 1;
        }
        if added > 0 {
            self.config.save();
            self.sidebar_fingerprint = 0;
            self.sync_sidebar();
        }
        added
    }

    fn open_network_dialog(&mut self) {
        self.network_edit_target = None;
        self.ui.set_network_dialog_title(self.tr("添加网络位置"));
        self.ui.set_network_dialog_server("".into());
        self.ui.set_network_dialog_alias("".into());
        self.ui.set_network_dialog_error("".into());
        self.ui.set_network_dialog_open(true);
        self.focus_network_dialog();
    }

    /// 编辑已添加的网络位置：标题不同，并把旧地址/别名填进去。
    fn open_network_edit_dialog(&mut self, path: &str) {
        let Some(entry) = self
            .config
            .network_locations
            .iter()
            .find(|n| n.path.eq_ignore_ascii_case(path))
            .cloned()
        else {
            return;
        };
        self.network_edit_target = Some(entry.path.clone());
        self.ui.set_network_dialog_title(self.tr("编辑网络地址"));
        self.ui.set_network_dialog_server(entry.path.into());
        self.ui.set_network_dialog_alias(entry.label.into());
        self.ui.set_network_dialog_error("".into());
        self.ui.set_network_dialog_open(true);
        self.focus_network_dialog();
    }

    fn focus_network_dialog(&self) {
        // 弹窗刚变可见时输入框还无法聚焦，等一帧。
        let ui = self.ui.as_weak();
        slint::Timer::single_shot(std::time::Duration::from_millis(16), move || {
            if let Some(ui) = ui.upgrade() {
                ui.invoke_focus_network_dialog();
            }
        });
    }

    fn close_network_dialog(&mut self) {
        self.network_edit_target = None;
        self.ui.set_network_dialog_open(false);
        self.ui.set_network_dialog_error("".into());
    }

    /// 写入配置、刷新侧栏，并在新标签页打开。
    fn connect_network_location(&mut self) -> bool {
        let raw = self.ui.get_network_dialog_server().to_string();
        let alias = self.ui.get_network_dialog_alias().trim().to_string();
        let Some(path) = crate::config::normalize_network_path(&raw) else {
            let message = if raw.trim().is_empty() {
                self.tr("请输入服务器地址")
            } else {
                self.tr("地址应形如 \\\\服务器 或 \\\\服务器\\共享名")
            };
            self.ui.set_network_dialog_error(message.into());
            return false;
        };
        let label = if alias.is_empty() {
            crate::config::network_label_from_path(&path)
        } else {
            alias
        };
        let entry = crate::config::NetworkLocation {
            label: label.clone(),
            path: path.clone(),
        };
        // 编辑模式下地址可能改了：先拿掉旧那条，再把新的插回原位置。
        let edit_target = self.network_edit_target.take();
        let insert_at = edit_target.as_ref().and_then(|old| {
            let index = self
                .config
                .network_locations
                .iter()
                .position(|n| n.path.eq_ignore_ascii_case(old))?;
            self.config.network_locations.remove(index);
            Some(index)
        });
        match self
            .config
            .network_locations
            .iter_mut()
            .find(|existing| existing.path.eq_ignore_ascii_case(&path))
        {
            Some(existing) => *existing = entry,
            None => match insert_at {
                Some(index) => self
                    .config
                    .network_locations
                    .insert(index.min(self.config.network_locations.len()), entry),
                None => self.config.network_locations.push(entry),
            },
        }
        self.config.save();
        self.close_network_dialog();
        self.sidebar_fingerprint = 0;
        self.sync_sidebar();

        let sort = self.state.active().active().sort;
        self.state.new_tab(Some(PathBuf::from(&path)), sort);
        self.sync_active_tab();
        self.reload_active();
        self.ui.invoke_scroll_tabs_to_end();
        logln!("[net] 添加网络位置 {path}（别名 {label}）");
        true
    }

    /// 取消固定：只从侧边栏移除，不动磁盘上的共享。
    fn unpin_network_location(&mut self, path: &str) {
        let before = self.config.network_locations.len();
        self.config
            .network_locations
            .retain(|n| !n.path.eq_ignore_ascii_case(path));
        if self.config.network_locations.len() == before {
            return;
        }
        self.config.save();
        self.sidebar_fingerprint = 0;
        self.sync_sidebar();
        self.sync_sidebar_active();
        logln!("[net] 取消固定网络位置 {path}");
    }

    /// 侧栏网络位置拖拽排序（phase：0 按下 / 1 拖动 / 2 松开）。
    /// 只在**用户添加过的**条目之间重排，系统检测出的映射盘不参与。
    fn sidebar_network_drag(&mut self, index: usize, phase: i32, y_in_row: f32) {
        let metrics = self.sidebar_row_metrics();
        let Some(&(row_top, _)) = metrics.get(index) else {
            return;
        };
        let pointer_y = row_top + y_in_row;
        let model = self.ui.get_sidebar_items();

        // 参与排序的行（用户添加过的网络位置）及其 y 区间。
        let rows: Vec<usize> = (0..model.row_count())
            .filter(|i| {
                model
                    .row_data(*i)
                    .map(|row| {
                        row.kind == "network"
                            && self
                                .config
                                .network_locations
                                .iter()
                                .any(|item| {
                                    item.path.eq_ignore_ascii_case(
                                        row.id.strip_prefix("net:").unwrap_or(""),
                                    )
                                })
                    })
                    .unwrap_or(false)
            })
            .collect();

        match phase {
            0 => {
                if rows.contains(&index) {
                    self.network_drag = Some((index, pointer_y, false));
                }
            }
            1 => {
                let Some((from, start_y, moved)) = self.network_drag else {
                    return;
                };
                if from != index {
                    return;
                }
                let moved = moved || (pointer_y - start_y).abs() > 4.0;
                self.network_drag = Some((from, start_y, moved));
                if !moved {
                    return;
                }
                // 一移动就置位：clicked 有可能比 up 事件先到。
                self.suppress_sidebar_click = Some(std::time::Instant::now());
                let (_, indicator_y) =
                    Self::network_drop_slot(&metrics, &rows, from, pointer_y);
                self.ui.set_sidebar_drop_y(indicator_y);
                self.ui.set_sidebar_drop_visible(true);
            }
            2 => {
                self.ui.set_sidebar_drop_visible(false);
                let Some((from, _, moved)) = self.network_drag.take() else {
                    return;
                };
                if from != index || !moved {
                    return;
                }
                self.suppress_sidebar_click = Some(std::time::Instant::now());
                let Some(row) = model.row_data(from) else { return };
                let path = row.id.strip_prefix("net:").unwrap_or("");
                let Some(from_slot) = self
                    .config
                    .network_locations
                    .iter()
                    .position(|item| item.path.eq_ignore_ascii_case(path))
                else {
                    return;
                };
                let (slot, _) = Self::network_drop_slot(&metrics, &rows, from, pointer_y);
                crate::logv!(
                    "[sidebar] net drop from={from} slot={slot} pointer_y={pointer_y:.1} rows={rows:?}"
                );
                let entry = self.config.network_locations.remove(from_slot);
                self.config
                    .network_locations
                    .insert(slot.min(self.config.network_locations.len()), entry);
                self.config.save();
                self.sidebar_fingerprint = 0;
                self.sync_sidebar();
            }
            _ => {}
        }
    }

    /// 侧栏「快速访问」拖拽排序（含回收站；phase 与网络位置一致）。
    /// 落点计算复用 `network_drop_slot`，顺序直接落到 `config.quick_items` 并保存。
    fn sidebar_quick_drag(&mut self, index: usize, phase: i32, y_in_row: f32) {
        let metrics = self.sidebar_row_metrics();
        let Some(&(row_top, _)) = metrics.get(index) else {
            return;
        };
        let pointer_y = row_top + y_in_row;
        let model = self.ui.get_sidebar_items();
        let rows: Vec<usize> = (0..model.row_count())
            .filter(|i| {
                model
                    .row_data(*i)
                    .map(|row| {
                        row.kind == "quick"
                            && self.config.quick_items.iter().any(|q| {
                                q.id == row.id.strip_prefix("quick:").unwrap_or("")
                            })
                    })
                    .unwrap_or(false)
            })
            .collect();

        match phase {
            0 => {
                if rows.contains(&index) {
                    self.quick_drag = Some((index, pointer_y, false));
                }
            }
            1 => {
                let Some((from, start_y, moved)) = self.quick_drag else {
                    return;
                };
                if from != index {
                    return;
                }
                let moved = moved || (pointer_y - start_y).abs() > 4.0;
                self.quick_drag = Some((from, start_y, moved));
                if !moved {
                    return;
                }
                self.suppress_sidebar_click = Some(std::time::Instant::now());
                let (_, indicator_y) = Self::network_drop_slot(&metrics, &rows, from, pointer_y);
                self.ui.set_sidebar_drop_y(indicator_y);
                self.ui.set_sidebar_drop_visible(true);
            }
            2 => {
                self.ui.set_sidebar_drop_visible(false);
                let Some((from, _, moved)) = self.quick_drag.take() else {
                    return;
                };
                if from != index || !moved {
                    return;
                }
                self.suppress_sidebar_click = Some(std::time::Instant::now());
                let Some(row) = model.row_data(from) else { return };
                let key = row.id.strip_prefix("quick:").unwrap_or("").to_string();
                let Some(from_slot) = self.config.quick_items.iter().position(|q| q.id == key) else {
                    return;
                };
                let (slot, _) = Self::network_drop_slot(&metrics, &rows, from, pointer_y);
                // rows 只含当前显示的行；用落点对应的 id 反查配置里的插入位置，
                // 避免某个系统目录因路径不存在被跳过时槽位错位。
                let others: Vec<String> = rows
                    .iter()
                    .copied()
                    .filter(|i| *i != from)
                    .filter_map(|i| {
                        model.row_data(i).map(|r| {
                            r.id.strip_prefix("quick:").unwrap_or("").to_string()
                        })
                    })
                    .collect();
                let entry = self.config.quick_items.remove(from_slot);
                let insert_at = if let Some(target) = others.get(slot) {
                    self.config
                        .quick_items
                        .iter()
                        .position(|q| &q.id == target)
                        .unwrap_or(self.config.quick_items.len())
                } else {
                    others
                        .last()
                        .and_then(|last| {
                            self.config.quick_items.iter().position(|q| &q.id == last)
                        })
                        .map(|p| p + 1)
                        .unwrap_or(self.config.quick_items.len())
                };
                self.config
                    .quick_items
                    .insert(insert_at.min(self.config.quick_items.len()), entry);
                self.config.save();
                self.sidebar_fingerprint = 0;
                self.sync_sidebar();
            }
            _ => {}
        }
    }

    /// 顶部标签拖拽插入排序（phase：0 按下 / 1 拖动 / 2 松开，x 为行内局部横坐标）。
    /// 拖动期间只更新 Slint 的视觉状态，不重建 tabs 模型，落点线的滑动、
    /// 被拖标签淡出、目标标签高亮才能连续播放；松手才真正重排。
    fn tab_drag(&mut self, index: usize, phase: i32, x_in_row: f32) {
        let n = self.state.tabs.len();
        if index >= n {
            return;
        }
        // 标签宽 150px + 间距 3px。mouse-x 是相对行元素的局部坐标，
        // 行本身不移动，所以无需补偿 Flickable 的 content-x。
        let pointer_x = index as f32 * 153.0 + x_in_row;
        let slot_of = |px: f32| -> usize {
            ((px / 153.0).floor() as isize)
                .clamp(0, n.saturating_sub(1) as isize) as usize
        };

        match phase {
            0 => {
                self.tab_drag = Some((index, pointer_x, false));
            }
            1 => {
                let Some((from, start_x, moved)) = self.tab_drag else {
                    return;
                };
                if from != index {
                    return;
                }
                let moved = moved || (pointer_x - start_x).abs() > 4.0;
                self.tab_drag = Some((from, start_x, moved));
                if !moved {
                    return;
                }
                // 松手补发的 clicked 有可能早于 up，一移动就置位。
                self.suppress_tab_click = Some(std::time::Instant::now());
                self.ui.set_tab_drag_index(from as i32);
                self.ui.set_tab_drop_slot(slot_of(pointer_x) as i32);
                self.ui.set_tab_drop_visible(true);
            }
            2 => {
                let Some((from, _, moved)) = self.tab_drag.take() else {
                    return;
                };
                self.ui.set_tab_drop_visible(false);
                self.ui.set_tab_drag_index(-1);
                self.ui.set_tab_drop_slot(-1);
                if from != index || !moved {
                    return;
                }
                self.suppress_tab_click = Some(std::time::Instant::now());
                let to = slot_of(pointer_x);
                if to == from || to >= self.state.tabs.len() {
                    return;
                }
                // 拖拽插入：先摘出被拖标签，再插到目标槽位，其余标签依次顺移。
                // 不是两两对调——[A,B,C,D] 把 A 拖到 C 的位置应得到 [B,C,A,D]。
                let active_id = self.state.tabs[self.state.active_tab].id;
                let entry = self.state.tabs.remove(from);
                let at = to.min(self.state.tabs.len());
                self.state.tabs.insert(at, entry);
                // active_tab 是索引：记住稳定 id，重排后按 id 反查，避免指错标签。
                self.state.active_tab = self
                    .state
                    .tab_by_id(active_id)
                    .unwrap_or(self.state.active_tab);
                self.sync_tabs();
            }
            _ => {}
        }
    }

    /// 计算拖拽落点：参照项中「垂直中心在指针上方」的数量即插入槽位；
    /// 返回 (槽位, 指示线 y)。落点的判据是**每一行自己的中心**，
    /// 不再用「下一行顶部」推中点（那样最后一行会被撑到下个分区或列表末尾）。
    fn network_drop_slot(
        metrics: &[(f32, f32)],
        rows: &[usize],
        from: usize,
        pointer_y: f32,
    ) -> (usize, f32) {
        let center = |i: usize| -> f32 {
            metrics
                .get(i)
                .map(|(top, height)| top + height / 2.0)
                .unwrap_or(pointer_y)
        };
        let others: Vec<usize> = rows.iter().copied().filter(|i| *i != from).collect();
        let slot = others.iter().filter(|i| center(**i) < pointer_y).count();
        let indicator = match others.get(slot) {
            Some(i) => metrics.get(*i).map(|(top, _)| *top).unwrap_or(pointer_y),
            None => others
                .last()
                .and_then(|i| metrics.get(*i))
                .map(|(top, height)| top + height + 2.0)
                .unwrap_or(pointer_y),
        };
        (slot, indicator)
    }

    /// 列头点击：切换排序键/方向。
    fn header_clicked(&mut self, key: &str) {
        let new_key = match key {
            "name" => SortKey::Name,
            "size" => SortKey::Size,
            "modified" => SortKey::Modified,
            "type" => SortKey::Type,
            "path" => SortKey::Path,
            // 「此电脑」视图第一列表头是「盘符」。
            "drive" => SortKey::Drive,
            _ => return,
        };
        let tab = self.state.active();
        let current_sort = tab.active().sort;
        if current_sort.key == new_key {
            self.apply_sort(new_key, !current_sort.ascending);
        } else {
            self.apply_sort(new_key, true);
        }
    }

    /// 提交重命名。
    fn commit_rename(&mut self, path: &Path, new_name: &str) {
        // 正常交互路径（回车/失焦/点空白都会走到），降为详细级；异常分支仍留在 logln。
        logv!(
            "[rename] commit_rename path={:?} new_name={}",
            path, new_name
        );

        // 核心修复：防重复提交。输入框的 accepted 与 changed has-focus 可能先后都触发
        // （回车后 rename-path 被清空 → 输入框销毁 → 失焦），点击别处也可能重入。
        // 首次提交后 Rust 已把 rename-path 清空，所以这里用“当前 rename_path 是否仍等于
        // 目标路径”做幂等守门，避免第二次对已不存在的旧名 rename 报错。
        let current = self.ui.get_rename_path();
        let target = path.to_string_lossy();
        if current.as_str() != target.as_ref() {
            logv!(
                "[rename] commit ignored (already handled) path={:?} current={}",
                path, current
            );
            return;
        }

        self.ui.set_rename_path(SharedString::default());
        // 重命名结束，展开层要回到“单选 / 无”的状态。
        self.sync_expand_targets();

        // 核心修复：不管输入的名字是否为空、是否包含非法字符、是否重名、是否重命名成功，
        // 只要触发了 commit 就代表退出重命名态，必须立刻释放拖拽锁。
        // 否则空名/非法名/重名等提前 return 的分支会漏掉解锁，锁残留 → 滚轮冻结。
        self.drag_pending = None;
        self.ui.invoke_set_drag_armed(false);

        // 超长名字截断到系统上限（截断后可能留下尾部空格，一并去掉）。
        // 正常交互不会再传超长文本——输入超限会直接退出重命名，这里只是兜底。
        let clamped = clamp_file_name(new_name.trim());
        let new_name = clamped.trim_end();
        if new_name.is_empty() {
            return;
        }
        if new_name == "." || new_name == ".." || new_name.contains(['\\', '/', ':', '*', '?', '"', '<', '>', '|']) {
            self.set_status_job(self.tr("名称包含非法字符"));
            return;
        }
        let Some(parent) = path.parent() else {
            return;
        };
        let new_path = parent.join(new_name);
        if new_path == path {
            return;
        }
        if new_path.exists() {
            self.set_status_job(self.tr("同名文件已存在"));
            return;
        }
        match std::fs::rename(path, &new_path) {
            Ok(()) => {
                // 记下该条目当前的显示位置：新列表到达后把它钉回原位，
                // 避免“改名即重排”造成行跳动（与资源管理器一致）。
                let pin_index = {
                    let pane = self.state.active().active();
                    pane.display
                        .iter()
                        .position(|&ei| pane.entries.get(ei).is_some_and(|e| e.path == *path))
                };
                // 更新选择/暂存/剪切集合中的旧路径。
                let tab = self.state.active_mut();
                if tab.active_mut().selection.remove(path) {
                    tab.active_mut().selection.insert(new_path.clone());
                }
                self.state.cut_paths.remove(path);
                if let Some(idx) = pin_index {
                    logv!("[rename] pin new={:?} at display index {}", new_path, idx);
                    self.rename_pin = Some((
                        parent.to_path_buf(),
                        new_path.clone(),
                        idx,
                        Instant::now() + Duration::from_secs(3),
                    ));
                }
                self.reload_active();
            }
            Err(e) => self.set_status_job(self.trf("重命名失败：{}", &[e.to_string()])),
        }
    }

    /// 预测复制/移动完成后在目标目录里出现的新路径，让列表把它们滚进可视区。
    /// Shell 的同名冲突可能改名，预测不准时列举结果里找不到 → 不会滚动，不会误滚。
    fn expect_new_entries(&mut self, sources: &[PathBuf], dest_dir: &Path) {
        self.scroll_to_paths = sources
            .iter()
            .filter_map(|s| s.file_name().map(|n| dest_dir.join(n)))
            .collect();
        // 粘贴/移动/还原完成后选中新条目（与资源管理器一致）。
        self.scroll_select = !self.scroll_to_paths.is_empty();
    }

    /// 暂存盘内容复制/移动到当前目录。
    fn staging_transfer(&mut self, is_move: bool) {
        let entries = self.state.staging_present();
        if entries.is_empty() {
            self.set_status_job(self.tr("暂存盘为空（或文件已不存在）"));
            self.sync_staging();
            return;
        }
        let Some(dest) = self.state.active().active().path.clone() else {
            self.set_status_job(self.tr("请先进入目标文件夹"));
            return;
        };

        // 回收站是虚拟目录，不能当普通目标目录交给 Shell（会得到错误码 123）。
        // 复制没有语义，直接给友好提示；「移动」按资源管理器语义改为删除到回收站。
        if dest == fs::drives::recycle_virtual_path() {
            if !is_move {
                self.set_status_job(
                    self.tr("回收站不支持复制到此处；如需把这些文件放入回收站，请点「移动」"),
                );
                return;
            }
            let paths: Vec<PathBuf> = entries.iter().map(|e| e.path.clone()).collect();
            if self.submit_job(JobKind::Delete { paths, to_trash: true }) {
                // 删除成功后才清空：用户取消确认框或删除失败时保留暂存盘内容以便重试。
                self.clear_staging_on_delete_success = true;
            }
            return;
        }

        let sources: Vec<PathBuf> = entries.iter().map(|e| e.path.clone()).collect();
        self.expect_new_entries(&sources, &dest);
        let kind = if is_move {
            JobKind::Move {
                sources,
                dest_dir: dest,
            }
        } else {
            JobKind::Copy {
                sources,
                dest_dir: dest,
            }
        };
        self.submit_job(kind);
        if is_move {
            self.state.staging.clear();
            self.sync_staging();
        }
    }

    /// 锁住当前滚动位置，供重命名期间拦下 Slint 的自动 reveal（详见字段注释）。
    fn arm_rename_scroll_guard(&mut self) {
        let (s0, s1) = {
            let tab = self.state.active();
            (
                tab.panes.first().map(|p| p.scroll_y).unwrap_or(0.0),
                tab.panes.get(1).map(|p| p.scroll_y).unwrap_or(0.0),
            )
        };
        self.rename_scroll_guard =
            Some((s0, s1, Instant::now() + Duration::from_millis(250)));
    }

    /// 判断并拦下重命名期间“非用户发起”的滚动事件（Slint 自动 reveal 造成）。
    ///
    /// 返回 true 表示已丢弃该事件（并把视图推回原位置），调用方应直接 return。
    /// 用户真的在滚轮/拖滚动条时，`on_file_scroll_user_input` 已经把 guard 清掉，
    /// 所以这里不会误杀。
    fn reject_spurious_rename_scroll(&mut self, pane: usize, offset: f32) -> bool {
        let Some((keep0, keep1, until)) = self.rename_scroll_guard else {
            return false;
        };
        if Instant::now() >= until {
            self.rename_scroll_guard = None;
            return false;
        }
        let keep = if pane == 0 { keep0 } else { keep1 };
        if (offset - keep).abs() <= 1.0 {
            // 回显（我们自己推的值又打回来）——归一化为预期值，不算异常。
            return false;
        }
        logln!(
            "[rename] !! drop spurious scroll pane={} {:.1} -> keep {:.1}",
            pane, offset, keep
        );
        push_scroll(&self.ui, pane, keep);
        true
    }

    /// 开始重命名选中项（仅单项）。
    /// 进入重命名态（所有入口统一走这里）。
    /// 除了设置目标路径，还要把当前文件名填进 `rename-text`，
    /// 因为图标视图的重命名输入框是列表最上层的浮层，不在行内，拿不到 `row.name`。
    fn begin_rename(&mut self, path: &Path) {
        let name = path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        // 三个入口（新建文件夹、F2、右键重命名）都作用于当前聚焦面板，
        // 记下栏号，让只有该栏的 FileList 实例进入重命名态。
        self.ui
            .set_rename_pane(self.state.active().active_pane as i32);
        self.ui
            .set_rename_path(path.to_string_lossy().into_owned().into());
        self.ui.set_rename_text(name.as_str().into());
        // 只选主名，不选扩展名。-1 表示目录/无扩展名时全选。
        // set-selection-offsets 用 UTF-8 字节偏移，rfind('.') 正是字节索引；
        // 开头点（.gitignore）和末尾点都不算扩展名，与 clamp_file_name 同口径。
        let stem_end = if path.is_dir() {
            -1
        } else {
            match name.rfind('.') {
                Some(i) if i > 0 && i + 1 < name.len() => i as i32,
                _ => -1,
            }
        };
        self.ui.set_rename_select_end(stem_end);
        self.arm_rename_scroll_guard();
        self.sync_expand_targets();
    }

    fn start_rename(&mut self) {
        // 核心修复：强行清空拖拽上膛锁，防止在行上按下时残留 drag-armed，
        // 导致重命名弹出输入框接管鼠标后 TouchArea 收不到 up 事件，锁永久残留、滚轮冻结。
        self.drag_pending = None;
        self.ui.invoke_set_drag_armed(false);

        let paths = self.state.active().active().selected_paths();
        if paths.len() == 1 {
            logv!("[rename] start_rename for {:?}", paths[0]);
            let path = paths[0].clone();
            self.begin_rename(&path);
        }
    }

    /// 延迟触发重命名：单击文件名先不起重命名，留出双击窗口。
    /// 若窗口内再来一次按下（判定为双击打开），`cancel_rename_defer` 会取消本定时器；
    /// 否则到点才真正进入重命名——这样「单击选中后双击名称」不再被抢成重命名。
    fn schedule_rename(&mut self, path: PathBuf) {
        let Some(weak) = self.self_weak.clone() else {
            // 兜底：弱引用未就绪时直接重命名（不应发生）。
            self.begin_rename(&path);
            return;
        };
        self.rename_defer_timer.stop();
        self.rename_defer_timer.start(
            TimerMode::SingleShot,
            Duration::from_millis(effective_double_click_ms() + 50),
            move || {
                let Some(app) = weak.upgrade() else { return };
                // try_borrow_mut：避免与同在主线程的其它 UI 回调形成 RefCell 嵌套借入而 panic；
                // 借不到就放弃本次重命名（用户再点一次即可），绝不 panic。
                let Ok(mut app) = app.try_borrow_mut() else { return };
                // 仅当该路径仍是唯一选中项时才重命名，避免窗口期内切换了选择导致重命名错对象。
                let sel = app.state.active().active().selected_paths();
                if sel.len() == 1 && sel[0] == path {
                    logv!("[rename] deferred rename firing for {:?}", path);
                    app.begin_rename(&path);
                }
            },
        );
    }

    /// 取消挂起的延迟重命名（双击打开时调用）。
    fn cancel_rename_defer(&mut self) {
        self.rename_defer_timer.stop();
    }

    /// 打开选中项。
    fn open_selection(&mut self) {
        let paths = self.state.active().active().selected_paths();
        if let Some(path) = paths.first() {
            let path = path.clone();
            self.open_path(&path);
        }
    }

    /// 删除选中项。
    fn delete_selection(&mut self, to_trash: bool) {
        let paths = self.state.active().active().selected_paths();
        if !paths.is_empty() {
            // 低频、关键时刻：Shell 确认框会在此期间拦截打开/删除类按键。
            logln!(
                "[delete] submit n={} to_trash={}",
                paths.len(),
                to_trash
            );
            self.submit_job(JobKind::Delete { paths, to_trash });
        }
    }

    /// 全选当前显示项。
    fn select_all(&mut self) {
        let selected: HashSet<PathBuf> = {
            let tab = self.state.active();
            tab.active().display
                .iter()
                .filter_map(|&i| tab.active().entries.get(i).map(|e| e.path.clone()))
                .collect()
        };
        self.state.active_mut().active_mut().selection = selected;
        self.visible_cache = None;
        self.refresh_visible_rows();
        self.sync_status();
    }
}

// ---------------------------------------------------------------------------
// 动作执行
// ---------------------------------------------------------------------------

impl App {
    /// 执行右键菜单动作。
    fn run_menu_action(&mut self, id: &str) {
        // 0. 侧栏网络位置的编辑/删除（不依赖文件选择）
        if let Some(rest) = id.strip_prefix("netloc:") {
            if let Some(path) = rest.strip_prefix("edit:") {
                let path = path.to_string();
                self.open_network_edit_dialog(&path);
            } else if let Some(path) = rest.strip_prefix("unpin:") {
                let path = path.to_string();
                self.unpin_network_location(&path);
            }
            return;
        }

        // 0.1 侧栏「快速访问」取消固定（不依赖文件选择）。
        if let Some(key) = id.strip_prefix("quick:unpin:") {
            let key = key.to_string();
            self.unpin_quick_item(&key);
            return;
        }

        // 1. 视图模式切换（优先执行！不依赖选中的文件 target，杜绝工具栏调用被拦截）
        if let Some(mode) = id.strip_prefix("view:") {
            self.set_view_mode(mode);
            return;
        }

        // 1.1 排序方式切换
        if let Some(sort_mode) = id.strip_prefix("sort:") {
            let tab = self.state.active().active();
            let mut key = tab.sort.key;
            let mut asc = tab.sort.ascending;
            match sort_mode {
                "name" => { key = SortKey::Name; asc = true; }
                "drive" => { key = SortKey::Drive; asc = true; }
                "modified" => { key = SortKey::Modified; asc = false; } // 修改时间默认最新在前
                "type" => { key = SortKey::Type; asc = true; }
                "size" => { key = SortKey::Size; asc = false; } // 大小默认最大在前
                "asc" => { asc = true; }
                "desc" => { asc = false; }
                _ => {}
            }
            self.apply_sort(key, asc);
            return;
        }

        // 2. 需要操作目标的常规文件动作
        let Some(target) = self.menu_scope.clone() else {
            return;
        };
        // actions.toml 驱动的动作。
        // 标准命令（打开 / 打开方式 / 重命名 / 删除 / 属性 / 发送到）由系统菜单提供，
        // 不再有 `act:builtin.*` 的菜单分支；它们的实现函数仍在快捷键路径上使用。
        if let Some(action_id) = id.strip_prefix("act:") {
            match action_id {
                "builtin.copy" => {
                    self.copy_selection(false);
                    return;
                }
                "builtin.move" => {
                    self.copy_selection(true);
                    return;
                }
                "builtin.stage" => {
                    if let MenuScope::Selection(paths) = &target {
                        let added = self.state.stage_paths(paths);
                        self.sync_staging();
                        self.set_status_job(self.trf("已加入暂存盘 {} 项", &[added.to_string()]));
                    }
                    return;
                }
                "builtin.terminal" => {
                    let dir = match &target {
                        MenuScope::Selection(paths) => paths
                            .first()
                            .and_then(|p| if p.is_dir() { Some(p.clone()) } else { p.parent().map(|d| d.to_path_buf()) }),
                        MenuScope::Background => self.state.active().active().path.clone(),
                    };
                    if let Some(dir) = dir {
                        if let Err(e) = ops::open_terminal(&dir) {
                            self.set_status_job(e);
                        }
                    }
                    return;
                }
                "builtin.pin_quick" => {
                    let dirs: Vec<PathBuf> = match &target {
                        MenuScope::Selection(paths) => paths.clone(),
                        MenuScope::Background => self
                            .state
                            .active()
                            .active()
                            .path
                            .clone()
                            .into_iter()
                            .collect(),
                    };
                    let added = self.pin_quick_folders(&dirs);
                    self.set_status_job(if added > 0 {
                        self.trf("已固定 {} 个文件夹到快速访问", &[added.to_string()])
                    } else {
                        self.tr("文件夹已在快速访问中").to_string()
                    });
                    return;
                }
                _ => {
                    let entry = self.actions.iter().find(|a| a.id == action_id).cloned();
                    if let Some(entry) = entry {
                        let targets = match &target {
                            MenuScope::Selection(p) => p.clone(),
                            _ => Vec::new(),
                        };
                        match crate::ops::actions::run_action(&entry, &targets) {
                            Ok(()) => {
                                self.set_status_job(format!(
                                    "{}：{}",
                                    self.tr("已执行"),
                                    self.tr(&entry.label)
                                ));
                                self.reload_active();
                            }
                            Err(e) => self.set_status_job(self.trf("执行失败：{}", &[e.to_string()])),
                        }
                    }
                    return;
                }
            }
        }
        if id == CMD_MANAGE_ACTIONS {
            // 显式管理动作：force_subpage = true，聚焦并切换到本地右键菜单页
            self.open_settings_tab(crate::state::SettingsSubpage::ContextMenu, true);
            return;
        }
        // 回收站视图动作。
        match id {
            "recycle-restore" => {
                if let MenuScope::Selection(paths) = &target {
                    let mut restored = 0;
                    for p in paths {
                        restored += fs::drives::restore_recycle_entry(p);
                    }
                    if restored > 0 {
                        self.set_status_job(
                            self.trf("已还原 {} 项到原位置", &[restored.to_string()]),
                        );
                    } else {
                        self.set_status_job(self.tr("还原失败"));
                    }
                    self.reload_active();
                    self.sidebar_fingerprint = 0;
                }
            }
            // 与 Ctrl+X 同一条路径：写入 Rove 内部剪贴板并置半透明剪切态，
            // 粘贴到普通目录时按唯一 Shell id 还原后再移动到目标目录。
            "recycle-cut" => self.copy_selection(true),
            "recycle-purge" => {
                if let MenuScope::Selection(paths) = &target {
                    let n = fs::drives::purge_recycle_entries(paths);
                    self.set_status_job(if n > 0 {
                        self.trf("已彻底删除 {} 项", &[n.to_string()])
                    } else {
                        self.tr("删除失败").to_string()
                    });
                    self.reload_active();
                    self.sidebar_fingerprint = 0;
                }
            }
            "recycle-empty" => {
                if fs::drives::empty_recycle_bin() {
                    self.set_status_job(self.tr("回收站已清空"));
                } else {
                    self.set_status_job(self.tr("清空回收站失败"));
                }
                self.reload_active();
                self.sidebar_fingerprint = 0;
            }
            "recycle-properties" => {
                if let MenuScope::Selection(paths) = &target {
                    // Shell 的 properties 动词一次只处理一个条目，多选时显示第一项。
                    if let Some(p) = paths.first() {
                        if let Err(e) = crate::sys::shell::show_properties(p) {
                            self.set_status_job(self.trf("无法显示属性：{}", &[e.to_string()]));
                        }
                    }
                }
            }
            _ => {}
        }
        if id.starts_with("recycle-") {
            return;
        }
        match id {
            CMD_PASTE => self.paste_clipboard(),
            CMD_TERMINAL => {
                let dir = match &target {
                    MenuScope::Selection(paths) => paths
                        .first()
                        .and_then(|p| if p.is_dir() { Some(p.clone()) } else { p.parent().map(|d| d.to_path_buf()) }),
                    MenuScope::Background => self.state.active().active().path.clone(),
                };
                if let Some(dir) = dir {
                    if let Err(e) = ops::open_terminal(&dir) {
                        self.set_status_job(e);
                    }
                }
            }
            CMD_OPEN_LOCATION => {
                if let MenuScope::Selection(paths) = &target {
                    // 多选时优先定位「鼠标右键所在的那一行」，而不是选中集里
                    // 按显示顺序排第一的那个（否则定位到別的文件去了）。
                    let chosen = self
                        .menu_anchor_path
                        .clone()
                        .filter(|a| paths.contains(a))
                        .or_else(|| paths.first().cloned());
                    if let Some(p) = chosen {
                        if let Some(dir) = p.parent().map(|d| d.to_path_buf()) {
                            self.navigate_active(Some(dir.clone()));
                            // 导航会清空选择；把目标文件重新选中，列表加载完就能看到它。
                            let tab = self.state.active_mut().active_mut();
                            tab.selection.clear();
                            tab.selection.insert(p.clone());
                            tab.anchor = Some(p.clone());
                            // 让列表加载完成后滚动到目标文件（否则可能停在视口外看不到）。
                            // scroll_select=true：列举结果回来时把目标重新设为唯一选中项，
                            // 并换算成显示索引做最小位移滚动。
                            self.scroll_to_paths = vec![p.clone()];
                            self.scroll_select = true;
                            self.visible_cache = None;
                            self.refresh_visible_rows();
                            self.sync_status();
                            logln!(
                                "[open-location] target={} dir={} scroll_armed=true",
                                p.display(),
                                dir.display()
                            );
                        }
                    }
                }
            }
            #[cfg(windows)]
            CMD_REVEAL_EXPLORER => {
                if let MenuScope::Selection(paths) = &target {
                    // 同上：多选时优先右键对应文件。
                    let chosen = self
                        .menu_anchor_path
                        .clone()
                        .filter(|a| paths.contains(a))
                        .or_else(|| paths.first().cloned());
                    if let Some(p) = chosen {
                        let arg = format!("/select,{}", p.to_string_lossy());
                        if let Err(e) = std::process::Command::new("explorer")
                            .arg(arg)
                            .spawn()
                        {
                            self.set_status_job(self.trf("打开路径失败：{}", &[e.to_string()]));
                        }
                    }
                }
            }
            CMD_NEW_FOLDER => self.create_folder(),
            CMD_REFRESH => self.user_refresh_active(),
            CMD_SELECT_ALL => {
                let tab = self.state.active_mut().active_mut();
                let all: Vec<PathBuf> = tab.display
                    .iter()
                    .filter_map(|&i| tab.entries.get(i).map(|e| e.path.clone()))
                    .collect();
                tab.selection = all.into_iter().collect();
                self.refresh_visible_rows();
                self.sync_status();
            }
            _ => {}
        }
    }

    /// 打开文件/目录。
    fn open_path(&mut self, path: &Path) {
        let is_dir = std::fs::metadata(path).map(|m| m.is_dir()).unwrap_or(false);
        if is_dir {
            self.navigate_active(Some(path.to_path_buf()));
        } else if let Err(e) = open::that_detached(path) {
            self.set_status_job(self.trf("打开失败：{}", &[e.to_string()]));
        }
    }

    /// 激活行（双击 / Enter）：普通路径按类型打开；
    /// 回收站视图下还原到原位置。
    fn activate_row(&mut self, index: usize) {
        let Some(entry) = self.state.active().active().display_entry(index).cloned() else {
            return;
        };
        if self.state.active().active().path.as_deref() == Some(fs::drives::recycle_virtual_path().as_path()) {
            let n = fs::drives::restore_recycle_entry(&entry.path);
            if n > 0 {
                self.set_status_job(self.trf("已还原 {} 项到原位置", &[n.to_string()]));
                self.reload_active();
                self.sidebar_fingerprint = 0;
            } else {
                self.set_status_job(self.tr("还原失败"));
            }
            return;
        }
        self.open_path(&entry.path);
    }

    /// 当前可粘贴的剪贴板状态：`(路径列表, 是否为移动)`。
    ///
    /// 系统剪贴板是唯一权威：资源管理器等外部程序随时会覆盖它，进程内缓存
    /// 只在「回收站内剪切」这类不写 CF_HDROP 的场景兜底。绝不能把系统值与
    /// 陈旧的缓存值做 OR，否则外部复制会被误判成剪切（把源文件移走）。
    fn paste_state(&self) -> (Vec<PathBuf>, bool) {
        let files = clipboard::read_files();
        if !files.paths.is_empty() {
            return (files.paths, files.operation == "move");
        }
        if self.state.clipboard_from_recycle && !self.state.clipboard_paths.is_empty() {
            return (
                self.state.clipboard_paths.clone(),
                self.state.clipboard_operation == "move",
            );
        }
        // 系统里没有文件：常规缓存可能已被外部覆盖或清空，不能拿旧路径再粘一次。
        (Vec::new(), false)
    }

    /// 复制/剪切当前选择到系统剪贴板。
    fn copy_selection(&mut self, is_move: bool) {
        let paths = self.state.active().active().selected_paths();
        if paths.is_empty() {
            return;
        }

        let is_recycle =
            self.state.active().active().path.as_deref() == Some(fs::drives::recycle_virtual_path().as_path());
        if is_recycle {
            // 回收站条目的 path 是唯一 Shell id，物理文件在 $Recycle.Bin 下，
            // 不向系统 CF_HDROP 写入它（避免 Shell 报错），由 Rove 内部接管。
            // 内部剪贴板同样存唯一 id：还原时按 id 精确命中，不会把同名
            // 条目一起还原；需要原位置时再按需查询。
            self.state.clipboard_from_recycle = true;
            self.state.clipboard_paths = paths.clone();
            self.state.clipboard_operation = "move".into();
            self.state.cut_paths = paths.into_iter().collect();
            self.visible_cache = None;
            self.refresh_visible_rows();
            self.set_status_job(self.trf(
                "已剪切 {} 个回收站项目（粘贴即可移动到目标文件夹）",
                &[self.state.clipboard_paths.len().to_string()],
            ));
            return;
        }

        // 常规目录正常写入系统剪贴板
        self.state.clipboard_from_recycle = false;
        match clipboard::set_files(&paths, is_move) {
            Ok(()) => {
                self.state.clipboard_paths = paths.clone();
                self.state.clipboard_operation = if is_move { "move".into() } else { "copy".into() };
                if is_move {
                    self.state.cut_paths = paths.into_iter().collect();
                } else {
                    self.state.cut_paths.clear();
                }
                self.visible_cache = None;
                self.refresh_visible_rows();
                self.set_status_job(if is_move {
                    self.trf(
                        "已剪切 {} 项",
                        &[self.state.clipboard_paths.len().to_string()],
                    )
                } else {
                    self.trf(
                        "已复制 {} 项",
                        &[self.state.clipboard_paths.len().to_string()],
                    )
                });
            }
            Err(e) => self.set_status_job(self.trf("写入剪贴板失败：{}", &[e.to_string()])),
        }
    }

    /// 粘贴剪贴板内容到当前目录（含回收站双向闭环）。
    fn paste_clipboard(&mut self) {
        let Some(dest) = self.state.active().active().path.clone() else {
            self.set_status_job("请先进入目标文件夹");
            return;
        };

        // 场景 1：在回收站视图内粘贴 -> 删除到回收站（与资源管理器语义一致）。
        if dest == fs::drives::recycle_virtual_path() {
            // 防护：回收站内部剪切的内容本就已在回收站中，不能再次删除。
            if self.state.clipboard_from_recycle {
                self.set_status_job(self.tr("这些项目已在回收站中"));
                return;
            }
            let (paths, is_move) = self.paste_state();
            if paths.is_empty() {
                return;
            }
            self.submit_job(JobKind::Delete { paths, to_trash: true });
            if is_move {
                self.state.clipboard_paths.clear();
                self.state.cut_paths.clear();
                self.visible_cache = None;
            }
            return;
        }

        // 场景 2：从回收站剪切后粘贴到常规目录 -> 先还原，再移动到目标目录。
        if self.state.clipboard_from_recycle && !self.state.clipboard_paths.is_empty() {
            let paths = std::mem::take(&mut self.state.clipboard_paths);
            self.state.clipboard_from_recycle = false;
            self.state.cut_paths.clear();
            self.visible_cache = None;

            let mut restored = Vec::new();
            for p in &paths {
                // 先记下删除前的位置，再按唯一 id 精确还原这一条；
                // 同名条目各有自己的 id，不会互相牵连。
                if let Some(original) = fs::drives::recycle_original_path(p) {
                    if fs::drives::restore_recycle_entry(p) > 0 {
                        restored.push(original);
                    }
                }
            }

            if restored.is_empty() {
                self.set_status_job(self.tr("还原失败（项目可能已被彻底删除）"));
                return;
            }

            // 还原位置与目标目录不一致的项目需要移动过去。
            let restored_paths = restored.clone();
            let to_move: Vec<PathBuf> = restored
                .into_iter()
                .filter(|p| p.parent() != Some(dest.as_path()))
                .collect();

            if !to_move.is_empty() {
                self.expect_new_entries(&to_move, &dest);
                self.submit_job(JobKind::Move {
                    sources: to_move,
                    dest_dir: dest,
                });
            } else {
                self.set_status_job(self.trf(
                    "已从回收站还原 {} 个项目到当前目录",
                    &[paths.len().to_string()],
                ));
                self.scroll_to_paths = restored_paths;
                self.scroll_select = true;
                self.reload_active();
            }
            self.sidebar_fingerprint = 0;
            return;
        }

        // 场景 3：常规目录之间粘贴。copy / move 一律以系统剪贴板为准。
        let (paths, is_move) = self.paste_state();
        if paths.is_empty() {
            return;
        }
        self.state.clipboard_paths = paths.clone();
        self.state.clipboard_operation = if is_move { "move".into() } else { "copy".into() };
        self.expect_new_entries(&paths, &dest);
        let kind = if is_move {
            JobKind::Move {
                sources: paths,
                dest_dir: dest,
            }
        } else {
            JobKind::Copy {
                sources: paths,
                dest_dir: dest,
            }
        };
        self.submit_job(kind);
    }

    /// 在当前目录新建文件夹并进入重命名。
    fn create_folder(&mut self) {
        let Some(dir) = self.state.active().active().path.clone() else {
            self.set_status_job(self.tr("请先进入一个文件夹"));
            return;
        };
        let base = dir.join("新建文件夹");
        let mut path = base.clone();
        let mut counter = 2;
        while path.exists() {
            path = dir.join(format!("新建文件夹 ({counter})"));
            counter += 1;
        }
        match std::fs::create_dir(&path) {
            Ok(()) => {
                // 核心修复：清除拖拽上膛锁，防止新建后滚动被禁用（drag-armed 为 true 时
                // Flickable 的 interactive 滚动被关，必须等下次目录刷新才重置）。
                self.drag_pending = None;
                self.ui.invoke_set_drag_armed(false);

                // 关键诊断（ROVE_LOG=1）：用户反馈「新建文件夹有概率没默认选中，
                // 但名称框可以编辑」。这条链路跨异步列举 + 行模型重建，逐环打点，
                // 回来测试时看哪一环断了即可定位。
                logln!(
                    "[new-folder] created={} scroll_select_prev={} scroll_to_paths_prev={} rename_path_prev={}",
                    path.display(),
                    self.scroll_select,
                    self.scroll_to_paths.len(),
                    self.ui.get_rename_path()
                );
                // 新条目会按当前排序插到任意位置（名称排序 + 全英文目录 → 沉到底部），
                // 所以要告诉列表：数据就位后把这条滚进可视区，否则用户得自己找。
                // 显式复位 scroll_select：它是全局状态，若上一次粘贴的 scroll_to_paths
                // 因路径改名没匹配上就不会被消费，残留到本次新建 → 把选择集换成
                // 别处、看起来「没选中」或「选中错行」。
                self.scroll_to_paths = vec![path.clone()];
                self.scroll_select = false;
                self.reload_active();
                let new_path = path.clone();
                self.begin_rename(&new_path);
                logln!(
                    "[new-folder] armed: rename_path={} scroll_to_paths={} scroll_select={}",
                    self.ui.get_rename_path(),
                    self.scroll_to_paths.len(),
                    self.scroll_select
                );
            }
            Err(e) => self.set_status_job(self.trf("创建文件夹失败：{}", &[e.to_string()])),
        }
    }

    /// 提交后台文件任务；返回是否真的提交成功（已有任务在进行时为 false）。
    fn submit_job(&mut self, kind: JobKind) -> bool {
        self.shell_op_running = true;
        self.shell_key_suppressed_logged = false;
        let ok = self.jobs.submit(kind);
        if !ok {
            self.set_status_job(self.tr("有任务正在进行，请稍候"));
        }
        ok
    }

    fn set_status_job(&self, message: impl Into<SharedString>) {
        let message = message.into();
        // 危险提示用红色：失败、名称超限、删除（含删除取消）等需要一眼看到。
        self.ui.set_status_danger(status_is_danger(message.as_str()));
        self.ui.set_status_job(message);
    }

    /// 任务完成后的处理：刷新受影响目录。
    fn on_job_finished(&mut self, summary: JobSummary) {
        self.shell_op_running = false;
        self.set_status_job(summary.message.clone());
        let affected = &summary.affected_dirs;
        let mut requests = Vec::new();

        // 遍历所有标签页的所有面板，精准找出受影响的 pane。
        for index in 0..self.state.tabs.len() {
            let tab = &mut self.state.tabs[index];
            for (pane_idx, pane) in tab.panes.iter_mut().enumerate() {
                let is_recycle = pane.path.as_deref() == Some(fs::drives::recycle_virtual_path().as_path());
                let needs = is_recycle
                    || pane.path.as_ref().is_some_and(|p| affected.iter().any(|a| a == p || p.starts_with(a)));
                if needs {
                    pane.loading = true;
                    requests.push((tab.id, pane_idx, pane.generation, pane.path.clone(), pane.sort));
                }
            }
        }

        // 统一发起重新列举请求。
        for (id, pane, gen, path, sort) in requests {
            self.request_listing(id, pane, gen, path, sort);
        }

        // 如果当前有标签页因刷新而处于 loading 状态，同步更新 UI。
        if self.state.tabs.iter().any(|t| t.active().loading) {
            self.sync_file_list();
            self.sync_status();
        }

        // 移动操作会消耗剪贴板：系统剪贴板里仍是「剪切」语义时一并清掉，
        // 否则菜单会继续显示「粘贴（移动）」，再点就是移动已不存在的源文件。
        // 内部拖拽移动不写剪切标记，不会误伤用户的剪贴板。
        if summary.outcome == crate::ops::jobs::JobOutcome::Moved {
            if clipboard::read_files().operation == "move" {
                let _ = clipboard::clear();
                self.sync_status();
            }
            self.state.clipboard_paths.clear();
            self.state.cut_paths.clear();
            self.visible_cache = None;
        }
        // 回收站下暂存盘点「移动」：只有真正删除成功才清空暂存盘，
        // 「删除已取消 / 删除失败」时保留内容，避免一次取消就丢掉整个暂存列表。
        if self.clear_staging_on_delete_success {
            self.clear_staging_on_delete_success = false;
            if summary.failed == 0 && summary.outcome == crate::ops::jobs::JobOutcome::Deleted {
                self.state.staging.clear();
            }
        }
        self.sync_staging();
    }
}

// ---------------------------------------------------------------------------
// 导航
// ---------------------------------------------------------------------------

impl App {
    /// 当前标签导航到路径（压入历史）。
    fn navigate_active(&mut self, path: Option<PathBuf>) {
        self.state.active_mut().active_mut().navigate(path);
        self.sync_after_navigate();
    }

    fn sync_after_navigate(&mut self) {
        // 导航时取消挂起的延迟重命名：导航会换目录/选择集，原路径可能不再有效，
        // 否则 400ms 后会在新页面里误弹出重命名。
        self.cancel_rename_defer();
        // 进出「此电脑」/「回收站」时，把该视图自己的排序状态装回面板，
        // 普通目录则用全局排序。必须在下发列举请求（reload_active）之前做。
        self.sync_view_sort();
        // 导航后退出地址栏编辑态。
        self.ui.set_path_editing(false);
        self.sync_tabs();
        self.sync_nav();
        self.sync_breadcrumbs();
        self.sync_sidebar_active();
        self.sync_status();
        self.ui.set_rename_path(SharedString::default());
        // 导航后的页面自带搜索词（搜索页/目录页），不要无条件清空搜索框。
        let nav_filter = self.state.active().active().filter.clone();
        let is_search_page = !nav_filter.trim().is_empty();
        self.ui.set_filter_text(nav_filter.clone().into());
        self.visible_cache = None;

        // 将内存中正确的 scroll_y 推给 UI，防止 UI 在重绘前乱跳。
        // 真正的“跨帧防污染”保护靠 pane.pending_scroll_restore：异步列表未回来、
        // 内容高度被临时置 0 时，Slint 自动 Clamp 触发的 scroll-changed(0) 回调会被
        // on_file_scroll_changed 里的 pending 检查直接忽略，不会把 scroll_y 覆盖为 0。
        let (scroll0, is_split, scroll1) = {
            let tab = self.state.active();
            (
                tab.panes.first().map(|p| p.scroll_y).unwrap_or(0.0),
                tab.is_split,
                tab.panes.get(1).map(|p| p.scroll_y).unwrap_or(0.0),
            )
        };
        push_scroll(&self.ui, 0, scroll0);
        if is_split {
            push_scroll(&self.ui, 1, scroll1);
        }

        self.sync_file_list();
        if is_search_page {
            // 历史落点是搜索页：由 after_history_jump 的 restore_search_page 重发搜索。
            // 这里绝不能 reload_active —— 列举结果回来时「此电脑」分支会
            // pane.filter.clear()，把刚恢复的搜索词冲掉（结果变少的第二个原因）。
            // 但也因此不会有列举结果来复位 loading / pending_scroll_restore，
            // jump_to 刚置的 true 会一直挂着 → 列表永远盖着「正在读取…」。
            logv!(
                "[nav] search page: skip re-listing, clearing loading flag (filter={})",
                nav_filter
            );
            let pane = self.state.active().active_pane;
            self.state.active_mut().panes[pane].loading = false;
            self.state.active_mut().panes[pane].pending_scroll_restore = false;
            self.ui.set_file_loading(false);
            self.ui.set_tab_loading(false);
        } else {
            self.reload_active();
        }
    }

    fn nav_back(&mut self) {
        let tab = self.state.active();
        if !tab.active().can_go_back() {
            return;
        }
        let index = tab.active().history_index - 1;
        let before = tab.active().path.clone();
        self.state.active_mut().active_mut().jump_to(index);
        self.after_history_jump(before);
    }

    fn nav_forward(&mut self) {
        let tab = self.state.active();
        if !tab.active().can_go_forward() {
            return;
        }
        let index = tab.active().history_index + 1;
        let before = tab.active().path.clone();
        self.state.active_mut().active_mut().jump_to(index);
        self.after_history_jump(before);
    }

    /// 前进/后退的收尾：跨目录按正常导航走；同一目录内的「搜索页 ↔ 目录页」
    /// 只重建搜索结果，不能重新列举目录（会把搜索结果冲掉）。
    fn after_history_jump(&mut self, before: Option<PathBuf>) {
        let (same_path, has_filter) = {
            let p = self.state.active().active();
            (p.path == before, !p.filter.trim().is_empty())
        };
        logv!(
            "[nav] history jump same_path={} has_filter={} path={:?} before={:?} filter={:?} gen={} dgen={}",
            same_path,
            has_filter,
            self.state.active().active().path,
            before,
            self.state.active().active().filter,
            self.state.active().active().generation,
            self.state.active().active().deep_generation
        );
        if same_path {
            self.restore_search_page(false);
        } else {
            self.sync_after_navigate();
            // 跨目录后退/前进落到「带搜索词的历史页」时，sync_after_navigate 只
            // 重新列举目录，不会重发深度/全局搜索：deep_entries 被 jump_to 清空后
            // 一直为空，搜索结果退化成当前目录的本地过滤子集（表现为结果变少）。
            // 这里补一次搜索页恢复，把 filter 对应的完整结果重新拉回来。
            if has_filter {
                logv!("[nav] history jump to a search page (cross-dir), re-running search");
                self.restore_search_page(true);
            }
        }
    }

    /// 恢复一个搜索页。`cross_dir` = 落点目录与离开时不同：此时 pane.entries
    /// 还是旧目录的内容，必须先重新列举，否则本地过滤是在错的目录上跑的。
    fn restore_search_page(&mut self, cross_dir: bool) {
        let (filter, scroll, is_root, pane_index, was_loading) = {
            let tab = self.state.active();
            let p = tab.active();
            (
                p.filter.clone(),
                p.scroll_y,
                p.path.is_none(),
                tab.active_pane,
                p.loading,
            )
        };
        // 跨目录后退落到搜索页：jump_to 会把 pane.loading 置 true，但搜索页
        // 结果来自索引/deep，不会走「目录列举完成」那条复位路径（sync_after_navigate
        // 特意跳过 reload_active，否则列举结果会 filter.clear() 冲掉搜索词）。
        // 如果不在这里复位，列表会永远盖着「正在读取…」占位层。
        if was_loading {
            logv!("[nav] restore search page: clearing stale loading flag (filter={})", filter);
        }
        self.state.active_mut().panes[pane_index].loading = false;
        // 跨目录 jump_to 会置 pending_scroll_restore=true，正常由列举结果复位；
        // 搜索页跳过列举，这里必须自己清，否则滚动回调被永久忽略。
        self.state.active_mut().panes[pane_index].pending_scroll_restore = false;
        self.ui.set_filter_text(filter.clone().into());
        if is_root {
            self.run_global_search(&filter);
        } else {
            logv!(
                "[nav] restore search page: cross_dir={} root={:?} filter={:?} gen={} dgen={} entries={}",
                cross_dir,
                self.state.active().active().path,
                filter,
                self.state.active().active().generation,
                self.state.active().active().deep_generation,
                self.state.active().active().entries.len()
            );
            self.start_deep_search();
            self.state.active_mut().active_mut().rebuild_display();
            if cross_dir {
                // 深度搜索只返回 depth>=1 的结果（depth 0 = 当前目录直接子项，
                // 由本地过滤负责）。旧目录的 entries 会让本地过滤永远匹配不到，
                // 表现为“此文件夹为空”且一直不回。这里补列举，结果到达后
                // 由 apply_list_result → rebuild_display 自动接上搜索词。
                //
                // 注意 apply_list_result 只在「此电脑」分支清 filter，不会冲掉搜索词。
                self.reload_active();
                // 列举在跑（网络共享可能要好几秒），“此文件夹为空”会跟真正的
                // 空目录混淆。保持 loading，让列表盖着“正在读取…”，
                // 由 apply_list_result 复位。
                self.state.active_mut().panes[pane_index].loading = true;
            }
        }
        self.visible_cache = None;
        self.sync_nav();
        self.sync_status();
        self.sync_tabs();
        self.sync_file_list();
        push_scroll(&self.ui, pane_index, scroll);
    }

    fn nav_up(&mut self) {
        let Some(parent) = self.state.active().active().parent_path() else {
            return;
        };
        self.navigate_active(parent);
    }
}

// ---------------------------------------------------------------------------
// 选择与行交互
// ---------------------------------------------------------------------------

/// 框选命中计算用的列表几何：循环外读一次 UI 属性，避免每条目重复查询。
struct RubberGeom {
    is_icons: bool,
    cells_per_row: usize,
    row_h: f32,
    cell_w: f32,
    cell_h: f32,
    /// 图标网格均分后的间隙（与 file_list.slint 的 grid-gap 同源）。
    grid_gap: f32,
}

impl RubberGeom {
    /// right=true 时读右分屏的间隙：两栏宽度可能不同，gap 也各自不同。
    fn read(ui: &MainWindow, right: bool) -> Self {
        let (cell_w, cell_h) = cell_metrics(ui);
        Self {
            is_icons: ui.get_is_icons_view_out(),
            cells_per_row: ui.get_cells_per_row().max(1) as usize,
            row_h: ui.get_row_height_out(),
            cell_w,
            cell_h,
            grid_gap: if right {
                ui.get_grid_gap_right_out() as f32
            } else {
                ui.get_grid_gap_out() as f32
            },
        }
    }

    /// 第 di 个条目的 AABB（绝对内容坐标）。x 与 file_list.slint 的
    /// cell-x(col) = grid-gap + col*(item-w + grid-gap) 严格一致。
    fn rect(&self, di: usize) -> (f32, f32, f32, f32) {
        if self.is_icons {
            let col = di % self.cells_per_row;
            let row = di / self.cells_per_row;
            let item_w = (self.cell_w - 8.0).max(1.0);
            (
                self.grid_gap + col as f32 * (item_w + self.grid_gap),
                row as f32 * self.cell_h + 4.0,
                item_w,
                self.cell_h - 8.0,
            )
        } else {
            // 列表视图横向全宽
            (0.0, di as f32 * self.row_h, 5000.0, self.row_h)
        }
    }
}

#[cfg(windows)]
fn key_down(vk: i32) -> bool {
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::GetKeyState;
    // GetKeyState 返回 i16；高位为 1 表示按下。用 u16 掩码避免 0x8000 字面量
    // 溢出 i16 的编译错误。
    (unsafe { GetKeyState(vk) } as u16) & 0x8000 != 0
}

#[cfg(not(windows))]
fn key_down(_vk: i32) -> bool {
    false
}

const VK_SHIFT: i32 = 0x10;
const VK_CONTROL: i32 = 0x11;
const VK_MENU: i32 = 0x12;

impl App {
    fn row_pointer_event(&mut self, index: usize, event: &PointerEvent, x: f32, y: f32, on_name: bool) {
        // 任务 2.3：拖出手势（按下 → 位移超阈值 → OLE 拖放循环）。
        match event.kind {
            PointerEventKind::Up | PointerEventKind::Cancel => {
                self.drag_pending = None;
                self.ui.invoke_set_drag_armed(false);
                // 多选下按在已选中行、但全程没拖动：此刻才塌缩成该行单选。
                // 若已发起拖出，pending 会在 Move 分支被清掉，这里不再改动选择集
                // （批量拖放后保持多选是符合资源管理器语义的）。
                if let Some(p) = self.pending_single_click.take() {
                    let tab = self.state.active_mut().active_mut();
                    tab.selection.clear();
                    tab.selection.insert(p.clone());
                    tab.anchor = Some(p);
                    self.visible_cache = None;
                    self.refresh_visible_rows();
                    self.sync_status();
                }
                logv!("[drag] armed=false (up/cancel)");
            }
            PointerEventKind::Move => {
                // 仅当位移越过阈值才取走待定手势并启动拖出；否则保留，
                // 否则首次微小移动就会清空 drag_pending 导致永远拖不出（见 #2 缺陷）。
                if let Some(paths) = drag_gesture_paths(&self.drag_pending, x, y) {
                    self.drag_pending = None;
                    // 已经真正开始拖出：不要再塌缩选择集，也不要在 400ms 后弹重命名。
                    self.pending_single_click = None;
                    self.cancel_rename_defer();
                    self.ui.invoke_set_drag_armed(false);
                    self.begin_drag_out(paths);
                }
            }
            _ => {}
        }
        if event.kind != PointerEventKind::Down {
            return;
        }
        let Some(path) = self.state.active().active().display_entry(index).map(|e| e.path.clone()) else {
            return;
        };
        match event.button {
            PointerEventButton::Right => {
                // 右键（不走 Left 分支）也要取消挂起的延迟重命名，否则 400ms 后
                // rename 输入框会与右键菜单共存/抢占焦点。
                self.cancel_rename_defer();
                // 未选中则先选中它，再弹菜单。
                if !self.state.active().active().selection.contains(&path) {
                    let tab = self.state.active_mut().active_mut();
                    tab.selection.clear();
                    tab.selection.insert(path.clone());
                    tab.anchor = Some(path.clone());
                }
                // 记下右键所在行：多选时「打开文件所在位置」/「在资源管理器中显示」
                // 要优先定位它，而不是选中集里按显示顺序排第一的那个。
                self.menu_anchor_path = Some(path.clone());
                let target = MenuScope::Selection(self.state.active().active().selected_paths());
                self.menu_scope = Some(target.clone());
                self.visible_cache = None;
                self.refresh_visible_rows();
                self.sync_status();
                self.open_menu(target);
            }
            PointerEventButton::Left => {
                // 每次左键按下先取消挂起的延迟重命名，避免旧的 timer 在条件偶然重新
                // 满足时提前触发（单击 A→点 B→再点 A：t=200 点的 A 只过 200ms 却弹出 rename）。
                self.cancel_rename_defer();
                // 新的一次按下：丢掉上一次未完成的「延迟塌缩单选」。
                self.pending_single_click = None;
                // 双击检测在 Rust 侧做（有效双击间隔内同一路径二次按下）：
                // 不依赖 Slint 的 double-clicked，行模型重建也不会打断判定。
                // 注意：这个窗口必须 ≤ 下方延迟重命名（有效双击间隔 + 50ms），否则
                // 「慢双击」会先触发重命名再打开。
                let now = std::time::Instant::now();
                if self.last_click.as_ref().is_some_and(|(p, t)| {
                    *p == path && t.elapsed().as_millis() < effective_double_click_ms() as u128
                }) {
                    // 第二次按下 = 双击 → 打开，并取消挂起的延迟重命名。
                    self.cancel_rename_defer();
                    self.last_click = None;
                    self.activate_row(index);
                    return;
                }
                self.last_click = Some((path.clone(), now));
                let ctrl = key_down(VK_CONTROL);
                let shift = key_down(VK_SHIFT);
                // 批量拖出：先在「按下瞬间」给选中集拍快照。必须在任何选择集
                // 塌缩之前取，否则多选在 down 时就变成 1 个，拖出去只有当前行。
                let (multi, row_selected, drag_paths) = {
                    let pane = self.state.active().active();
                    let multi = pane.selection.len() > 1;
                    let row_selected = pane.selection.contains(&path);
                    let paths = if !ctrl && !shift && multi && row_selected {
                        pane.selected_paths()
                    } else {
                        vec![path.clone()]
                    };
                    (multi, row_selected, paths)
                };
                // 拖出手势登记提前到所有 return 之前：点名称进重命名的分支也能拖出。
                // 重命名输入态（真正已在编辑）不登记。
                if !(self.state.active().active().path.is_some()
                    && !self.ui.get_rename_path().is_empty())
                {
                    self.drag_pending = Some((x, y, drag_paths));
                    self.ui.invoke_set_drag_armed(true);
                    logv!("[drag] armed=true for row {}", index);
                }
                // 单选后（非双击、非 Ctrl/Shift）点击文件名 → 延迟进入重命名：
                // 先不起重命名，留出双击窗口；若再来一次按下（有效双击间隔内）则被判定为
                // 双击打开（见上方 cancel_rename_defer），否则（有效双击间隔 + 50ms）后到
                // 点才重命名。这样「单击选中后双击名称」不会被抢成重命名。
                // 由 Slint 判定是否落在名称列，避免点图标/类型/大小列误触发。
                if on_name {
                    logv!(
                        "[rename] on_name down path={} sel_len={} contains={}",
                        path.display(),
                        self.state.active().active().selection.len(),
                        self.state.active().active().selection.contains(&path),
                    );
                }
                if on_name && !ctrl && !shift && !multi && row_selected {
                    self.schedule_rename(path.clone());
                    return;
                }
                // 多选下按在某个「已选中行」：保留整个选择集，把「塌缩成单选」推迟
                // 到 Up（且全程未拖动）时才做。这里不重建行模型，保持按下时的
                // TouchArea 存活，后续 Move 才能持续送到同一行触发批量拖出。
                if !ctrl && !shift && multi && row_selected {
                    self.pending_single_click = Some(path.clone());
                    return;
                }
                let tab = self.state.active_mut().active_mut();
                logv!(
                    "[select] row down index={} ctrl={} shift={} anchor={:?} selected={}",
                    index,
                    ctrl,
                    shift,
                    tab.anchor.as_ref().and_then(|p| p.file_name()).map(|n| n.to_string_lossy().to_string()),
                    tab.selection.len()
                );
                if ctrl {
                    if tab.selection.contains(&path) {
                        tab.selection.remove(&path);
                    } else {
                        tab.selection.insert(path.clone());
                    }
                    tab.anchor = Some(path);
                } else if shift {
                    let anchor_pos = tab.anchor.clone().and_then(|anchor| {
                        tab.display
                            .iter()
                            .position(|&i| tab.entries.get(i).is_some_and(|e| e.path == anchor))
                    });
                    match anchor_pos {
                        Some(anchor_pos) => {
                            let (lo, hi) = if anchor_pos <= index {
                                (anchor_pos, index)
                            } else {
                                (index, anchor_pos)
                            };
                            logv!(
                                "[select] shift range {}..={} (anchor={}, click={})",
                                lo, hi, anchor_pos, index
                            );
                            tab.selection.clear();
                            for di in lo..=hi {
                                if let Some(e) = tab.display_entry(di) {
                                    tab.selection.insert(e.path.clone());
                                }
                            }
                        }
                        None => {
                            // 锚点已不在当前列表（换目录 / 排序变化 / 被过滤）：
                            // 退化为单击并重设锚点，不能让 Shift 点击毫无反应。
                            logv!("[select] shift without valid anchor, click={}", index);
                            tab.selection.clear();
                            tab.selection.insert(path.clone());
                            tab.anchor = Some(path);
                        }
                    }
                } else {
                    tab.selection.clear();
                    tab.selection.insert(path.clone());
                    tab.anchor = Some(path);
                }
                self.visible_cache = None;
                self.refresh_visible_rows();
                self.sync_status();
            }
            _ => {}
        }
    }

    /// 任务 2.3：发起 OLE 拖放循环（阻塞至释放）。
    fn begin_drag_out(&mut self, paths: Vec<PathBuf>) {
        if paths.is_empty() {
            return;
        }

        #[cfg(windows)]
        let ui_weak = self.ui.as_weak();
        #[cfg(windows)]
        let scale = self.ui.window().scale_factor() as f32;
        // 拖拽图像按当前视图图标的屏幕尺寸取图（dnd 模块不碰 Slint，尺寸由这里注入）。
        #[cfg(windows)]
        let icon_px = self.drag_icon_px();
        // 非 Windows 上 ops::thumbs 整个模块被 cfg 掉，这里只给个占位值
        //（dnd 的非 Windows 桩会忽略它）。
        #[cfg(not(windows))]
        let icon_px = 44u32;

        // 原生焦点框高亮（仅 Windows）：DrawFocusRect 对同一矩形二次绘制即为自擦除。
        #[cfg(windows)]
        // 原生 HWND（供 DrawFocusRect 绘制悬停焦点框；非 Windows 平台为 0）。
        let hwnd: isize = get_window_hwnd(&self.ui).unwrap_or(0);

        // 定义为嵌套 fn（不捕获环境），避免把借用带入 move 闭包与收尾擦除。
        #[cfg(windows)]
        unsafe fn draw_focus(hwnd: isize, rect: &windows_sys::Win32::Foundation::RECT) {
            use windows_sys::Win32::Graphics::Gdi::{DrawFocusRect, GetDC, ReleaseDC};
            let hdc = GetDC(hwnd as _);
            DrawFocusRect(hdc, rect);
            ReleaseDC(hwnd as _, hdc);
        }

        // dnd::begin_drag_out 的回调类型是 Fn()（非 FnMut），跨调用保留状态
        // 必须走内部可变性，否则闭包无法编译通过。
        #[cfg(windows)]
        let last_highlight_rect: std::rc::Rc<std::cell::RefCell<Option<windows_sys::Win32::Foundation::RECT>>> =
            std::rc::Rc::new(std::cell::RefCell::new(None));
        #[cfg(windows)]
        let last_pane: std::rc::Rc<std::cell::Cell<i32>> = std::rc::Rc::new(std::cell::Cell::new(-1));
        #[cfg(windows)]
        let last_idx: std::rc::Rc<std::cell::Cell<i32>> = std::rc::Rc::new(std::cell::Cell::new(-1));

        // 面包屑落点：几何与「是否可落点」在拖动前快照 —— DoDragDrop 期间不能再
        // 读 Slint（模态循环不跑事件循环），悬停高亮也只能靠 GDI 自绘。
        #[cfg(windows)]
        let crumbs = std::rc::Rc::new(std::cell::RefCell::new({
            // 落点优先级：面包屑 → 侧栏条目 → 文件行。前两者的几何都在拖动前
            // 快照，因为 DoDragDrop 的模态循环里不能再读 Slint。
            let mut zones = drag_crumbs(self);
            zones.extend(drag_sidebar_zones(self));
            zones
        }));
        #[cfg(windows)]
        let last_crumb: std::rc::Rc<std::cell::Cell<i32>> =
            std::rc::Rc::new(std::cell::Cell::new(-1));

        let outcome = crate::sys::dnd::begin_drag_out(&paths, icon_px, {
            #[cfg(windows)]
            let last_highlight_rect = last_highlight_rect.clone();
            #[cfg(windows)]
            let last_pane = last_pane.clone();
            #[cfg(windows)]
            let last_idx = last_idx.clone();
            #[cfg(windows)]
            let last_crumb = last_crumb.clone();
            #[cfg(windows)]
            let crumbs = crumbs.clone();
            move || {
                #[cfg(windows)]
                if let Some(ui) = ui_weak.upgrade() {
                    #[cfg(windows)]
                    if let Some((lx, ly)) = cursor_window_logical(&ui, scale) {
                        // 0. 面包屑优先：命中可落点段就只高亮它，并跳过文件行高亮。
                        let crumb = crumb_hit(&crumbs.borrow(), lx, ly);
                        if crumb != last_crumb.get() {
                            last_crumb.set(crumb);
                            if let Some(old_rect) = last_highlight_rect.borrow_mut().take() {
                                if hwnd != 0 {
                                    unsafe { draw_focus(hwnd, &old_rect) };
                                }
                            }
                            // 目标切换后，重新进入列表区域要重新判定并重绘。
                            last_pane.set(-1);
                            last_idx.set(-1);
                            if crumb >= 0 && hwnd != 0 {
                                let r = crumbs.borrow()[crumb as usize].rect;
                                let new_rect = windows_sys::Win32::Foundation::RECT {
                                    left: (r.0 * scale) as i32,
                                    top: (r.1 * scale) as i32,
                                    right: ((r.0 + r.2) * scale) as i32,
                                    bottom: ((r.1 + r.3) * scale) as i32,
                                };
                                unsafe { draw_focus(hwnd, &new_rect) };
                                *last_highlight_rect.borrow_mut() = Some(new_rect);
                            }
                        }

                        let res = ui.invoke_get_drop_target(lx, ly);
                        let pane = if res.row_count() == 2 { res.row_data(0).unwrap_or(0) as i32 } else { 0 };
                        let idx = if res.row_count() == 2 { res.row_data(1).unwrap_or(-1) as i32 } else { -1 };

                        if crumb < 0 && (pane != last_pane.get() || idx != last_idx.get()) {
                            last_pane.set(pane);
                            last_idx.set(idx);

                            // 1. 擦除上一个高亮框
                            if let Some(old_rect) = last_highlight_rect.borrow_mut().take() {
                                if hwnd != 0 {
                                    unsafe { draw_focus(hwnd, &old_rect) };
                                }
                            }

                            // 2. 在当前悬停项绘制原生高亮焦点框
                            if idx >= 0 && hwnd != 0 {
                                let rect_arr = ui.invoke_get_item_rect_target(pane, idx);
                                if rect_arr.row_count() == 4 {
                                    let rx = rect_arr.row_data(0).unwrap_or(0.0) * scale;
                                    let ry = rect_arr.row_data(1).unwrap_or(0.0) * scale;
                                    let rw = rect_arr.row_data(2).unwrap_or(0.0) * scale;
                                    let rh = rect_arr.row_data(3).unwrap_or(0.0) * scale;

                                    let new_rect = windows_sys::Win32::Foundation::RECT {
                                        left: rx as i32,
                                        top: ry as i32,
                                        right: (rx + rw) as i32,
                                        bottom: (ry + rh) as i32,
                                    };
                                    unsafe { draw_focus(hwnd, &new_rect) };
                                    *last_highlight_rect.borrow_mut() = Some(new_rect);
                                }
                            }
                        }
                    }
                }
            }
        });

        // 拖拽释放：彻底擦除高亮边框
        #[cfg(windows)]
        if let Some(old_rect) = last_highlight_rect.borrow_mut().take() {
            if hwnd != 0 {
                unsafe { draw_focus(hwnd, &old_rect) };
            }
        }

        // 拖到面包屑松手：无 IDropTarget 的区域只会返回 CANCEL，用「松手结束」
        // 标志 + 拖动前快照的几何判断落点，命中就直接移动。
        #[cfg(windows)]
        {
            let hit = last_crumb.get();
            if hit >= 0 && crate::sys::dnd::drag_released() {
                let target = crumbs
                    .borrow()
                    .get(hit as usize)
                    .and_then(|c| c.target.clone());
                if let Some(dir) = target {
                    drop_into_crumb(self, &paths, &dir);
                }
            }
        }

        self.ui.set_file_drop_hover_index(-1);
        self.ui.window().request_redraw();
        if let crate::sys::dnd::DragOutOutcome::Dropped(_) = outcome {
            self.reload_active();
        }
    }

    /// 任务 2.2：处理外部或内部拖入（支持左右分屏跨栏）。
    fn process_external_drop(&mut self) {
        let paths = std::mem::take(&mut self.drop_buffer);
        if paths.is_empty() || self.ui.get_settings_open() {
            return;
        }

        // 1. 根据鼠标光标位置判断目标是哪个 pane 和哪一行
        #[cfg(windows)]
        let (target_pane, target_row) = {
            let scale = self.ui.window().scale_factor() as f32;
            if let Some((lx, ly)) = cursor_window_logical(&self.ui, scale) {
                let res = self.ui.invoke_get_drop_target(lx, ly);
                if res.row_count() == 2 {
                    (
                        res.row_data(0).unwrap_or(0) as usize,
                        res.row_data(1).unwrap_or(-1) as i32,
                    )
                } else {
                    (self.state.active().active_pane, -1)
                }
            } else {
                (self.state.active().active_pane, -1)
            }
        };
        #[cfg(not(windows))]
        let (target_pane, target_row) = (self.state.active().active_pane, -1);

        // 2. 确定目标目录（先取出 owned 值，避免持有 self.state 借用时再调 &mut self）
        let target_dir_opt = self
            .state
            .active()
            .panes
            .get(target_pane)
            .map(|pane| pane.path.clone());
        let mut target_dir = match target_dir_opt {
            Some(Some(p)) => p,
            Some(None) => {
                self.set_status_job(self.tr("无法将文件拖放到此电脑"));
                return;
            }
            None => return,
        };

        // 3. 如果命中了文件夹，目标改为该文件夹
        if target_row >= 0 {
            let hit_dir = self
                .state
                .active()
                .panes
                .get(target_pane)
                .and_then(|pane| pane.display_entry(target_row as usize))
                .filter(|e| e.is_dir)
                .map(|e| e.path.clone());
            if let Some(dir) = hit_dir {
                target_dir = dir;
            }
        }

        // 4. 防止原地拖放或拖入自身子目录
        if paths.iter().all(|p| p.parent() == Some(target_dir.as_path())) {
            return; // 同一目录下移动/复制，直接忽略
        }
        if paths.iter().any(|p| p == &target_dir || target_dir.starts_with(p)) {
            self.set_status_job(self.tr("无法将文件夹移动到自身或其子文件夹中"));
            return;
        }

        // 5. 判定是复制还是移动（修饰键优先，否则同盘移动、跨盘复制）
        let ctrl = key_down(VK_CONTROL);
        let shift = key_down(VK_SHIFT);
        let is_move = if ctrl {
            false
        } else if shift {
            true
        } else {
            same_volume(&paths[0], &target_dir)
        };
        // 预测粘贴后的新路径，供列举就位后滚进可视区（预测不准也不会误滚）。
        self.expect_new_entries(&paths, &target_dir);
        let kind = if is_move {
            JobKind::Move { sources: paths, dest_dir: target_dir }
        } else {
            JobKind::Copy { sources: paths, dest_dir: target_dir }
        };
        self.submit_job(kind);
    }

    fn background_pointer_event(&mut self, event: &PointerEvent, _x: f32, _y: f32) {
        if event.kind != PointerEventKind::Down {
            return;
        }
        match event.button {
            PointerEventButton::Left => {
                // 资源管理器语义：按住 Ctrl/Shift 起手时保留已有选择，
                // 记下按下瞬间的选择集作为本轮框选的累加基线；
                // 普通框选才清空（含单击空白取消选择）。
                let additive = key_down(VK_CONTROL) || key_down(VK_SHIFT);
                self.rubber_band_base = if additive {
                    Some(self.state.active().active().selection.clone())
                } else {
                    None
                };
                logv!(
                    "[select] rubber-band down additive={} base={}",
                    additive,
                    self.rubber_band_base.as_ref().map_or(0, |b| b.len())
                );
                if !additive {
                    let tab = self.state.active_mut().active_mut();
                    tab.selection.clear();
                    tab.anchor = None;
                    self.visible_cache = None;
                    self.refresh_visible_rows();
                    self.sync_status();
                }
            }
            PointerEventButton::Right => {
                self.menu_anchor_path = None;
                // 记下当前目录快照：系统菜单里的「新建 → 文本文档」由 Shell 自己
                // 执行，Rove 拿不到新文件路径。菜单结束后列举结果回来再做 diff。
                if let Some(dir) = self.state.active().active().path.clone() {
                    let before: std::collections::HashSet<PathBuf> = self
                        .state
                        .active()
                        .active()
                        .entries
                        .iter()
                        .map(|e| e.path.clone())
                        .collect();
                    self.shell_new_watch = Some((dir, before, std::time::Instant::now()));
                } else {
                    self.shell_new_watch = None;
                }
                let target = MenuScope::Background;
                self.menu_scope = Some(target.clone());
                self.open_menu(target);
            }
            _ => {}
        }
    }

    /// 矩形与条目的 AABB 命中集合（纯几何，是否成框由调用方判定）。
    /// 背景 TouchArea 撑满 content-height，传来的 y 已是包含滚动偏移的绝对
    /// 内容坐标（与单元格同一坐标系），无需再补 scroll_y。
    fn rubber_band_hits(&self, x1: f32, y1: f32, x2: f32, y2: f32) -> HashSet<PathBuf> {
        let left = x1.min(x2);
        let right = x1.max(x2);
        let top = y1.min(y2);
        let bottom = y1.max(y2);

        let geom = RubberGeom::read(&self.ui, self.state.active().active_pane == 1);
        let tab = self.state.active().active();
        let mut hits = HashSet::new();
        for di in 0..tab.display.len() {
            let Some(entry) = tab.display_entry(di) else { continue };
            let (ix, iy, iw, ih) = geom.rect(di);
            if ix < right && ix + iw > left && iy < bottom && iy + ih > top {
                hits.insert(entry.path.clone());
            }
        }
        hits
    }

    /// 框选的「焦点项」，即后续 Shift 连续选择的锚点。
    /// 起点落在条目上时用该条目（资源管理器把拖拽起点当焦点）；框选通常只能
    /// 从空白起手，此时以框内显示顺序最靠前的条目为焦点，不受拖拽方向影响。
    /// 例：框选 3/4/5 后 Shift 点 1，得到 1/2/3。
    fn rubber_band_anchor(&self, x1: f32, y1: f32, hits: &HashSet<PathBuf>) -> Option<PathBuf> {
        let geom = RubberGeom::read(&self.ui, self.state.active().active_pane == 1);
        let tab = self.state.active().active();
        let mut first: Option<PathBuf> = None;
        for di in 0..tab.display.len() {
            let Some(entry) = tab.display_entry(di) else { continue };
            let (ix, iy, iw, ih) = geom.rect(di);
            if ix <= x1 && x1 < ix + iw && iy <= y1 && y1 < iy + ih {
                return Some(entry.path.clone());
            }
            if first.is_none() && hits.contains(&entry.path) {
                first = Some(entry.path.clone());
            }
        }
        first
    }

    /// 合并命中集与拖拽基线：Some(base) = 按住 Ctrl/Shift 的累加框选。
    /// 始终以按下瞬间的选择为底，矩形缩回时预览高亮也会跟着缩回。
    fn rubber_band_combine(&self, hits: &HashSet<PathBuf>) -> HashSet<PathBuf> {
        match &self.rubber_band_base {
            Some(base) => {
                let mut s = base.clone();
                s.extend(hits.iter().cloned());
                s
            }
            None => hits.clone(),
        }
    }

    /// 框选结束（每次抬手都会调用，包括误触与取消）。
    fn handle_rubber_band_select(&mut self, x1: f32, y1: f32, x2: f32, y2: f32) {
        // 4px 以内视为点击 / 误触，或 UI 传零矩形表示 cancel：恢复按下前的选择。
        let dragged = (x1 - x2).abs() > 4.0 || (y1 - y2).abs() > 4.0;
        let hits = if dragged {
            self.rubber_band_hits(x1, y1, x2, y2)
        } else {
            HashSet::new()
        };
        let new_selection = self.rubber_band_combine(&hits);
        let anchor = if dragged {
            self.rubber_band_anchor(x1, y1, &hits)
        } else {
            None
        };
        logv!(
            "[select] rubber-band up dragged={} hits={} base={} anchor={:?}",
            dragged,
            hits.len(),
            self.rubber_band_base.as_ref().map_or(0, |b| b.len()),
            anchor.as_ref().and_then(|p| p.file_name()).map(|n| n.to_string_lossy().to_string())
        );
        self.rubber_band_base = None;

        let tab = self.state.active_mut().active_mut();
        let changed = tab.selection != new_selection;
        if changed {
            tab.selection = new_selection;
        }
        if dragged {
            // 成框后把焦点项留作 Shift 连续选择的锚点；框空时保留原锚点，
            // 误触 / 取消则完全不动（Ctrl/Shift 基线会原样恢复）。
            if let Some(anchor) = anchor {
                tab.anchor = Some(anchor);
            }
        }
        if changed {
            self.visible_cache = None;
            self.refresh_visible_rows();
            self.sync_status();
        }
    }

    /// 处理文件列表的实时框选（拖动过程中）。
    fn handle_rubber_band_move(&mut self, x1: f32, y1: f32, x2: f32, y2: f32) {
        // 降级为 logv，避免高频刷屏
        logv!("[rubber-band] move x1={:.1}, y1={:.1}, x2={:.1}, y2={:.1}", x1, y1, x2, y2);
        let dragged = (x1 - x2).abs() > 4.0 || (y1 - y2).abs() > 4.0;
        let hits = if dragged {
            self.rubber_band_hits(x1, y1, x2, y2)
        } else {
            HashSet::new()
        };
        // 每次都基于按下瞬间的基线重算，而不是基于上一帧的选择集，
        // 否则 Ctrl 框选会变成「涂抹」：曾经划过、后来缩回的项仍旧留着。
        let new_selection = self.rubber_band_combine(&hits);

        // 仅当选择集发生变化时才更新 UI，避免高频无效重绘导致卡顿。
        let tab = self.state.active_mut().active_mut();
        if tab.selection != new_selection {
            logv!("[rubber-band] selection updated, count={}", new_selection.len());
            tab.selection = new_selection;
            self.visible_cache = None;
            self.refresh_visible_rows();
            self.sync_status();
        }
    }
}

// ---------------------------------------------------------------------------
// 事件循环装配
// ---------------------------------------------------------------------------

/// 轮询定时器：合并目录列举结果、刷新任务进度。
fn setup_timers(app: &Rc<RefCell<App>>) {
    // 列举结果合并（40ms）。
    {
        let timer_app = Rc::downgrade(app);
        app.borrow()
            .poll_timer
.start(TimerMode::Repeated, Duration::from_millis(40), move || {
    let Some(app) = timer_app.upgrade() else {
        logln!("poll timer: app gone");
        return;
    };

    // 合并窗口到点则触发索引重建（设置页连续改多项只重建一次）。
    maybe_run_pending_reindex();

    // 主窗 HWND 登记、无边框拖拽钩子、最大化还原、首次启动居中，已全部移至
    // on_winit_window_event 的首次 RedrawRequested（窗口 realize 后）统一应用，
    // 见 apply_deferred_geometry（由 setup_keyboard 注册）。
    // 布局落定前 Slint 几何可能仍是 0；每 40ms 重取一次，
    // 保证最终一定收敛到真实的 drag-spacer 边界（update 内部去重）。
    sync_drag_bounds(&app.borrow(), "poll");

    let mut app = app.borrow_mut();
                // 布局落定 / 窗口尺寸变化监听。
                // 启动首帧时 list_height=0、cells_per_row=1，按当时的几何算出的
                // 可见窗口偏小；这里一旦检测到几何变化就重算，否则会一直
                // 停在“只渲染前几行”的状态（「此电脑」部分盘符空白）。
                let geom = app.current_list_geom();
                if geom != app.list_geom {
                    crate::logv!(
                        "[geom] changed h={} cells={} row_h={} icons={}",
                        geom.0,
                        geom.1,
                        geom.2,
                        geom.3
                    );
                    app.list_geom = geom;
                    app.refresh_visible_rows();
                }
                while let Ok(result) = app.list_rx.try_recv() {
                    app.apply_list_result(result);
                }
                while let Ok(result) = app.deep_rx.try_recv() {
                    app.apply_deep_result(result);
                }
                while let Ok(result) = app.dir_size_rx.try_recv() {
                    app.apply_dir_size_result(result);
                }

                // 核心修复：缩略图批量到达后，统一在主线程刷新一次，避免 653 张
                // 连续 store 触发 653 次全量行重建导致掉帧。
                if app.thumb_needs_refresh {
                    app.thumb_needs_refresh = false;
                    app.visible_cache = None;
                    app.refresh_visible_rows();
                    // 含侧栏尺寸(32px)的图标更新时也刷新侧边栏（方块→原生图标）。
                    app.sidebar_fingerprint = 0;
                    app.sync_sidebar();
                }

                // 核心修复：用户空闲时静默重排，绝不打断当前操作。
                if app.size_sort_pending {
                    // 防抖判定：拖拽中 / 列表加载中 / 800ms 内有过点击（大概率在操作）
                    let is_interacting = app.drag_pending.is_some()
                        || app.ui.get_file_loading()
                        || app
                            .last_click
                            .as_ref()
                            .is_some_and(|(_, t)| t.elapsed().as_millis() < 800);

                    if !is_interacting {
                        app.size_sort_pending = false;
                        let mut resorted = false;
                        {
                            let tab = app.state.active_mut();
                            for pane in &mut tab.panes {
                                if pane.sort.key == SortKey::Size {
                                    crate::fs::sort_entries(&mut pane.entries, pane.sort);
                                    pane.rebuild_display();
                                    resorted = true;
                                }
                            }
                        }
                        if resorted {
                            app.visible_cache = None;
                            app.refresh_visible_rows();
                        }
                    }
                }

                // ---- 消费文件变更事件：逆向反查当前视图的直接子目录并防抖重算 ----
                let mut got_events = false;
                if let Some(cur_dir) = app.state.active().active().path.clone() {
                    while let Ok(changed_path) = app.watch_rx.try_recv() {
                        // 1. 过滤 Rove 自身读写文件，打破贪吃蛇死循环
                        //    （重算时 save_to_temp 会写 AppData\Local\Temp\rove_dir_sizes.json，
                        //     该路径落在监听子树内，反查到 Users 后又触发重算 -> 无限循环）。
                        let s = changed_path.to_string_lossy();
                        if s.ends_with("rove_dir_sizes.json") || s.ends_with("rove.log") {
                            continue;
                        }
                        // 2. 过滤系统与软件后台高频静默写入区
                        //    （Chrome/微信/系统日志每几百毫秒写入 AppData 与 Windows，
                        //     会不断唤醒 Users 重算导致界面卡顿）。
                        if s.contains(r"\AppData\Local\Temp\")
                            || s.contains(r"\AppData\Local\")
                            || s.contains(r"\AppData\Roaming\")
                            || s.contains(r"\Windows\")
                        {
                            continue;
                        }
                        // 关键逆向追溯：changed_path 属于 cur_dir 子树时，取其第一层子目录。
                        // 例：cur_dir=C:\，changed=C:\Users\captain\Downloads\9G.zip
                        //     -> rel=Users\captain\Downloads\9G.zip -> first=Users -> 锁定 C:\Users
                        if let Ok(rel) = changed_path.strip_prefix(&cur_dir) {
                            if let Some(first_comp) = rel.components().next() {
                                let direct_child = cur_dir.join(first_comp);
                                app.pending_invalidated_subdirs.insert(direct_child);
                                got_events = true;
                            }
                        }
                    }
                }
                if got_events {
                    app.last_watch_event = Some(std::time::Instant::now());
                }

                // 400ms 静默防抖：连续变动停止 400ms 后，批量穿透重算失效的文件夹
                if let Some(last_time) = app.last_watch_event {
                    if last_time.elapsed() >= std::time::Duration::from_millis(400) {
                        app.last_watch_event = None;
                        let dirs_to_rescan: Vec<PathBuf> =
                            app.pending_invalidated_subdirs.drain().collect();
                        if !dirs_to_rescan.is_empty() {
                            // 1. 先在全局缓存中清除这批目录，保证穿透重算
                            {
                                let mut store = global_dir_sizes().lock().unwrap();
                                for d in &dirs_to_rescan {
                                    store.cache.remove(d);
                                    store.order.retain(|p| p != d);
                                }
                                store.mark_modified();
                            }
                            // 2. 仅对当前存在且是目录的项派发重新统计
                            let mut rescan_reqs: Vec<(PathBuf, Option<SystemTime>)> = Vec::new();
                            let (tab_id, tab_generation) = {
                                let tab = app.state.active();
                                for d in &dirs_to_rescan {
                                    // 严禁重算推迟目录（如 C:\Windows、System Volume Information）
                                    if is_expensive_system_dir(&d) {
                                        continue;
                                    }
                                    // 单目录重算冷却：距上次不足 2.5s 则跳过，避免密集轰炸磁盘
                                    if let Some(last) = app.last_rescan_times.get(d) {
                                        if last.elapsed()
                                            < std::time::Duration::from_millis(2500)
                                        {
                                            continue;
                                        }
                                    }
                                    if let Some(entry) =
                                        tab.active().entries.iter().find(|e| &e.path == d && e.is_dir)
                                    {
                                        rescan_reqs.push((entry.path.clone(), entry.modified));
                                        dirsize_trace(format!(
                                            "watcher invalidate & rescan: {}",
                                            d.display()
                                        ));
                                    }
                                }
                                (tab.id, tab.active().generation)
                            };
                            if !rescan_reqs.is_empty() {
                                // 记录本次派发时刻，供冷却判断
                                let now = std::time::Instant::now();
                                for (p, _) in &rescan_reqs {
                                    app.last_rescan_times.insert(p.clone(), now);
                                }
                                let new_stop = std::sync::Arc::new(
                                    std::sync::atomic::AtomicBool::new(false),
                                );
                                app.dir_size_stop = Some(new_stop.clone());
                                let _ = app.dir_size_tx.send(DirSizeRequest {
                                    tab_id,
                                    generation: tab_generation,
                                    dirs: rescan_reqs,
                                    stop: new_stop,
                                });
                            }
                        }
                    }
                }
            });
    }
    // 任务 3：目录变更轮询（1s），检测文件被移动/重命名等后实时刷新。
    {
        let timer_app = Rc::downgrade(app);
        app.borrow()
            .refresh_timer
            .start(TimerMode::Repeated, Duration::from_millis(1000), move || {
                let Some(app) = timer_app.upgrade() else { return };
                let mut app = app.borrow_mut();
                app.poll_dir_changes();
                // 搜索设置页可见时刷新看板指标：索引构建中条目数会持续增长，
                // 只刷高频字段，不重建驱动器模型（避免打断悬停 / 开关动画）。
                let on_search_settings = {
                    let tab = app.state.active();
                    tab.kind == crate::state::TabKind::Settings
                        && tab.settings_subpage == crate::state::SettingsSubpage::SearchAndIndex
                };
                if on_search_settings {
                    app.sync_search_stats(false);
                }
            });
    }
    // 任务进度 + 完成处理（120ms）。
    {
        let timer_app = Rc::downgrade(app);
        let jobs_status = app.borrow().jobs.status();
        app.borrow()
            .sidebar_timer
            .start(TimerMode::Repeated, Duration::from_millis(120), move || {
                let Some(app) = timer_app.upgrade() else { return };
                let mut app = app.borrow_mut();
                let status = jobs_status.lock().ok();
                let Some(status) = status else { return };
                let running_text = status_text(&status, app.jobs.is_english());
                if app.ui.get_status_job().is_empty() || !running_text.is_empty() {
                    // 走统一入口：这里必须顺手把危险标记复位，否则上一条
                    // 红色提示（如「操作失败…」）会让进度文案也变红。
                    app.set_status_job(running_text.clone());
                }
                if let Some(summary) = status.finished.as_ref().cloned() {
                    drop(status);
                    // 消费完成事件：清空 finished 再处理。
                    if let Ok(mut s) = jobs_status.lock() {
                        s.finished = None;
                    }
                    app.on_job_finished(summary);
                }
            });
    }
    // 侧边栏周期刷新（15s）：驱动器容量、回收站统计、网络位置。
    // 同时承载「空闲时自动同步索引」策略。
    {
        let timer_app = Rc::downgrade(app);
        app.borrow()
            .sidebar_periodic_timer
            .start(TimerMode::Repeated, Duration::from_secs(15), move || {
                if let Some(app) = timer_app.upgrade() {
                    let mut app = app.borrow_mut();
                    app.sync_sidebar();
                    app.maybe_idle_rescan();
                }
            });
    }
}

/// 窗口原生键盘事件：应用级快捷键（非模态，仅常规加速键）。
fn setup_keyboard(app: &Rc<RefCell<App>>) {
    use slint::winit_030::{EventResult, WinitWindowAccessor};
    use slint::winit_030::winit::event::{ElementState, KeyEvent, MouseButton, WindowEvent};
    use slint::winit_030::winit::keyboard::Key;

    let key_app = Rc::downgrade(app);
    app.borrow()
        .ui
        .window()
        .on_winit_window_event(move |_window, event| {
        // 首帧重绘：此时窗口已真正 realize、HWND 与 scale 正确，
        // 一次性应用最大化还原 / 首次启动居中（见 apply_deferred_geometry）。
        #[cfg(windows)]
        if matches!(event, WindowEvent::RedrawRequested) {
            if let Some(rd_app) = key_app.upgrade() {
                apply_deferred_geometry(&rd_app);
            }
            return EventResult::Propagate;
        }

        // 窗口失焦：取消挂起的延迟重命名，否则切到别的应用后后台突然弹出 rename 输入框。
        if matches!(event, WindowEvent::Focused(false)) {
            if let Some(focus_app) = key_app.upgrade() {
                focus_app.borrow_mut().cancel_rename_defer();
                // 窗口失焦时左键的「松开」可能送到别的窗口，这里先清掉按住态，
                // 否则拖选贴边自动滚动的定时器会在后台一直推进选中范围。
                focus_app.borrow().ui.set_mouse_left_pressed(false);
            }
        }

        // 鼠标左键按住状态回填给 UI：Slint 的 TextInput 拖选期间抓住鼠标，
        // 任何 TouchArea 都收不到事件，重命名输入框的「拖选贴边自动滚动」
        // 定时器只能从这里判断拖选是否仍在进行（松开即停）。
        if let WindowEvent::MouseInput {
            state,
            button: MouseButton::Left,
            ..
        } = event
        {
            if let Some(btn_app) = key_app.upgrade() {
                btn_app
                    .borrow()
                    .ui
                    .set_mouse_left_pressed(matches!(state, ElementState::Pressed));
            }
            return EventResult::Propagate;
        }

        // 任务 2.2：外部 → Rove 拖入。winit 对一次投放逐文件发 DroppedFile，
        // 先缓冲，短延迟后统一判定目标并触发移动/复制管线。
        if let WindowEvent::DroppedFile(path) = event {
            let Some(key_app) = key_app.upgrade() else {
                return EventResult::Propagate;
            };
            let mut app = key_app.borrow_mut();
            app.drop_buffer.push(path.to_path_buf());
            let timer_app = key_app.clone();
            app.drop_timer.start(
                TimerMode::SingleShot,
                Duration::from_millis(120),
                move || {
                    timer_app.borrow_mut().process_external_drop();
                },
            );
            return EventResult::PreventDefault;
        }

        // 窗口尺寸/DPI 缩放变化会改变标题栏空白拖拽区的起止坐标，
        // 必须刷新给 WM_NCHITTEST 钩子，否则拖拽热区会停在旧位置。
        #[cfg(windows)]
        if matches!(
            event,
            WindowEvent::Resized(..) | WindowEvent::ScaleFactorChanged { .. }
        ) {
            if let Some(evt_app) = key_app.upgrade() {
                let app = evt_app.borrow();
                // 实时同步实际最大化状态给 Slint：无论点击按钮、双击标题栏，
                // 还是拖拽到屏幕顶端 Aero 贴靠，尺寸一变就读 IsZoomed 回填，
                // 驱动 resize-border-width 动态归零/恢复。
                if let Some(hwnd) = get_window_hwnd(&app.ui) {
                    let is_zoomed =
                        unsafe { windows_sys::Win32::UI::WindowsAndMessaging::IsZoomed(hwnd as _) != 0 };
                    app.ui.set_win_maximized(is_zoomed);
                }
                sync_drag_bounds(&app, "resize");
                drop(app);
                // 尺寸变化后强制下一轮 40ms 轮询重建可见行：行模型里的
                // name-truncated（浮层弹出依据）是按旧几何算的，行内文字的
                // 省略号由 Slint 实时画、两者会割裂（表现为「浮层按旧窗口宽
                // 判断截断」）。置哨兵值让轮询的几何比较必定命中一次。
                evt_app.borrow_mut().list_geom = (-1.0, 0, -1.0, false, -1.0, -1.0);
            }
            return EventResult::Propagate;
        }

        let WindowEvent::KeyboardInput {
            event:
                KeyEvent {
                    logical_key,
                    state: ElementState::Pressed,
                    ..
                },
            ..
        } = event
        else {
            return EventResult::Propagate;
        };

        let Some(key_app) = key_app.upgrade() else {
            return EventResult::Propagate;
        };
        let mut app = key_app.borrow_mut();
        // 焦点在过滤框 / 重命名输入框 / 地址栏编辑框 / 设置页文本框时，按键归输入框所有。
        // 关键：设置页文本框若不在此放行，全局 keymap 会先把 Enter 等按键 PreventDefault 掉，
        // 导致 TextInput 的 accepted 永远不触发（表现为“回车无效、点按钮却可以”）。
        //
        // 动作编辑器（新增/编辑自定义动作）也在此列：它是模态弹窗，里面的文本框
        // 没有逐个上报焦点，统一用“弹窗开着”作为开关，否则 Delete / Enter 等全局按键会把输入吃掉。
        if app.ui.get_filter_has_focus()
            || !app.ui.get_rename_path().is_empty()
            || app.ui.get_path_editing()
            || app.ui.get_settings_text_focus()
            || app.ui.get_settings_editor_open()
            || app.ui.get_network_dialog_open()
        {
            return EventResult::Propagate;
        }
        // 原生菜单打开时前台在菜单线程的窗口上，按键根本不会到 Rove；这里不再需要菜单开关。

        let ctrl = key_down(VK_CONTROL);
        let shift = key_down(VK_SHIFT);
        let alt = key_down(VK_MENU);

        // ===== 声明式快捷键：录制态优先拦截物理键 =====
        if app.recording_shortcut.is_some() {
            // Esc 取消录制：无论取消还是提交，都必须立即清除 UI 高亮态
            if matches!(logical_key, Key::Named(slint::winit_030::winit::keyboard::NamedKey::Escape)) {
                app.recording_shortcut = None;
                app.ui.set_settings_recording_item(slint::SharedString::default());
                return EventResult::PreventDefault;
            }
            if let Some(chord) = crate::ops::keymap::format_chord(logical_key, ctrl, shift, alt) {
                let (id, slot) = app.recording_shortcut.take().unwrap();
                app.ui.set_settings_recording_item(slint::SharedString::default());

                // 差量维护槽位：优先落入已有槽位，越界则追加
                let defaults = app
                    .keymap
                    .action_to_chords
                    .get(&id)
                    .cloned()
                    .unwrap_or_default();
                let slots = app.config.shortcuts.entry(id.clone()).or_insert(defaults);
                if slot < slots.len() {
                    slots[slot] = chord;
                } else {
                    slots.push(chord);
                }
                app.config.save();
                app.keymap = crate::ops::keymap::Keymap::new(&app.config.shortcuts);
                app.sync_settings();
                return EventResult::PreventDefault;
            }
            // 纯修饰键等无效输入：保持录制态，忽略
            return EventResult::PreventDefault;
        }

        // ===== 常规按键：规范化为 Chord 并在 Keymap 中解析 =====
        let Some(chord) = crate::ops::keymap::format_chord(logical_key, ctrl, shift, alt) else {
            return EventResult::Propagate;
        };
        let Some(action) = app.keymap.resolve(&chord).map(|s| s.to_string()) else {
            return EventResult::Propagate;
        };

        // 原生 Shell 文件操作进行中（删除/复制/移动会弹 Shell 自己的确认/进度框）。
        // 实测：Shell 在后台线程调 SHFileOperationW，它的对话框只对自己的线程模态，
        // 我们的主窗仍是 enabled —— 确认框里敲的回车会同时投递到 Rove。
        // 所以这里把所有“会作用到可能已被删路径”的动作全部拦下（导航不受影响）。
        if app.shell_op_running
            && matches!(
                action.as_str(),
                "open" | "delete" | "delete_perm" | "rename" | "paste" | "copy" | "cut"
            )
        {
            if !app.shell_key_suppressed_logged {
                app.shell_key_suppressed_logged = true;
                // 这是已知且预期的路径（Shell 对话框只对后台线程模态），故降为详细级。
                logv!(
                    "[keys] suppress action={} while shell file op running",
                    action
                );
            }
            return EventResult::PreventDefault;
        }

        match action.as_str() {
            "open" => app.open_selection(),
            "rename" => app.start_rename(),
            "delete" => app.delete_selection(true),
            "delete_perm" => app.delete_selection(false),
            "refresh" => app.user_refresh_active(),
            "focus_search" => {
                // 进入搜索框前取消挂起的延迟重命名，否则 400ms 后 rename 输入框
                // 弹出、抢走搜索框焦点。
                app.cancel_rename_defer();
                app.ui.invoke_focus_search_box();
            }
            "clear_filter" => {
                // 走统一入口：记录「回到目录页」的历史并恢复搜索前滚动位置。
                app.ui.set_filter_text(SharedString::default());
                app.apply_search_query("");
            }
            "copy" => app.copy_selection(false),
            "cut" => app.copy_selection(true),
            "paste" => app.paste_clipboard(),
            "select_all" => app.select_all(),
            _ => return EventResult::Propagate,
        }

        EventResult::PreventDefault
    });
}

/// 全部 UI 回调接线。
/// 注意：生成的 MainWindow 不可 Clone，所有回调统一经由 `app.borrow().ui`
/// 临时借用注册；闭包捕获的是 Rc 克隆，与后续的事件循环调用不冲突。
fn wire_callbacks(app: &Rc<RefCell<App>>) {
    let ui = app.borrow().ui.as_weak();
    let upgrade = move || ui.upgrade().expect("MainWindow dropped while wiring callbacks");

    // ---- 标签 ----
    {
        let app = Rc::downgrade(app);
        upgrade().on_new_tab_requested(move || {
            let Some(app) = app.upgrade() else { return };
            let (current, sort) = {
                let app = app.borrow();
                (app.state.active().active().path.clone(), app.state.active().active().sort)
            };
            app.borrow_mut().state.new_tab(current, sort);
            let mut app = app.borrow_mut();
            app.sync_active_tab();
            app.reload_active();
            app.ui.invoke_scroll_tabs_to_end();
            // 「+」新建标签是否进入地址栏编辑由设置开关控制（默认关闭）。
            // sync_active_tab 会清编辑态，所以必须放在整条链路最后。
            // begin-path-edit 预填当前路径并自动聚焦全选。
            if app.config.auto_edit_path_on_new_tab {
                app.ui.invoke_begin_path_edit();
            }
        });
    }
    {
        let app = Rc::downgrade(app);
        upgrade().on_tab_activated(move |id| {
            let Some(app) = app.upgrade() else { return };
            let mut app = app.borrow_mut();
            // 拖动交换松手会补发一次 clicked，丢弃它（400ms 上限，不误伤下次点击）。
            if let Some(at) = app.suppress_tab_click.take() {
                if at.elapsed() < std::time::Duration::from_millis(400) {
                    return;
                }
            }
            // 与 [nc-hit] 对照：有这行说明点击到了 Slint，没有则是被 HTCAPTION 吞了。
            logv!(
                "[tab] click id={} found={} active={}",
                id,
                app.state.tab_by_id(id as u32).is_some(),
                app.state.active_tab
            );
            if let Some(index) = app.state.tab_by_id(id as u32) {
                app.state.active_tab = index;
                app.sync_active_tab();
                // 标签内容已在状态中，直接展示；若正在加载则保持 loading。
                if app.state.active().active().loading {
                    app.reload_active();
                }
            }
        });
    }
    {
        let app = Rc::downgrade(app);
        upgrade().on_tab_closed(move |id| {
            let Some(app) = app.upgrade() else { return };
            let mut app = app.borrow_mut();
            if app.state.close_tab(id as u32) {
                app.sync_active_tab();
            } else if app.state.tabs.len() == 1 {
                // 关闭最后一个标签：销毁 UI，仅保留守护（Phase 2）。
                drop(app);
                schedule_destroy();
            }
        });
    }

    {
        let app = Rc::downgrade(app);
        upgrade().on_tab_drag(move |index, phase, x| {
            let Some(app) = app.upgrade() else { return };
            app.borrow_mut().tab_drag(index as usize, phase as i32, x);
        });
    }

    // ---- 导航 ----
    {
        let app = Rc::downgrade(app);
        upgrade().on_nav_back(move || {
            let Some(app) = app.upgrade() else { return };
            app.borrow_mut().nav_back();
        });
    }
    {
        let app = Rc::downgrade(app);
        upgrade().on_nav_forward(move || {
            let Some(app) = app.upgrade() else { return };
            app.borrow_mut().nav_forward();
        });
    }
    {
        let app = Rc::downgrade(app);
        upgrade().on_nav_up(move || {
            let Some(app) = app.upgrade() else { return };
            app.borrow_mut().nav_up();
        });
    }
    {
        let app = Rc::downgrade(app);
        upgrade().on_breadcrumb_clicked(move |path| {
            let Some(app) = app.upgrade() else { return };
            let path = path.to_string();
            let target = if path.is_empty() {
                None
            } else {
                Some(PathBuf::from(path))
            };
            app.borrow_mut().navigate_active(target);
        });
    }
    {
        let app = Rc::downgrade(app);
        // 地址栏路径编辑：点击面包屑空白处进入，回车提交。
        upgrade().on_path_edit_committed(move |path| {
            let Some(app) = app.upgrade() else { return };
            app.borrow_mut().commit_path_edit(path.as_ref());
        });
    }
    {
        // 诊断：地址栏面包屑流几何。窄窗口出现“地址栏空白”时，看这三个数就知道
        // 是内容宽度算错（content_w <= view_w 但 n > 0 仍看不到）、还是偏移越界。
        upgrade().on_crumb_dbg(move |view_w, content_w, content_x, n| {
            logv!(
                "[crumb] n={} view={:.1} content={:.1} x={:.1}",
                n, view_w, content_w, content_x
            );
        });
    }
    {
        // 诊断：地址栏空白处点击。没这行就说明点击没有被那一层接住。
        upgrade().on_crumb_edit_clicked(move |x, content_x| {
            logv!("[crumb] edit-click x={:.1} content-x={:.1}", x, content_x);
        });
    }
    {
        let app = Rc::downgrade(app);
        // 侧栏分区拖拽排序。
        upgrade().on_sidebar_header_drag(move |index, phase, y| {
            let Some(app) = app.upgrade() else { return };
            app.borrow_mut()
                .sidebar_header_drag(index as usize, phase as i32, y);
        });
    }
    {
        let app = Rc::downgrade(app);
        upgrade().on_sidebar_network_drag(move |index, phase, y| {
            let Some(app) = app.upgrade() else { return };
            app.borrow_mut()
                .sidebar_network_drag(index as usize, phase as i32, y);
        });
    }
    {
        let app = Rc::downgrade(app);
        upgrade().on_sidebar_quick_drag(move |index, phase, y| {
            let Some(app) = app.upgrade() else { return };
            app.borrow_mut()
                .sidebar_quick_drag(index as usize, phase as i32, y);
        });
    }
    {
        let app = Rc::downgrade(app);
        upgrade().on_sidebar_item_activated(move |id| {
            let Some(app) = app.upgrade() else { return };
            app.borrow_mut().sidebar_activated(&id);
        });
    }
    {
        let app = Rc::downgrade(app);
        upgrade().on_sidebar_item_right_clicked(move |id| {
            let Some(app) = app.upgrade() else { return };
            app.borrow_mut().sidebar_right_clicked(&id);
        });
    }
    {
        let app = Rc::downgrade(app);
        upgrade().on_network_dialog_connect(move || {
            let Some(app) = app.upgrade() else { return };
            app.borrow_mut().connect_network_location();
        });
    }
    {
        let app = Rc::downgrade(app);
        upgrade().on_network_dialog_cancel(move || {
            let Some(app) = app.upgrade() else { return };
            app.borrow_mut().close_network_dialog();
        });
    }

    // ---- 文件列表 ----
    {
        let app = Rc::downgrade(app);
        // 图标视图里单选后的文件名浮层：再次点击同样进入重命名。
        let overlay_app = app.clone();
        let hover_app = app.clone();
        let overlay_hover_app = app.clone();
        upgrade().on_file_row_pointer_event(move |index, event, x, y, on_name| {
            let Some(app) = app.upgrade() else { return };
            app.borrow_mut()
                .row_pointer_event(index as usize, &event, x, y, on_name);
        });
        upgrade().on_file_name_overlay_clicked(move || {
            let Some(app) = overlay_app.upgrade() else { return };
            let mut app = app.borrow_mut();
            // 浮层里的 TouchArea 吃掉了点击，格子那边收不到，所以这里要自己处理：
            //  · 该文件还没被选中 → 先选中它（单击选中，与点图标一致）；
            //  · 已经是唯一选中项 → 进入重命名（原有行为）。
            // 不做这一步的话，仅因悬停而弹出的浮层被单击后会毫无反应。
            //
            // 选中/重命名层和悬停层是两套属性：悬停弹出来的那一层只有
            // hover-expand-index 有值，所以两者都要看。悬停优先。
            let hv0 = app.ui.get_file_hover_expand_index();
            let hv1 = app.ui.get_file_hover_expand_index_right();
            let ex0 = app.ui.get_file_expand_index();
            let ex1 = app.ui.get_file_expand_index_right();
            let (pane, target) = if hv0 >= 0 {
                (0usize, hv0)
            } else if hv1 >= 0 {
                (1usize, hv1)
            } else if ex0 >= 0 {
                (0usize, ex0)
            } else if ex1 >= 0 {
                (1usize, ex1)
            } else {
                return;
            };
            if app.state.active().active_pane != pane {
                app.focus_pane(pane);
            }
            let path = app
                .state
                .active()
                .panes
                .get(pane)
                .and_then(|p| p.display_entry(target as usize))
                .map(|e| e.path.clone());
            let Some(path) = path else { return };
            // 双击检测：浮层盖在名称区上，格子自己的 TouchArea 收不到这些点击，
            // 所以双击打开必须在这里自己判（与 row_pointer_event 同一套机制：
            // 有效双击间隔内同一路径第二次点击 = 打开）。
            // 不做的话，长文件名在图标视图里双击会先选中、再进重命名，
            // 而短文件名双击是打开文档，两者不一致。
            let now = std::time::Instant::now();
            if app.last_click.as_ref().is_some_and(|(p, t)| {
                *p == path && t.elapsed().as_millis() < effective_double_click_ms() as u128
            }) {
                app.cancel_rename_defer();
                app.last_click = None;
                if app.state.active().active().selection.len() != 1
                    || !app.state.active().active().selection.contains(&path)
                {
                    let tab = app.state.active_mut().active_mut();
                    tab.selection.clear();
                    tab.selection.insert(path.clone());
                    tab.anchor = Some(path.clone());
                }
                app.activate_row(target as usize);
                return;
            }
            app.last_click = Some((path.clone(), now));
            let selected_single = {
                let tab = app.state.active().active();
                tab.selection.len() == 1 && tab.selection.contains(&path)
            };
            if !selected_single {
                let tab = app.state.active_mut().active_mut();
                tab.selection.clear();
                tab.selection.insert(path.clone());
                tab.anchor = Some(path);
                app.visible_cache = None;
                app.refresh_visible_rows();
                app.sync_status();
                return;
            }
            // 已经是唯一选中项：不要当场进重命名。与行内的名称点击一致，
            // 先挂一个延时，留出双击窗口：窗口内再来一击 = 双击打开（在上面
            // 已处理），否则窗口过后才进重命名。
            // 当场 start-rename 的话，重命名输入框会立刻顶掉浮层，
            // 第二次点击落到输入框上，双击打开就永远做不到。
            app.schedule_rename(path);
        });
        upgrade().on_file_overlay_hover(move |pane, inside| {
            let Some(app) = overlay_hover_app.upgrade() else { return };
            let mut app = app.borrow_mut();
            let p = pane.clamp(0, 1) as usize;
            app.overlay_hovered[p] = inside;
            if !inside {
                // 指针离开浮层：若格子那边也已经不在悬停，就把悬停索引清掉，
                // 让浮层正常收起（否则会一直挂着不走）。
                let effective = if app.cell_hovered[p] >= 0 {
                    app.cell_hovered[p]
                } else {
                    -1
                };
                if app.hover_display_index[p] != effective {
                    app.hover_display_index[p] = effective;
                    app.expand_sig = None;
                    app.sync_expand_targets();
                }
            }
        });
        upgrade().on_file_hover_row(move |pane, index| {
            let Some(app) = hover_app.upgrade() else { return };
            let mut app = app.borrow_mut();
            let p = pane.clamp(0, 1) as usize;
            app.cell_hovered[p] = index;
            // 指针还在浮层上时，格子自己的 hover=false 不能把悬停清掉，
            // 否则浮层会随鼠标下移到文件名上而反复闪烁。
            let effective = if index >= 0 {
                index
            } else if app.overlay_hovered[p] {
                app.hover_display_index[p]
            } else {
                -1
            };
            if app.hover_display_index[p] == effective {
                return;
            }
            app.hover_display_index[p] = effective;
            // 重算图标视图的名称浮层目标（选中 / 悬停 / 重命名）。
            // 小图标视图有自己的行内浮层，不需要这里重建嵌套模型。
            let icons_big = matches!(
                app.config.view_mode.as_str(),
                "icons-medium" | "icons-large" | "icons-xl"
            );
            if icons_big {
                app.expand_sig = None;
                app.sync_expand_targets();
            }
        });
    }
    {
        upgrade().on_file_row_double_clicked(move |_index| {
            // 双击激活已由 row_pointer_event 的 Rust 侧判定处理
            // （500ms 阈值，不受行模型重建影响），这里不再重复触发，
            // 否则导航/还原会被执行两次。
        });
    }
    {
        let app = Rc::downgrade(app);
        upgrade().on_file_header_clicked(move |key| {
            let Some(app) = app.upgrade() else { return };
            app.borrow_mut().header_clicked(key.as_ref());
        });
    }
    {
        let app = Rc::downgrade(app);
        upgrade().on_file_scroll_dbg(move |pane, offset, ch, vh, miny, cy| {
            let Some(app) = app.upgrade() else { return };
            let app = app.borrow();
            // 只在重命名期间落日志（正是排查窗口），平时完全静默。
            if app.ui.get_rename_path().is_empty() {
                return;
            }
            crate::logv!(
                "[scroll-dbg] pane={} pos={:.1} content_h={} view_h={} min_y={} content_y={}",
                pane,
                offset,
                ch,
                vh,
                miny,
                cy
            );
        });
    }
    {
        let app = Rc::downgrade(app);
        upgrade().on_file_scroll_changed(move |offset| {
            let Some(app) = app.upgrade() else { return };
            let mut app = app.borrow_mut();
            // 核心修复：切标签期间忽略因布局 Clamp 产生的残留滚动事件。
            if app.syncing_tab { return; }

            // 核心修复：历史导航等待数据期间，UI 会因列表清空把滚动条自动 Clamp 到 0，
            // 并触发 scroll-changed 假回调。必须全部忽略（含程序 push 的回显——
            // set-scroll-y 的 suppress 只能覆盖赋值那一瞬，changed 是下一帧才跑，
            // 回显仍会打进来；如果用“非 0 就当真实滚动”去解标记，就会在数据到之前
            // 提前解掉保护，随后真实的 clamp-to-0 就会把位置冲掉——这就是记忆丢失的原因）。
            // 标记的解除统一由 apply_list_result 在数据就位时负责，且有兵底清理保证不会卡死。
            if app.state.tabs[app.state.active_tab]
                .panes
                .get(0)
                .is_some_and(|p| p.pending_scroll_restore)
            {
                logv!("[scroll] ignore while restoring (left) offset={:.1}", offset);
                return;
            }
            // 排查：重命名期间用户的每次滚动都记下来，用于确认“滚动条往上跑”
            // 是用户/惯性造成的，还是程序 clamp 反向推的。
            if !app.ui.get_rename_path().is_empty() {
                logv!("[rename] scroll (left) -> {:.1}", offset);
            }
            // 重命名期间拦下 Slint 的自动 reveal 滚动（详见 rename_scroll_guard 注释）。
            if app.reject_spurious_rename_scroll(0, offset) {
                return;
            }
            // 左栏固定映射 pane 0（活动栏可能已切到右侧）。
            let active_tab = app.state.active_tab;
            if let Some(pane) = app.state.tabs[active_tab].panes.get_mut(0) {
                pane.scroll_y = offset;
            }
            app.refresh_visible_rows();
        });
    }
    {
        let app = Rc::downgrade(app);
        upgrade().on_file_rename_dbg(move |pane, kind, has_focus| {
            let Some(app) = app.upgrade() else { return };
            let app = app.borrow();
            crate::logv!(
                "[rename-dbg] pane={} kind={} has_focus={} rename_path_len={}",
                pane,
                kind,
                has_focus,
                app.ui.get_rename_path().len()
            );
        });
    }
    {
        let app = Rc::downgrade(app);
        #[cfg(windows)]
        if let Some(app_rc) = app.upgrade() {
            // 只把几何容器 clone 进回调：回调不再触碰 App 的 RefCell，因此即使
            // 触发点落在「App 正被可变借用」的路径上也不会 panic。
            let crumb_rects = app_rc.borrow().crumb_rects.clone();
            // 面包屑各段自己上报窗口几何（Slint 函数访问不到 for 里的 id）。
            upgrade().on_crumb_rect(move |index, x, y, w, h| {
                let i = index.max(0) as usize;
                let mut store = crumb_rects.borrow_mut();
                if store.len() <= i {
                    store.resize(i + 1, (0.0, 0.0, 0.0, 0.0));
                }
                store[i] = (x, y, w, h);
            });
        }

        #[cfg(windows)]
        if let Some(app_rc) = app.upgrade() {
            // 同理：侧栏几何只借用独立容器，不碰 App 的 RefCell。
            let sidebar_rects = app_rc.borrow().sidebar_rects.clone();
            upgrade().on_sidebar_rect(move |id, x, y, w, h| {
                sidebar_rects
                    .borrow_mut()
                    .insert(id.to_string(), (x, y, w, h));
            });
        }

        #[cfg(windows)]
        {
            // 列宽拖动/双击结束后回传，按当前目录存进配置并落盘。
            let app = app.clone();
            upgrade().on_file_col_resized(move |pane, index, width, name_w| {
                let Some(app) = app.upgrade() else { return };
                let mut app = app.borrow_mut();
                record_col_width(&mut app, pane.max(0) as usize, index, width, name_w);
            });
        }

        upgrade().on_file_name_debug(move |pane, info, name| {
            let Some(app) = app.upgrade() else { return };
            // 纯诊断回调：重命名提交等路径会在持有可变借用时同步触发它，
            // 拿不到借用就跳过（不能 borrow()，否则 RefCell 直接 panic）。
            let Ok(app) = app.try_borrow() else { return };
            let n = name.to_string();
            let total = n.chars().count();
            let head: String = n.chars().take(14).collect();
            let tail: String = {
                let v: Vec<char> = n.chars().collect();
                v[total.saturating_sub(14)..].iter().collect()
            };
            // 关键：判断该行走「普通多行 Text」还是「搜索高亮 HighlightName」。
            // HighlightName 是单行组件，若 name-parts 非空就能解释多行名字异常。
            let rows = if pane == 1 {
                app.ui.get_file_rows_right()
            } else {
                app.ui.get_file_rows()
            };
            let mut parts = -1i32; // -1 = 当前可见窗口里没找到该行
            for i in 0..rows.row_count() {
                if let Some(r) = rows.row_data(i) {
                    if r.name.as_str() == n.as_str() {
                        parts = r.name_parts.row_count() as i32;
                        break;
                    }
                }
            }
            // 诊断信息留在详细级：ROVE_LOG=2 才会输出，常态 1 级日志保持干净。
            crate::logv!(
                "[name] pane={} {} len={} head={:?} tail={:?} parts={}",
                pane,
                info,
                total,
                head,
                tail,
                parts
            );
        });
    }
    {
        let app = Rc::downgrade(app);
        upgrade().on_file_sb_debug(move |pane, info| {
            let Some(app) = app.upgrade() else { return };
            let _app = app.borrow();
            crate::logv!("[sb] pane={} {}", pane, info);
        });
    }
    {
        let app = Rc::downgrade(app);
        upgrade().on_file_rename_text_changed(move |pane, text| {
            let Some(app) = app.upgrade() else { return };
            // 输入达到系统上限（255 UTF-16 码元）时**直接退出重命名**：
            // 不回写文本（回写会让 TextInput 光标错位、把字插进扩展名内部），
            // 也不提交截断后的名字（用户输入与实际生效不一致更糟），
            // 浮层同时不会无限往下长。退出动作与 Esc 取消完全一致。
            if utf16_len(text.as_str()) > MAX_FILE_NAME_UNITS {
                let mut guard = app.borrow_mut();
                guard.ui.set_rename_path(SharedString::default());
                guard.sync_expand_targets();
                guard.drag_pending = None;
                guard.ui.invoke_set_drag_armed(false);
                let too_long = guard.trf(
                    "名称超过 {} 个字符上限，已退出重命名",
                    &[MAX_FILE_NAME_UNITS.to_string()],
                );
                guard.set_status_job(too_long);
                return;
            }
            let app = app.borrow();
            // 不在输入过程中回写文本：Slint 的 TextInput 被外部改 text 后，
            // 光标（按字符索引维护）会错位，用户继续输入就会插进扩展名内部，
            // 表现为 `.mp4` 被插成 `.xmpx4`。TextInput 也没有 max-length 之类的
            // 声明式限长属性，所以长度上限统一在提交时由 clamp_file_name()
            // 处理（截断主名、保留扩展名），输入阶段不打断用户。
            //
            // 浮层行按输入框里的真实文本重算，保证与显示内容一致。
            let width = app.ui.get_file_expand_box_w();
            let box_w = if width > 1.0 {
                width
            } else {
                (app.ui.get_cell_w_out() - 8.0).max(24.0)
            };
            let parts = vec![(text.to_string(), false)];
            let lines = grid_display_lines(&parts, (box_w - 8.0).max(20.0), 64);
            let model = ModelRc::new(VecModel::from(
                lines
                    .into_iter()
                    .map(|line| GridLine {
                        parts: ModelRc::new(VecModel::from(
                            line.into_iter()
                                .map(|(t, hl)| NamePart {
                                    text: t.into(),
                                    hl,
                                })
                                .collect::<Vec<_>>(),
                        )),
                    })
                    .collect::<Vec<_>>(),
            ));
            if pane == 1 {
                app.ui.set_file_expand_lines_right(model);
            } else {
                app.ui.set_file_expand_lines(model);
            }
        });
    }
    {
        let app = Rc::downgrade(app);
        let wheel_app = app.clone();
        upgrade().on_file_scroll_user_input(move |_pane| {
            let Some(app) = app.upgrade() else { return };
            let mut app = app.borrow_mut();
            // 用户真的在滚轮/拖滚动条：立即解除重命名期的假滚动拦截。
            app.rename_scroll_guard = None;
        });
        upgrade().on_file_wheel_view(move |delta| {
            // Slint 的 WheelEvent 不带修饰键，用 Win32 的 GetKeyState 同步轮询。
            if !key_down(VK_CONTROL) {
                return false;
            }
            let Some(app) = wheel_app.upgrade() else { return false };
            app.borrow_mut().cycle_view_mode(delta > 0.0);
            true
        });
    }
    {
        let app = Rc::downgrade(app);
        upgrade().on_file_scroll_changed_right(move |offset| {
            let Some(app) = app.upgrade() else { return };
            let mut app = app.borrow_mut();
            // 核心修复：切标签期间忽略因布局 Clamp 产生的残留滚动事件。
            if app.syncing_tab { return; }

            // 核心修复：同上，保护分屏右侧面板。
            if app.state.tabs[app.state.active_tab]
                .panes
                .get(1)
                .is_some_and(|p| p.pending_scroll_restore)
            {
                logv!("[scroll] ignore while restoring (right) offset={:.1}", offset);
                return;
            }
            if app.reject_spurious_rename_scroll(1, offset) {
                return;
            }
            let active_tab = app.state.active_tab;
            if let Some(pane) = app.state.tabs[active_tab].panes.get_mut(1) {
                pane.scroll_y = offset;
            }
            app.refresh_visible_rows();
        });
    }
    {
        let app = Rc::downgrade(app);
        upgrade().on_toggle_split(move || {
            let Some(app) = app.upgrade() else { return };
            app.borrow_mut().toggle_split();
        });
    }
    {
        let app = Rc::downgrade(app);
        upgrade().on_pane_focused(move |index| {
            let Some(app) = app.upgrade() else { return };
            app.borrow_mut().focus_pane(index.max(0) as usize);
        });
    }
    {
        let app = Rc::downgrade(app);
        upgrade().on_split_ratio_changed(move |ratio| {
            let Some(app) = app.upgrade() else { return };
            let mut app = app.borrow_mut();
            let active_tab = app.state.active_tab;
            if let Some(tab) = app.state.tabs.get_mut(active_tab) {
                tab.split_ratio = ratio.clamp(0.2, 0.8);
            }
        });
    }
    {
        let app = Rc::downgrade(app);
        upgrade().on_file_background_pointer_event(move |event, x, y| {
            let Some(app) = app.upgrade() else { return };
            app.borrow_mut()
                .background_pointer_event(&event, x, y);
        });
    }
    {
        let app = Rc::downgrade(app);
        upgrade().on_file_rubber_band_selected(move |x1, y1, x2, y2| {
            let Some(app) = app.upgrade() else { return };
            let mut app = app.borrow_mut();
            app.handle_rubber_band_select(x1, y1, x2, y2);
        });
    }
    {
        let app = Rc::downgrade(app);
        upgrade().on_file_rubber_band_move(move |x1, y1, x2, y2| {
            let Some(app) = app.upgrade() else { return };
            let mut app = app.borrow_mut();
            app.handle_rubber_band_move(x1, y1, x2, y2);
        });
    }
    {
        let app = Rc::downgrade(app);
        upgrade().on_file_rename_committed(move |path, new_name| {
            logv!("[rename] UI committed path={} name={}", path, new_name);
            let Some(app) = app.upgrade() else { return };
            app.borrow_mut()
                .commit_rename(&PathBuf::from(path.as_str()), new_name.as_str());
        });
    }
    {
        let app = Rc::downgrade(app);
        upgrade().on_file_rename_cancelled(move || {
            let Some(app) = app.upgrade() else { return };
            let mut app = app.borrow_mut();
            app.ui.set_rename_path(SharedString::default());
            // 退出重命名后，图标视图的展开层回到“只显示名字 / 隐藏”。
            app.sync_expand_targets();
            // 核心修复：取消重命名时也必须解锁，否则锁残留 → 滚轮冻结。
            app.drag_pending = None;
            app.ui.invoke_set_drag_armed(false);
        });
    }

    // ---- 暂存盘 ----
    {
        let app = Rc::downgrade(app);
        upgrade().on_staging_item_removed(move |index| {
            let Some(app) = app.upgrade() else { return };
            let mut app = app.borrow_mut();
            let index = index as usize;
            if index < app.state.staging.len() {
                app.state.staging.remove(index);
                app.sync_staging();
            }
        });
    }
    {
        let app = Rc::downgrade(app);
        upgrade().on_staging_clear_requested(move || {
            let Some(app) = app.upgrade() else { return };
            let mut app = app.borrow_mut();
            app.state.staging.clear();
            app.sync_staging();
        });
    }
    {
        let app = Rc::downgrade(app);
        upgrade().on_staging_copy_here_requested(move || {
            let Some(app) = app.upgrade() else { return };
            app.borrow_mut().staging_transfer(false);
        });
    }
    {
        let app = Rc::downgrade(app);
        upgrade().on_staging_move_here_requested(move || {
            let Some(app) = app.upgrade() else { return };
            app.borrow_mut().staging_transfer(true);
        });
    }
    {
        let app = Rc::downgrade(app);
        // 折叠/展开落到 config：启动时 set_staging_expanded 灌的是同值，
        // 幂等判断会挡掉那次无谓的 save。
        upgrade().on_staging_expanded_changed(move |expanded| {
            let Some(app) = app.upgrade() else { return };
            let mut app = app.borrow_mut();
            if app.config.staging_expanded == expanded {
                return;
            }
            app.config.staging_expanded = expanded;
            app.config.save();
        });
    }

    // ---- 工具栏 ----
    {
        let app = Rc::downgrade(app);
        upgrade().on_filter_edited(move |text| {
            let Some(app) = app.upgrade() else { return };
            let mut app = app.borrow_mut();
            app.apply_search_query(&text.to_string());
        });
    }
    {
        let app = Rc::downgrade(app);
        upgrade().on_new_folder_requested(move || {
            let Some(app) = app.upgrade() else { return };
            app.borrow_mut().create_folder();
        });
    }
    {
        let app = Rc::downgrade(app);
        upgrade().on_refresh_requested(move || {
            let Some(app) = app.upgrade() else { return };
            let mut app = app.borrow_mut();
            app.user_refresh_active();
            app.sync_sidebar();
        });
    }
    {
        let app = Rc::downgrade(app);
        upgrade().on_paste_requested(move || {
            let Some(app) = app.upgrade() else { return };
            app.borrow_mut().paste_clipboard();
        });
    }
    {
        let app = Rc::downgrade(app);
        upgrade().on_view_picker_requested(move || {
            let Some(app) = app.upgrade() else { return };
            app.borrow_mut().open_view_picker();
        });
    }
    {
        let app = Rc::downgrade(app);
        upgrade().on_settings_requested(move || {
            let Some(app) = app.upgrade() else { return };
            // force_subpage = false：已打开过则仅聚焦，不改变用户正在看的子页面
            app.borrow_mut()
                .open_settings_tab(crate::state::SettingsSubpage::General, false);
        });
    }
    {
        let app = Rc::downgrade(app);
        upgrade().on_settings_nav_changed(move |id| {
            let Some(app) = app.upgrade() else { return };
            let mut app = app.borrow_mut();
            let subpage = match id.as_str() {
                "search" => crate::state::SettingsSubpage::SearchAndIndex,
                "context" => crate::state::SettingsSubpage::ContextMenu,
                "shortcuts" => crate::state::SettingsSubpage::Shortcuts,
                _ => crate::state::SettingsSubpage::General,
            };
            app.state.active_mut().settings_subpage = subpage;
            app.sync_settings();
            app.sync_tabs(); // 修复核心：点击后立即刷新标签栏标题，无需切标签！
        });
    }

    // ---- 搜索与索引设置页交互 ----
    {
        let app = Rc::downgrade(app);
        upgrade().on_search_rescan_clicked(move || {
            let Some(app) = app.upgrade() else { return };
            crate::search::index::rebuild();
            let app = app.borrow();
            app.sync_settings();
            app.set_status_job(app.tr("正在重新扫描驱动器并构建索引..."));
        });
    }
    {
        let app = Rc::downgrade(app);
        upgrade().on_search_drive_toggled(move |letter, indexed| {
            let Some(app) = app.upgrade() else { return };
            let mut app = app.borrow_mut();
            let letter = letter.to_string().to_ascii_uppercase();
            if indexed {
                app.config.search_excluded_drives.retain(|d| d != &letter);
            } else if !app.config.search_excluded_drives.contains(&letter) {
                app.config.search_excluded_drives.push(letter);
            }
            app.config.save();
            reindex_if_active();
            app.sync_settings();
        });
    }
    {
        // 核心修复：在后台线程弹出文件夹选择框，避免阻塞 Slint 主线程导致吸附窗卡死无法渲染。
        // App 句柄非 Send，故后台线程只负责弹窗取路径，结果通过 SESSION 在主线程事件循环内写回。
        upgrade().on_search_pick_folder_clicked(move || {
            std::thread::spawn(|| {
                // pick_folder_path 内部已自行初始化 COM(STA)，无需在此重复。
                if let Some(folder) = crate::sys::pick::pick_folder_path() {
                    let _ = slint::invoke_from_event_loop(move || {
                        let Some(app) = SESSION.with(|slot| slot.borrow().clone()) else { return };
                        let mut app = app.borrow_mut();
                        if !app.config.search_excluded_paths.contains(&folder) {
                            // 头插：新加的排在最左侧，避免被横向滚动条遮挡
                            app.config.search_excluded_paths.insert(0, folder);
                            app.config.save();
                            reindex_if_active();
                            app.sync_settings();
                        }
                    });
                }
            });
        });
    }
    {
        let app = Rc::downgrade(app);
        upgrade().on_search_add_exclusion_clicked(move || {
            let Some(app) = app.upgrade() else { return };
            let mut app = app.borrow_mut();
            let text = app.ui.get_search_exclude_input().trim().to_string();

            if text.is_empty() {
                return;
            }

            if !app.config.search_excluded_paths.contains(&text) {
                // 头插：新加的排在最左侧
                app.config.search_excluded_paths.insert(0, text);
                app.config.save();
                reindex_if_active();
                app.sync_settings();
            }
            // 添加后清空输入框。
            app.ui.set_search_exclude_input(SharedString::default());
        });
    }
    {
        let app = Rc::downgrade(app);
        upgrade().on_search_reset_exclusions_clicked(move || {
            let Some(app) = app.upgrade() else { return };
            let mut app = app.borrow_mut();
            // 一键清空自定义规则（恢复至仅有硬保护内置项的状态）。
            if !app.config.search_excluded_paths.is_empty() {
                app.config.search_excluded_paths.clear();
                app.config.save();
                reindex_if_active();
                app.sync_settings();
            }
        });
    }
    {
        let app = Rc::downgrade(app);
        upgrade().on_search_remove_exclusion_clicked(move |path| {
            let Some(app) = app.upgrade() else { return };
            let mut app = app.borrow_mut();
            let p_str = path.to_string();
            app.config.search_excluded_paths.retain(|p| p != &p_str);
            app.config.save();
            reindex_if_active();
            app.sync_settings();
        });
    }
    {
        let app = Rc::downgrade(app);
        upgrade().on_menu_hide_add_clicked(move || {
            let Some(app) = app.upgrade() else { return };
            let mut app = app.borrow_mut();
            let text = app.ui.get_menu_hide_input().trim().to_string();
            if text.is_empty() {
                return;
            }
            if !app.config.context_menu_hide.contains(&text) {
                // 头插：新加的排在最左侧
                app.config.context_menu_hide.insert(0, text);
                app.config.save();
                app.sync_settings();
            }
            // 添加后清空输入框。
            app.ui.set_menu_hide_input(SharedString::default());
        });
    }
    {
        let app = Rc::downgrade(app);
        upgrade().on_menu_hide_remove_clicked(move |keyword| {
            let Some(app) = app.upgrade() else { return };
            let mut app = app.borrow_mut();
            let k = keyword.to_string();
            let before = app.config.context_menu_hide.len();
            app.config.context_menu_hide.retain(|x| x != &k);
            if app.config.context_menu_hide.len() != before {
                app.config.save();
                app.sync_settings();
            }
        });
    }
    {
        let app = Rc::downgrade(app);
        upgrade().on_menu_hide_restore_clicked(move || {
            let Some(app) = app.upgrade() else { return };
            let mut app = app.borrow_mut();
            // 恢复为默认项（含用户自定义的全部清掉，与搜索页「恢复」语义一致）。
            let defaults = crate::config::default_context_menu_hide();
            if app.config.context_menu_hide != defaults {
                app.config.context_menu_hide = defaults;
                app.config.save();
                app.sync_settings();
            }
        });
    }
    {
        let app = Rc::downgrade(app);
        upgrade().on_search_idle_toggled(move |enabled| {
            let Some(app) = app.upgrade() else { return };
            let mut app = app.borrow_mut();
            app.config.search_idle_rescan_minutes = if enabled { 60 } else { 0 };
            app.config.save();
            app.sync_settings();
        });
    }
    {
        let app = Rc::downgrade(app);
        upgrade().on_search_idle_minutes_changed(move |mins| {
            let Some(app) = app.upgrade() else { return };
            let mut app = app.borrow_mut();
            app.config.search_idle_rescan_minutes = mins.clamp(10, 360) as u32;
            app.config.save();
        });
    }
    {
        let app = Rc::downgrade(app);
        upgrade().on_search_max_results_changed(move |res| {
            let Some(app) = app.upgrade() else { return };
            let mut app = app.borrow_mut();
            app.config.search_max_results = res.clamp(100, 10000) as usize;
            app.config.save();
        });
    }
    {
        let app = Rc::downgrade(app);
        upgrade().on_settings_shortcut_record(move |id, slot| {
            let Some(app) = app.upgrade() else { return };
            let mut app = app.borrow_mut();
            let key = format!("{}:{}", id, slot);
            app.recording_shortcut = Some((id.to_string(), slot as usize));
            app.ui.set_settings_recording_item(key.into());
        });
    }
    {
        let app = Rc::downgrade(app);
        upgrade().on_settings_shortcut_reset(move |id, _slot| {
            let Some(app) = app.upgrade() else { return };
            let mut app = app.borrow_mut();
            app.config.shortcuts.remove(id.as_str());
            app.config.save();
            app.keymap = crate::ops::keymap::Keymap::new(&app.config.shortcuts);
            app.sync_settings();
        });
    }
    {
        let app = Rc::downgrade(app);
        upgrade().on_settings_shortcut_unbind(move |id, slot| {
            let Some(app) = app.upgrade() else { return };
            let mut app = app.borrow_mut();
            let id_str = id.to_string();
            let defaults = app
                .keymap
                .action_to_chords
                .get(&id_str)
                .cloned()
                .unwrap_or_default();
            let slots = app.config.shortcuts.entry(id_str).or_insert(defaults);
            if (slot as usize) < slots.len() {
                slots[slot as usize] = String::new(); // 清空，界面显示为 "—"
            }
            app.config.save();
            app.keymap = crate::ops::keymap::Keymap::new(&app.config.shortcuts);
            app.sync_settings();
        });
    }
    {
        let app = Rc::downgrade(app);
        upgrade().on_settings_shortcut_add_alternate(move |id| {
            let Some(app) = app.upgrade() else { return };
            let mut app = app.borrow_mut();
            let id_str = id.to_string();
            let defaults = app
                .keymap
                .action_to_chords
                .get(&id_str)
                .cloned()
                .unwrap_or_default();
            let slots = app.config.shortcuts.entry(id_str).or_insert(defaults);
            slots.push(String::new()); // 追加空白备用槽位
            app.config.save();
            app.keymap = crate::ops::keymap::Keymap::new(&app.config.shortcuts);
            app.sync_settings();
        });
    }
    {
        let app = Rc::downgrade(app);
        upgrade().on_settings_shortcut_remove_alternate(move |id, slot| {
            let Some(app) = app.upgrade() else { return };
            let mut app = app.borrow_mut();
            let id_str = id.to_string();
            let defaults = app
                .keymap
                .action_to_chords
                .get(&id_str)
                .cloned()
                .unwrap_or_default();
            let slots = app.config.shortcuts.entry(id_str).or_insert(defaults);
            let s = slot as usize;
            // 仅允许移除备用槽位（slot > 0），首项为主槽位不可移除
            if s > 0 && s < slots.len() {
                slots.remove(s);
            }
            app.config.save();
            app.keymap = crate::ops::keymap::Keymap::new(&app.config.shortcuts);
            app.sync_settings();
        });
    }
    {
        let app = Rc::downgrade(app);
        upgrade().on_settings_shortcut_reset_all(move || {
            let Some(app) = app.upgrade() else { return };
            let mut app = app.borrow_mut();
            app.config.shortcuts.clear();
            app.config.save();
            app.keymap = crate::ops::keymap::Keymap::new(&app.config.shortcuts);
            app.sync_settings();
        });
    }
    {
        let app = app.clone();
        let weak = Rc::downgrade(&app);
        app.borrow().jump_window.on_text_changed(move |query| {
            let Some(app) = weak.upgrade() else { return };
            let app = app.borrow_mut();

            // 空查询：列出最新的 5 个历史目录（含 clear-query 的程序化清空）
            if query.is_empty() {
                let mut hits = crate::search::jump::search("", None);
                
                // 核心新增：将 Rove 当前聚焦的目录插入到最前面
                let current_path = app.state.active().active().path.clone();
                if let Some(p) = current_path {
                    if p != crate::fs::drives::recycle_virtual_path() {
                        let name = p.file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_else(|| p.to_string_lossy().into_owned());
                        
                        let ch = crate::search::jump::JumpHit {
                            name: name.clone(),
                            path: p.clone(),
                            score: 0,
                            name_parts: vec![
                                (name.clone(), false), 
                                (format!(" ({})", app.tr("当前")), true)
                            ],
                            is_local: true,
                            is_history: false,
                            last_time: 0,
                        };
                        
                        hits.retain(|h| h.path != ch.path);
                        hits.insert(0, ch);
                        hits.truncate(6); // 允许比平时多展示一个
                    }
                }

                let results: Vec<JumpResult> = hits
                    .into_iter()
                    .map(|hit| JumpResult {
                        name: hit.name.into(),
                        path: hit.path.to_string_lossy().into_owned().into(),
                        name_parts: ModelRc::new(VecModel::from(
                            hit.name_parts
                                .into_iter()
                                .map(|(t, hl)| NamePart { text: t.into(), hl })
                                .collect::<Vec<_>>(),
                        )),
                        is_history: hit.is_history,
                    })
                    .collect();

                app.jump_window.set_results(ModelRc::new(VecModel::from(results)));

                // bottom 吸附时窗口高度随结果数变化，需重新锚定底边：
                // bottom 延迟一轮（等 Slint 完成 height 重算后原子锚定），top 立即执行
                #[cfg(windows)]
                if let Some((gx, gy, gw, ref gplacement)) = app.jump_geom {
                    if gplacement == "bottom" {
                        reposition_bottom_next_tick(gx, gy, gw, gplacement.clone());
                    } else {
                        position_jump_window(&app, gx, gy, gw, gplacement);
                    }
                }
                return;
            }

            // 🎯 核心补丁：每次输入实时从对话框提取最新路径，避免跳转后目录滞后
            #[cfg(windows)]
            let current_latest_dir = app
                .jump_target
                .as_ref()
                .and_then(|(hwnd, _)| crate::sys::dialog_jump::extract_current_folder(*hwnd as _));
            #[cfg(not(windows))]
            let current_latest_dir: Option<std::path::PathBuf> = None;

            let hits = crate::search::jump::search(&query, current_latest_dir.as_deref());
            let results: Vec<JumpResult> = hits
                .into_iter()
                .map(|hit| JumpResult {
                    name: hit.name.into(),
                    path: hit.path.to_string_lossy().into_owned().into(),
                    name_parts: ModelRc::new(VecModel::from(
                        hit.name_parts
                            .into_iter()
                            .map(|(t, hl)| NamePart { text: t.into(), hl })
                            .collect::<Vec<_>>(),
                    )),
                    is_history: hit.is_history,
                })
                .collect();

            app.jump_window.set_results(ModelRc::new(VecModel::from(results)));

            // bottom 吸附时窗口高度随结果数变化，需重新锚定底边：
            // bottom 延迟一轮（等 Slint 完成 height 重算后原子锚定），top 立即执行
            #[cfg(windows)]
            if let Some((gx, gy, gw, ref gplacement)) = app.jump_geom {
                if gplacement == "bottom" {
                    reposition_bottom_next_tick(gx, gy, gw, gplacement.clone());
                } else {
                    position_jump_window(&app, gx, gy, gw, gplacement);
                }
            }
        });
    }
    #[cfg(windows)]
    {
        let app = app.clone();
        let weak = Rc::downgrade(&app);
        app.borrow().jump_window.on_focus_changed(move || {
            let Some(app) = weak.upgrade() else { return };
            let app = app.borrow();

            // 核心修复：吸附窗获得焦点且输入框为空时，主动拉取一次历史记录
            if app.jump_window.get_query().is_empty() {
                let mut hits = crate::search::jump::search("", None);
                
                // 核心新增：将 Rove 当前聚焦的目录插入到最前面
                let current_path = app.state.active().active().path.clone();
                if let Some(p) = current_path {
                    if p != crate::fs::drives::recycle_virtual_path() {
                        let name = p.file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_else(|| p.to_string_lossy().into_owned());
                        
                        let ch = crate::search::jump::JumpHit {
                            name: name.clone(),
                            path: p.clone(),
                            score: 0,
                            name_parts: vec![
                                (name.clone(), false), 
                                (format!(" ({})", app.tr("当前")), true)
                            ],
                            is_local: true,
                            is_history: false,
                            last_time: 0,
                        };
                        
                        hits.retain(|h| h.path != ch.path);
                        hits.insert(0, ch);
                        hits.truncate(6); // 允许比平时多展示一个
                    }
                }

                let results: Vec<JumpResult> = hits
                    .into_iter()
                    .map(|hit| JumpResult {
                        name: hit.name.into(),
                        path: hit.path.to_string_lossy().into_owned().into(),
                        name_parts: ModelRc::new(VecModel::from(
                            hit.name_parts
                                .into_iter()
                                .map(|(t, hl)| NamePart { text: t.into(), hl })
                                .collect::<Vec<_>>(),
                        )),
                        is_history: hit.is_history,
                    })
                    .collect();
                app.jump_window.set_results(ModelRc::new(VecModel::from(results)));
            }

            // 失焦收缩 / 聚焦展开都会改变窗口高度，bottom 吸附需重新锚定底边：
            if let Some((gx, gy, gw, ref gplacement)) = app.jump_geom {
                if gplacement == "bottom" {
                    reposition_bottom_next_tick(gx, gy, gw, gplacement.clone());
                } else {
                    position_jump_window(&app, gx, gy, gw, gplacement);
                }
            }
        });
    }
    {
        let app = app.clone();
        let weak = Rc::downgrade(&app);
        app.borrow().jump_window.on_item_activated(move |_path| {
            let Some(app) = weak.upgrade() else { return };
            let jump_target = app.borrow().jump_target.clone();
            if let Some((_hwnd, _)) = jump_target {
                #[cfg(windows)]
                let _ = crate::sys::dialog_jump::navigate_dialog(_hwnd as _, Path::new(_path.as_str()));
            }
            // 修复：将写历史记录丢到后台线程，避免同步磁盘 IO 导致主线程 IME 上下文丢失
            let path_str = _path.to_string();
            std::thread::spawn(move || {
                crate::search::jump::record_history(Path::new(&path_str));
            });

            app.borrow().jump_window.invoke_clear_query();
        });
        // 吸附窗是独立 Window，主窗口的 winit 事件钩子收不到它的鼠标事件；
        // 单独挂一个，把左键按住状态回填给它的输入框（拖选贴边自动滚动用）。
        {
            use slint::winit_030::winit::event::{ElementState, MouseButton, WindowEvent};
            use slint::winit_030::{EventResult, WinitWindowAccessor};
            let jw = app.borrow().jump_window.clone_strong();
            app.borrow()
                .jump_window
                .window()
                .on_winit_window_event(move |_w, event| {
                    if let WindowEvent::MouseInput {
                        state,
                        button: MouseButton::Left,
                        ..
                    } = event
                    {
                        jw.set_mouse_left_pressed(matches!(state, ElementState::Pressed));
                    }
                    // 与主窗口同理：失焦后“松开”可能收不到，先清掉按住态，
                    // 避免自动滚动定时器在后台继续跑。
                    if matches!(event, WindowEvent::Focused(false)) {
                        jw.set_mouse_left_pressed(false);
                    }
                    EventResult::Propagate
                });
        }
    }
    {
        // 文件对话框吸附轮询：跑在主线程事件循环上。
        // 不能挂在钩子线程——那里会被 WINEVENT 的事件洪水饿死 WM_TIMER。
        app.borrow().jump_poll_timer.start(
            TimerMode::Repeated,
            Duration::from_millis(500),
            || {
                #[cfg(windows)]
                crate::sys::dialog_jump::poll_foreground();
            },
        );
    }
    {
        let app = Rc::downgrade(app);
        // 供下面两个下拉回调共用（第一个闭包会移走 app）。
        let lang_app = app.clone();
        let startup_app = app.clone();
        let auto_edit_app = app.clone();
        upgrade().on_auto_edit_path_on_new_tab_toggled(move |enabled| {
            let Some(app) = auto_edit_app.upgrade() else { return };
            let mut app = app.borrow_mut();
            app.config.auto_edit_path_on_new_tab = enabled;
            app.config.save();
            app.ui.set_auto_edit_path_on_new_tab(enabled);
        });
        upgrade().on_show_hidden_toggled(move |enabled| {
            let Some(app) = app.upgrade() else { return };
            let mut app = app.borrow_mut();
            app.config.show_hidden = enabled;
            app.config.save();
            app.ui.set_show_hidden(enabled); // 核心修复：实时回填 UI 状态，开关才会翻转
            // 实时全局生效：刷新所有已打开的文件标签页
            let n = app.state.tabs.len();
            for i in 0..n {
                if app.state.tabs[i].kind == crate::state::TabKind::Explorer {
                    app.reload_tab(i);
                }
            }
        });
        // 通用设置：显示语言（即时换整套文案）与启动时打开。
        upgrade().on_language_changed(move |index| {
            let Some(app) = lang_app.upgrade() else { return };
            let mut app = app.borrow_mut();
            app.config.language = match index {
                1 => "zh-CN",
                2 => "en",
                _ => "system",
            }
            .to_string();
            app.config.save();
            i18n::apply(&app.ui, &app.config.language);
            app.jobs
                .set_english(!i18n::is_chinese(&app.config.language));
            // Slint 里绑定的文案会自动刷新；由 Rust 生成的标签要重新下发。
            app.sync_settings();
            app.sync_tabs();
            app.sync_sidebar();
            app.sync_breadcrumbs();
            app.sync_status();
        });
        upgrade().on_startup_changed(move |index| {
            let Some(app) = startup_app.upgrade() else { return };
            let mut app = app.borrow_mut();
            app.config.startup_tab = if index == 1 { "computer" } else { "last_tabs" }.to_string();
            app.config.save();
        });
    }
    {
        let app = Rc::downgrade(app);
        upgrade().on_settings_action_toggled(move |id, enabled| {
            let Some(app) = app.upgrade() else { return };
            let mut app = app.borrow_mut();
            let id = id.to_string();
            if let Some(idx) = app.actions.iter().position(|a| a.id == id) {
                app.actions[idx].enabled = enabled;
                app.save_actions_and_reload();
                app.sync_settings();
            }
        });
    }
    {
        let app = Rc::downgrade(app);
        upgrade().on_settings_group_toggled(move |name, expanded| {
            let Some(app) = app.upgrade() else { return };
            let app = app.borrow();
            // 分组展开状态保存在 UI 模型里（重进设置页会重置）。
            let model = app.ui.get_settings_action_groups();
            for i in 0..model.row_count() {
                if let Some(mut g) = model.row_data(i) {
                    if g.name == name {
                        g.expanded = expanded;
                        model.set_row_data(i, g);
                        break;
                    }
                }
            }
            // 同步内容高度（与 Slint 实测值取 max，见 settings_page.slint）。
            app.ui.set_settings_content_height(sum_group_heights(&model));
        });
    }
    {
        // 诊断用（ROVE_LOG=2 才输出）：打印设置页滚动几何，用于区分
        // 「内容真的放得下」与「某路高度少报」。
        upgrade().on_settings_scroll_debug(move |viewport, content, preferred| {
            logv!(
                "settings scroll: viewport={:.1} content={:.1} preferred={:.1} scrollable={:.1}",
                viewport,
                content,
                preferred,
                content - viewport
            );
        });
    }
    {
        let app = Rc::downgrade(app);
        upgrade().on_settings_action_add(move || {
            let Some(app) = app.upgrade() else { return };
            let app = app.borrow_mut();
            let stamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or(0);
            app.ui.set_settings_editor_title(app.tr("新建动作"));
            app.ui.set_settings_editor_group_options(ModelRc::new(VecModel::from(
                group_options(&app.actions, &app.tr("一级菜单")),
            )));
            app.ui.set_settings_editor_open(true);
            app.ui.set_settings_editor_id(format!("custom.{stamp}").into());
            app.ui.set_settings_editor_label("".into());
            app.ui.set_settings_editor_group("".into());
            app.ui.set_settings_editor_target("any".into());
            app.ui.set_settings_editor_extensions("".into());
            app.ui.set_settings_editor_program("".into());
            // 参数模板默认给 "{target_path}"（带双引号），符合任务书约定。
            app.ui.set_settings_editor_args("\"{target_path}\"".into());
        });
    }
    {
        let app = Rc::downgrade(app);
        upgrade().on_settings_action_edit(move |id| {
            let Some(app) = app.upgrade() else { return };
            let app = app.borrow();
            let id = id.to_string();
            if let Some(a) = app.actions.iter().find(|a| a.id == id) {
                app.ui.set_settings_editor_title(app.tr("编辑动作"));
                // 层级归属候选：已有二级分组 + 内置「一级菜单」（当前语言文案）。
                app.ui.set_settings_editor_group_options(ModelRc::new(VecModel::from(
                    group_options(&app.actions, &app.tr("一级菜单")),
                )));
                app.ui.set_settings_editor_open(true);
                app.ui.set_settings_editor_id(a.id.clone().into());
                app.ui.set_settings_editor_label(a.label.clone().into());
                app.ui.set_settings_editor_group(a.group.clone().into());
                app.ui.set_settings_editor_target(a.target.clone().into());
                app.ui.set_settings_editor_extensions(a.extensions.join(", ").into());
                app.ui.set_settings_editor_program(a.program.clone().into());
                app.ui.set_settings_editor_args(a.args.clone().into());
            }
        });
    }
    {
        let app = Rc::downgrade(app);
        upgrade().on_settings_action_delete(move |id| {
            let Some(app) = app.upgrade() else { return };
            let mut app = app.borrow_mut();
            let id = id.to_string();
            app.actions.retain(|a| a.id != id);
            app.save_actions_and_reload();
            app.sync_settings();
        });
    }
    {
        let app = Rc::downgrade(app);
        upgrade().on_settings_editor_save(move |id, label, group, target, extensions, program, args| {
            let Some(app) = app.upgrade() else { return };
            let mut app = app.borrow_mut();
            let id = id.to_string();
            let extensions: Vec<String> = extensions
                .split(',')
                .map(|s| s.trim().to_lowercase())
                .filter(|s| !s.is_empty())
                .collect();
            // 层级归属：空 / 「一级菜单」（含当前语言的内置项文案）= 一级项；
            // 其他文本直接作为二级菜单名。同时保留中文原文匹配，兼容旧数据/手输。
            let group = group.trim();
            let group = if group == "一级菜单" || group == app.tr("一级菜单").as_str() {
                ""
            } else {
                group
            };
            let action = crate::ops::actions::ActionEntry {
                id: id.clone(),
                label: label.to_string(),
                tool: "".into(),
                program: program.to_string(),
                args: args.to_string(),
                working_dir: "{target_dir}".into(),
                target: target.to_string(),
                extensions,
                enabled: true,
                group: group.to_string(),
                builtin: false,
            };
            if let Some(idx) = app.actions.iter().position(|a| a.id == id) {
                app.actions[idx] = action;
            } else {
                app.actions.push(action);
            }
            app.save_actions_and_reload();
            app.ui.set_settings_editor_open(false);
            app.sync_settings();
        });
    }
    {
        let app = Rc::downgrade(app);
        upgrade().on_settings_editor_cancel(move || {
            let Some(app) = app.upgrade() else { return };
            app.borrow().ui.set_settings_editor_open(false);
        });
    }
    {
        let app = Rc::downgrade(app);
        upgrade().on_settings_editor_browse(move || {
            let Some(app) = app.upgrade() else { return };
            // 原生打开对话框（嵌套消息循环，阻塞至用户选择完成）。
            if let Some(path) = crate::sys::pick::pick_program_path() {
                app.borrow().ui.set_settings_editor_program(path.into());
            }
        });
    }

    // ---- 窗口控制（无边框标题栏：最小化 / 最大化 / 关闭 / 拖拽移动） ----
    // 原生窗口操作仅 Windows 有意义，其他平台整体不注册（无边框标题栏不启用）。
    #[cfg(windows)]
    {
        let app = Rc::downgrade(app);
        upgrade().on_window_minimize(move || {
            let Some(app) = app.upgrade() else { return };
            // 先取 HWND 再进 unsafe 块：避免 `if let`  scrutinee 里的
            // 临时 RefCell 借用跨块存活，引发 E0597。
            let hwnd = get_window_hwnd(&app.borrow().ui);
            if let Some(hwnd) = hwnd {
                unsafe {
                    windows_sys::Win32::UI::WindowsAndMessaging::ShowWindow(
                        hwnd as _,
                        windows_sys::Win32::UI::WindowsAndMessaging::SW_MINIMIZE,
                    );
                }
            }
        });
    }
    #[cfg(windows)]
    {
        let app = Rc::downgrade(app);
        upgrade().on_window_maximize(move || {
            let Some(app) = app.upgrade() else { return };
            let hwnd = get_window_hwnd(&app.borrow().ui);
            if let Some(hwnd) = hwnd {
                unsafe {
                    use windows_sys::Win32::UI::WindowsAndMessaging::{
                        IsZoomed, SW_MAXIMIZE, SW_RESTORE, ShowWindow,
                    };
                    // 已最大化则还原，否则最大化——与原生标题栏按钮行为一致。
                    let cmd = if IsZoomed(hwnd as _) != 0 { SW_RESTORE } else { SW_MAXIMIZE };
                    // 一行记完前后状态：最大化失效时（曾被「固定宽度」的布局信息卡死）
                    // 能直接从这一行看出是系统没切还是 Slint 尺寸没跟上。
                    let (was_zoomed, before_w, before_h) = {
                        let w = app.borrow();
                        let scale = w.ui.window().scale_factor() as f32;
                        let sz = w.ui.window().size();
                        (
                            IsZoomed(hwnd as _),
                            (sz.width as f32 / scale).round(),
                            (sz.height as f32 / scale).round(),
                        )
                    };
                    ShowWindow(hwnd as _, cmd);
                    let now_zoomed = IsZoomed(hwnd as _) != 0;
                    logln!(
                        "[max] cmd={} zoomed {}->{} size {}x{}",
                        cmd, was_zoomed, now_zoomed, before_w, before_h
                    );
                    // 立即回传最大化状态，驱动 resize-border-width 归零/恢复。
                    // 用独立的 win-maximized 而非 Slint 内置 maximized：后者是
                    // 命令属性，回传会形成最大化/还原的回路。
                    app.borrow().ui.set_win_maximized(!now_zoomed);
                }
            }
        });
    }
    {
        // 关闭：跨平台注册（保存配置 + 销毁会话，与系统关闭请求同一条路径）。
        let app = Rc::downgrade(app);
        upgrade().on_window_close(move || {
            logln!("window close button");
            close_and_destroy(&app);
        });
    }

    // 注：窗口移动不在此接线——TabBar 空白区使用 Slint 原生 WindowMoveArea，
    // 由后端直接完成移动，无需 Win32 消息劫持（ReleaseCapture + SendMessageW）。
    // 那套方案会把鼠标捕获全局劫持，曾导致全窗口被误判为拖拽区、事件死锁。

    // ---- 关闭：保存配置并销毁会话（守护常驻） ----
    {
        let app = Rc::downgrade(app);
        upgrade().window().on_close_requested(move || {
            logln!("close requested");
            close_and_destroy(&app);
            slint::CloseRequestResponse::HideWindow
        });
    }

    // 侧边栏宽度拖拽落盘（只由用户拖拽触发；窗口挤压导致的收缩不落盘）。
    {
        let app = Rc::downgrade(app);
        upgrade().on_sidebar_width_changed(move |w| {
            let Some(app) = app.upgrade() else { return };
            let mut app = app.borrow_mut();
            // 防御：窗口最小化时上报的宽度可能是 0/异常小值，不写配置。
            if !(70.0..=1200.0).contains(&w) {
                logln!("[sidebar] !! ignore width {} (out of range)", w);
                return;
            }
            if (app.config.sidebar_width - w).abs() < 0.5 {
                return;
            }
            logv!(
                "[sidebar] persist width {} -> {}",
                app.config.sidebar_width,
                w
            );
            app.config.sidebar_width = w;
            app.config.save();
            // 核心修复：侧栏宽度改变后必须让可见行缓存失效，重新计算列数排版！
            app.visible_cache = None;
            app.refresh_visible_rows();
        });
    }
}

/// 延迟销毁会话：投递到事件循环下一拍执行（避免在回调栈内 drop 窗口）。
/// 用 `invoke_from_event_loop` 而非 Timer：最后一个窗口隐藏后
/// winit 后端会挂起事件循环，Timer 不再触发，而用户事件能唤醒它。
fn schedule_destroy() {
    let _ = slint::invoke_from_event_loop(destroy_session);
}

/// 关闭流程：按**逻辑像素**保存窗口尺寸 → 排定会话销毁（守护进程常驻）。
///
/// 系统关闭请求（Alt+F4 / 任务栏）与自绘关闭按钮共用此函数，
/// 避免两处逻辑各自演化后漂移（尤其 DPI 换算这种易错点）。
/// 关闭请求（窗口 X / 自绘关闭按钮）：只排定销毁。
/// 「窗口几何 + 上次标签页 + 最大化」的存档统一在 `destroy_session()` 完成，
/// 让窗口关闭与托盘退出两条出口都不会漏存。
fn close_and_destroy(_app_weak: &std::rc::Weak<RefCell<App>>) {
    schedule_destroy();
}

/// 取窗口的原生 Win32 HWND（仅 Windows）。用于无边框标题栏的
/// 最小化 / 最大化 / 拖拽移动等原生窗口操作。
#[cfg(windows)]
fn get_window_hwnd(ui: &MainWindow) -> Option<isize> {
    use raw_window_handle::RawWindowHandle;
    if let Ok(h) = raw_window_handle::HasWindowHandle::window_handle(&ui.window().window_handle()) {
        if let RawWindowHandle::Win32(win) = h.as_raw() {
            return Some(win.hwnd.get() as _);
        }
    }
    None
}

/// 无边框窗口的 `WM_NCHITTEST` 子类化：把标题栏空白拖拽区回报为 `HTCAPTION`。
///
/// **目的**：让 Windows **原生**处理该区域的拖拽。winit 的 `drag_window()`
/// （Slint `WindowMoveArea` 的底座）只支持鼠标；触屏手指拖拽必须经由
/// 非客户区命中测试走系统路径，否则手指按在标题栏空白处毫无反应。
///
/// 设计要点：
/// - 先透传原窗口过程，仅在它回报 `HTCLIENT`（客户区）时才判定拖拽区，
///   因此 `resize-border-width` 生成的边缘缩放手柄（`HTLEFT`/`HTTOP`/…）
///   与原生标题栏语义都不被破坏；
/// - 用原子量而非 `static mut` 保存边界与前一过程，避免 edition 2021 的
///   `static_mut_refs` 警告（本项目要求 Windows 零警告）；
/// - 安装时若当前过程已是自身则跳过，防止守护进程多次建会话时把前一过程
///   链成自身而造成无限递归。
#[cfg(windows)]
mod drag_hittest {
    use std::sync::atomic::{AtomicI64, AtomicU32, AtomicU64, Ordering};

    /// 前一窗口过程（`SetWindowLongPtrW` 的返回值）。
    static PREV_WNDPROC: AtomicI64 = AtomicI64::new(0);
    /// 钩子已安装到的 HWND（0 = 未安装）。
    ///
    /// 守护进程常驻时会多次建会话、每次都是新窗口；只有"同一个 HWND 已装过"
    /// 才允许跳过，否则第二个窗口永远拿不到钩子。
    static INSTALLED_HWND: AtomicI64 = AtomicI64::new(0);
    /// 拖拽区起点 x（逻辑像素，以 f32 位模式存放）。
    static DRAG_X: AtomicU32 = AtomicU32::new(0);
    /// 拖拽区宽度（逻辑像素，f32 位模式）。
    static DRAG_W: AtomicU32 = AtomicU32::new(0);
    /// 窗口缩放系数（物理像素 / 逻辑像素）。
    static DRAG_SCALE: AtomicU32 = AtomicU32::new(1.0f32.to_bits());
    /// 标签栏高度（逻辑像素）：仅纵向落在此范围内才可能是标题栏拖拽区。
    const TAB_BAR_H: f32 = 38.0;
    /// 命中日志节流（毫秒）：鼠标移动时 WM_NCHITTEST 高频触发，别把日志刷爆。
    static LAST_HIT_LOG_MS: AtomicU64 = AtomicU64::new(0);

    fn bits(v: &AtomicU32) -> f32 {
        f32::from_bits(v.load(Ordering::Relaxed))
    }

    /// 更新拖拽区边界；值未变化则跳过写入。返回本次是否真的改变，
    /// 供调用方只在变化时打日志（poll_timer 每 40ms 都会来一次）。
    pub fn update(x: f32, w: f32, scale: f32) -> bool {
        let (xb, wb, sb) = (x.to_bits(), w.to_bits(), scale.to_bits());
        if DRAG_X.load(Ordering::Relaxed) == xb
            && DRAG_W.load(Ordering::Relaxed) == wb
            && DRAG_SCALE.load(Ordering::Relaxed) == sb
        {
            return false;
        }
        DRAG_X.store(xb, Ordering::Relaxed);
        DRAG_W.store(wb, Ordering::Relaxed);
        DRAG_SCALE.store(sb, Ordering::Relaxed);
        true
    }

    /// 标题栏命中诊断（ROVE_LOG=2）：标签被判成 HTCAPTION 时会直接留下坐标证据。
    /// 高频逐次命中只在详细级输出，且距上一条 200ms 以上才写一行。
    fn log_hit(
        reason: &str,
        lx: f32,
        ly: f32,
        scale: f32,
        win_w: f32,
        x: f32,
        w: f32,
        zoomed: bool,
    ) {
        if ly < 0.0 || ly > TAB_BAR_H || !crate::app::log_enabled(2) {
            return;
        }
        let now = unsafe { windows_sys::Win32::System::SystemInformation::GetTickCount64() };
        let last = LAST_HIT_LOG_MS.load(Ordering::Relaxed);
        if now.saturating_sub(last) < 200 {
            return;
        }
        LAST_HIT_LOG_MS.store(now, Ordering::Relaxed);
        logv!(
            "[nc-hit] {reason} lx={lx:.1} ly={ly:.1} scale={scale:.2} win_w={win_w:.1} x={x:.1} w={w:.1} zoomed={zoomed}"
        );
    }

    /// 窗口过程拦截。
    ///
    /// # Safety
    /// 只能经 [`install`] 由 `SetWindowLongPtrW` 安装后交由 Windows 调用；
    /// 调用时 `PREV_WNDPROC` 必须已保存合法的前一过程。
    pub unsafe extern "system" fn wndproc(
        hwnd: windows_sys::Win32::Foundation::HWND,
        msg: u32,
        wparam: windows_sys::Win32::Foundation::WPARAM,
        lparam: windows_sys::Win32::Foundation::LPARAM,
    ) -> windows_sys::Win32::Foundation::LRESULT {
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            CallWindowProcW, HTBOTTOM, HTBOTTOMLEFT, HTBOTTOMRIGHT, HTCAPTION, HTCLIENT, HTLEFT,
            HTRIGHT, HTTOP, HTTOPLEFT, HTTOPRIGHT, IsZoomed, SC_MOVE, WNDPROC, WM_GETMINMAXINFO,
            WM_NCHITTEST, WM_NCLBUTTONDOWN, WM_NCRBUTTONDOWN, WM_SYSCOMMAND,
        };

        // 标题栏空白是**非客户区**（HTCAPTION），Slint 收不到这次点击，
        // 所以只能在这里清焦点：否则编辑态/重命名退不出去。
        // 必须同步——Windows 随后会进入自己的模态移动循环，
        // invoke_from_event_loop 排队的闭包要等循环结束才跑（表现为“点了没反应”）。
        if msg == WM_NCLBUTTONDOWN || msg == WM_NCRBUTTONDOWN {
            logv!("[nc] nclbuttondown msg={} ht={}", msg, wparam);
            super::clear_global_focus();
        }
        // 键盘/系统发起的“移动窗口”（Alt+Space → M）不走上面的路径，补一道。
        // 特意不接 WM_ENTERSIZEMOVE：拖边框**缩放**窗口在资源管理器里不会中断重命名，
        // 接了就变成“拉一下窗口边就把重命名提交了”。
        if msg == WM_SYSCOMMAND && (wparam & 0xFFF0) == SC_MOVE as usize {
            super::clear_global_focus();
        }

        let prev: WNDPROC = std::mem::transmute(PREV_WNDPROC.load(Ordering::Relaxed));

        // 诊断：窗口能被拉到的极限尺寸（winit/Slint 会按布局信息写 min/max track）。
        // 只打日志，不干预，目的是排查「无法拉宽 / 无法最大化」。
        if msg == WM_GETMINMAXINFO {
            let res = CallWindowProcW(prev, hwnd, msg, wparam, lparam);
            let info = &*(lparam as *const windows_sys::Win32::UI::WindowsAndMessaging::MINMAXINFO);
            logv!(
                "[max] mmi min_track={}x{} max_track={}x{} max_size={}x{}",
                info.ptMinTrackSize.x,
                info.ptMinTrackSize.y,
                info.ptMaxTrackSize.x,
                info.ptMaxTrackSize.y,
                info.ptMaxSize.x,
                info.ptMaxSize.y
            );
            return res;
        }

        if msg == WM_NCHITTEST {
            // Rove 自身发起的拖拽期间，拦截所有非客户区，防止系统用缩放光标
            // 覆盖 OLE 拖放自带的拖放光标（表现为拖到 Rove 边缘变成调整大小箭头）。
            if crate::sys::dnd::IS_DRAGGING.load(Ordering::SeqCst) {
                return HTCLIENT as isize;
            }

            let scale = bits(&DRAG_SCALE);
            let (x, w) = (bits(&DRAG_X), bits(&DRAG_W));
            let sx = (lparam as i32 & 0xFFFF) as i16 as i32;
            let sy = ((lparam as i32 >> 16) & 0xFFFF) as i16 as i32;
            let mut pt = windows_sys::Win32::Foundation::POINT { x: sx, y: sy };
            windows_sys::Win32::Graphics::Gdi::ScreenToClient(hwnd as _, &mut pt);
            let lx = pt.x as f32 / scale;
            let ly = pt.y as f32 / scale;

            let mut rect = windows_sys::Win32::Foundation::RECT { left: 0, top: 0, right: 0, bottom: 0 };
            windows_sys::Win32::UI::WindowsAndMessaging::GetClientRect(hwnd, &mut rect);
            let win_w = (rect.right - rect.left) as f32 / scale;
            let win_h = (rect.bottom - rect.top) as f32 / scale;
            let is_maximized = IsZoomed(hwnd as _) != 0;

            // 核心修复：右上角 172px（设置 36 + 最小化 44 + 最大化 44 + 关闭 46
            // = 170px + 2px 余量）为窗口控制按钮的绝对独占区。任何缩放或窗口
            // 尺寸下，只要落入此处 100% 回报 HTCLIENT，绝不允许被后面的标题栏
            // 拖拽判定吞成 HTCAPTION（窄窗口下按钮失灵的根因）。
            if (0.0..=TAB_BAR_H).contains(&ly) && lx >= win_w - 172.0 {
                log_hit("CLIENT-controls", lx, ly, scale, win_w, x, w, is_maximized);
                return HTCLIENT as isize;
            }

            if !is_maximized {
                let border = 6.0;

                // 1. 顶部边缘（宽 8px 防漏网）
                if ly <= 8.0 {
                    if lx <= border { return HTTOPLEFT as isize; }
                    if lx >= win_w - border { return HTTOPRIGHT as isize; }
                    return HTTOP as isize;
                }

                // 2. 底部边缘
                if ly >= win_h - border {
                    if lx <= border { return HTBOTTOMLEFT as isize; }
                    if lx >= win_w - border { return HTBOTTOMRIGHT as isize; }
                    return HTBOTTOM as isize;
                }

                // 3. 左侧边缘
                if lx <= border {
                    return HTLEFT as isize;
                }

                // 4. 右侧边缘
                if lx >= win_w - border {
                    return HTRIGHT as isize;
                }
            }

            // 非缩放边缘：判断是否在标题栏空白拖拽区（坐标已在上面转换好）。
            if w > 0.0 {
                if (0.0..=TAB_BAR_H).contains(&ly) && lx >= x && lx <= x + w {
                    log_hit("CAPTION", lx, ly, scale, win_w, x, w, is_maximized);
                    return HTCAPTION as isize;
                }
            }
            log_hit("CLIENT", lx, ly, scale, win_w, x, w, is_maximized);
            return HTCLIENT as isize;
        }
        CallWindowProcW(prev, hwnd, msg, wparam, lparam)
    }

    /// 指定窗口是否已装上钩子。
    pub fn is_installed(hwnd: isize) -> bool {
        INSTALLED_HWND.load(Ordering::Relaxed) == hwnd as i64
    }

    /// 在指定窗口上安装子类化钩子（幂等）。
    ///
    /// `show()` 刚返回时 winit 窗口可能尚未 realize，`window_handle()` 会失败；
    /// 因此允许反复调用，由调用方在拿到有效句柄后重试。
    pub fn install(hwnd: isize) {
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            GetWindowLongPtrW, GWLP_WNDPROC, SetWindowLongPtrW,
        };
        unsafe {
            let current = GetWindowLongPtrW(hwnd as _, GWLP_WNDPROC);
            // 0 = 句柄无效；已是自身 = 重复安装；本窗口已装过 = 跳过。
            // 二者都必须跳过，否则会把前一过程链成自身 → 无限递归爆栈。
            if current == 0
                || current == wndproc as *const () as isize
                || is_installed(hwnd)
            {
                return;
            }
            PREV_WNDPROC.store(current as i64, Ordering::Relaxed);
            let _ = SetWindowLongPtrW(hwnd as _, GWLP_WNDPROC, wndproc as *const () as isize);
            INSTALLED_HWND.store(hwnd as i64, Ordering::Relaxed);
            logv!("drag hittest: hooked hwnd={hwnd:#x} prev={current:#x}");
        }
    }
}

/// 把标题栏空白拖拽区边界同步给 Win32 命中测试钩子（仅 Windows）。
///
/// 优先采用 Slint 布局算出的真实 `drag-spacer` 几何；旧版纯 Rust 估算只作
/// 窗口未 realize 时的兜底。估算曾用 `window().size()/scale` 再 `.max(400.0)`，
/// 而 `size()` 在 show() 刚返回或部分后端上为 0 → 窗口宽被当成 400 →
/// 拖拽区起点落到第二个标签里，标签被回报为 HTCAPTION，点击全部失效。
/// 现在窗口宽度改从 HWND 的 `GetClientRect` 取（物理像素，一定准确），
/// 宽度为 0 时直接跳过本次更新，绝不再用假宽度覆盖上次的有效边界。
#[cfg(windows)]
fn sync_drag_bounds(app: &App, reason: &str) {
    let scale = app.ui.window().scale_factor() as f32;
    let mut physical_w = app.ui.window().size().width as f32;
    if let Some(hwnd) = get_window_hwnd(&app.ui) {
        let mut rect = windows_sys::Win32::Foundation::RECT {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        };
        unsafe {
            windows_sys::Win32::UI::WindowsAndMessaging::GetClientRect(
                hwnd as windows_sys::Win32::Foundation::HWND,
                &mut rect,
            );
        }
        let w = (rect.right - rect.left) as f32;
        if w > 0.0 {
            physical_w = w;
        }
    }
    let window_w = physical_w / scale.max(0.01);
    if window_w <= 0.0 {
        logv!("[drag-bounds] skip ({reason}) window not realized yet, scale={scale:.2}");
        return;
    }

    // 旧估算：仅作为 Slint 几何不可用时的兜底，并保留在日志中便于对比。
    let max_tabs_w = (window_w - 270.0).max(0.0);
    let tabs_w = app.state.tabs.len() as f32 * 153.0; // 单标签 150px + 3px 间距
    let actual_tabs_w = tabs_w.min(max_tabs_w);
    let rust_x = 8.0 + actual_tabs_w + 3.0 + 28.0 + 4.0;
    let rust_w = (window_w - 176.0 - rust_x).max(0.0);

    // Slint 真实布局几何（`tab_bar.slint` 导出的 drag-x / drag-w）。
    let slint_x = app.ui.get_drag_area_x() as f32;
    let slint_w = app.ui.get_drag_area_w() as f32;
    let (x, w, source) = if slint_w > 0.0 && slint_x > 0.0 {
        (slint_x, slint_w, "slint")
    } else {
        (rust_x, rust_w, "rust-estimate")
    };

    // 只在边界真的变化时输出（poll_timer 每 40ms 都会来一次），诊断细节放详细级。
    if drag_hittest::update(x, w, scale) {
        logv!(
            "[drag-bounds] {reason} -> {source} x={x:.1} w={w:.1} | rust x={rust_x:.1} w={rust_w:.1} | slint x={slint_x:.1} w={slint_w:.1} | scale={scale:.2} physical_w={physical_w:.1} win_w={window_w:.1} tabs={}",
            app.state.tabs.len()
        );
    }
}

/// 主显示器工作区（逻辑像素），用于把窗口尺寸钳制在屏幕内。
#[cfg(windows)]
fn work_area_logical(scale: f32) -> Option<(f32, f32)> {
    use windows_sys::Win32::Foundation::RECT;
    use windows_sys::Win32::UI::WindowsAndMessaging::{SPI_GETWORKAREA, SystemParametersInfoW};
    let mut rect = RECT { left: 0, top: 0, right: 0, bottom: 0 };
    let ok = unsafe {
        SystemParametersInfoW(SPI_GETWORKAREA as u32, 0, &mut rect as *mut RECT as _, 0)
    };
    if ok == 0 {
        return None;
    }
    let sf = if scale >= 1.0 { scale } else { 1.0 };
    Some((
        (rect.right - rect.left) as f32 / sf,
        (rect.bottom - rect.top) as f32 / sf,
    ))
}

#[cfg(not(windows))]
fn work_area_logical(_scale: f32) -> Option<(f32, f32)> {
    None
}

/// 屏幕分辨率（逻辑像素），用于首次启动按 75% 计算窗口大小并居中。
/// 用物理分辨率而不是工作区，避免任务栏把「75%」再缩一次。
#[cfg(windows)]
fn screen_size_logical(scale: f32) -> Option<(f32, f32)> {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetSystemMetrics, SM_CXSCREEN, SM_CYSCREEN,
    };
    let w = unsafe { GetSystemMetrics(SM_CXSCREEN) };
    let h = unsafe { GetSystemMetrics(SM_CYSCREEN) };
    if w <= 0 || h <= 0 {
        return None;
    }
    let sf = if scale >= 1.0 { scale } else { 1.0 };
    Some((w as f32 / sf, h as f32 / sf))
}

#[cfg(not(windows))]
fn screen_size_logical(_scale: f32) -> Option<(f32, f32)> {
    None
}

// 活跃 UI 会话槽：None 表示 UI 已销毁、仅剩守护。
// 存放于事件循环线程的 thread_local —— 管道线程发来的唤醒请求经
// `invoke_from_event_loop`（空捕获、可跨线程）在本线程执行时访问。
thread_local! {
    static SESSION: RefCell<Option<Rc<RefCell<App>>>> = const { RefCell::new(None) };
}

/// 原生菜单的结果回投（由菜单线程经 `invoke_from_event_loop` 调回 UI 线程）。
///
/// 这里**不能**在刚拿到结果时就同步执行——它会从后台线程被调度过来，
/// 只靠 SESSION 取 App，不存在借用冲突。
#[cfg(windows)]
pub(crate) fn handle_native_menu_action(action: crate::sys::menu::MenuAction) {
    use crate::sys::menu::MenuAction;
    let Some(app) = SESSION.with(|slot| slot.borrow().clone()) else {
        return;
    };
    let mut app = app.borrow_mut();
    match action {
        MenuAction::App(key) => app.run_menu_action(&key),
        MenuAction::Rename => {
            // Shell 的「重命名」回到应用内的就地重命名（不弹系统小窗）。
            if let Some(MenuScope::Selection(paths)) = app.menu_scope.clone() {
                if paths.len() == 1 {
                    let path = paths[0].clone();
                    app.begin_rename(&path);
                }
            }
        }
        // Shell 的「打开 / 在新窗口中打开」：文件夹回应用内导航，
        // 不要跳去资源管理器。
        MenuAction::Navigate(dir) => app.open_path(&dir),
        MenuAction::ShellOpBegin => {
            // 与自带任务共用同一个闸门：Shell 的确认/进度框只对自己的线程模态，
            // 不拦住的话回车会穿透到 Rove（Shift+Delete 那个坑）。
            app.shell_op_running = true;
            app.shell_key_suppressed_logged = false;
        }
        MenuAction::ShellOpEnd => {
            app.shell_op_running = false;
        }
        MenuAction::Refresh => app.user_refresh_active(),
    }
}

/// 在事件循环线程清空全部输入聚焦（供跨模块 / 窗口消息钩子调用）。
#[cfg(windows)]
pub(crate) fn clear_global_focus() {
    // 关键：**不能持有 App 的 RefCell 借用**去调 Slint。
    // clear-all-focus() 会回调 file_rename_committed 之类的 Rust 处理器，
    // 它们内部要 app.borrow_mut()，包着借用进去会直接 panic（重命名后点标题栏必崩）。
    // 所以只取出可 clone 的 UI 句柄，借用立即释放。
    let ui = SESSION.with(|slot| {
        let app = slot.borrow().clone()?;
        let app = app.try_borrow().ok()?;
        Some(slint::ComponentHandle::clone_strong(&app.ui))
    });
    if let Some(ui) = ui {
        logv!("[nc] clear_global_focus sync");
        ui.invoke_clear_all_focus();
    } else {
        logln!("[nc] !! clear_global_focus deferred (sync path unavailable)");
        let _ = slint::invoke_from_event_loop(|| {
            let ui = SESSION.with(|slot| {
                let app = slot.borrow().clone()?;
                let app = app.try_borrow().ok()?;
                Some(slint::ComponentHandle::clone_strong(&app.ui))
            });
            if let Some(ui) = ui {
                ui.invoke_clear_all_focus();
            }
        });
    }
}

/// 按吸附方向重定位吸附窗（参考 Pulse 行为：搜索框钉在对话框边缘不动，结果向对话框内侧生长）。
///
/// - `top`：窗口顶边固定在对话框顶边上方 40px，搜索框不动，结果向下生长进对话框；
/// - `bottom`：窗口底边固定在对话框底边下方 40px，搜索框不动，结果向上生长进对话框，
///   因此顶边需按 Slint 实时高度（`content-height`）上移。
///
/// bottom 的坑：Slint 窗口只能设左上角，resize 默认顶边锚定 → 高度一变底边就弹走一帧。
/// 故 bottom 不走 `set_position`，而是直接调 Win32 `SetWindowPos`，在**同一次 API 调用**
/// 里同时设定位置与大小，把底边锚定做成原子的，不留中间帧。
#[cfg(windows)]
fn position_jump_window(app: &App, x: i32, y: i32, w: i32, placement: &str) {
    let window = app.jump_window.window();
    let scale = window.scale_factor();
    let bar: i32 = (40.0 * scale) as i32;

    // 1. 先同步逻辑宽度和吸附方向给 Slint，确保 Slint 视口按正确宽度渲染，
    //    避免 SetWindowPos 设完物理尺寸后又被逻辑宽度回写打桩
    app.jump_window.set_window_width((w as f32 / scale) as f32);
    app.jump_window.set_placement(placement.into());

    if placement == "top" {
        // 顶边固定：搜索框不动，结果向下生长，resize 不会让搜索框移动
        window.set_position(slint::PhysicalPosition::new(x, y - bar));
    } else {
        // bottom：底边必须固定 → 用 SetWindowPos 原子设定位置 + 大小
        let h = (app.jump_window.get_content_height() * scale) as i32;
        let top = y + bar - h;
        if let Some(hwnd) = jump_window_hwnd(window) {
            use windows_sys::Win32::UI::WindowsAndMessaging::{
                SetWindowPos, SWP_NOCOPYBITS, SWP_NOACTIVATE, SWP_NOZORDER,
            };
            unsafe {
                SetWindowPos(
                    hwnd,
                    // SWP_NOZORDER 下插入位置无意义，传空指针避免干扰 Z 序
                    std::ptr::null_mut(),
                    x,
                    top,
                    // w 已经是对话框实测的物理像素宽度，切勿再除以 scale，
                    // 否则 HWND 实际宽度小于 Slint 视口宽度，右侧被系统裁剪（渲染不完整）
                    w,
                    h,
                    SWP_NOACTIVATE | SWP_NOZORDER | SWP_NOCOPYBITS,
                );
            }
        } else {
            // 句柄暂不可得（极端时序）时的保底，仍可能有一帧底边弹动
            window.set_position(slint::PhysicalPosition::new(x, top));
        }
    }
}

/// 取 Slint 吸附窗的 Win32 HWND（复用 show_dialog_jump 里 window_handle 的取法）。
#[cfg(windows)]
fn jump_window_hwnd(window: &slint::Window) -> Option<windows_sys::Win32::Foundation::HWND> {
    use raw_window_handle::RawWindowHandle;
    if let Ok(h) = raw_window_handle::HasWindowHandle::window_handle(&window.window_handle()) {
        if let RawWindowHandle::Win32(win) = h.as_raw() {
            return Some(win.hwnd.get() as windows_sys::Win32::Foundation::HWND);
        }
    }
    None
}

/// 在下一轮事件循环里对 bottom 吸附窗做原子重定位。
///
/// 必须延后一轮：设置 `results` 后 `content-height` 尚未重新计算，立即读到的还是旧高度，
/// 会导致底边锚定算错。延后到下一轮 Slint 已完成 height 重算，再调 `SetWindowPos` 原子锚定。
/// 闭包只捕获 `i32/String`（均可 Send），运行时从事件循环线程经 `SESSION` 取 App，
/// 避免携带 `Weak<RefCell<App>>`（非 Send）跨线程。
#[cfg(windows)]
fn reposition_bottom_next_tick(x: i32, y: i32, w: i32, placement: String) {
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(app_rc) = SESSION.with(|slot| slot.borrow().clone()) {
            let app = app_rc.borrow();
            position_jump_window(&app, x, y, w, &placement);
        }
    });
}

/// 呼出文件对话框跳转吸附窗，并贴合到对话框边缘。
#[cfg(windows)]
pub(crate) fn show_dialog_jump(
    hwnd: usize,
    current_dir: Option<PathBuf>,
    x: i32,
    y: i32,
    w: i32,
    placement: String,
) {
    if let Some(app) = SESSION.with(|slot| slot.borrow().clone()) {
        let mut app = app.borrow_mut();
        app.jump_target = Some((hwnd, current_dir));
        app.jump_geom = Some((x, y, w, placement.clone()));

        // 1. 先 show() 让底层 HWND 创建完毕，否则首次呼出时
        //    jump_window_hwnd 拿不到句柄，SetWindowPos 定位会静默失效
        let window = app.jump_window.window();
        let _ = app.jump_window.show();

        // 2. 再定位（此时必然能取到有效 HWND，底边锚定一次到位）
        position_jump_window(&app, x, y, w, &placement);

        // 默认不抢焦点：由用户主动点击吸附条再聚焦，避免在原生对话框里打字被劫持。
        // 仅登记吸附窗自身 HWND，供事件钩子豁免「用户点击吸附条」引发的 FOREGROUND 事件。
        use raw_window_handle::HasWindowHandle;
        let handle = window.window_handle();
        if let Ok(rh) = handle.window_handle() {
            if let raw_window_handle::RawWindowHandle::Win32(win) = rh.as_raw() {
                let slint_hwnd = win.hwnd.get() as windows_sys::Win32::Foundation::HWND;
                crate::sys::dialog_jump::register_jump_window(slint_hwnd as isize);
            }
        }
    }
}

/// 对话框移动时实时跟随更新吸附窗位置与吸附方向。
#[cfg(windows)]
pub(crate) fn update_dialog_jump_position(x: i32, y: i32, w: i32, placement: String) {
    if let Some(app) = SESSION.with(|slot| slot.borrow().clone()) {
        let mut app = app.borrow_mut();
        app.jump_geom = Some((x, y, w, placement.clone()));
        position_jump_window(&app, x, y, w, &placement);
    }
}

/// 对话框失焦 / 销毁时自动隐藏吸附窗。
#[cfg(windows)]
pub(crate) fn hide_dialog_jump() {
    logv!("[rove:dialog_jump] hide_dialog_jump called!");
    if let Some(app) = SESSION.with(|slot| slot.borrow().clone()) {
        let mut app = app.borrow_mut();
        app.jump_target = None;
        app.jump_geom = None;
        let _ = app.jump_window.window().hide();
        // 不再 clear_query：防止切换输入法/失焦时清空用户输入。
        // 只有用户真正跳转或主动关闭时才清空。
    }
    crate::sys::dialog_jump::register_jump_window(0);
}

pub fn run() {
    logln!("starting, args: {:?}", std::env::args().collect::<Vec<_>>());

    // 守护模式启动：拉起 Windows 文件对话框监听器（非 Windows 平台为空实现）。
    crate::sys::dialog_jump::start_daemon();

    let args: Vec<String> = std::env::args().skip(1).collect();

    // ---- 单实例：已存在守护则转发路径后立即退出 ----
    if crate::daemon::try_forward(&args) {
        logln!("forwarded to running daemon, exiting");
        return;
    }

    // ---- 守护模式：管道服务线程（唤醒请求投递到事件循环） ----
    let bound = crate::daemon::spawn_server(move |path| {
        // 闭包只捕获 path（Send），会话槽在事件循环线程的 thread_local 里。
        let _ = slint::invoke_from_event_loop(move || {
            wake_session(path);
        });
    });
    if !bound {
        // 竞态失败：短暂重试转发（另一实例正在启动）。
        for _ in 0..15 {
            std::thread::sleep(Duration::from_millis(100));
            if crate::daemon::try_forward(&args) {
                logln!("forwarded after race, exiting");
                return;
            }
        }
        logln!("ipc bind failed; running standalone");
    }

    // 首个会话：若启动参数带目录则直接打开。
    let initial = crate::daemon::wake_path_from_args(&args);
    wake_session(initial);

    // 守护托盘：UI 销毁后的常驻入口（打开/退出）。
    // 托盘句柄保活至进程结束，drop 即移除图标。
    #[cfg(windows)]
    let _tray = crate::tray::create();

    // 事件循环常驻：窗口全部销毁后保持空闲（0% CPU），
    // 等待管道唤醒/托盘点击重建 UI；退出由托盘菜单触发。
    logln!("running persistent event loop");
    let result = slint::run_event_loop_until_quit();
    logln!("event loop exited: {result:?}");

    // 管道服务线程阻塞在 ConnectNamedPipe 上，直接结束进程。
    std::process::exit(0);
    #[allow(unreachable_code)]
    {
        let _ = result;
    }
}

/// 原生探测磁盘介质类型：通过查询寻道惩罚（Seek Penalty）精准判定。
/// SSD 无寻道惩罚，机械硬盘有寻道惩罚。
/// 核心改进：先将卷映射为物理驱动器，穿透卷管理器；遇到底层驱动拒绝回答时，强制兜底为 HDD 保护。
#[cfg(windows)]
fn is_ssd_drive(drive_letter: char) -> bool {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING};
    use windows_sys::Win32::System::IO::DeviceIoControl;

    let path = format!("\\\\.\\{}:", drive_letter);
    let wide_path: Vec<u16> = std::ffi::OsStr::new(&path).encode_wide().chain(std::iter::once(0)).collect();

    unsafe {
        // 1. 打开卷设备（传 0 权限，无需管理员即可查询元数据）
        let vol_handle = CreateFileW(
            wide_path.as_ptr(),
            0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            std::ptr::null(),
            OPEN_EXISTING,
            0,
            0 as _,
        );

        if vol_handle == INVALID_HANDLE_VALUE {
            // 核心防御：打不开也默认当机械盘保护，防误杀电机
            return false;
        }

        // 2. 将卷映射为物理驱动器号
        #[repr(C)]
        struct DISK_EXTENT { disk_number: u32, starting_offset: i64, extent_length: i64 }
        #[repr(C)]
        struct VOLUME_DISK_EXTENTS { number_of_disk_extents: u32, extents: [DISK_EXTENT; 1] }

        let mut extents: VOLUME_DISK_EXTENTS = std::mem::zeroed();
        let mut bytes_returned = 0;
        let ioctl_volume_get_extents = 5636096;

        let res_extents = DeviceIoControl(
            vol_handle, ioctl_volume_get_extents, std::ptr::null_mut(), 0,
            &mut extents as *mut _ as *mut std::ffi::c_void, std::mem::size_of::<VOLUME_DISK_EXTENTS>() as u32,
            &mut bytes_returned, std::ptr::null_mut()
        );

        let mut query_handle = vol_handle;
        let mut phys_handle = INVALID_HANDLE_VALUE;

        if res_extents != 0 && extents.number_of_disk_extents > 0 {
            let phys_path = format!("\\\\.\\PhysicalDrive{}", extents.extents[0].disk_number);
            let phys_wide: Vec<u16> = std::ffi::OsStr::new(&phys_path).encode_wide().chain(std::iter::once(0)).collect();
            phys_handle = CreateFileW(
                phys_wide.as_ptr(),
                0,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                std::ptr::null(),
                OPEN_EXISTING,
                0,
                0 as _,
            );
            if phys_handle != INVALID_HANDLE_VALUE {
                query_handle = phys_handle;
            }
        }

        // 3. 查询寻道惩罚 (IOCTL_STORAGE_QUERY_PROPERTY)
        #[repr(C)]
        struct STORAGE_PROPERTY_QUERY { property_id: u32, query_type: u32, additional_parameters: [u8; 1] }
        #[repr(C)]
        struct DEVICE_SEEK_PENALTY_DESCRIPTOR { version: u32, size: u32, incurs_seek_penalty: u8 }

        let mut query = STORAGE_PROPERTY_QUERY { property_id: 7, query_type: 0, additional_parameters: [0] };
        let mut descriptor = DEVICE_SEEK_PENALTY_DESCRIPTOR { version: 0, size: 0, incurs_seek_penalty: 0 };
        bytes_returned = 0;
        let ioctl_storage_query_property = 2954240;

        let res_query = DeviceIoControl(
            query_handle, ioctl_storage_query_property,
            &mut query as *mut _ as *mut std::ffi::c_void, std::mem::size_of::<STORAGE_PROPERTY_QUERY>() as u32,
            &mut descriptor as *mut _ as *mut std::ffi::c_void, std::mem::size_of::<DEVICE_SEEK_PENALTY_DESCRIPTOR>() as u32,
            &mut bytes_returned, std::ptr::null_mut()
        );

        if phys_handle != INVALID_HANDLE_VALUE { CloseHandle(phys_handle); }
        CloseHandle(vol_handle);

        if res_query != 0 {
            // 返回 true 的唯一合法条件：底层驱动明确答复“本盘没有机械寻道延迟”
            return descriptor.incurs_seek_penalty == 0;
        }

        // 核心修复：查询失败（如 E 盘老硬盘返回 31 错误码），
        // 出于最高级别的保护原则，只要不能 100% 证明是 SSD，一律按 HDD 处理！
        false
    }
}

#[cfg(not(windows))]
fn is_ssd_drive(_drive_letter: char) -> bool {
    true
}

/// 系统双击时间间隔（毫秒）。用户在控制面板可调（默认 500，最快约 200）。
/// Rust 侧的双击判定与延迟重命名都基于它，避免硬编码 350/400 与用户实际
/// 双击速度不一致导致「慢手双击被判成两次单击 → 先 rename 再打开」。
fn double_click_ms() -> u64 {
    #[cfg(windows)]
    {
        use windows_sys::Win32::UI::Input::KeyboardAndMouse::GetDoubleClickTime;
        let ms = unsafe { GetDoubleClickTime() } as u64;
        if ms == 0 {
            500
        } else {
            ms
        }
    }
    #[cfg(not(windows))]
    {
        500
    }
}

/// 用于「双击打开 / 延迟重命名」的有效双击间隔：取系统值但封顶 300ms。
/// 否则系统默认 500ms 会让单击重命名延迟到 550ms 太不跟手；封顶后默认 Windows
/// 下窗口=300、延迟=350，既跟手又保证延迟（窗口+50）> 窗口，不破坏正常双击打开。
/// 系统设得比 300 更快时仍精确跟随。
fn effective_double_click_ms() -> u64 {
    double_click_ms().min(300)
}

/// 创建一个 UI 会话（窗口 + 状态 + 回调 + 定时器）。
/// 回调/定时器均以 Weak 持有 App，销毁时整图释放。
/// 窗口真正 realize 后的首帧重绘（RedrawRequested）时一次性应用：
/// 主窗 HWND 登记、无边框拖拽钩子安装、最大化还原、首次启动 75% 居中。
///
/// 这些操作都依赖「窗口已落到显示器、HWND 与 scale_factor() 正确」。show() 之后立刻做
/// 会拿到空 HWND / scale=1.0（实测：首次启动尺寸偏大、偏离中心）。借 on_winit_window_event
/// 的 RedrawRequested（show() 后由 request_redraw() 立即触发）应用，比 poll_timer 每 40ms
/// 轮询更确定、且无尺寸闪烁。first_run_pending 由 create_session 提前置位、此处消费后清除；
/// 首帧若 HWND 仍空则保留标志，下次 redraw 重试。最大化还原已改在 create_session 的
/// show() 之前用 ui.window().set_maximized() 于窗口构建阶段生效，不依赖首帧 redraw。
#[cfg(windows)]
fn apply_deferred_geometry(app: &Rc<RefCell<App>>) {
    let hwnd = get_window_hwnd(&app.borrow().ui);
    let Some(hwnd) = hwnd else {
        // HWND 尚未就绪（首帧 redraw 来得比窗口 realize 早）：再请求一帧，
        // 下次 RedrawRequested 继续重试，直到拿到 HWND 才真正居中。否则只触发
        // 一次 redraw 时若此时 HWND 为空，首启窗口会一直停在默认左上角。
        // 但必须设上限，避免极端情况下 HWND 永远取不到导致每帧 request_redraw
        // 死循环、CPU 打满。
        let (need_redraw, give_up) = {
            let mut a = app.borrow_mut();
            if a.geom_retries < 120 {
                a.geom_retries += 1;
                (true, false)
            } else {
                // 放弃居中：清掉 pending，窗口停默认位置（总比死循环好）。
                a.first_run_pending = false;
                (false, true)
            }
        };
        if need_redraw {
            app.borrow().ui.window().request_redraw();
        } else if give_up {
            logln!("[geom] HWND never ready after 120 retries, give up centering");
        }
        return;
    };
    // 登记主窗 HWND 给后台 Shell 对话框做宿主（复制/移动/删除确认框），
    // 与无边框钩子安装一并仅在首次（未安装时）执行，避免每帧空转。
    if !drag_hittest::is_installed(hwnd) {
        crate::sys::shell::set_owner_hwnd(hwnd);
        drag_hittest::install(hwnd);
    }
    // 注意：最大化还原已移到 create_session 的 show() 之前，用
    // ui.window().set_maximized(config.window_maximized) 在 winit 窗口构建阶段
    // 直接生效（不依赖 HWND / 首帧 redraw）。否则首帧 RedrawRequested 会被首屏
    // 索引/列表渲染拖到数秒后才触发，期间窗口停在默认位置、造成明显闪烁。
    // 首次启动 75% 居中：此刻 scale_factor() 才是真实值（仍须等 realize 后）。
    if app.borrow().first_run_pending {
        app.borrow_mut().first_run_pending = false;
        let s2 = app.borrow().ui.window().scale_factor() as f32;
        if let Some((sw, sh)) = screen_size_logical(s2) {
            let mut w = (sw * 0.75).max(480.0);
            let mut h = (sh * 0.75).max(360.0);
            if let Some((aw, ah)) = work_area_logical(s2) {
                w = w.min(aw).max(480.0);
                h = h.min(ah - 60.0).max(360.0); // 留出标题栏
            }
            app.borrow()
                .ui
                .window()
                .set_size(WindowSize::Logical(LogicalSize::new(w, h)));
            let x = ((sw - w) / 2.0).max(0.0);
            let y = ((sh - h) / 2.0).max(0.0);
            app.borrow()
                .ui
                .window()
                .set_position(WindowPosition::Logical(LogicalPosition::new(x, y)));
            let mut cfg = app.borrow_mut();
            cfg.config.window_width = w as u32;
            cfg.config.window_height = h as u32;
            cfg.config.window_x = x as i32;
            cfg.config.window_y = y as i32;
            cfg.config.save();
            logln!(
                "[win-restore] first_run centered on redraw: w={} h={} x={} y={} scale={}",
                w, h, x, y, s2
            );
        }
    }
}

fn create_session(initial_path: Option<PathBuf>, jobs: JobManager) {
    let mut config = Config::load();
    // 首次启动判定快照：用「是否首次运行」标记，而不是坐标 < 0（见下方窗口几何处说明）。
    // 必须在下方 HDD 初始化逻辑消费 is_first_run 之前抓取。
    let first_run = config.is_first_run;
    // config 稍后会移入 App，先把「启动时打开」的标签页快照取出来。
    let startup_snapshot = (
        config.startup_tab.clone(),
        config.last_tabs.clone(),
        config.last_active_tab,
    );

    // 首次启动保护：自动将所有非系统盘的 HDD 机械硬盘加入排除名单，避免唤醒电机
    if config.is_first_run {
        config.is_first_run = false;
        for d in crate::fs::list_drives()
            .into_iter()
            .filter(|d| matches!(d.drive_type, crate::fs::drives::DriveType::Fixed))
        {
            if !is_ssd_drive(d.letter) && !d.letter.to_string().eq_ignore_ascii_case("C") {
                config.search_excluded_drives.push(d.letter.to_string().to_ascii_uppercase());
            }
        }
        config.save();
    }

    let ui = match MainWindow::new() {
        Ok(ui) => ui,
        Err(e) => {
            eprintln!("[rove] failed to create UI: {e}");
            panic!("failed to create UI: {e}");
        }
    };
    logln!(
        "window physical={:?} scale={}",
        ui.window().size(),
        ui.window().scale_factor()
    );
    apply_theme(&ui, config.theme);
    // 界面文案按「显示语言」整体下发；下拉选中项同步到配置。
    i18n::apply(&ui, &config.language);
    ui.set_language_index(match config.language.as_str() {
        "zh-CN" => 1,
        "en" => 2,
        _ => 0,
    });
    ui.set_startup_index(if config.startup_tab == "computer" { 1 } else { 0 });
    // 窗口初始尺寸：直接用配置里的值（首次启动即配置默认值 1150×690）。
    // 不要在这里用 scale_factor() 去算 75%——此时窗口尚未 show，scale 恒为 1.0，
    // 在 2× 屏上会先弹出一个逻辑像素意义上“超屏”的瞬时大窗，再到首帧
    // apply_deferred_geometry 用真实 scale 重算修正，产生可见闪烁。
    // 首次启动的 75% 居中由 apply_deferred_geometry（首帧、真实 scale 已就绪）统一完成。
    let (w, h) = (config.window_width as f32, config.window_height as f32);
    ui.window().set_size(WindowSize::Logical(LogicalSize::new(w, h)));
    if first_run {
        // 位置/75% 居中交给首帧 apply_deferred_geometry；这里只给安全默认尺寸，
        // 避免 show 前用假 scale 算出超屏大窗。
    } else {
        ui.window()
            .set_position(WindowPosition::Logical(LogicalPosition::new(
                config.window_x as f32,
                config.window_y as f32,
            )));
    }
    ui.set_staging_expanded(config.staging_expanded);
    logln!(
        "[win-restore] first_run={} w={} h={} x={} y={} maximized={}",
        first_run, w, h, config.window_x, config.window_y, config.window_maximized
    );
    logv!("[sidebar] want={}px (persisted)", config.sidebar_width);
    ui.set_sidebar_want(config.sidebar_width as f32);
    ui.set_view_mode(config.view_mode.clone().into());

    // 目录列举通道 + 工作线程（会话私有，销毁时工作线程随之退出）。
    let (list_tx, worker_rx) = mpsc::channel::<ListRequest>();
    let (worker_tx, list_rx) = mpsc::channel::<ListResult>();
    spawn_list_worker(worker_rx, worker_tx);

    // 深度搜索通道 + 工作线程（文件夹视图子树递归查找）。
    let (deep_tx, deep_worker_rx) = mpsc::channel::<DeepRequest>();
    let (deep_worker_tx, deep_rx) = mpsc::channel::<DeepResult>();
    spawn_deep_worker(deep_worker_rx, deep_worker_tx);

    // 缩略图 worker：路径 → 生成缩略图 → 回事件循环写缓存。
    let (thumb_tx, thumb_rx) = mpsc::channel::<ThumbKey>();
    let (thumb_prio_tx, thumb_prio_rx) = mpsc::channel::<ThumbKey>();
    #[cfg(windows)]
    spawn_thumb_worker(thumb_rx, thumb_prio_rx);
    #[cfg(not(windows))]
    drop((thumb_rx, thumb_prio_rx));

    // 文件夹体积统计 worker：逐个递归计算目录大小，算完一个回传一个。
    let (dir_size_tx, dir_size_worker_rx) = mpsc::channel::<DirSizeRequest>();
    let (dir_size_worker_tx, dir_size_rx) = mpsc::channel::<DirSizeResult>();
    spawn_dir_size_worker(dir_size_worker_rx, dir_size_worker_tx);

    // 文件变更监听通道（worker 线程 -> 主线程 poll_timer 消费）。
    let (watch_tx, watch_rx) = mpsc::channel::<PathBuf>();

    let app = Rc::new(RefCell::new(App::new(
        ui,
        config,
        jobs,
        list_tx,
        list_rx,
        deep_tx,
        deep_rx,
        thumb_tx,
        thumb_prio_tx,
        dir_size_tx,
        dir_size_rx,
        watch_tx,
        watch_rx,
    )));
    // 供延迟重命名等定时器在回调里升级回 App。
    app.borrow_mut().self_weak = Some(Rc::downgrade(&app));
    {
        let raw = crate::ops::actions::load_or_create();
        app.borrow_mut().actions = crate::ops::actions::heal_full(&raw);
    }

    // 先入槽再接线：会话槽持有唯一强引用，回调/定时器均为 Weak；
    // 漏掉此行会导致 App 在本函数返回时即被释放（列表收不到数据）。
    SESSION.with(|slot| *slot.borrow_mut() = Some(app.clone()));
    // 启动即预热全盘索引：懒加载会让首次搜索撞上冷启动（等待扫描），
    // 且状态栏空等。放在这里可在会话建立瞬间就让后台线程开始静默建库。
    // shared() 是幂等的（OnceLock），守护进程常驻时重复调用无副作用。
    let _ = crate::search::index::shared();

    wire_callbacks(&app);
    setup_timers(&app);
    setup_keyboard(&app);

    // 初始数据。
    {
        let mut app = app.borrow_mut();
        app.state.recent = app.config.recent.clone();
        if let Some(path) = initial_path {
            let sort = app.state.active().active().sort;
            app.state.new_tab(Some(path), sort);
        } else if startup_snapshot.0 == "last_tabs" && !startup_snapshot.1.is_empty() {
            // 「上次的标签页(包括分屏)」：按快照重建标签与各栏路径。
            let sort = app.state.active().active().sort;
            let mut tabs = Vec::new();
            for (i, snap) in startup_snapshot.1.iter().enumerate() {
                let first = snap
                    .paths
                    .first()
                    .and_then(|p| p.as_ref())
                    .map(PathBuf::from);
                let mut tab = if snap.is_settings {
                    crate::state::TabState::new_settings(
                        (i + 1) as u32,
                        crate::state::SettingsSubpage::default(),
                    )
                } else {
                    crate::state::TabState::new((i + 1) as u32, first, sort)
                };
                if !snap.is_settings && snap.paths.len() > 1 {
                    let second = snap
                        .paths
                        .get(1)
                        .and_then(|p| p.as_ref())
                        .map(PathBuf::from);
                    tab.panes
                        .push(crate::state::PaneState::new(second, sort));
                    tab.is_split = true;
                    tab.active_pane = snap.active_pane.min(tab.panes.len().saturating_sub(1));
                }
                tabs.push(tab);
            }
            app.state.tabs = tabs;
            app.state.active_tab = startup_snapshot.2.min(app.state.tabs.len() - 1);
            app.state.next_tab_id = app.state.tabs.len() as u32 + 1;
            // 先把每个面板的排序对齐到它所在视图的保存值，
            // 否则下面这次列举会带着「上一个目录的排序」发出，worker 白排一次。
            app.sync_view_sort();
            // 快照只记路径：给每个恢复出来的标签/分屏都发一次列举，
            // 否则切过去时那一页是空的（sync_active_tab 不会主动列举）。
            let requests: Vec<(u32, usize, u64, Option<PathBuf>, crate::fs::SortState)> = app
                .state
                .tabs
                .iter()
                .flat_map(|t| {
                    t.panes.iter().enumerate().filter_map(move |(pi, p)| {
                        if t.kind != crate::state::TabKind::Explorer {
                            return None;
                        }
                        Some((t.id, pi, p.generation, p.path.clone(), p.sort))
                    })
                })
                .collect();
            for (id, pi, gen, path, sort) in requests {
                app.request_listing(id, pi, gen, path, sort);
            }
        }
        // 「电脑选项」不做事：AppState 初始就是单栏的「此电脑」。
        // 但它的排序必须用「此电脑」自己的状态（主键是盘符），
        // 不能沿用普通目录的全局排序。
        app.sync_view_sort();
        app.sync_tabs();
        app.sync_nav();
        app.sync_breadcrumbs();
        app.sync_status();
        app.sync_sidebar();
        app.sync_staging();
        app.sync_file_list();
        app.reload_active();
        // 快照恢复出来的标签可能直接就是设置页：补一次导航/标题下发。
        if app.state.active().kind == crate::state::TabKind::Settings {
            app.sync_settings();
        }

        // 自动化/外部唤起钩子：ROVE_SEARCH 预填搜索词并触发完整搜索
        // （本地过滤 + 深度搜索 + 高亮），与手动输入完全同一条路径。
        if let Ok(q) = std::env::var("ROVE_SEARCH") {
            let q = q.trim().to_string();
            if !q.is_empty() {
                app.ui.set_filter_text(q.clone().into());
                app.apply_search_query(&q);
            }
        }
    }

    // Slint 的窗口在 show() 后才真正创建并可见（new() 仅构建组件树）。
    // 因此原生窗口操作（HWND 子类化、拖拽热区几何）必须放在 show() 之后：
    // 提前安装会拿不到有效句柄，或装到随后被重建的窗口上而丢失。
    // 启动最大化还原：在 show() 之前用 winit 的 set_maximized 在窗口构建阶段直接生效，
    // 不依赖 HWND / 首帧 redraw——后者会被首屏索引/列表渲染拖到数秒后才触发，造成
    // “先以普通窗口停在默认位置、再跳到最大化”的明显闪烁。win_maximized 仅控制自定义
    // 边框的 1px 缩放边是否显示：最大化时隐藏（原生最大化时不需要可拖拽的边）。
    app.borrow().ui.window().set_maximized(app.borrow().config.window_maximized);
    app.borrow().ui.set_win_maximized(!app.borrow().config.window_maximized);

    app.borrow().ui.show().unwrap();

    // 强制立即重绘：让首次 RedrawRequested 尽快触发，及时应用首次启动 75% 居中
    // （否则首屏列表/索引等工作会把首帧推迟到数秒后，出现“先默认位置、再居中”的闪烁）。
    app.borrow().ui.window().request_redraw();

    // 首次启动 75% 居中：必须在窗口 realize 后（scale_factor 正确）由首帧 redraw 应用，
    // 见 apply_deferred_geometry（on_winit_window_event 注册）。
    if first_run {
        app.borrow_mut().first_run_pending = true;
    }
    sync_drag_bounds(&app.borrow(), "show");

    logln!("session created");
}

/// 唤醒：无 UI 则重建会话（冷启动 < 50ms）；有 UI 则前置窗口并导航。
pub fn wake_session(path: Option<PathBuf>) {
    let existing = SESSION.with(|slot| slot.borrow().clone());
    if let Some(app) = existing {
        let mut app = app.borrow_mut();
        if let Some(p) = path.clone() {
            app.navigate_active(Some(p));
        }
        let ui = &app.ui;
        let _ = ui.show();
        focus_window(ui);
        logln!("woke existing session, path={path:?}");
        return;
    }
    // JobManager 的任务线程跨会话存活；此处为新会话重建一份管理句柄，
    // 使 jobs 状态与 UI 生命周期解耦。
    create_session(path, JobManager::new());
}

/// 销毁当前会话：丢弃窗口（释放 GPU 资源与组件树）与全部状态，
/// 仅保留管道守护线程，常驻内存降到守护水位。
pub fn destroy_session() {
    logln!("destroy_session running");
    let app = SESSION.with(|slot| slot.borrow_mut().take());
    let Some(app) = app else {
        return;
    };
    // 退出存档：窗口几何（逻辑像素）+ 最大化状态 + 上次标签页快照。
    // 统一在销毁出口完成，保证「窗口 X」与「托盘退出」两条路径都能落盘。
    {
        let mut app = app.borrow_mut();
        // 注意：size() 是物理像素，配置里的窗口尺寸按逻辑像素保存，
        // 否则在高 DPI 下每次会话窗口都会按缩放系数膨胀。
        let window = app.ui.window();
        let scale = window.scale_factor() as f64;
        // 退出存档的窗口几何统一按「逻辑像素」保存（size()/矩形都是物理像素，
        // 不折算的话高 DPI 下每次会话窗口都会按缩放系数膨胀）。
        // 关键：必须用 GetWindowPlacement 的 rcNormalPosition（恢复正常矩形），
        // 而非 GetWindowRect——后者在最大化/最小化时返回的是「最大化矩形」（会溢出
        // 工作区、left/top 为负）。据此存盘会把正常几何污染成最大化几何，导致下次
        // 启动还原最大化后、再「退出最大化」时落到错误的尺寸/位置。
        #[cfg(windows)]
        if let Some(hwnd) = get_window_hwnd(&app.ui) {
            use windows_sys::Win32::UI::WindowsAndMessaging::{
                GetWindowPlacement, IsZoomed, WINDOWPLACEMENT,
            };
            use windows_sys::Win32::Foundation::{POINT, RECT};
            let mut wp = WINDOWPLACEMENT {
                length: std::mem::size_of::<WINDOWPLACEMENT>() as u32,
                flags: 0,
                showCmd: 0,
                ptMinPosition: POINT { x: 0, y: 0 },
                ptMaxPosition: POINT { x: 0, y: 0 },
                rcNormalPosition: RECT { left: 0, top: 0, right: 0, bottom: 0 },
            };
            if unsafe { GetWindowPlacement(hwnd as _, &mut wp as *mut _ as _) } != 0 {
                let r = wp.rcNormalPosition;
                app.config.window_x = (r.left as f64 / scale).round() as i32;
                app.config.window_y = (r.top as f64 / scale).round() as i32;
                app.config.window_width = ((r.right - r.left) as f64 / scale).round() as u32;
                app.config.window_height = ((r.bottom - r.top) as f64 / scale).round() as u32;
                logv!(
                    "[win-save] normal rect l={} t={} r={} b={} scale={} -> x={} y={} w={} h={}",
                    r.left, r.top, r.right, r.bottom, scale,
                    app.config.window_x, app.config.window_y,
                    app.config.window_width, app.config.window_height
                );
            } else {
                // 兜底：GetWindowPlacement 异常时退回当前矩形（GetWindowRect）。
                let mut rect = RECT { left: 0, top: 0, right: 0, bottom: 0 };
                if unsafe {
                    windows_sys::Win32::UI::WindowsAndMessaging::GetWindowRect(
                        hwnd as _,
                        &mut rect as *mut _ as _,
                    )
                } != 0
                {
                    app.config.window_x = (rect.left as f64 / scale).round() as i32;
                    app.config.window_y = (rect.top as f64 / scale).round() as i32;
                    app.config.window_width =
                        ((rect.right - rect.left) as f64 / scale).round() as u32;
                    app.config.window_height =
                        ((rect.bottom - rect.top) as f64 / scale).round() as u32;
                }
            }
            app.config.window_maximized = unsafe { IsZoomed(hwnd as _) } != 0;
        }
        #[cfg(not(windows))]
        {
            let size = window.size();
            app.config.window_width = (size.width as f64 / scale).round() as u32;
            app.config.window_height = (size.height as f64 / scale).round() as u32;
        }
        // 「上次的标签页(包括分屏)」：记下每个标签各栏的路径。
        app.config.last_tabs = app
            .state
            .tabs
            .iter()
            .map(|t| crate::config::TabSnapshot {
                paths: t
                    .panes
                    .iter()
                    .map(|p| p.path.as_ref().map(|x| x.to_string_lossy().into_owned()))
                    .collect(),
                active_pane: t.active_pane,
                is_settings: t.kind == crate::state::TabKind::Settings,
            })
            .collect();
        app.config.last_active_tab = app.state.active_tab;
        logv!(
            "[win-save] w={} h={} x={} y={} maximized={} hwnd_present={} tabs={}",
            app.config.window_width,
            app.config.window_height,
            app.config.window_x,
            app.config.window_y,
            app.config.window_maximized,
            get_window_hwnd(&app.ui).is_some(),
            app.config.last_tabs.len()
        );
        app.config.save();
    }
    // 显式隐藏后整体 drop：回调均为 Weak，无强引用环，
    // Slint 组件树与 winit 窗口（GPU 纹理、表面）随之释放。
    let ui = app.borrow().ui.as_weak();
    if let Some(w) = ui.upgrade() {
        let _ = w.hide();
    }
    // 会话销毁时，将累计的目录容量缓存刷写到系统 Temp 目录（重启不丢）。
    if let Ok(mut store) = global_dir_sizes().lock() {
        store.save_to_temp();
    }
    drop(app);
    logln!("session destroyed; daemon resident");
}

/// 把窗口前置并聚焦（WSLg/真实 Windows 均适用）。
fn focus_window(ui: &MainWindow) {
    #[cfg(windows)]
    {
        use raw_window_handle::RawWindowHandle;
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            SetForegroundWindow, ShowWindow, SW_RESTORE,
        };
        // 两步获取：HasWindowHandle::window_handle() → rwh WindowHandle → as_raw()。
        if let Ok(h) = raw_window_handle::HasWindowHandle::window_handle(&ui.window().window_handle())
        {
            if let RawWindowHandle::Win32(win) = h.as_raw() {
                let hwnd = win.hwnd.get() as isize as windows_sys::Win32::Foundation::HWND;
                unsafe {
                    ShowWindow(hwnd, SW_RESTORE);
                    SetForegroundWindow(hwnd);
                }
            }
        }
    }
    #[cfg(not(windows))]
    let _ = ui;
}

/// 光标（屏幕）位置 → 窗口客户区逻辑坐标。
#[cfg(windows)]
fn cursor_window_logical(ui: &MainWindow, scale: f32) -> Option<(f32, f32)> {
    use raw_window_handle::RawWindowHandle;
    use windows::Win32::Foundation::POINT;
    use windows::Win32::Graphics::Gdi::ScreenToClient;
    use windows::Win32::UI::WindowsAndMessaging::GetCursorPos;

    let binding = ui.window().window_handle();
    let h = raw_window_handle::HasWindowHandle::window_handle(&binding).ok()?;
    let RawWindowHandle::Win32(win) = h.as_raw() else {
        return None;
    };
    let hwnd = windows::Win32::Foundation::HWND(win.hwnd.get() as *mut _);
    unsafe {
        let mut pt = POINT::default();
        GetCursorPos(&mut pt).ok()?;
        let _ = ScreenToClient(hwnd, &mut pt);
        Some((pt.x as f32 / scale, pt.y as f32 / scale))
    }
}

/// 判断两个路径是否位于同一卷（盘符比较，任务 2.1）。
fn same_volume(a: &std::path::Path, b: &std::path::Path) -> bool {
    let vol = |p: &std::path::Path| -> String {
        p.components()
            .next()
            .map(|c| c.as_os_str().to_string_lossy().to_uppercase())
            .unwrap_or_default()
    };
    let (va, vb) = (vol(a), vol(b));
    !va.is_empty() && va == vb
}
