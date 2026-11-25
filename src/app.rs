//! src/app.rs
//! 应用装配：状态机 + UI 回调 + 后台任务。
//!
//! 线程模型：
//! - UI 线程：Slint 事件循环；所有回调与状态变更都在这里发生。
//! - 目录列举线程：`try_list_directory` / 驱动器枚举在此执行，结果经通道
//!   回传，由轮询定时器合并（代号 generation 过期即丢弃）。
//! - 文件操作线程：复制/移动/删除，进度经共享状态暴露。

use std::cell::RefCell;
use std::collections::{HashMap, HashSet, VecDeque};
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
    level <= log_level()
}

/// 日志级别（0=静默 1=常规 2=详细），每个进程只解析一次。
///
/// 环境变量之外还有一个**标记文件**开关：资源管理器/浏览器拉起的接管进程
/// 不是我们启动的，继承不到 `ROVE_LOG`，而这类冷启动恰恰是最需要日志的
/// 场景。exe 同目录放一个空文件 `rove.verbose` 即可，效果等同 `ROVE_LOG=2`。
fn log_level() -> u8 {
    static LEVEL: std::sync::OnceLock<u8> = std::sync::OnceLock::new();
    *LEVEL.get_or_init(|| {
        if let Ok(v) = std::env::var("ROVE_LOG") {
            let v = v.trim();
            return if v == "2" || v.eq_ignore_ascii_case("verbose") || v.eq_ignore_ascii_case("debug")
            {
                2
            } else {
                1
            };
        }
        let marker = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|d| d.join("rove.verbose")))
            .map(|p| p.exists())
            .unwrap_or(false);
        if marker {
            2
        } else {
            0
        }
    })
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
    log_write_lvl("L0", msg);
}

/// 带级别标记写一行日志。
///
/// 每行都标出自己属于哪一级，用户拿一份 `ROVE_LOG=2` 的日志时能一眼看出
/// 哪些行在默认级别（`L1`）本来就不会有：
///
/// * `L0` —— 无条件写入，正常使用一行都不会产生（`logal!`）。
/// * `L1` —— `ROVE_LOG=1`：一次性的状态、异常、取证告警。
/// * `L2` —— `ROVE_LOG=2`：逐条/逐帧的流量日志（缩略图、登记、行补丁…）。
pub(crate) fn log_write_lvl(lvl: &str, msg: &str) {
    use std::io::Write as _;
    // L1 才降噪：`L0` 是必须查清的偶发故障（本来就极少），`L2` 是用户显式
    // 开到「逐条」的详细档，两者都不该被动折叠。
    let mut folded = 0u32;
    let mut global_folded = 0u32;
    if lvl == "L1" {
        match l1_admit(msg) {
            Some((same, global)) => {
                folded = same;
                global_folded = global;
            }
            // 这一条被折叠掉了：只记账，不写盘。
            None => return,
        }
    }
    let path = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("rove.log")))
        .unwrap_or_else(|| std::env::temp_dir().join("rove.log"));
    maybe_rotate_log(&path);
    // 必须先拼成整行、再一次 write_all：`write_fmt` 会按格式片段分多次
    // write_all，多个后台线程并发写日志时会把两行交错成一团
    // （表现为时间戳重复、两行粘成一行）。
    let mut line = format!("[rove:{lvl}] {} {}", log_stamp(), msg);
    if global_folded > 0 {
        line.push_str(&format!("（上一秒日志过密，另有 {global_folded} 行已折叠）"));
    }
    if folded > 0 {
        line.push_str(&format!("（上一窗口同类已折叠 {folded} 条）"));
    }
    line.push('\n');
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = f.write_all(line.as_bytes());
    }
}

/// L1 的防爆措施：**同一类**日志最多连续灌这么多次，超过就折叠成计数。
///
/// `ROVE_LOG=1` 是给用户/排查开的一档「日常事件」，一个卡住的循环、一次
/// 40ms 轮询里忘了节流的一行、拖着一列文件反复触发的一次逻辑，都能在一秒内
/// 刷出成千上万行，把真正的问题埋掉（还会把日志文件撑大）。所以：
///
/// * 按**标签**（`[menu]` / `[rows]` / `[thumb]` …）发令牌，容量 6、每秒回 6 个；
///   折叠掉的数量攒着，等下一条同类放行时补一句「已折叠 N 条」。
/// * 全局限额每秒 240 行（所有标签合计），超了就整段丢弃并计数。
///
/// 被折叠的永远是**重复**：每个标签每分钟的头几条、状态变化、``!!`` 异常
/// （走 `L0`，完全不经过这里）都不受影响。
const L1_BURST: f32 = 6.0;
const L1_REFILL_PER_SEC: f32 = 6.0;
const L1_GLOBAL_BURST: f32 = 240.0;
const L1_GLOBAL_REFILL_PER_SEC: f32 = 240.0;
/// 最多记多少个不同标签的桶（超出的标签共用最后那个桶，照样被限流）。
const L1_MAX_TAGS: usize = 48;

struct L1Bucket {
    tag: String,
    tokens: f32,
    last_ms: u64,
    folded: u32,
}

struct L1State {
    buckets: Vec<L1Bucket>,
    global_tokens: f32,
    global_last_ms: u64,
    global_folded: u32,
}

fn l1_state() -> &'static std::sync::Mutex<L1State> {
    static STATE: std::sync::OnceLock<std::sync::Mutex<L1State>> = std::sync::OnceLock::new();
    STATE.get_or_init(|| {
        std::sync::Mutex::new(L1State {
            buckets: Vec::new(),
            global_tokens: L1_GLOBAL_BURST,
            global_last_ms: 0,
            global_folded: 0,
        })
    })
}

/// 取这条 L1 日志的「标签」（第一个方括号里的词）；没有标签就按第一个词算，
/// 这样 `index: rebuild …` 这种也归得到一起。
fn l1_tag(msg: &str) -> &str {
    if let Some(rest) = msg.strip_prefix('[') {
        if let Some(end) = rest.find(']') {
            return rest.get(..end).unwrap_or("other");
        }
    }
    match msg.split([' ', ':']).next() {
        Some("") | None => "other",
        Some(word) => word,
    }
}

/// 补令牌：返回补完之后的令牌数（顺便把 `last_ms` 推到现在）。
fn refill(tokens: f32, last_ms: &mut u64, now: u64, burst: f32, per_sec: f32) -> f32 {
    let elapsed = now.saturating_sub(*last_ms) as f32 / 1000.0;
    *last_ms = now;
    (tokens + elapsed * per_sec).min(burst)
}

/// 这一条 L1 该不该写。`Some((同类折叠数, 全局折叠数))` = 写，
/// `None` = 折叠掉。
fn l1_admit(msg: &str) -> Option<(u32, u32)> {
    let now = now_ms();
    let mut st = match l1_state().lock() {
        Ok(g) => g,
        // 锁中毒（某次日志里 panic）也不能因为写日志把程序拖垮。
        Err(e) => e.into_inner(),
    };
    st.global_tokens = refill(
        st.global_tokens,
        &mut st.global_last_ms,
        now,
        L1_GLOBAL_BURST,
        L1_GLOBAL_REFILL_PER_SEC,
    );
    if st.global_tokens < 1.0 {
        st.global_folded = st.global_folded.saturating_add(1);
        return None;
    }
    let tag = l1_tag(msg).to_string();
    let idx = match st.buckets.iter().position(|b| b.tag == tag) {
        Some(i) => i,
        None => {
            if st.buckets.len() >= L1_MAX_TAGS {
                st.buckets.len() - 1
            } else {
                st.buckets.push(L1Bucket {
                    tag,
                    tokens: L1_BURST,
                    last_ms: now,
                    folded: 0,
                });
                st.buckets.len() - 1
            }
        }
    };
    let bucket = &mut st.buckets[idx];
    bucket.tokens = refill(bucket.tokens, &mut bucket.last_ms, now, L1_BURST, L1_REFILL_PER_SEC);
    if st.buckets[idx].tokens < 1.0 {
        st.buckets[idx].folded = st.buckets[idx].folded.saturating_add(1);
        return None;
    }
    st.buckets[idx].tokens -= 1.0;
    st.global_tokens -= 1.0;
    let same = std::mem::take(&mut st.buckets[idx].folded);
    let global = std::mem::take(&mut st.global_folded);
    Some((same, global))
}

/// 日志文件上限：到顶就把它改名成 `rove.log.old`（旧的直接覆盖），重新开始。
/// 排查日志没人清，一次 `ROVE_LOG=2` 的长会话就能写到几十上百 MB。
const MAX_LOG_BYTES: u64 = 4 * 1024 * 1024;
static LOG_WRITES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static LOG_ROTATE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// 每 512 行检查一次大小（`metadata` 也要系统调用，不必每行都问）。
fn maybe_rotate_log(path: &std::path::Path) {
    use std::sync::atomic::Ordering;
    if LOG_WRITES.fetch_add(1, Ordering::Relaxed) % 512 != 0 {
        return;
    }
    let Ok(meta) = std::fs::metadata(path) else { return };
    if meta.len() < MAX_LOG_BYTES {
        return;
    }
    let Ok(guard) = LOG_ROTATE_LOCK.lock() else { return };
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "rove.log".into());
    let old = path.with_file_name(format!("{name}.old"));
    // 别的线程可能正开着它写：Windows 上 rename 会失败，那就下次再来。
    let _ = std::fs::rename(path, &old);
    drop(guard);
}

/// 单调的毫秒时钟，只用于日志节流（不参与任何逻辑判定）。
pub(crate) fn now_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// 诊断日志：`ROVE_LOG` 存在时写入 `rove.log`（带本地时间戳）。
#[macro_export]
macro_rules! logln {
    ($($arg:tt)*) => {{
        if $crate::app::log_enabled(1) {
            $crate::app::log_write_lvl("L1", &format!($($arg)*));
        }
    }};
}

/// 详细级诊断日志：仅 `ROVE_LOG=2|verbose|debug` 时写入。
/// 用于高频、仅在排查布局/虚拟窗口问题时才有价值的噪音日志。
#[macro_export]
macro_rules! logv {
    ($($arg:tt)*) => {{
        if $crate::app::log_enabled(2) {
            $crate::app::log_write_lvl("L2", &format!($($arg)*));
        }
    }};
}

/// **无条件**写入的异常日志：不需要 `ROVE_LOG`，也不需要 `rove.verbose`。
///
/// 只给「复现不了但必须查清」的偶发故障用（目录读失败、标签拖拽状态残留、
/// 空结果）。正常使用一行都不会产生，所以不会让 `rove.log` 天天长大；
/// 也正因为它绕过了级别开关，绝不能拿来做常规诊断。
#[macro_export]
macro_rules! logal {
    ($($arg:tt)*) => {{
        $crate::app::log_write_lvl("L0", &format!($($arg)*));
    }};
}


/// 窗口焦点抖动取证：3 秒窗口内的获焦/失焦次数。
static FOCUS_WINDOW_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static FOCUS_COUNT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
static FOCUS_LOGGED_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// `sync_tabs` 每秒调用次数统计（标签栏闪烁取证用）。
static SYNC_TABS_WINDOW_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static SYNC_TABS_COUNT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
static SYNC_TABS_LOGGED_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// 虚拟列表行高（与 ui/file_list.slint 的 row-h 保持一致）。
const ROW_HEIGHT: f32 = 30.0;
/// 视口上下过扫描行数。
const OVERSCAN: usize = 4;
/// 单条索引记录的**兜底**粗估（真实值由 `GlobalIndex::heap_bytes` 上报）。
///
/// 曾经写的是 80 B/条（那份注释假设「路径 + 名称 + 拼音字段」共 80 字节），
/// 实测仅结构体内联就有 120 字节，再加路径 / 名字 / 全拼 / 简拼四条堆字符串，
/// 平均约 220 字节——旧常数让设置页把占用少报了 3 倍。去掉反向定位表后
/// 内联降到 96 字节（ASCII 名约 155 B、中文名约 205 B），
/// 真实值由 `heap_bytes` 上报，这个常数只在不该有索引时兜底。
const INDEX_ENTRY_BYTES: usize = 220;

/// 后台预建拼音缓存的条目数上限：比这更大的目录只在真的有搜索词时才算
/// （否则每进一个大目录就白烧一个后台核）。
const PINYIN_EAGER_MAX: usize = 20000;
/// 缩略图缓存最大项数（宽松限制，主要受字节预算约束）。
const THUMB_CACHE_MAX_ITEMS: usize = 4096;
/// 缩略图缓存字节预算（192 MB）。Rove 的 Image 常驻 CPU 侧 RGBA，
/// 按最大尺寸(132px*2.0=264px)每张约 278 KB，192 MB 可存约 690 张。
const THUMB_CACHE_MAX_BYTES: usize = 192 * 1024 * 1024;
/// 全盘索引空闲多久后把内存还给系统（分钟）。
///
/// 索引常驻要几十兆，而绝大多数会话根本不做全盘搜索。清掉之后再次搜索会
/// 重新读盘（约 0.3 s、后台线程、不卡输入），比长期白占划算。
const INDEX_UNLOAD_IDLE_MINS: u64 = 5;
/// 失败负缓存的条数上限（先进先出淘汰）。
const THUMB_FAILED_MAX_ITEMS: usize = 4096;
/// 缩略图磁盘缓存预算（512 MB，对标 Pulse）。超限按最后修改时间淘汰到 80%。
const THUMB_DISK_MAX_BYTES: u64 = 512 * 1024 * 1024;

// ---- 右键菜单动作 ID ----
const CMD_PASTE: &str = "paste";
const CMD_TERMINAL: &str = "terminal";
const CMD_NEW_FOLDER: &str = "new_folder";
const CMD_NEW_TXT: &str = "new_txt";
const CMD_BG_PROPERTIES: &str = "bg_properties";
const CMD_REFRESH: &str = "refresh";
const CMD_SELECT_ALL: &str = "select_all";
const CMD_INVERT_SELECTION: &str = "invert_selection";
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
/// 类型筛选下拉里的一行（Rust 侧）。文案是给人看的类型名，
/// `keys` 是它的真实筛选键集合（`dir` / 小写扩展名 / `noext`）。
struct TypeFilterGroup {
    label: String,
    keys: Vec<String>,
    count: usize,
}

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
    /// 列举本身失败了（权限 / 掉线的共享 / 路径已删），`entries` 一定是空的。
    /// 与「目录真的是空的」必须分开处理：以前两者长得一模一样，「此文件夹为空」
    /// 就是一次网络抖动留下的假象，而且再也不会自己恢复。
    failed: bool,
}

/// 地址栏路径校验的后台结果。
///
/// `exists()` 在网络盘 / WSL 共享上能卡好几秒，所以真正的校验放在后台线程，
/// 结果回到 poll_timer 再决定「导航」还是「提示路径不存在」。
struct PathProbe {
    /// 请求序号：用户连按几次回车时只认最后一次。
    seq: u64,
    /// 用户输入的原文（错误提示里显示它，而不是展开变量后的路径）。
    label: String,
    /// 发起时当前面板所在的目录。结果回来时如果已经不在那儿了，
    /// 说明用户自己换过目录，这次回车就别再硬拽他走。
    from: Option<PathBuf>,
    /// Ok = 可用目录，Err = 不可用。
    outcome: Result<PathBuf, ()>,
}

/// 深度搜索请求：在目录子树中递归查找匹配项（文件夹视图搜索）。
#[derive(Clone)]
struct DeepRequest {
    tab_id: u32,
    pane: usize,
    generation: u64,
    deep_generation: u64,
    root: PathBuf,
    query: String,
    /// 本批最多要多少条；`0` = 丢掉当前扫描（结果已经没用了，别白烧 CPU）。
    want: usize,
}

/// 深度搜索结果。
struct DeepResult {
    tab_id: u32,
    pane: usize,
    generation: u64,
    deep_generation: u64,
    query: String,
    /// 本批新增的命中（**追加**到已有结果，不是替换）。
    items: Vec<crate::search::index::DeepItem>,
    /// 子文件夹已经搜完，不会再有下一批。
    done: bool,
    /// 是被上限掐停的（不是真的扫完），状态栏要说明结果不完整。
    capped: bool,
    /// 已经扫过多少个条目（只用于状态栏进度，不改结果）。
    scanned: usize,
}

/// 拼音缓存的后台计算结果（大目录里同步算要十几到几十毫秒）。
///
/// `seq` 对不上（期间条目换了 / 又被重新排序）就整批丢弃：缓存与 `entries`
/// 逐项平行，用错位的数据去匹配会命中错误的文件。
struct PinyinJobResult {
    tab_id: u32,
    pane: usize,
    seq: u64,
    fields: Vec<crate::search::PinyinFields>,
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

/// 「想要」集合：当前窗口（可见行 + 上下缓冲行）的缩略图 key。
///
/// 每次重建可视行模型时由 UI 线程**整体替换**。worker 每取到一个任务先查这里：
/// 不在集合里说明用户已经滚走 / 跳走，直接丢弃不浪费解码时间——连续快速跳转时，
/// 新窗口的请求不会排在上一个窗口剩下的几十条后面（那些会被瞬间跳过）。
/// 集合里装的是**整个窗口**（可见行 + 上下缓冲行）的 key，不只是可见行。
fn thumb_wanted() -> &'static Mutex<std::collections::HashSet<ThumbKey>> {
    static WANTED: OnceLock<Mutex<std::collections::HashSet<ThumbKey>>> = OnceLock::new();
    WANTED.get_or_init(|| Mutex::new(Default::default()))
}

/// 把一条 key 加入「想要」集合（发请求时立刻加，避免 worker 抢在整轮发布之前跳过它）。
fn thumb_want(key: &ThumbKey) {
    if let Ok(mut w) = thumb_wanted().lock() {
        w.insert(key.clone());
    }
}

/// 侧栏图标（快速访问 / 网络位置）的像素尺寸。
/// 这类请求不属于列表可视屏幕，缩略图调度整体替换「想要」集合时**不丢掉**，
/// 否则侧栏图标会被当成过期请求排掉。`build_sidebar` 也用这份常量。
const SIDEBAR_ICON_PX: u32 = 32;

/// 「Shell 系统菜单新建文件」快照的有效期：菜单从右键弹到点「新建」再落地，
/// 期间任何一次列举结果都可能把它消化掉，所以给得比一次操作宽一些。
const SHELL_NEW_WATCH: std::time::Duration = std::time::Duration::from_secs(20);

/// 整体替换「想要」集合（一轮行模型建完时调用）。
/// 侧栏小图标（`SIDEBAR_ICON_PX`）会保留，其余全部按本轮屏幕内容重建。
fn publish_thumb_wanted(keys: std::collections::HashSet<ThumbKey>) {
    if let Ok(mut w) = thumb_wanted().lock() {
        w.retain(|k| k.px_size == SIDEBAR_ICON_PX);
        w.extend(keys);
    }
}

fn thumb_is_wanted(key: &ThumbKey) -> bool {
    thumb_wanted().lock().map(|w| w.contains(key)).unwrap_or(true)
}

/// worker 判定为「已过期」而跳过的请求，攒在这里由 UI 线程统一清理。
///
/// 一条一跳 `invoke_from_event_loop` 会把事件循环灌满：大幅滚动一次就作废
/// 上百条排队请求，每条都要借一次 App、置一次刷新标记——重绘反而被拖慢，
/// 而它们本来就只是「从 pending 里摘掉」。
fn thumb_skipped() -> &'static Mutex<Vec<ThumbKey>> {
    static SKIPPED: OnceLock<Mutex<Vec<ThumbKey>>> = OnceLock::new();
    SKIPPED.get_or_init(|| Mutex::new(Vec::new()))
}

/// 跳过批次溢出（UI 线程太久没来取）：标记一下，让 UI 用「不在当前窗口就不算在解」
/// 兜底清理一遍。直接丢掉批次会让那些 key 永远留在 `thumb_pending` 里，
/// 滚回去时那一行就再也不会发请求（永久空白）。
static THUMB_SKIP_OVERFLOW: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// 记下一条被跳过的请求（worker 线程调用，多生产者）。
fn note_thumb_skipped(key: ThumbKey) {
    if let Ok(mut v) = thumb_skipped().lock() {
        if v.len() > 4096 {
            v.clear();
            THUMB_SKIP_OVERFLOW.store(true, std::sync::atomic::Ordering::Release);
        }
        v.push(key);
    }
}

/// 取走当前批次（UI 线程调用，单消费者）。
fn take_thumb_skipped() -> Vec<ThumbKey> {
    thumb_skipped()
        .lock()
        .map(|mut v| std::mem::take(&mut *v))
        .unwrap_or_default()
}

/// 缓存项：原图 + 估算字节数（用于字节预算淘汰）。
struct CachedThumb {
    img: slint::Image,
    bytes: usize,
    /// 插入序号（进程内单调递增）。行模型做增量更新时靠它判断
    /// 「这一行的缩略图到底变没变」——`slint::Image` 本身没法比较相等。
    epoch: u64,
    /// 最近一次被用到的序号（进程内单调递增，LRU 淘汰依据）。
    ///
    /// 以前用一条 `Vec<ThumbKey>` 维护 LRU 顺序，**每次命中**都要线性扫描
    /// 整个顺序表（上限 4096 条，每行每帧都命中）才能把它挪到队尾。
    /// 改成记一个序号：命中只写一个 u64，淘汰时才扫一遍找最久未用的。
    last_used: u64,
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
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
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

    /// 面板级判定：**搜索态与回收站共用同一份排序状态**。
    ///
    /// 两者都是「跨目录的结果列表」，用户在这里按路径排序的意图是一样的；
    /// 各记一套只会让人怀疑「刚才排好的顺序怎么没了」。
    fn of_pane(path: Option<&std::path::Path>, searching: bool) -> Self {
        if searching {
            return Self::Recycle;
        }
        Self::of(path)
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

/// 目录 mtime 探测结果（后台 `symlink_metadata` 的产物）。
struct DirMtimeProbe {
    path: PathBuf,
    mtime: Option<SystemTime>,
}

/// 启动目录 mtime 探测线程：每次收到一批路径就跑一遍并回传结果。
fn spawn_mtime_probe(
    req_rx: std::sync::mpsc::Receiver<Vec<PathBuf>>,
    out_tx: std::sync::mpsc::Sender<Vec<DirMtimeProbe>>,
) {
    let _ = std::thread::Builder::new()
        .name("rove-mtime".into())
        .spawn(move || {
            while let Ok(paths) = req_rx.recv() {
                let results: Vec<DirMtimeProbe> = paths
                    .into_iter()
                    .map(|path| {
                        let mtime = std::fs::symlink_metadata(&path)
                            .and_then(|m| m.modified())
                            .ok();
                        DirMtimeProbe { path, mtime }
                    })
                    .collect();
                if out_tx.send(results).is_err() {
                    break;
                }
            }
        });
}

/// 侧栏后台探测的一批系统数据：盘符容量 / 回收站统计 / 网络位置。
///
/// 这几项全是系统调用（`GetVolumeInformationW` + `GetDiskFreeSpaceExW`、
/// `SHQueryRecycleBinW`、`WNetOpenEnumW` 枚举、Network Shortcuts 目录遍历），
/// 遇到掉线的映射盘或休眠的移动盘会阻塞几百毫秒到几秒，
/// **绝不能放在 UI 线程上**。探测线程只在收到请求时跑一轮。
#[derive(Default)]
struct SidebarProbe {
    drives: Vec<DriveInfo>,
    stats: crate::fs::drives::RecycleBinStats,
    wnet: Vec<String>,
    places: Vec<(String, String)>,
}

/// 启动侧栏探测线程：收到一个 `()` 就跑一轮并把结果发回去。
fn spawn_sidebar_probe(
    req_rx: std::sync::mpsc::Receiver<()>,
    out_tx: std::sync::mpsc::Sender<SidebarProbe>,
) {
    let _ = std::thread::Builder::new()
        .name("rove-sidebar".into())
        .spawn(move || {
            while req_rx.recv().is_ok() {
                let probe = SidebarProbe {
                    drives: list_drives(),
                    stats: recycle_bin_stats(),
                    wnet: list_network_locations(),
                    places: list_network_places(),
                };
                if out_tx.send(probe).is_err() {
                    break;
                }
            }
        });
}

/// 应用主体。
pub struct App {
    ui: MainWindow,
    state: AppState,
    config: Config,
    jobs: JobManager,
    drives: Vec<DriveInfo>,
    /// 回收站条目数 / 体积（来自后台探测，UI 线程不查）。
    recycle_stats: crate::fs::drives::RecycleBinStats,
    /// WNet 枚举到的网络位置（后台探测）。
    net_wnet: Vec<String>,
    /// `%APPDATA%\...\Network Shortcuts` 里的快捷方式（后台探测）。
    net_places: Vec<(String, String)>,
    /// 侧栏探测请求 / 结果通道（系统调用全在后台线程，见 `SidebarProbe`）。
    sidebar_probe_tx: std::sync::mpsc::Sender<()>,
    sidebar_probe_rx: std::sync::mpsc::Receiver<SidebarProbe>,
    /// 已有一个探测在跑：不重复排队（15s 定时器 / 刷新按钮 / 缩略图刷新会挤在一起）。
    sidebar_probe_busy: bool,
    /// 侧栏 32px 图标已更新：需要同步侧栏（只此一种情况，别为每批缩略图重建侧栏）。
    sidebar_icon_dirty: bool,
    /// 目录 mtime 探测请求 / 结果通道（stat 在后台线程，见 `poll_dir_changes`）。
    mtime_probe_tx: std::sync::mpsc::Sender<Vec<PathBuf>>,
    mtime_probe_rx: std::sync::mpsc::Receiver<Vec<DirMtimeProbe>>,
    /// 已有一轮探测在跑：1 秒定时器到点时直接跳过，不排队。
    mtime_probe_busy: bool,
    /// 修改时间文本的记忆表（`SystemTime` → "yyyy-MM-dd HH:mm"）。
    ///
    /// `fs::format_time` 每次都要走一遍 Win32 的 `SystemTimeToTzSpecificLocalTime`，
    /// 而滚动 / 缩略图刷新会在一秒内把同一批行重建几十次。有上限（超了整体清空）：
    /// 一次可见的只有几十行，4096 条足够覆盖来回滚动的范围。
    time_text_memo: std::collections::HashMap<Option<SystemTime>, SharedString>,
    /// 搜索框输入防抖：`(待应用的查询词, 该词最后一次输入的时刻)`。
    ///
    /// 每个字符都会触发一次「全目录匹配 + 打分 + 重排」；几万条的目录里
    /// 逐字重建会明显卡手，攒 150ms 再跑一次。
    pending_search: Option<(String, std::time::Instant)>,
    /// 类型筛选下拉当前挂在哪个窗格上（`None` = 没开）。
    type_filter_pane: Option<usize>,
    /// 下拉每行背后的真实类型键。
    ///
    /// 下拉模型里只有「文案 / 数量 / 勾选」——文案是给人看的类型名（`PNG 图像`），
    /// 而筛选判定要用扩展名。同一个类型名可能对应多个扩展名（jpg/jpeg 都叫
    /// 「JPEG 图像」），所以点击回投时必须靠这张表还原成键集合。
    type_filter_groups: Vec<TypeFilterGroup>,
    /// 侧边栏数据指纹，避免无变化时重建模型打断滚动。
    sidebar_fingerprint: u64,
    /// 左栏本帧真正写进行模型的窗口 (first, count)。
    ///
    /// 两个用途：
    /// 1. 各处置 None 作为「内容变了、必须重建」的失效信号；
    /// 2. 滚动回调里比对：行在 Slint 里是**绝对定位**（`y: index * row-h`），
    ///    只要渲染窗口没变，像素级滚动就无需重建整整两栏的行模型。
    visible_cache: Option<(usize, usize)>,
    /// 右栏同上。
    visible_cache_right: Option<(usize, usize)>,
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
    /// 上一次左键按下的行。
    ///
    /// 「点名称进入重命名」要求这是**同一行的连续第二次按下**（资源管理器就是
    /// 这个手感：先选中，再点名称才开始改名）。这样右键菜单之后随手点一下文字
    /// 永远不会变成改名——那一次只是「第一次按下」。
    ///
    /// 之所以不用「菜单开过就抑制下一次点击」那套：菜单在独立线程上弹，关掉菜单
    /// 的那一下点击会直接落到列表上，而且可能落在名称以外的位置，标志很容易被
    /// 提前吃掉或过期，日志里表现为「偶尔触发 / 要点两下才灵」。
    last_left_press_row: Option<PathBuf>,
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
    /// 原生右键菜单收起后（阻塞调用返回）推迟一拍重载目录，让「新建」出来的
    /// 文件尽快被上面的快照 diff 认出来，不必等 1 秒的 mtime 轮询。
    shell_new_reload_at: Option<std::time::Instant>,
    /// 文件任务刚结束时的一小段“保护期”：这段时间里到达的**不完整**列举
    /// （任务期间发起、只看到一部分新文件的那些）不消费“待揭示目标”。
    reveal_grace_until: Option<std::time::Instant>,
    /// 哪个面板的列举结果可以消费「待揭示目标」（`None` = 用当前聚焦面板）。
    /// 跨分屏拖放时目标在**另一个**面板里，不能只认聚焦那一栏。
    reveal_pane: Option<usize>,
    /// 后退回父目录时，把「刚离开的那个子目录」选中（资源管理器同款）。
    /// (标签 id, 面板下标, 条目路径)：只在那一个标签那一片的下一份列举里
    /// 消费一次，命中与否都清掉。
    pending_history_select: Option<(u32, usize, PathBuf)>,
    /// `scroll_to_index` 该滚哪个面板（`None` = 当前聚焦面板）。
    scroll_to_pane: Option<usize>,
    /// diff 命中后待触发的重命名路径（等列表/滚动就绪再执行，避免抢焦点）。
    pending_shell_new_rename: Option<PathBuf>,
    /// 中/大/超大图标视图「展开名称」浮层的定位缓存：每栏记住上次
    /// （目标路径 → 显示行号）。先用它验一次，命中就不必扫整个显示列表。
    expand_row_cache: [Option<(PathBuf, i32)>; 2],
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
    /// 缩略图诊断（ROVE_LOG=1 可观测）：当前可视窗口签名，变了才重新打印一轮。
    thumb_diag_sig: Option<(String, usize, usize, usize, usize)>,
    /// 本窗口还可打印多少条「发出 / 到图」明细（用完后本轮静默）。
    thumb_diag_left: u32,
    thumb_diag_got: u32,
    /// 可见窗口内「还没出图」的首个条目下标 + 空白数，变化时才打印。
    thumb_blank_sig: Option<(i32, usize)>,
    /// 本帧窗口（可见行 + 上下缓冲行）的 key 集合：一轮行模型建完时整体发布。
    window_wanted: HashSet<ThumbKey>,
    /// 被 worker 丢弃的过期请求计数 + 上次汇总时刻（避免逐条刷屏）。
    thumb_cancel_n: usize,
    thumb_cancel_t: std::time::Instant,
    /// 缩略图插入序号发生器（配合 `CachedThumb::epoch` 做行增量更新）。
    thumb_epoch_next: u64,
    /// 上一帧写进左/右栏行模型的内容，用于逐行比对。
    row_diff_left: Option<RowDiffState>,
    row_diff_right: Option<RowDiffState>,
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
    /// 列举读失败后的自动重试：(标签, 面板, 登记时刻, 已重试次数)。
    ///
    /// 网络共享慎一下是常事，不该让用户手动刷新。重试 3 次仍失败就收手，
    /// 标记留在面板上，等切标签时再试（见 retry_failed_panes）。
    list_retry: Option<(usize, usize, std::time::Instant, u8)>,
    /// 标签拖动松手后那次补发的 clicked 要丢弃，避免误切标签。
    suppress_tab_click: Option<std::time::Instant>,
    /// 会话内见过的最大的窗口缩放系数（取图尺寸用，见 `ui_scale()`）。
    ui_scale: f32,
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
    /// 本轮按下被「空白层兜底」当成行点击处理了（见 `background_pointer_event`）。
    /// 抬手时空白层会把 `hits=0` 的单击当「点空白 → 取消选择」，
    /// 不拦住的话刚选中的行会立刻被取消，表现为闪一下选中又掉。
    bg_hit_row: bool,
    /// 空白处右键按下时先存下菜单，等抬手再弹（见 `background_pointer_event`）。
    deferred_bg_menu: Option<MenuScope>,
    /// 行内右键：按下时只记下要弹的菜单，抬手才真的弹（见 `row_pointer_event`）。
    pending_row_menu: Option<MenuScope>,
    /// 空白处左键按下的位置（视口坐标）：抬手时用来区分「点一下」和「拖着框选」。
    blank_press: Option<(f32, f32)>,
    /// 空白处上一次单击的 (时刻, x, y, 窗格)：两次够近才算双击 → 回上一级。
    /// 带上窗格是防「左栏点一下、右栏点一下」被误判成双击。
    blank_click: Option<(std::time::Instant, f32, f32, usize)>,
    /// 索引还没加载时敲下的那次全盘搜索。加载完成后由轮询定时器补跑，
    /// 不依赖“索引就绪”那条通知 —— 通知只要漏一次，用户就只看到空列表。
    pending_global_search: Option<String>,
    /// 已经做过「列宽自适应」的目录键，按栏记录（左 0 / 右 1）。
    /// 每个目录只在打开时算一次，避免滚动 / 刷新时反复重算导致列宽跳动。
    /// 必须按栏分：两栏可能在不同目录，共用一个键会让切焦点时用另一栏的内容
    /// 重新拟合，表现为「顶栏列宽自己变」。
    autofit_key: [Option<String>; 2],
    /// 界面上当前实际生效的四列宽度（名称/修改时间/类型/大小），按栏一份。
    /// 拖动某列时以它为基准，避免把没动过的列弹回默认值。
    cur_col_widths: [[f32; 5]; 2],
    /// 后台大小变化时置位，等待主线程空闲时静默重排。
    pub size_sort_pending: bool,
    /// 撤消 / 重做栈（见 `ops::undo`）。
    pub undo: crate::ops::undo::UndoStack,
    /// 用户最后一次自己滚动列表（滚轮 / 拖滚动条）的时刻。
    /// 「空闲时静默重排」靠它避开正在滚动的情形，否则滚到一半列表自己跳一下。
    pub last_scroll: Option<Instant>,
    /// 切换标签时置位，屏蔽切换过程中 Slint 因内容高度变化自动 Clamp 而
    /// 产生的残留 scroll-changed 事件污染新标签数据（否则表现为切标签白屏）。
    syncing_tab: bool,
    /// 首次启动的 75% 居中：show() 后 scale_factor() 仍可能是 1.0（窗口尚未真正
    /// 落到显示器），必须等到首帧 RedrawRequested 窗口已 realize、scale 正确时再居中，
    /// 否则尺寸会按物理像素当逻辑像素计算而偏大、偏离中心。
    first_run_pending: bool,
    /// 会话是 `--background` 建的、窗口还没显示过。
    ///
    /// 那种情况下 `set_maximized` / `show` / 拖拽热区初始化全都没做，等真正
    /// 显示窗口（托盘点击 / 资源管理器接管 / 二次启动）时补做一次，然后清位。
    window_pending_show: bool,
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
    /// 设置页点过压缩关联的「更新」：后台扫描线程跑着，结果由主循环收。
    archive_scan_pending: bool,
    /// 上面那次扫描只刷新的那一项 id；`None` = 「更新全部」。
    archive_rescan_id: Option<String>,
    /// 正在等的那一轮扫描的序号（连续「更新」时旧结果会被丢弃）。
    archive_scan_gen: u64,
    /// 「解压文件」的目录选择框正在弹（防止再弹第二个）。
    archive_extract_picking: bool,
    /// 设置页点过「重新扫描驱动器」：状态栏已挂上「正在重新扫描…」，
    /// 等索引线程回报就绪后补发一条完成信息，避免那句话永久留在右下角。
    index_rescan_status: bool,
    /// 激活后的按键屏障截止时刻（见 `setup_keyboard` 里的说明）。
    /// 窗口刚被外部激活时，手上还按着的键往往属于上一个窗口的那次操作，
    /// 必须丢掉；`None` = 不拦。
    keys_block_until: Option<std::time::Instant>,
    /// 屏障期的「已记过一笔」标记：按住不放的键会以 30Hz 重复到，不能每次刷一行日志。
    keys_block_logged: bool,
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
    /// LRU 用的单调递增序号（每次命中 / 插入都 +1）。
    thumb_use_seq: u64,
    /// 当前缓存的估算总字节数（用于字节预算淘汰）。
    thumb_cache_bytes: usize,
    /// 已请求未完成的缩略图（防重复提交）。
    thumb_pending: HashSet<ThumbKey>,
    /// 上次 `sync_tabs` 下发的标签模型指纹（内容相同就不换模型，见该函数）。
    last_tabs_sig: std::cell::Cell<u64>,
    /// 上次打印内存明细的时刻（`ROVE_LOG=2` 节流 2 秒）。
    last_mem_log: Option<std::time::Instant>,
    /// 失败负缓存（本会话内不再重试）。
    ///
    /// 有上限：长会话翻遍一堆没有缩略图的目录（压缩包、字体、系统文件）
    /// 会攒出几万条 PathBuf，只增不减就是慢性泄漏。上限按先进先出淘汰，
    /// 淘汰掉的老条目最多被重试一次，代价远小于永不释放。
    thumb_failed: HashSet<ThumbKey>,
    /// 负缓存的插入顺序（FIFO 淘汰用，与 `thumb_failed` 同步增删）。
    thumb_failed_order: VecDeque<ThumbKey>,
    /// 批量刷新标记：后台有缩略图到达时置位，由 poll_timer 统一刷新。
    thumb_needs_refresh: bool,
    /// 缩略图请求通道：**全部**请求走同一条队列，顺序即渲染顺序。
    ///
    /// 曾经分成「优先池 / 主池」两套（可见行 2 条线程、缓冲行 6 条），
    /// 结果是排队顺序不重要、并发度最重要：网络盘上 6 条线程会先把看不见的
    /// 缓冲行啃完，可见行还在 2 条线程上慢慢排。现在只有一条队列，
    /// 请求按 `thumb_order_key` 的三段式顺序入队，所有 worker 按同一顺序取活。
    thumb_tx: Sender<ThumbKey>,
    list_tx: Sender<ListRequest>,
    list_rx: Receiver<ListResult>,
    /// 深度搜索通道（文件夹视图子树搜索，独立线程执行）。
    deep_tx: Sender<DeepRequest>,
    deep_rx: Receiver<DeepResult>,
    /// 拼音缓存后台计算通道（大目录避免 UI 线程卡顿）。
    pinyin_tx: Sender<PinyinJobResult>,
    pinyin_rx: Receiver<PinyinJobResult>,
    /// 地址栏路径校验通道（后台 stat，避免网络路径卡住界面线程）。
    path_probe_tx: Sender<PathProbe>,
    path_probe_rx: Receiver<PathProbe>,
    /// 最近一次路径校验的序号，用于丢弃过期结果。
    path_probe_seq: u64,
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
    /// 单击文件名进入重命名的延迟定时器：留下一个双击窗口。
    ///
    /// 不能一按下就弹输入框——「选中某行后双击它的名称打开」是最常见的动作，
    /// 立刻弹框会把第二下点击吃进输入框（表现为「双击打不开文件」）。
    /// 所以按下后等一个有效双击间隔，窗口内没有第二下按下才真正改名。
    rename_defer_timer: Timer,
    /// 接管资源管理器转来的「新标签打开」：先释放 App 借用，下一轮事件循环再执行。
    /// 直接在管道回调里导航会与 Slint 布局期触发的其它 UI 回调形成 RefCell 嵌套
    /// 借入（进程被 abort，窗口直接消失）。
    shell_open_timer: Timer,
    /// 待打开的资源管理器请求（None = 此电脑）。用队列而不是单个槽位，
    /// 连点两个文件夹时两条请求都要能打开。
    /// 队列元素：(要打开的目录, 「在文件夹中显示」要选中的条目)。
    shell_open_queue: Vec<(Option<PathBuf>, Option<PathBuf>)>,
    /// 本次会话是否被「关闭全部 / 关掉最后一个标签」显式收尾。
    ///
    /// 这两种收尾等于用户主动说「这次就这样了」：存档时不能把当前标签写进
    /// `last_tabs`，否则选了「启动时打开上次的标签页」时，下次又把这些标签
    /// 原样拉回来（用户预期是只开一个「此电脑」）。
    tabs_discarded: bool,
    /// 自身弱引用：供定时器回调 / 事件钩子升级回 App。
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
        // 拼音缓存的后台通道：只有"有搜索词且缓存还没算好"时才用得上。
        let (pinyin_tx, pinyin_rx) = mpsc::channel();
        let state = AppState::new(None, sort);
        let keymap = crate::ops::keymap::Keymap::new(&config.shortcuts);
        let (path_probe_tx, path_probe_rx) = mpsc::channel::<PathProbe>();
        let (sidebar_probe_tx, sidebar_probe_req_rx) = mpsc::channel::<()>();
        let (sidebar_probe_out_tx, sidebar_probe_rx) = mpsc::channel::<SidebarProbe>();
        spawn_sidebar_probe(sidebar_probe_req_rx, sidebar_probe_out_tx);
        let (mtime_probe_tx, mtime_probe_req_rx) = mpsc::channel::<Vec<PathBuf>>();
        let (mtime_probe_out_tx, mtime_probe_rx) = mpsc::channel::<Vec<DirMtimeProbe>>();
        spawn_mtime_probe(mtime_probe_req_rx, mtime_probe_out_tx);
        // 启动时立刻排一轮侧栏探测（盘符 / 回收站 / 网络位置），
        // 结果由 40ms 轮询应用 —— 连盘符也不在这里同步取：掉线的映射盘
        // 上一次 `GetVolumeInformationW` 就能卡住好几秒，那会卡死在启动路径上。
        let _ = sidebar_probe_tx.send(());
        Self {
            ui,
            state,
            config,
            keymap,
            jobs,
            drives: Vec::new(),
            recycle_stats: Default::default(),
            net_wnet: Vec::new(),
            net_places: Vec::new(),
            sidebar_probe_tx,
            sidebar_probe_rx,
            sidebar_probe_busy: true,
            sidebar_icon_dirty: false,
            mtime_probe_tx,
            mtime_probe_rx,
            mtime_probe_busy: false,
            time_text_memo: std::collections::HashMap::new(),
            pending_search: None,
            type_filter_pane: None,
            type_filter_groups: Vec::new(),
            sidebar_fingerprint: 0,
            visible_cache: None,
            visible_cache_right: None,
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
            last_left_press_row: None,
            pending_single_click: None,
            archive_scan_pending: false,
            archive_extract_picking: false,
            archive_rescan_id: None,
            archive_scan_gen: 0,
            shell_new_watch: None,
            shell_new_reload_at: None,
            reveal_grace_until: None,
            reveal_pane: None,
            pending_history_select: None,
            scroll_to_pane: None,
            pending_shell_new_rename: None,
            expand_row_cache: [None, None],
            hover_display_index: [-1, -1],
            overlay_hovered: [false, false],
            cell_hovered: [-1, -1],
            tip_log_sig: None,
            thumb_diag_sig: None,
            thumb_diag_left: 0,
            thumb_diag_got: 0,
            thumb_blank_sig: None,
            window_wanted: HashSet::new(),
            thumb_cancel_n: 0,
            thumb_cancel_t: std::time::Instant::now(),
            thumb_epoch_next: 0,
            row_diff_left: None,
            row_diff_right: None,
            last_click: None,
            blank_press: None,
            blank_click: None,
            sidebar_drag: None,
            network_drag: None,
            quick_drag: None,
            tab_drag: None,
            list_retry: None,
            suppress_tab_click: None,
            ui_scale: 1.0,
            suppress_sidebar_click: None,
            network_edit_target: None,
            drop_buffer: Vec::new(),
            dir_mtimes: HashMap::new(),
            drop_timer: Timer::default(),
            drag_pending: None,
            rubber_band_base: None,
            bg_hit_row: false,
            deferred_bg_menu: None,
            pending_row_menu: None,
            pending_global_search: None,
            autofit_key: [None, None],
            cur_col_widths: [
                crate::config::DEFAULT_COLUMN_WIDTHS,
                crate::config::DEFAULT_COLUMN_WIDTHS,
            ],
            size_sort_pending: false,
            undo: crate::ops::undo::UndoStack::new(),
            last_scroll: None,
            list_geom: (-1.0, 0, -1.0, false, -1.0, -1.0),
            recording_shortcut: None,
            path_probe_tx,
            path_probe_rx,
            path_probe_seq: 0,
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
            thumb_use_seq: 0,
            thumb_cache_bytes: 0,
            thumb_pending: HashSet::new(),
            last_tabs_sig: std::cell::Cell::new(0),
            last_mem_log: None,
            thumb_failed: HashSet::new(),
            thumb_failed_order: VecDeque::new(),
            thumb_needs_refresh: false,
            syncing_tab: false,
            first_run_pending: false,
            window_pending_show: false,
            geom_retries: 0,
            pending_scroll_index: None,
            rename_scroll_guard: None,
            rename_pin: None,
            scroll_to_paths: Vec::new(),
            scroll_select: false,
            shell_op_running: false,
            index_rescan_status: false,
            keys_block_until: None,
            keys_block_logged: false,
            shell_key_suppressed_logged: false,
            clear_staging_on_delete_success: false,
            scroll_to_index: None,
            thumb_tx,
            list_tx,
            list_rx,
            deep_tx,
            pinyin_tx,
            pinyin_rx,
            deep_rx,
            poll_timer: Timer::default(),
            sidebar_timer: Timer::default(),
            refresh_timer: Timer::default(),
            sidebar_periodic_timer: Timer::default(),
            rename_defer_timer: Timer::default(),
            shell_open_timer: Timer::default(),
            shell_open_queue: Vec::new(),
            tabs_discarded: false,
            self_weak: None,
        }
    }

    /// 请求列举目录（工作线程执行）。
    #[track_caller]
    fn request_listing(
        &self,
        tab_id: u32,
        pane: usize,
        generation: u64,
        path: Option<PathBuf>,
        sort: SortState,
    ) {
        logv!(
            "request_listing tab={tab_id} pane={pane} gen={generation} path={path:?} from {}",
            std::panic::Location::caller()
        );
        let _ = self.list_tx.send(ListRequest {
            tab_id,
            pane,
            generation,
            path,
            sort,
        });
    }

    /// 重新列举指定标签的指定面板。
    ///
    /// 分屏时两个面板各有一份独立的列举状态（entries / generation / sort），
    /// 所以刷新必须按**面板**发请求，不能只刷当前聚焦的那一个。
    fn reload_pane(&self, tab_index: usize, pane_index: usize) {
        let Some(tab) = self.state.tabs.get(tab_index) else {
            return;
        };
        let Some(pane) = tab.panes.get(pane_index) else {
            return;
        };
        // 只有聚焦面板才点亮标签的“加载中”，否则非聚焦面板的结果不会去清它。
        if pane_index == tab.active_pane {
            tab_loading_flag(
                &self.ui,
                tab_index == self.state.active_tab,
                true,
                !pane.display.is_empty(),
            );
        }
        self.request_listing(
            tab.id,
            pane_index,
            pane.generation,
            pane.path.clone(),
            pane.sort,
        );
    }

    /// 重新列举指定标签的当前聚焦面板。
    fn reload_tab(&self, tab_index: usize) {
        let Some(tab) = self.state.tabs.get(tab_index) else {
            return;
        };
        self.reload_pane(tab_index, tab.active_pane);
    }

    /// 重载所有正看着 `path` 的面板（同一标签的分屏另一半 + 别的标签）。
    fn reload_panes_viewing(&self, path: &std::path::Path) {
        for (ti, tab) in self.state.tabs.iter().enumerate() {
            for (pi, pane) in tab.panes.iter().enumerate() {
                if pane.path.as_deref() == Some(path) {
                    self.reload_pane(ti, pi);
                }
            }
        }
    }

    /// 任务 3：轮询各打开目录的 mtime，检测外部/内部变更后实时刷新对应标签。
    ///
    /// 只负责**收集路径并派给后台线程**：`symlink_metadata` 在休眠的移动盘、
    /// 掉线的映射盘上会阻塞几百毫秒到几秒，以前每秒在主线程上跑一遍（每个标签
    /// 一次），正好违反「碰文件系统不许在主线程」这条线。
    fn poll_dir_changes(&mut self) {
        if self.ui.get_settings_open() || self.mtime_probe_busy {
            return;
        }
        let mut paths: Vec<PathBuf> = Vec::new();
        for tab in &self.state.tabs {
            // 分屏的每个面板都要看：非聚焦面板以前完全不轮询，它的目录变了
            // 永远不会自己刷新。
            for pane in &tab.panes {
                let Some(path) = pane.path.clone() else {
                    continue;
                };
                // 网络路径不轮询 mtime：掉线共享上的 stat 会无限超时。
                if crate::fs::dir::is_remote_path(&path) {
                    continue;
                }
                if !paths.contains(&path) {
                    paths.push(path);
                }
            }
        }
        if paths.is_empty() {
            return;
        }
        if self.mtime_probe_tx.send(paths).is_ok() {
            self.mtime_probe_busy = true;
        }
    }

    /// 消费后台 mtime 探测结果：只有「仍被某个标签打开 + mtime 真的变了」才重载。
    fn apply_dir_mtime_probe(&mut self, probed: Vec<DirMtimeProbe>) {
        self.mtime_probe_busy = false;
        let open: Vec<PathBuf> = self
            .state
            .tabs
            .iter()
            .flat_map(|t| t.panes.iter().filter_map(|p| p.path.clone()))
            .collect();
        let mut to_reload: Vec<PathBuf> = Vec::new();
        for item in probed {
            let Some(mtime) = item.mtime else {
                // 读不到（已删除 / 无权限 / 盘已拔出）：下一轮再看，别在这里重载。
                continue;
            };
            if !open.contains(&item.path) {
                self.dir_mtimes.remove(&item.path);
                continue;
            }
            match self.dir_mtimes.get(&item.path) {
                Some(prev) if *prev == mtime => {}
                _ => {
                    self.dir_mtimes.insert(item.path.clone(), mtime);
                    to_reload.push(item.path);
                }
            }
        }
        for path in to_reload {
            // 同一个目录可能被多个标签、以及分屏的两个面板看着：全部重载。
            self.reload_panes_viewing(&path);
        }
    }

    /// 重新列举当前标签（常规操作复用：新建/删除/拖放/还原等，不清体积缓存）。
    fn reload_active(&mut self) {
        // 列举请求会带上 pane.sort：新建标签 / 添加网络位置 / 切换到设置页再回来这类
        // 不经过 navigate 的路径，pane.sort 可能是从上一个视图（比如此电脑的盘符）
        // 继承来的，直接发出去会用错排序。这里统一对齐到当前视图自己的状态。
        self.sync_view_sort();
        let tab_index = self.state.active_tab;
        let Some(tab) = self.state.tabs.get(tab_index) else {
            return;
        };
        let active_path = tab.active().path.clone();
        // 分屏里另一半可能正看着同一个目录：新建/删除/重命名之后它也得跟着变，
        // 不能只刷新当前聚焦面板（以前它只能等 mtime 轮询，甚至完全等不到）。
        let panes: Vec<usize> = match active_path {
            Some(path) => tab
                .panes
                .iter()
                .enumerate()
                .filter(|(_, p)| p.path.as_deref() == Some(path.as_path()))
                .map(|(i, _)| i)
                .collect(),
            None => vec![tab.active_pane],
        };
        for pi in panes {
            self.reload_pane(tab_index, pi);
        }
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
        // 列表内容换了（刷新/外部变更/新目录）：下拉里的类型行已经过时，
        // 留着会让「点的行 ↔ 实际类型」错位，直接关掉。
        if result.tab_id == self.state.tabs[self.state.active_tab].id {
            self.close_type_filter_menu();
        }
        // 新内容到了：连续按下链断掉（否则「上一目录点过 A，进来点同名 A 的名称」
        // 会被当成第二次按下直接进重命名）。
        self.last_left_press_row = None;
        let Some(tab_index) = self.state.tab_by_id(result.tab_id) else {
            logln!("apply_list_result: tab not found");
            return;
        };
        // 先快照各面板的目录：下面要按住 `&mut TabState`，那时不能再读 `self.state`。
        // 用途见「待揭示目标是否已过期」的判定。
        let pane_dirs: Vec<PathBuf> = self.state.tabs[tab_index]
            .panes
            .iter()
            .filter_map(|p| p.path.clone())
            .collect();
        let is_active_tab = tab_index == self.state.active_tab;
        let tab = &mut self.state.tabs[tab_index];
        if tab.panes[result.pane].generation != result.generation {
            return; // 过期结果
        }
        // 读失败：保留上一次的列表（不能把好好的内容清成「此文件夹为空」），
        // 记下标记，等用户切回这个标签时 retry_failed_panes 补一次。
        if result.failed {
            let pane = &mut tab.panes[result.pane];
            pane.loading = false;
            pane.list_failed = true;
            pane.pending_scroll_restore = false;
            logal!(
                "!! [list] read failed: tab={} pane={} path={:?} keeping {} entries",
                result.tab_id,
                result.pane,
                pane.path,
                pane.entries.len()
            );
            if result.pane == tab.active_pane {
                tab_loading_flag(&self.ui, is_active_tab, false, true);
            }
            // 自动重试登记：同一面板连续失败时累加次数，3 次以后收手。
            let tries = self
                .list_retry
                .as_ref()
                .filter(|(t, p, _, _)| *t == tab_index && *p == result.pane)
                .map(|(_, _, _, n)| *n)
                .unwrap_or(0);
            self.list_retry = Some((tab_index, result.pane, Instant::now(), tries.saturating_add(1)));
            return;
        }
        // 按「显示隐藏文件」开关过滤：关闭时剔除具有隐藏属性的条目。
        let show_hidden = self.config.show_hidden;
        let listed_is_active = result.pane == tab.active_pane;
        // 原始条数（过滤隐藏项之前）：给「空结果」日志用。
        let raw_count = result.entries.len();

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
            // 拼音缓存与 entries 逐项平行：只有「名字序列一模一样」时才能留用。
            // 后退回搜索页会顺带重新列举同一目录（内容没变），此时保留缓存才有
            // 拼音结果——否则中文名的搜索页会先闪一下「此文件夹为空」。
            let keep_pinyin = !pane.pinyin_cache.is_empty()
                && pane.pinyin_cache.len() == entries.len()
                && pane.entries.len() == entries.len()
                && pane
                    .entries
                    .iter()
                    .zip(entries.iter())
                    .all(|(a, b)| a.name == b.name);
            pane.entries = entries;
            if keep_pinyin {
                crate::logv!(
                    "[pinyin] keep cache across re-list pane={} n={}",
                    result.pane,
                    pane.entries.len()
                );
            } else {
                // 拼音缓存随 entries 失效（后台重算，见 request_pinyin_jobs）。
                pane.invalidate_pinyin();
            }
            // 新列表到达即失效全局搜索的截断提示。
            pane.search_total = None;
            // 「此电脑」重载 = 回到驱动器列表，残留的全局搜索词会误过滤空表。
            if pane.path.is_none() {
                pane.filter.clear();
            }
            pane.loading = false;
            pane.list_failed = false;
            // 列举成功：清掉自动重试的登记，下一次失败重新计数。
            self.list_retry = None;
            // 「读成功但一条都没有」也要留证据：如果下次出现的空白不属于读失败，
            // 这一行就能证明它是一份空结果，指向另一条 bug 路径。
            if raw_count == 0 && pane.path.is_some() {
                logal!(
                    "!! [list] empty result: tab={} pane={} path={:?} raw={}",
                    result.tab_id,
                    result.pane,
                    pane.path,
                    raw_count
                );
            }
            pane.rebuild_display();
            // Shell 系统菜单「新建 → 文本文档」：菜单前的快照 diff 出唯一新增文件。
            // 只在限期内、同一目录、恰好新增 1 个「文件」时生效，避免把粘贴 /
            // 批量新建 / 无关的后台创建误判成一次新建。zsdesk 用 IFileOperation
            // 的 PostNewItem 回调拿路径，Rove 走 Shell 菜单没有该回调，故用列举 diff。
            //
            // 快照**只在真正命中时才消费**。以前这里无条件 `take()`，于是任何一次
            // 路过本面板的列举（右键前的在途刷新、别的标签的定时刷新）都会把快照
            // 吞掉，随后真正带着新文件的那次列举就再也认不出来 —— 表现就是
            // 「新建 RTF 没进重命名」，而且日志里一行都没有。
            if let Some((_, _, at)) = &self.shell_new_watch {
                if at.elapsed() >= SHELL_NEW_WATCH {
                    // 详细级：这个快照**每一次空白处右键**都会挂上（不论用户
                    // 有没有点「新建」），所以过期本身不是异常，只是收尾。
                    // 真没建出文件的情况由上面的 "no new file in listing yet" 记录。
                    crate::logv!(
                        "[shell-new] watch expired after {}s (nothing new appeared)",
                        at.elapsed().as_secs()
                    );
                    self.shell_new_watch = None;
                }
            }
            // 只认「Shell 真的新建了一个文件」：自己有没有文件任务在跑也要看，
            // 任务期间出现的中间列举不能当依据。
            let watch_hit = !self.shell_op_running
                && self.shell_new_watch.as_ref().is_some_and(|(dir, _, _)| {
                    pane.path.as_deref() == Some(dir.as_path())
                        && result.pane == tab.active_pane
                        && tab_index == self.state.active_tab
                });
            if watch_hit {
                let (dir, before, at) = self.shell_new_watch.take().unwrap();
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
                    self.reveal_pane = Some(result.pane);
                    self.visible_cache = None;
                    self.visible_cache_right = None;
                } else if added.is_empty() {
                    // 这次列举还不含新文件（Shell 可能还没真正落地 / 这是在途的旧
                    // 结果）：把快照放回去继续等下一次列举，别把它丢了。
                    crate::logv!(
                        "[shell-new] no new file in listing yet (dir={})",
                        dir.display()
                    );
                    self.shell_new_watch = Some((dir, before, at));
                } else {
                    logln!("[shell-new] ignore diff: {} new entries", added.len());
                }
            }
            // 新建文件夹 / 粘贴完成：数据就位后把新条目换算成显示索引，交给
            // refresh_visible_rows 做“最小位移滚进可视区”（否则它们按排序沉到底部，看不到）。
            // 多条时取最靠前的一条，保证粘贴块的起始处可见。
            // 只让「当前可见标签的可见栏」消费这个挂起目标：后台标签的定期刷新
            // 若恰好也含同一路径，会把 scroll_to_index / 选择集写到别的标签上。
            let reveal_pane_ok = match self.reveal_pane {
                Some(p) => p == result.pane,
                None => result.pane == tab.active_pane,
            };
            if !self.scroll_to_paths.is_empty()
                && reveal_pane_ok
                && tab_index == self.state.active_tab
            {
                let mut matched: Vec<(usize, PathBuf)> = Vec::new();
                for (pos, &ei) in pane.display.iter().enumerate() {
                    if let Some(e) = pane.entries.get(ei) {
                        // 宽松比较：外部（Shell 的 SelectItem / 资源管理器 `/select,`）
                        // 给的路径大小写和磁盘上的实际名字不一定一致，精确相等会漏配。
                        if self
                            .scroll_to_paths
                            .iter()
                            .any(|p| *p == e.path || crate::fs::dir::same_path_loose(p, &e.path))
                        {
                            matched.push((pos, e.path.clone()));
                        }
                    }
                }
                // 只有「全部目标都到齐」或「文件任务已经结束」才消费。
                // 粘贴多文件时复制是逐个落地的：中途的列举只含一部分新文件，
                // 那时若照旧消费，就只选中了先到的那几个，剩下的永远不选。
                let all_matched = matched.len() == self.scroll_to_paths.len();
                let grace = self
                    .reveal_grace_until
                    .is_some_and(|t| std::time::Instant::now() < t);
                if !all_matched && (self.shell_op_running || grace) {
                    crate::logv!(
                        "[reveal] waiting for all targets: {}/{} (job running)",
                        matched.len(),
                        self.scroll_to_paths.len()
                    );
                    matched.clear();
                }
                if let Some((pos, first)) = matched.first() {
                    self.scroll_to_index = Some(*pos);
                    // 滚的是“新文件所在的那一栏”，不是聚焦栏（跨分屏拖放）。
                    self.scroll_to_pane = Some(result.pane);
                    // 粘贴/还原后选中它们（资源管理器同款）；新建文件夹不选（正在重命名）。
                    if self.scroll_select {
                        pane.anchor = Some(first.clone());
                        pane.selection = matched.iter().map(|(_, p)| p.clone()).collect();
                        self.visible_cache = None;
                        self.visible_cache_right = None;
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
                    self.reveal_pane = None;
                } else {
                    // 列举结果里还没出现目标（异步竞态）→ 保留 pending，等下一批。
                    // 但目标准确地属于别的目录（用户已经导航走 / 取消了那次新建），
                    // 就再也等不到了：丢掉，免得它一直挂着，直到某次回到那个目录
                    // 时突然触发一次莫名其妙的滚动+选中。
                    //
                    // 判定必须看**所有面板**的目录：分屏时目标是往另一半粘贴进去的，
                    // 只看结果所属的这个面板会把等待中的目标误判成过期。路径比较
                    // 也要宽松（大小写 / 分隔符），否则大小写不同就白丢一次揭示。
                    let stale = pane.filter.trim().is_empty()
                        && !pane_dirs.is_empty()
                        && self.scroll_to_paths.iter().all(|t| match t.parent() {
                            Some(parent) => !pane_dirs
                                .iter()
                                .any(|d| crate::fs::dir::same_path_loose(d, parent)),
                            None => true,
                        });
                    if stale {
                        crate::logv!(
                            "[reveal] drop pending target (not in any pane): {}",
                            self.scroll_to_paths.len()
                        );
                        self.scroll_to_paths.clear();
                        self.scroll_select = false;
                        self.reveal_pane = None;
                    } else {
                        crate::logv!(
                            "[new-folder] list MISS: waiting={} total={} rename_path={}",
                            self.scroll_to_paths.len(),
                            pane.display.len(),
                            self.ui.get_rename_path()
                        );
                    }
                }
            }
            // 后退选回刚离开的子目录：只在这一次列举里生效。
            // 找不到（被隐藏开关过滤 / 已经改名删除）就安静放弃，不重试。
            //
            // 只认「那一个标签的那一片」的结果：分屏时另一栏先到的列举不能把
            // 待办吃掉；后台标签恰好停在同一个父目录时，也不能替它选中
            // （标签 id 才是可靠的身份，下标会随关闭标签变化）。
            // 有「待揭示」在等（外部程序让我打开某个文件）时让位，那是更明确的请求。
            let sel_hit = tab_index == self.state.active_tab
                && self
                    .pending_history_select
                    .as_ref()
                    .is_some_and(|(t, p, _)| *t == tab.id && *p == result.pane)
                && self.scroll_to_paths.is_empty();
            if sel_hit {
                if let Some((_, _, target)) = self.pending_history_select.take() {
                    if let Some(pos) = pane.display.iter().position(|&ei| {
                        pane.entries.get(ei).is_some_and(|e| {
                            e.path == target
                                || crate::fs::dir::same_path_loose(&e.path, &target)
                        })
                    }) {
                        pane.anchor = Some(target.clone());
                        pane.selection = [target].into_iter().collect();
                        self.scroll_to_index = Some(pos);
                        self.scroll_to_pane = Some(result.pane);
                        self.visible_cache = None;
                        self.visible_cache_right = None;
                    }
                }
            }
        }
        let listed_path = tab.panes[result.pane].path.clone();

        // 核心修复：列举完成后立即记下目录 mtime，供 poll_dir_changes 比对。
        // 否则该目录在 dir_mtimes 里没有条目，下一次轮询（约几十 ms 后）会判定“已变化”，
        // 立即再全量列举一遍 —— 表现日志里就是每次导航/重命名后都出现两条同样的
        // request_listing + list_result。网络路径不参与轮询，无需记录。
        if let Some(p) = listed_path.as_ref() {
            if !crate::fs::dir::is_remote_path(p) {
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
            self.visible_cache_right = None;
            self.sync_file_list();
            // 进目录 / 刷新后打一条内存明细（`ROVE_LOG=2`，2 秒节流）：
            // 「越用越大」这类慢性问题只有在日常导航中连续采样才看得出来，
            // 只在启动时打一条的话，翻十分钟大目录那一段完全是盲区。
            self.log_memory_snapshot();
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
            let is_net = crate::fs::dir::is_remote_path(current_path);
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
                    self.visible_cache_right = None;
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
                    let is_net = crate::fs::dir::is_remote_path(&path);
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

        // 非聚焦面板的列举结果：条目已经换新，但这里从来没重建行模型
        // （以前只有滚动/选中才会刷），于是分屏的另一半「始终不刷新」。
        if !listed_is_active && tab_index == self.state.active_tab {
            self.visible_cache = None;
            self.visible_cache_right = None;
            self.refresh_visible_rows();
            self.sync_status();
        }

        // Shell 系统菜单「新建 → 文本文档」：到这里列表数据、行模型、滚动都已
        // 就位，再进入重命名（在 diff 当场调用会抢在行模型重建之前，输入框拿不到行）。
        if let Some(path) = self.pending_shell_new_rename.take() {
            logln!("[shell-new] entering rename for {}", path.display());
            self.begin_rename(&path);
        }
    }

    /// 立刻结算还没到期的搜索防抖。
    ///
    /// 搜索框输入要攒 150ms 才生效，但导航/切标签/切分屏会把这个挂起的词丢掉：
    /// 敲完 `xiyou` 马上点文件夹，真正生效并记进历史页的仍是上一个结算过的词
    /// （比如 `xi`），后退回搜索页时就只剩那个词的结果。离开前先结算一次。
    fn flush_pending_search(&mut self) {
        self.pending_search = None;
        // 设置页没有搜索框：那上面的文本不属于任何面板，别拿它去搜索。
        if self.state.active().kind != crate::state::TabKind::Explorer {
            return;
        }
        // 以输入框的文本为准：防抖里那个词可能还没落地（甚至已经被 40ms 轮询
        // 当成过期丢弃），而输入框里始终是最新的。
        let text = self.ui.get_filter_text().to_string();
        if self.state.active().active().filter == text {
            return;
        }
        logv!("[search] 离开前先结算搜索词 {:?}", text);
        self.apply_search_query(&text);
    }

    /// 统一应用搜索词（搜索框编辑 / 外部唤起预填共用）：
    /// 「此电脑」= 全局索引秒搜；目录视图 = 本地过滤 + 异步深度搜索。
    fn apply_search_query(&mut self, text: &str) {
        logv!("search query: {text:?}");
        let is_root = self.state.active().active().path.is_none();
        // 记录搜索页面（可前进/后退）以及搜索前的滚动位置。
        self.close_type_filter_menu();
        self.state
            .active_mut()
            .active_mut()
            .set_filter_from_search(text);
        let clearing = text.trim().is_empty();
        // 排序状态跟着视图走：搜索态与回收站共用一套，清空搜索回到该视图自己那套。
        self.sync_view_sort();
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
        self.visible_cache_right = None;
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
            // 只在聚焦面板内滚动：以前固定滚左栏，右分屏搜索时滚动条留在原处
            // （甚至因为内容变矮被夹到底部），新结果一出来就看不到开头。
            let pane = self.state.active().active_pane;
            if let Some(p) = self.state.active_mut().panes.get_mut(pane) {
                p.scroll_y = 0.0;
            }
            push_scroll(&self.ui, pane, 0.0);
        }
    }

    /// 打开「类型」表头下拉：列出当前目录里真实存在的类型。
    ///
    /// 与搜索框是两套独立条件（AND）：搜索框按名字收窄，这里按扩展名收窄。
    /// 空目录不开 —— 开了也只能给一个空盒子。
    fn open_type_filter_menu(&mut self, pane_index: usize) {
        if self.state.active().kind != crate::state::TabKind::Explorer {
            return;
        }
        let has_entries = self
            .state
            .active()
            .panes
            .get(pane_index)
            .is_some_and(|p| !p.entries.is_empty());
        if !has_entries {
            logv!("[type-filter] 空目录，不打开下拉 pane={pane_index}");
            return;
        }
        self.sync_type_filter_menu(pane_index);
        self.type_filter_pane = Some(pane_index);
        self.ui.set_type_filter_pane(pane_index as i32);
    }

    /// 重建下拉模型（勾选状态跟着当前筛选走）。
    fn sync_type_filter_menu(&mut self, pane_index: usize) {
        let Some(pane) = self.state.active().panes.get(pane_index) else {
            return;
        };
        // 分组按「类型名」而不是扩展名：与列表「类型」列显示的一致，
        // jpg/jpeg 这种同名的会并成一行（资源管理器也是这么分的）。
        let mut groups: Vec<TypeFilterGroup> = Vec::new();
        let mut index_of: std::collections::HashMap<String, usize> = Default::default();
        for e in &pane.entries {
            let label = self.tr_type(&e.type_text).to_string();
            let key = crate::state::type_key(e).to_string();
            let slot = *index_of.entry(label.clone()).or_insert_with(|| {
                groups.push(TypeFilterGroup {
                    label,
                    keys: Vec::new(),
                    count: 0,
                });
                groups.len() - 1
            });
            let g = &mut groups[slot];
            g.count += 1;
            if !g.keys.contains(&key) {
                g.keys.push(key);
            }
        }
        // 目录行固定排最前（资源管理器一致），其余按类型名排。
        groups.sort_by(|a, b| {
            let ad = a.keys.iter().any(|k| k == "dir");
            let bd = b.keys.iter().any(|k| k == "dir");
            bd.cmp(&ad).then_with(|| a.label.cmp(&b.label))
        });
        let active = !pane.type_filter.is_empty();
        let items: Vec<TypeFilterItem> = groups
            .iter()
            .map(|g| TypeFilterItem {
                label: g.label.clone().into(),
                count: g.count as i32,
                // 一行代表一组扩展名：整组都选中才算勾上。
                checked: active && g.keys.iter().all(|k| pane.type_filter.contains(k)),
            })
            .collect();
        self.ui
            .set_type_filter_items(ModelRc::new(VecModel::from(items)));
        self.type_filter_groups = groups;
    }

    /// 下拉里点了一行：`toggle = true` 是点前面的勾选框（多选/取消），
    /// `false` 是点名称（单选；点已经在筛的那个名称则取消筛选）。
    fn type_filter_pick(&mut self, pane_index: usize, index: i32, toggle: bool) {
        if index < 0 {
            return;
        }
        let Some(group) = self.type_filter_groups.get(index as usize) else {
            return;
        };
        let group_keys = group.keys.clone();
        let current = self
            .state
            .active()
            .panes
            .get(pane_index)
            .map(|p| p.type_filter.clone())
            .unwrap_or_default();
        let all_selected = group_keys.iter().all(|k| current.contains(k));
        let mut keys = current.clone();
        if toggle {
            if all_selected {
                keys.retain(|k| !group_keys.contains(k));
            } else {
                for key in group_keys {
                    if !keys.contains(&key) {
                        keys.push(key);
                    }
                }
            }
        } else if all_selected && current.len() == group_keys.len() {
            // 再点一次「唯一已筛选」的名称 = 取消筛选。
            keys.clear();
        } else {
            keys = group_keys;
        }
        self.apply_type_filter(pane_index, keys);
        // 点名称是「一次选定」：选完就收起来（勾选框那条路保持展开，
        // 方便连着勾好几类）。
        if !toggle {
            self.close_type_filter_menu();
        }
    }

    /// 应用类型筛选：与搜索词一样记进历史页（可前进/后退），随后重建可见行。
    fn apply_type_filter(&mut self, pane_index: usize, keys: Vec<String>) {
        if let Some(pane) = self.state.active_mut().panes.get_mut(pane_index) {
            pane.set_type_filter(keys);
            pane.rebuild_display();
        }
        self.visible_cache = None;
        self.visible_cache_right = None;
        self.sync_file_list();
        self.sync_status();
        // 筛选改了历史页，前进/后退按钮的可用状态要跟着变。
        self.sync_nav();
        // 下拉开着的时候就地刷新勾选（资源管理器勾完不关菜单）。
        if self.type_filter_pane == Some(pane_index) {
            self.sync_type_filter_menu(pane_index);
        }
    }

    /// 关掉类型筛选下拉（筛选结果本身保留）。
    fn close_type_filter_menu(&mut self) {
        if self.type_filter_pane.take().is_some() {
            self.ui.set_type_filter_pane(-1);
        }
    }

    /// 启动/重置当前标签的深度搜索（filter 变化时调用）。
    /// 空查询仅清空残留结果；有效查询在代号递增后发往工作线程。
    fn start_deep_search(&mut self) {
        let (tab_index, pane_index) = (self.state.active_tab, self.state.active().active_pane);
        self.start_deep_search_for(tab_index, pane_index);
    }

    /// 同上，但指定面板：文件任务改动了某个搜索面板的子树之后要把它重跑一遍，
    /// 否则结果里会同时留着「旧位置的同一条」和「新位置的那一条」。
    fn start_deep_search_for(&mut self, tab_index: usize, pane_index: usize) {
        let Some(tab) = self.state.tabs.get_mut(tab_index) else {
            return;
        };
        let Some(pane) = tab.panes.get_mut(pane_index) else {
            return;
        };
        pane.clear_deep();
        let (tab_id, generation, deep_generation, root, query) = (
            tab.id,
            pane.generation,
            pane.deep_generation,
            pane.path.clone(),
            pane.filter.trim().to_string(),
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
            pane: pane_index,
            generation,
            deep_generation,
            root,
            query,
            want: crate::search::index::DEEP_BATCH,
        });
        if let Some(pane) = self.state.tabs[tab_index].panes.get_mut(pane_index) {
            pane.deep_searching = true;
        }
    }

    /// 给「有搜索词但拼音缓存还没算好」的面板派一次后台计算。
    ///
    /// 拼音字段（含多音字表）对每个条目都要算一遍，上万条时在 UI 线程上
    /// 是十几到几十毫秒的卡顿，正好落在「刚敲下第一个字符」那一刻。
    /// 现在改由后台线程算，算好前用纯文本匹配兜底（见 `rebuild_display`）。
    fn request_pinyin_jobs(&mut self) {
        struct Job {
            tab_id: u32,
            pane: usize,
            seq: u64,
            names: Vec<String>,
        }
        let mut jobs: Vec<Job> = Vec::new();
        for tab in self.state.tabs.iter_mut() {
            if tab.kind != crate::state::TabKind::Explorer {
                continue;
            }
            for (pi, pane) in tab.panes.iter_mut().enumerate() {
                if pane.pinyin_pending.is_some()
                    || pane.pinyin_cache.len() == pane.entries.len()
                    || pane.entries.is_empty()
                {
                    continue;
                }
                // 中小目录：列举一到就在后台预建，用户几秒后再搜时缓存已经在手，
                // 不会出现「先按纯文本过滤、拼音结果晚半拍补上」的空档。
                // 超大目录不预建（白烧 CPU），等真的有搜索词再算。
                if pane.filter.trim().is_empty() && pane.entries.len() > PINYIN_EAGER_MAX {
                    continue;
                }
                pane.pinyin_pending = Some(pane.pinyin_seq);
                jobs.push(Job {
                    tab_id: tab.id,
                    pane: pi,
                    seq: pane.pinyin_seq,
                    names: pane.entries.iter().map(|e| e.name.clone()).collect(),
                });
            }
        }
        for job in jobs {
            let tx = self.pinyin_tx.clone();
            let tab_id = job.tab_id;
            let pane = job.pane;
            let seq = job.seq;
            let spawned = std::thread::Builder::new()
                .name("rove-pinyin".into())
                .spawn(move || {
                    let n = job.names.len();
                    // 兜底：万一某个名字让转换 panic，线程带着"正在算"的标记一起死掉，
                    // 这个面板就再也等不到结果 —— 表现为"搜索词怎么敲都显示全部条目"。
                    // 这里的失败只退化成"没有拼音"，纯文本匹配照旧可用。
                    let fields: Vec<crate::search::PinyinFields> =
                        std::panic::catch_unwind(move || {
                            job.names
                                .iter()
                                .map(|s| crate::search::pinyin_fields(s))
                                .collect()
                        })
                        .unwrap_or_else(|_| vec![crate::search::PinyinFields::default(); n]);
                    let _ = tx.send(PinyinJobResult {
                        tab_id,
                        pane,
                        seq,
                        fields,
                    });
                });
            if let Err(e) = spawned {
                // 起不了线程：清掉标记，下一轮还会再试；这期间照常按纯文本过滤。
                crate::logln!("!! 拼音后台线程启动失败: {e}");
                if let Some(tab) = self.state.tabs.iter_mut().find(|t| t.id == tab_id) {
                    if let Some(p) = tab.panes.get_mut(pane) {
                        p.pinyin_pending = None;
                    }
                }
            }
        }
    }

    /// 拼音缓存算好了：写回并重新过滤一次（期间是纯文本兜底，可能少一些拼音命中）。
    fn apply_pinyin_result(&mut self, result: PinyinJobResult) {
        let Some(ti) = self
            .state
            .tabs
            .iter()
            .position(|t| t.id == result.tab_id)
        else {
            return;
        };
        let active = ti == self.state.active_tab;
        let mut n = 0usize;
        let mut filtered = false;
        let ok = {
            let Some(pane) = self.state.tabs[ti].panes.get_mut(result.pane) else {
                return;
            };
            // 只有"同一世代的结算"才清标记：过期结果不能把新一轮的标记清掉。
            if pane.pinyin_pending == Some(result.seq) {
                pane.pinyin_pending = None;
            }
            // 过期（期间换了目录 / 重新排序 / 又失效了一次）就整批丢弃。
            if pane.pinyin_seq != result.seq || result.fields.len() != pane.entries.len() {
                crate::logv!(
                    "[pinyin] drop stale result tab={} pane={} seq={} (now {})",
                    result.tab_id,
                    result.pane,
                    result.seq,
                    pane.pinyin_seq
                );
                false
            } else {
                n = result.fields.len();
                pane.pinyin_cache = result.fields;
                // 没有搜索词时 display 是恒等映射，重算没有意义。
                if !pane.filter.trim().is_empty() {
                    pane.rebuild_display();
                    filtered = true;
                }
                true
            }
        };
        if !ok {
            return;
        }
        if !filtered {
            return;
        }
        crate::logv!(
            "[pinyin] cache ready tab={} pane={} n={}",
            result.tab_id,
            result.pane,
            n
        );
        if active {
            self.visible_cache = None;
            self.visible_cache_right = None;
            self.refresh_visible_rows();
            self.sync_status();
        }
    }

    /// 深度搜索结果到达：标签存在、代号与查询都匹配才应用。
    /// 叫停一条过期结果的扫描。代号必须原样带上：工作线程只认「和我正在跑的
    /// 这一条同代号」的取消，别的一律忽略，否则会把更新的请求连坐掉。
    fn cancel_deep(&self, result: &DeepResult) {
        let _ = self.deep_tx.send(DeepRequest {
            tab_id: result.tab_id,
            pane: result.pane,
            generation: result.generation,
            deep_generation: result.deep_generation,
            root: PathBuf::new(),
            query: String::new(),
            want: 0,
        });
    }

    fn apply_deep_result(&mut self, result: DeepResult) {
        logv!(
            "deep result tab={} gen={} dgen={} n={} done={}",
            result.tab_id,
            result.generation,
            result.deep_generation,
            result.items.len(),
            result.done
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
            // 顺手叫停后台扫描：结果已经没人要了，没必要继续翻目录。
            // **必须带上这条过期结果自己的代号**：以前的取消是「无条件停」，
            // 而节流折叠只留队列里最后一个请求 —— 用户连打几个字时，取消会
            // 排在新请求后面、把它一起顶掉，于是新查询永远没人做（真机：
            // 输入框已经写着 ziyan，状态栏「已扫 0 项」挂着二十多秒）。
            self.cancel_deep(&result);
            return; // 过期结果
        }
        if pane.filter.trim().to_lowercase() != result.query.trim().to_lowercase() {
            logln!(
                "[deep] !! drop mismatched filter: pane={:?} result={:?}",
                pane.filter,
                result.query
            );
            self.cancel_deep(&result);
            return;
        }
        logv!(
            "[deep] apply pane={} +{} (total {}) done={} filter={:?}",
            result.pane,
            result.items.len(),
            pane.deep_entries.len() + result.items.len(),
            result.done,
            result.query
        );
        // 追加，不是替换：一个查询会分很多批回来，替换会让前一批凭空消失。
        //
        // 但**同一路径只收一次**：走查是边扫边读，文件在扫描途中被移动/复制时，
        // 旧目录和新目录各可能报一遍，列表里就出现两个一模一样的文件。
        let mut seen: std::collections::HashSet<PathBuf> = pane
            .entries
            .iter()
            .map(|e| e.path.clone())
            .chain(pane.deep_entries.iter().map(|e| e.path.clone()))
            .collect();
        let before = result.items.len();
        let items = dedupe_deep_batch(result.items, &mut seen);
        if items.len() != before {
            logv!(
                "[deep] 去掉重复路径 {} 条（本批 {} → {}）",
                before - items.len(),
                before,
                items.len()
            );
        }
        let added = !items.is_empty();
        pane.deep_entries.extend(items.iter().map(|i| i.entry.clone()));
        pane.deep_pinyin.extend(items.into_iter().map(|i| i.fields));
        pane.deep_searching = !result.done;
        pane.deep_capped = result.capped;
        pane.deep_scanned = result.scanned;
        // 只有「这一批真的带来新条目」才需要动行模型：收尾那种空结果只更新
        // 状态位，重建一遍纯属白烧 UI 线程。
        //
        // 流式期间**只追加不重排**（见 `PaneState::deep_sort_pending`）：每批都
        // 整体按分数重排，用户正在看的那一行会被后插进来的高分条目挤走，屏幕上
        // 就是列表一直在跳。排序推迟到「用户停手」或「扫描结束」那一刻。
        if result.done {
            // 收尾那一批常常是空的（`n=0 done=true`）：那种情况也要把欠着的
            // 重排结清，否则列表会一直停在「到达顺序」，直到下一次空闲重排。
            if added || pane.deep_sort_pending {
                pane.rebuild_display();
                pane.deep_sort_pending = false;
            }
        } else if added {
            pane.extend_display_streaming();
            pane.deep_sort_pending = true;
        }
        if tab_index == self.state.active_tab {
            self.visible_cache = None;
            self.visible_cache_right = None;
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
            self.visible_cache_right = None;
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

/// 标签图标处的加载圆点（仅影响激活标签）。
///
/// 只在「这一栏还没有任何内容」时点亮：刷新、被文件操作重载、恢复会话时的
/// 重复列举都会短暂把 loading 置真，那时列表里明明已经有内容，圆点却把标签
/// 图标顶掉再换回来 —— 看起来就是「标签图标闪了一下圆点」。
fn tab_loading_flag(ui: &MainWindow, is_active: bool, loading: bool, has_content: bool) {
    if is_active {
        ui.set_tab_loading(loading && !has_content);
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

/// 缩略图磁盘缓存版本：绘图管线一变（如「小图大盘子」裁切上线）就要 +1，
/// 否则旧缓存里的旧位图会原样继续显示，看着像修复没生效。
const THUMB_CACHE_VERSION: u32 = 6;

/// 缩略图磁盘缓存路径，使用两级哈希子目录避免单目录文件过多。
fn thumb_disk_path(key: &ThumbKey) -> PathBuf {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    THUMB_CACHE_VERSION.hash(&mut hasher);
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
                    let (entries, failed) = match &req.path {
                        None => (list_drives().iter().map(FileEntry::drive).collect(), false),
                        Some(path) if *path == fs::drives::recycle_virtual_path() => {
                            // 回收站站内浏览：虚拟目录，条目来自系统回收站。
                            let mut entries = fs::drives::list_recycle_entries();
                            sort_entries(&mut entries, req.sort);
                            (entries, false)
                        }
                        Some(path) => {
                            let started = std::time::Instant::now();
                            match fs::dir::try_list_directory(path) {
                                Ok(mut entries) => {
                                    sort_entries(&mut entries, req.sort);
                                    (entries, false)
                                }
                                Err(err) => {
                                    // 失败单独标记，UI 侧保留旧列表而不是清空。
                                    logal!(
                                        "!! [list] worker read failed: path={:?} err={} after {}ms",
                                        path,
                                        err,
                                        started.elapsed().as_millis()
                                    );
                                    (Vec::new(), true)
                                }
                            }
                        }
                    };
                    let _ = tx.send(ListResult {
                        tab_id: req.tab_id,
                        pane: req.pane,
                        generation: req.generation,
                        entries,
                        failed,
                    });
                });
            }
        })
        .expect("spawn rove-lister dispatcher");
}

/// 启动深度搜索工作线程（文件夹视图子树递归查找）。
///
/// 现在**持续回传**：一个查询切成 300 条一批往 UI 送，直到扫完或撞上限
/// （`DEEP_MAX_RESULTS = 1 000` 条 / `DEEP_MAX_ENTRIES = 50 万` 条目）。
/// 分批只影响「第一屏多快出现」，不是分页——不会因为你往下滚再续一批；
/// 到上限时状态栏会说「仅显示前 1000 项」。
///
/// 流式期间用户改词怎么办：每批之间 `try_recv`，队列里只要有新请求就
/// 立刻换口味（连打几个字只做最后一次），旧扫描整个丢掉。
/// 排队请求的两分法：新查询（新词优先，只留最后一条）和取消（另放一格，
/// 绝不和新查询抢那个「最后一个」的位置）。
/// 取消只对「同一条任务」生效：标签、窗格、两次代号全对上才算。
fn same_deep_job(a: &DeepRequest, b: &DeepRequest) -> bool {
    a.tab_id == b.tab_id
        && a.pane == b.pane
        && a.generation == b.generation
        && a.deep_generation == b.deep_generation
}

/// 同一个面板（标签 + 栏号）的请求：它才是「同一份结果」的竞争者。
fn same_deep_pane(a: &DeepRequest, b: &DeepRequest) -> bool {
    a.tab_id == b.tab_id && a.pane == b.pane
}

/// 把请求放进待跑队列：同一面板只留最新一条（用户连打字时旧的直接扔掉），
/// 与正在跑的那条完全相同的请求直接忽略（重跑一遍只会白白再扫一趟）。
fn push_deep_queue(
    queue: &mut Vec<DeepRequest>,
    req: DeepRequest,
    active: Option<&DeepRequest>,
) {
    if let Some(act) = active {
        if same_deep_job(act, &req) {
            return;
        }
    }
    match queue.iter_mut().find(|r| same_deep_pane(r, &req)) {
        Some(slot) => *slot = req,
        None => queue.push(req),
    }
}

fn spawn_deep_worker(rx: Receiver<DeepRequest>, tx: Sender<DeepResult>) {
    std::thread::Builder::new()
        .name("rove-deepsearch".into())
        .spawn(move || {
            logln!("deepsearch worker started");
            // 当前任务：请求 + 可续跑的扫描状态。
            let mut active: Option<(DeepRequest, crate::search::index::DeepScan)> = None;
            // 等着跑的请求：工作线程只有一个，**别的面板**的查询必须排队，
            // 不能像以前那样「只留最后一个」—— 否则分屏两栏同时搜索时，先发的
            // 那一栏永远等不到结果（被顶掉后只收到一条空 done，子文件夹结果全丢）。
            let mut queue: Vec<DeepRequest> = Vec::new();
            // 空批次（纯进度）的上报节流，见下面 `batch.is_empty()` 那段。
            let mut last_progress = std::time::Instant::now() - std::time::Duration::from_secs(1);
            loop {
                // 1) 收请求：取消单独拎出来，其余按面板去重后排队。
                //
                // 取消（want = 0）**不能**和普通请求混在同一个槽里：界面丢一条过期
                // 结果就会顺手发一个取消，它常常排在新请求后面，混在一起会把新请求
                // 顶掉 —— 新查询于是永远没人做。
                let mut cancel: Option<DeepRequest> = None;
                loop {
                    match rx.try_recv() {
                        Ok(req) => {
                            if req.want == 0 {
                                cancel = Some(req);
                            } else {
                                push_deep_queue(&mut queue, req, active.as_ref().map(|(r, _)| r));
                            }
                        }
                        Err(std::sync::mpsc::TryRecvError::Empty) => break,
                        Err(std::sync::mpsc::TryRecvError::Disconnected) => return,
                    }
                }
                if let (Some(c), Some((req, _))) = (&cancel, &active) {
                    // 只停「同一条」：代号对不上说明它已经过期，取消它等于误杀。
                    if same_deep_job(c, req) {
                        logv!("[deep] cancelled gen={}", c.deep_generation);
                        active = None;
                    }
                }
                // 2) 挑这一轮要跑的那条：空着就按先来先到取队首；正跑着时只有
                // 「同一面板的新请求」才顶掉它（用户连打字），别的面板排队等 ——
                // 一个面板的搜索不能把别人的结果挤掉。
                let mut fresh: Option<DeepRequest> = None;
                match active.as_ref() {
                    Some((act, _)) => {
                        if let Some(pos) = queue.iter().position(|r| same_deep_pane(r, act)) {
                            fresh = Some(queue.remove(pos));
                        }
                    }
                    None => {
                        if !queue.is_empty() {
                            fresh = Some(queue.remove(0));
                        }
                    }
                }
                if fresh.is_none() && active.is_none() && queue.is_empty() {
                    // 闲置：阻塞等一个请求（不烧 CPU）。
                    match rx.recv() {
                        Ok(req) => {
                            if req.want != 0 {
                                push_deep_queue(&mut queue, req, None);
                            }
                            continue;
                        }
                        Err(_) => return,
                    }
                }
                if let Some(req) = fresh {
                    // 先把被顶掉的那条扫描收尾：工作线程只有一个，另一栏（或另一个
                    // 标签）发起新查询时旧任务直接被换掉，那一栏再没人给它 `done`，
                    // 就会永远挂在「正在搜索…已扫 N 项」。补一条空结果告诉它结束。
                    if let Some((old, scan)) = active.as_ref() {
                        if !same_deep_job(old, &req) {
                            let _ = tx.send(DeepResult {
                                tab_id: old.tab_id,
                                pane: old.pane,
                                generation: old.generation,
                                deep_generation: old.deep_generation,
                                query: old.query.clone(),
                                items: Vec::new(),
                                done: true,
                                capped: false,
                                scanned: scan.scanned(),
                            });
                        }
                    }
                    let query = req.query.trim().to_lowercase();
                    if req.want == 0 {
                        // 取消：结果已经过期，直接丢掉当前扫描。
                        active = None;
                        continue;
                    }
                    if query.is_empty() || req.root.as_os_str().is_empty() {
                        let _ = tx.send(DeepResult {
                            tab_id: req.tab_id,
                            pane: req.pane,
                            generation: req.generation,
                            deep_generation: req.deep_generation,
                            query: req.query,
                            items: Vec::new(),
                            done: true,
                            capped: false,
                            scanned: 0,
                        });
                        active = None;
                        continue;
                    }
                    logln!("deep req tab={} root={:?} q={query}", req.tab_id, req.root);
                    let scan = crate::search::index::DeepScan::new(&req.root, &query);
                    active = Some((req, scan));
                }
                let Some((req, scan)) = active.as_mut() else {
                    continue;
                };
                let batch = scan.next_batch(crate::search::index::DEEP_BATCH);
                let done = scan.is_done();
                let capped = scan.hit_cap();
                // 空批次不能每次都送：一个五百万条目的盘里命中很稀，扫完两万个
                // 条目才凑出 0 条，每次空结果都让 UI 重建一遍行模型、重画一遍状态栏，
                // 几千次下来界面就等于卡死。但也不能**完全不报**：网络盘 / 大盘上
                // 命中先出完了、后面几十秒一条都没有，用户看到的就是「卡在 300 条、
                // 状态栏永远是正在搜索」——所以按时间节流报进度（4 次/秒）。
                if batch.is_empty() && !done {
                    if last_progress.elapsed() < std::time::Duration::from_millis(250) {
                        continue;
                    }
                    last_progress = std::time::Instant::now();
                    logv!("[deep] progress scanned={}", scan.scanned());
                }
                let sent = tx.send(DeepResult {
                    tab_id: req.tab_id,
                    pane: req.pane,
                    generation: req.generation,
                    deep_generation: req.deep_generation,
                    query: req.query.clone(),
                    items: batch,
                    done,
                    capped,
                    scanned: scan.scanned(),
                });
                if done || sent.is_err() {
                    active = None;
                } else {
                    // 让出一点：UI 线程要处理这一批（重建行模型），别把它挤住。
                    std::thread::sleep(std::time::Duration::from_millis(2));
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
/// 只有**一条**队列，所有线程从同一条队列按序取活：队列里的顺序就是渲染
/// 顺序（`thumb_order_key`：可见自上而下 → 下方自上而下 → 上方自下而上）。
///
/// 曾经分成「可见行走优先池 2 条线程 / 缓冲行走主池 6 条线程」，两个池同时
/// 开工 —— 网络盘上 6 条线程会把看不见的缓冲行先啃完，用户看到的是
/// 「眼前这一屏还没出来，下面那些行一个个亮了」。单队列没有这个问题，
/// 也不牺牲并发：所有线程都在同一条队列上全力工作。
#[cfg(windows)]
fn spawn_thumb_worker(rx: mpsc::Receiver<ThumbKey>) {
    // 启动时初始化磁盘字节统计（仅计数，超预算才异步裁剪）。
    std::thread::spawn(init_thumb_disk_cache);

    std::thread::Builder::new()
        .name("rove-thumbs-dispatcher".into())
        .spawn(move || {
            // 线程数随 CPU 核数自适应：留 2 核给 UI + 系统，夹在 [2, 6] 之间。
            // 下限 2 = 至少 1 条图片专用 + 1 条视频；上限 6 避免抢光多核机的算力。
            let cores = std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(4);
            let num_threads = (cores.saturating_sub(2)).clamp(2, 6);
            let mut worker_txs = Vec::with_capacity(num_threads);

            // 1. 启动后台工作线程（数量自适应，见上方 num_threads 计算）。
            // 慢格式隔离（抄 Pulse 的 slow_cap=lanes-1）：
            //   慢格式（视频海报）在途数上限 slow_cap = num_threads-1，
            //   且只派到 1..n 号车道 —— 0 号车道永远没有视频，
            //   视频再多的文件夹也卡不住图片。
            // 线程够多（>2）才给图片留一条专用车道；只有 2 条时留不起 ——
            // 留了就等于「视频永远只有 1 条线程在解」（4 核机上实测就是这样，
            // 一个视频文件夹变成一条线程单干）。
            let reserved_fast = num_threads > 2;
            let slow_cap = if reserved_fast {
                num_threads - 1
            } else {
                num_threads
            };
            crate::logln!(
                "[thumb] pool: cores={} threads={} slow_cap={} reserved_fast={}",
                cores,
                num_threads,
                slow_cap,
                reserved_fast,
            );
            // 车道负载：每条线程手上还有几个任务、其中几个是视频。
            // 派发时挑**最闲**的一条：一屏图片会同时铺到所有线程上（以前快格式
            // 固定走 0 号车道，一条线程单干、其余全闲着）。图片优先挑没有视频
            // 压着的车道，避免排在一张视频后面；视频只在 1..n 里挑，
            // 0 号车道因此永远没有视频，图片总有一条通道不被视频卡住。
            let mut lane_load = vec![0usize; num_threads];
            let mut lane_slow = vec![0usize; num_threads];
            let (done_tx, done_rx) = mpsc::channel::<(usize, bool)>();
            for i in 0..num_threads {
                let (tx, worker_rx) = mpsc::channel::<ThumbKey>();
                worker_txs.push(tx);
                let done_tx = done_tx.clone();
                let spawned = std::thread::Builder::new()
                    .name(format!("rove-thumb-worker-{i}"))
                    .spawn(move || {
                        crate::logv!("[thumb] worker-{i} started");
                        while let Ok(key) = worker_rx.recv() {
                            let slow = crate::ops::thumbs::is_slow_thumbnail(&key.path);
                            thumb_decode_one("worker", i, key);
                            if done_tx.send((i, slow)).is_err() {
                                break;
                            }
                        }
                        crate::logv!("[thumb] worker-{i} channel closed");
                    });
                match spawned {
                    Ok(_) => crate::logv!("[thumb] worker-{i} spawned"),
                    Err(e) => crate::logln!("[thumb] worker-{i} SPAWN FAILED: {e} (requests on this lane will be LOST)"),
                }
            }
            drop(done_tx);

            // 图片：打分 = 手上还有几个任务 + 其中几个是视频（视频慢，排在它后面
            // 等于再等一张海报）。空车道总是先被填满，不会像以前那样全挤在 0 号；
            // 但只要还有一条车道没在解视频，图片就不会去排视频的队。
            let pick_fast = |load: &[usize], slow: &[usize]| -> usize {
                load.iter()
                    .enumerate()
                    .min_by_key(|(l, n)| **n + slow[*l])
                    .map_or(0, |(l, _)| l)
            };
            // 视频：有专用图片车道时只在 1..n 里挑，否则全线参与。
            let pick_slow = |load: &[usize]| -> usize {
                let skip = usize::from(reserved_fast);
                load.iter()
                    .enumerate()
                    .skip(skip)
                    .min_by_key(|(_, n)| **n)
                    .map_or(skip, |(l, _)| l)
            };
            // 车道通道断了（该线程已死）时，把这条请求登记为失败：
            // 留在 pending 里会让那一行永久空白且不再重试。
            let report_lost = |key: ThumbKey| {
                let _ = slint::invoke_from_event_loop(move || {
                    let Some(app) = SESSION.with(|slot| slot.borrow().clone()) else {
                        return;
                    };
                    app.borrow_mut().fail_thumbnail(key);
                });
            };

            // 2. 分发循环：队列里的顺序就是渲染顺序（见 `thumb_order_key`），
            //    所有线程从同一条队列按序取活，谁闲谁上。
            let mut slow_in_flight = 0usize;
            // 慢格式在途已满时的暂存区（避免阻塞分发线程、饿死快格式）。
            let mut slow_stash: std::collections::VecDeque<ThumbKey> = Default::default();
            loop {
                // 回收已完成的任务：腾出慢格式名额、更新车道负载。
                while let Ok((lane, slow)) = done_rx.try_recv() {
                    retire_thumb_task(
                        &mut lane_load,
                        &mut lane_slow,
                        &mut slow_in_flight,
                        lane,
                        slow,
                    );
                }
                // 有空位就排空暂存的视频任务。
                while slow_in_flight < slow_cap {
                    let Some(k) = slow_stash.pop_front() else {
                        break;
                    };
                    let lane = pick_slow(&lane_load);
                    match worker_txs[lane].send(k) {
                        Ok(()) => {
                            lane_load[lane] += 1;
                            lane_slow[lane] += 1;
                            slow_in_flight += 1;
                        }
                        Err(e) => {
                            crate::logln!("[thumb] !! slow lane={lane} send FAILED (worker dead)");
                            report_lost(e.0);
                        }
                    }
                }
                // 视频名额已满、手上还压着排不进队的视频：此刻**不能**在 `rx`
                // 上空等 50ms。该等的是「哪条线程空出来了」，而 `rx` 里往往没有
                // 新请求（用户已停手，只剩队列里的存活）。空等的结果是每 50ms
                // 才放进一条视频，一屏 117 个视频光派发就要 3.6s —— 表现就是
                // 「跳过去先干等一两秒，然后一行行慢慢亮」。
                if !slow_stash.is_empty() && slow_in_flight >= slow_cap {
                    match done_rx.recv_timeout(std::time::Duration::from_millis(50)) {
                        Ok((lane, slow)) => {
                            retire_thumb_task(
                                &mut lane_load,
                                &mut lane_slow,
                                &mut slow_in_flight,
                                lane,
                                slow,
                            );
                            continue;
                        }
                        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                        // 所有 worker 都没了：退回下面的正常路径（派发会失败并登记）。
                        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {}
                    }
                }
                let req = match rx.recv_timeout(std::time::Duration::from_millis(50)) {
                    Ok(r) => r,
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                };
                if crate::ops::thumbs::is_slow_thumbnail(&req.path) {
                    if slow_in_flight < slow_cap {
                        let lane = pick_slow(&lane_load);
                        match worker_txs[lane].send(req) {
                            Ok(()) => {
                                lane_load[lane] += 1;
                                lane_slow[lane] += 1;
                                slow_in_flight += 1;
                            }
                            Err(e) => {
                                crate::logln!("[thumb] !! slow lane={lane} send FAILED (worker dead)");
                                report_lost(e.0);
                            }
                        }
                    } else {
                        slow_stash.push_back(req);
                    }
                } else {
                    let lane = pick_fast(&lane_load, &lane_slow);
                    match worker_txs[lane].send(req) {
                        Ok(()) => lane_load[lane] += 1,
                        Err(e) => {
                            crate::logln!("[thumb] !! fast lane={lane} send FAILED (worker dead)");
                            report_lost(e.0);
                        }
                    }
                }
            }
            crate::logv!("[thumb] dispatcher loop ended");
        })
        .ok();
}

/// 回收一条完成事件：更新车道负载与慢格式在途计数。
///
/// 单独成函数而不是闭包：闭包会把 `lane_load` 长期可借走，
/// 导致同一条循环里的 `pick_slow(&lane_load)` 借不到。
#[cfg(windows)]
fn retire_thumb_task(
    lane_load: &mut [usize],
    lane_slow: &mut [usize],
    slow_in_flight: &mut usize,
    lane: usize,
    slow: bool,
) {
    if let Some(n) = lane_load.get_mut(lane) {
        *n = n.saturating_sub(1);
    }
    if slow {
        *slow_in_flight = slow_in_flight.saturating_sub(1);
        if let Some(n) = lane_slow.get_mut(lane) {
            *n = n.saturating_sub(1);
        }
    }
}

/// 单个缩略图任务：解码 → （可选）落盘缓存 → 回主线程写内存 LRU。
///
/// `tag` / `idx` 只用于日志区分是哪条 worker 线程。
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
    // 取到的任务如果已经不在当前窗口（用户滚走 / 跳走 / 换目录），直接丢弃：
    // 跳过是一次哈希查找，比解码便宜几个数量级，连续快速跳转时旧请求会被瞬间排空。
    if !thumb_is_wanted(&key) {
        crate::logv!("[thumb] {tag}-{idx} skip stale {}", key.path.display());
        note_thumb_skipped(key);
        return;
    }
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
                    // 会话已经销毁（关窗口 / 退出时正好有缩略图在飞）。没有窗口，
                    // 自然也没有「行」要填，属于正常收尾，不该记成异常。
                    logv!(
                        "[thumb] 会话已销毁，丢弃缩略图 {}",
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

    fn deep_req(dgen: u64, want: usize) -> DeepRequest {
        DeepRequest {
            tab_id: 1,
            pane: 0,
            generation: 0,
            deep_generation: dgen,
            root: PathBuf::from("O:\\Video"),
            query: "ziyan".into(),
            want,
        }
    }

    /// 图标视图搜索态的路径浮层只显示目录：盘根、盘根下一层、深层、UNC 都要对。
    #[test]
    fn dir_path_text_strips_file_name() {
        assert_eq!(dir_path_text(r"C:\Users\a\b.txt"), r"C:\Users\a");
        assert_eq!(dir_path_text(r"C:\b.txt"), r"C:\");
        assert_eq!(dir_path_text(r"C:\"), r"C:\");
        assert_eq!(dir_path_text(r"\\nas\share\f.txt"), r"\\nas\share");
        // 共享根里的条目：再往上只有服务器名，那不是可用目录，原样返回。
        assert_eq!(dir_path_text(r"\\nas\share"), r"\\nas\share");
        assert_eq!(dir_path_text(r"\\nas"), r"\\nas");
        // 目录自己作为条目出现时，浮层显示它的上一级（名字在格子上）。
        assert_eq!(dir_path_text(r"D:\work\proj"), r"D:\work");
    }

    /// 真机 bug：连打 `ziya` → `ziyan` 时，界面丢掉一条过期结果会顺手发取消，
    /// 取消排在新查询后面，以前两者共用一个槽 → 新查询被顶掉，界面就这样
    /// 挂着「已扫 0 项」不动了。取消必须单独处理，且不能顶掉新查询。
    #[test]
    fn cancel_never_swallows_newer_query() {
        let mut queue: Vec<DeepRequest> = Vec::new();
        push_deep_queue(&mut queue, deep_req(4, 300), None);
        // 取消不进队列（工作线程单独拎出来），所以队列里只剩新查询。
        assert_eq!(queue.len(), 1);
        assert_eq!(queue[0].deep_generation, 4, "新查询必须留下");
        // 而且这条取消对不上正在跑的第 4 代任务，不能停它。
        assert!(!same_deep_job(&deep_req(3, 0), &deep_req(4, 300)));
        assert!(same_deep_job(&deep_req(4, 0), &deep_req(4, 300)));
    }

    /// 连着打好几个字：同一面板只留最后一个。
    #[test]
    fn newer_query_replaces_older() {
        let mut queue: Vec<DeepRequest> = Vec::new();
        for dgen in [1, 2, 3] {
            push_deep_queue(&mut queue, deep_req(dgen, 300), None);
        }
        assert_eq!(queue.len(), 1);
        assert_eq!(queue[0].deep_generation, 3);
    }

    /// 分屏两栏同时搜索：两个面板的请求都要留着，谁也不能把谁挤掉
    ///（以前队列只留最后一个，先发的那一栏只收到一条空 done，子文件夹结果全丢）。
    #[test]
    fn both_panes_of_a_split_keep_their_request() {
        let mut queue: Vec<DeepRequest> = Vec::new();
        let mut left = deep_req(1, 300);
        left.pane = 0;
        let mut right = deep_req(1, 300);
        right.pane = 1;
        right.root = PathBuf::from("D:\\Music");
        push_deep_queue(&mut queue, left, None);
        push_deep_queue(&mut queue, right, None);
        assert_eq!(queue.len(), 2, "两个面板各自都要有结果");
        // 同一面板再发一次：只更新它自己那一格，不动别人的。
        let mut left2 = deep_req(2, 300);
        left2.pane = 0;
        push_deep_queue(&mut queue, left2, None);
        assert_eq!(queue.len(), 2);
        assert_eq!(queue[0].deep_generation, 2);
        assert_eq!(queue[1].pane, 1);
    }

    /// 与正在跑的那条完全一样的请求不再入队（重跑一趟纯属白扫）。
    #[test]
    fn identical_running_request_is_ignored() {
        let mut queue: Vec<DeepRequest> = Vec::new();
        let running = deep_req(7, 300);
        push_deep_queue(&mut queue, deep_req(7, 300), Some(&running));
        assert!(queue.is_empty());
        push_deep_queue(&mut queue, deep_req(8, 300), Some(&running));
        assert_eq!(queue.len(), 1, "同面板的新查询仍然要排队顶掉旧的");
    }

    fn stamp(i: i32, thumb: u64) -> RowStamp {
        RowStamp {
            index: i,
            name: format!("row{i}").into(),
            path_text: format!("C:\\row{i}").into(),
            type_text: "txt".into(),
            size_text: "10 B".into(),
            modified_text: "2024-01-01 00:00".into(),
            is_dir: false,
            is_hidden: false,
            selected: false,
            cut: false,
            truncated: false,
            size: 10,
            modified: Some(std::time::SystemTime::UNIX_EPOCH),
            thumb,
        }
    }

    fn ctx(path: &str, generation: u64) -> RowCtx {
        RowCtx {
            tab: 0,
            path: Some(PathBuf::from(path)),
            generation,
            filter: String::new(),
            view_mode: "details".into(),
            name_w_q: 400,
            total: 2,
            computer: false,
            split: false,
        }
    }

    fn diff_state(ctx: RowCtx, stamps: Vec<RowStamp>) -> RowDiffState {
        RowDiffState { ctx, stamps }
    }

    /// 增量更新只在“同一屏、内容有变”时逐行补；其余情况整表换。
    #[test]
    fn rows_to_patch_only_touches_changed_rows() {
        let c = ctx("C:\\a", 1);
        let stamps = vec![stamp(0, 0), stamp(1, 0), stamp(2, 0)];
        // 没有上一帧 → 整表换。
        assert!(rows_to_patch(None, &c, &stamps, 3).is_none());
        // 完全相同 → 一行都不用写。
        let prev = diff_state(c.clone(), stamps.clone());
        assert_eq!(rows_to_patch(Some(&prev), &c, &stamps, 3), Some(vec![]));
        // 只有第 1 行来了缩略图 → 只写第 1 行。
        let mut changed = stamps.clone();
        changed[1] = stamp(1, 42);
        assert_eq!(rows_to_patch(Some(&prev), &c, &changed, 3), Some(vec![1]));
        // 上下文变了（换目录 / 换搜索词 / 换视图）→ 整表换。
        let other = ctx("C:\\a", 2);
        assert!(rows_to_patch(Some(&prev), &other, &stamps, 3).is_none());
        let mut other = c.clone();
        other.filter = "png".into();
        assert!(rows_to_patch(Some(&prev), &other, &stamps, 3).is_none());
        // 行数变了（窗口大小变化）→ 整表换。
        assert!(rows_to_patch(Some(&prev), &c, &stamps, 5).is_none());
        let longer = vec![stamp(0, 0), stamp(1, 0), stamp(2, 0), stamp(3, 0)];
        assert!(rows_to_patch(Some(&prev), &c, &longer, 4).is_none());
    }

    /// 选中态变化必须触发对应行的重写（否则点击无反馈）。
    #[test]
    fn rows_to_patch_reacts_to_selection() {
        let c = ctx("C:\\a", 1);
        let stamps = vec![stamp(0, 0), stamp(1, 0)];
        let prev = diff_state(c.clone(), stamps.clone());
        let mut changed = stamps.clone();
        changed[0].selected = true;
        assert_eq!(rows_to_patch(Some(&prev), &c, &changed, 2), Some(vec![0]));
    }

    /// 缩略图缓存淘汰：挑最久未用的那条（命中路径不再维护顺序表）。
    #[test]
    fn lru_victim_picks_least_recently_used() {
        let mut m: std::collections::HashMap<u32, u64> = std::collections::HashMap::new();
        m.insert(1, 30);
        m.insert(2, 10);
        m.insert(3, 20);
        assert_eq!(lru_victim(&m, |v| *v), Some(2));
        // 用过 2 之后，它不再是牺牲对象。
        m.insert(2, 40);
        assert_eq!(lru_victim(&m, |v| *v), Some(3));
        assert_eq!(
            lru_victim(&std::collections::HashMap::<u32, u64>::new(), |v| *v),
            None
        );
    }

    /// 图标视图浮层定位：缓存行号只在「那一行还是同一个条目」时复用，
    /// 顺序真变了就重新定位；目标不在显示列表里时返回 None。
    #[test]
    fn expand_locate_reuses_cached_row_until_order_changes() {
        let mut pane = crate::state::PaneState::new(None, crate::fs::SortState::default());
        pane.entries = ["a.txt", "b.txt", "c.txt"]
            .iter()
            .map(|n| crate::fs::dir::test_entry(n, false, 0))
            .collect();
        pane.rebuild_display();
        let b = std::path::Path::new("b.txt");
        let mut cache = None;
        assert_eq!(expand_locate(&pane, b, &mut cache), Some(1));
        assert_eq!(cache, Some((PathBuf::from("b.txt"), 1)));
        // 顺序变了（换排序）：缓存行号验证不上 → 重新定位到新位置。
        pane.display = vec![2, 0, 1];
        assert_eq!(expand_locate(&pane, b, &mut cache), Some(2));
        // 目标不在显示列表里（被搜索过滤掉）→ None，不会拿旧行号凑数。
        assert_eq!(
            expand_locate(&pane, std::path::Path::new("zzz.txt"), &mut cache),
            None
        );
    }

    /// 新建会话时初始路径只能顶掉「那个还没列举过的默认此电脑标签」。
    ///
    /// 这一条管的是「冷启动 / 守护进程重建会话」：此时除了恢复出来的用户标签，
    /// 剩下的只有默认「此电脑」标签 —— 留着它就是最左侧那个空的此电脑标签
    /// （「此文件夹为空」），无论请求方有没有要求新开标签。
    #[test]
    fn fresh_session_reuses_the_default_tab() {
        // 没恢复上次的标签页：占掉默认标签，绝不多留一个空的「此电脑」。
        assert!(!initial_path_opens_new_tab(false));
        // 恢复过上次的标签页：那是用户的标签，必须新开，不能顶掉。
        assert!(initial_path_opens_new_tab(true));
    }

    /// 正则片段按钮插到光标处：有选中时替换选中内容，返回新光标位置。
    #[test]
    fn token_inserts_at_caret_or_replaces_selection() {
        // 光标在末尾。
        assert_eq!(insert_at_selection("abc", "\\d", 3, 3), ("abc\\d".to_string(), 5));
        // 有选中（anchor 在前）：选中 abc 的 bc 被替换。
        assert_eq!(insert_at_selection("abc", "\\d", 3, 1), ("a\\d".to_string(), 3));
        // caret 在 anchor 前面时也要正确排序。
        assert_eq!(insert_at_selection("abc", "X", 1, 3), ("aX".to_string(), 2));
        // 偏移越界要夹住，不能切在 UTF-8 中间（"名字" 是 6 个字节）。
        let (text, caret) = insert_at_selection("名字", "\\w", 99, 99);
        assert_eq!(text, "名字\\w");
        assert_eq!(caret, 8);
        let (text, _) = insert_at_selection("名字", "X", 5, 5);
        assert_eq!(text, "名X字");
    }

    /// 参数模板按钮：没点过参数框就追加到末尾并补空格，点过则按光标插。
    #[test]
    fn arg_token_appends_when_caret_is_unknown() {
        // 空文本：直接就是那个参数。
        assert_eq!(insert_arg_token("", "{name}", -1, -1), ("\"{name}\"".to_string(), 8));
        // 非空且末尾不是空格：补一个空格再追加。
        assert_eq!(
            insert_arg_token("cmd", "{name}", -1, -1),
            ("cmd \"{name}\"".to_string(), 12)
        );
        // 末尾已经是空格：不再重复补。
        assert_eq!(
            insert_arg_token("cmd ", "{name}", -1, -1),
            ("cmd \"{name}\"".to_string(), 12)
        );
        // 点过参数框（光标在后面）：插到光标处。
        assert_eq!(
            insert_arg_token("ab", "{name}", 1, 1),
            ("a\"{name}\"b".to_string(), 9)
        );
        // 点过且选中了一段：替换选中内容。
        assert_eq!(
            insert_arg_token("ab", "{name}", 2, 0),
            ("\"{name}\"".to_string(), 8)
        );
    }

    /// 手写窗口化：只推视口附近的行，换个位置要换一批。
    #[test]
    fn visible_range_keeps_buffer_around_viewport() {
        // 100 行、每行 27px、视口 500px、上下各 300px 缓冲。
        let rows: Vec<(f32, f32)> = (0..100).map(|i| (i as f32 * 27.0, 27.0)).collect();
        let yh = |r: &(f32, f32)| *r;

        // 顶部：缓冲够不着上面，从第 0 行开始。
        let (start, end) = visible_range(&rows, 0.0, 500.0, 300.0, yh);
        assert_eq!(start, 0);
        // 0..800px 覆盖到 (800/27)≈29.6，所以是 0..30。
        assert_eq!(end, 30);

        // 中间：上下都有缓冲，起止都落在视口外 300px 处。
        let (start, end) = visible_range(&rows, 1350.0, 500.0, 300.0, yh);
        assert_eq!(start, 38); // 第 38 行底部 1053 > 1050，仍要留一截缓冲
        assert_eq!(end, 80); // (1350+500+300)/27 = 79.6

        // 滚到底：不会越界。
        let (_, end) = visible_range(&rows, 2200.0, 500.0, 300.0, yh);
        assert_eq!(end, 100);

        // 空列表。
        assert_eq!(visible_range::<(f32, f32)>(&[], 0.0, 500.0, 300.0, yh), (0, 0));

        // 行高不一致（多行名字）：矮行不会把长行截掉。
        let rows = [(0.0, 27.0), (27.0, 100.0), (127.0, 27.0)];
        let yh = |r: &(f32, f32)| *r;
        let (start, end) = visible_range(&rows, 30.0, 100.0, 0.0, yh);
        assert_eq!((start, end), (1, 3));
    }

    /// 缩略图请求顺序：屏幕内自上而下，缓冲行排后面。
    #[test]
    fn thumb_order_starts_at_the_top_screen_row() {
        // 滚动条在顶部：屏幕是第 0..5 行，缓冲行只有下方 5..13。
        let mut rows: Vec<usize> = (0..13).collect();
        rows.sort_by_key(|r| thumb_order_key(*r, 0, 5));
        assert_eq!(rows, (0..13).collect::<Vec<usize>>());

        // 滚到中间：屏幕 20..25 自上而下，然后下方 25..29 继续自上而下，
        // 最后才是上方的行（自下而上）。
        let mut rows: Vec<usize> = (16..29).collect();
        rows.sort_by_key(|r| thumb_order_key(*r, 20, 25));
        assert_eq!(
            rows,
            vec![20, 21, 22, 23, 24, 25, 26, 27, 28, 19, 18, 17, 16]
        );

        // 底部钳制：屏幕 8..12，只有一个上缓冲方向。
        let mut rows: Vec<usize> = (3..12).collect();
        rows.sort_by_key(|r| thumb_order_key(*r, 8, 12));
        assert_eq!(rows, vec![8, 9, 10, 11, 7, 6, 5, 4, 3]);
    }

    /// 「想要」集合：换一屏时旧 key 被丢弃，侧栏小图标不丢。
    #[test]
    fn publish_thumb_wanted_drops_stale_but_keeps_sidebar() {
        let k = |name: &str, px: u32| ThumbKey {
            path: PathBuf::from(name),
            mtime_ms: 0,
            file_size: 0,
            px_size: px,
        };
        let sidebar = k("side", SIDEBAR_ICON_PX);
        let screen_a = k("a", 90);
        let screen_b = k("b", 90);
        publish_thumb_wanted([sidebar.clone(), screen_a.clone()].into_iter().collect());
        assert!(thumb_is_wanted(&screen_a));

        // 新一屏只有 b：a 变成过期请求，侧栏图标保留。
        publish_thumb_wanted([screen_b.clone()].into_iter().collect());
        assert!(!thumb_is_wanted(&screen_a));
        assert!(thumb_is_wanted(&screen_b));
        assert!(thumb_is_wanted(&sidebar));
    }

    /// 屏幕行数：与视口相交的行都算（包括底部那半行）。
    #[test]
    fn visible_row_count_includes_partial_bottom_row() {
        // 日志里的真实几何：scroll=6429.5、row_h=144、视口 659.33。
        // 顶行 44.65、底边 49.23 → 屏幕上是第 44–49 共 6 行。
        assert_eq!(visible_row_count(6429.5, 659.33325, 144.0), 6);
        // 顶部：4.58 行 → 5 行。
        assert_eq!(visible_row_count(0.0, 659.33325, 144.0), 5);
        // 刚好一整行高 → 1 行，不能变 2。
        assert_eq!(visible_row_count(0.0, 144.0, 144.0), 1);
        assert_eq!(visible_row_count(0.0, 288.0, 144.0), 2);
        // 非零且不对齐：跨 3 行。
        assert_eq!(visible_row_count(10.0, 300.0, 144.0), 3);
        // 行高未知时的兜底。
        assert_eq!(visible_row_count(0.0, 500.0, 0.0), 1);
    }

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
    /// 搜索态与回收站共用同一份排序状态；普通目录仍然归自己。
    #[test]
    fn search_sort_shares_with_recycle() {
        let dir = std::path::Path::new(r"D:\work");
        let recycle = crate::fs::drives::recycle_virtual_path();
        assert_eq!(
            ViewSort::of_pane(Some(dir), true),
            ViewSort::of_pane(Some(recycle.as_path()), false)
        );
        assert_eq!(ViewSort::of_pane(Some(dir), false), ViewSort::Normal);
        assert_eq!(ViewSort::of_pane(None, false), ViewSort::ThisPc);
        // 「此电脑」的全局搜索也归到同一套：那里要能按名称/路径排序，
        // 而 ThisPc 视图本身不允许「名称」键。
        assert_eq!(ViewSort::of_pane(None, true), ViewSort::Recycle);
    }

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
        // 缤纷色板按序号轮替：鲜亮蓝 → 翠绿 → 琥珀橙 → 紫罗兰 → 珊瑚红 → 薄荷青 → 回到蓝。
        let expected = [
            (0x25, 0x7A, 0xFD),
            (0x10, 0xB9, 0x81),
            (0xF5, 0x9E, 0x0B),
            (0xA8, 0x55, 0xF7),
            (0xEF, 0x44, 0x44),
            (0x14, 0xB8, 0xA6),
            (0x25, 0x7A, 0xFD),
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
        assert_eq!((edge.red(), edge.green(), edge.blue()), (0x25, 0x7A, 0xFD));
    }

    #[test]
    fn network_active_id_matches_ancestor_by_path_boundary() {
        let targets = [
            r"\\192.168.1.5\Video",
            r"\\192.168.1.5\Video\bsf",
            r"\\nas\V",
        ];
        let pick = |p: &str| network_active_id(p, targets.iter().copied());
        // 精确命中。
        assert_eq!(pick(r"\\192.168.1.5\Video"), r"net:\\192.168.1.5\Video");
        // 子目录命中祖先（这是本次修的 bug：以前这里返回空）。
        assert_eq!(
            pick(r"\\192.168.1.5\Video\bsf"),
            r"net:\\192.168.1.5\Video\bsf"
        );
        assert_eq!(
            pick(r"\\192.168.1.5\Video\bsf\2024"),
            r"net:\\192.168.1.5\Video\bsf"
        );
        // 前缀必须在分隔符边界上：`\\nas\V` 不能吃掉 `\\nas\Video`。
        assert_eq!(pick(r"\\nas\Video\x"), "");
        // 大小写不敏感（UNC 服务器名经常大小写混写）；回填的 id 用配置里原本的大小写。
        assert_eq!(
            pick(r"\\192.168.1.5\VIDEO\x"),
            r"net:\\192.168.1.5\Video"
        );
        // 一个都不沾边。
        assert_eq!(pick(r"\\other\share"), "");
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
        let long = format!("{}.mp4", "Supercalifragilistic".repeat(5));
        let out2 = grid_display_name(&long, 116.0, 3);
        let lines: Vec<&str> = out2.split('\n').collect();
        assert_eq!(lines.len(), 3, "超长单词仍应压到 3 行: {out2:?}");
        assert!(!lines[0].is_empty(), "首行不能为空: {out2:?}");
        assert!(lines[2].ends_with('…'), "末行应有省略号: {out2:?}");
    }

    /// 批量重命名的折行必须和真实渲染对得上。
    ///
    /// 真机遇到的两条：① 右列名字明明放得下却折成两行，整行比左边高一截；
    /// ② 每行末尾都多出「…」——因为字宽表照 Segoe UI 测，而界面字体是
    /// Microsoft YaHei UI，拉丁字形宽 8.7%，估出来的每一行都刚好超一点点。
    /// 所以这条测试守两件事：行数按 YaHei 的真实宽度算，且**任何一行都不许
    /// 超过列宽**（超过才会被 `overflow: elide` 收成「…」）。
    #[test]
    fn batch_rename_wrap_matches_rendered_width() {
        let new = "eunyoung1238_arostnearosnteaornsteaorsnetoarnsktoarsetnoarsj_Sooplive_20260117_1.mp4";
        let orig = "eunyoung1238_Sooplive_20260117_1.mp4";
        // 批量重命名窗口首次打开 = 屏幕 75%；1920x1200 屏、150% 缩放时约 1441 逻辑像素。
        for win_w in [1441.0, 1600.0, 1900.0] {
            let col_w = batch_col_w(win_w, 0.0);
            let budget = batch_wrap_budget(col_w);
            // 折行预算换算回 12.5px 的真实渲染宽度，必须还在列宽以内。
            let render_cap = (budget - 11.5) * BATCH_WRAP_SCALE;
            assert!(
                render_cap <= col_w,
                "预算换算回去（{render_cap:.1}）超过列宽 {col_w:.1}（win_w={win_w}）"
            );
            for text in [new, orig] {
                let lines = grid_display_lines(&[(text.to_string(), false)], budget, 12);
                for line in &lines {
                    let w: f32 = line.iter().map(|p| est_text_w(std::slice::from_ref(p))).sum();
                    assert!(
                        w <= budget,
                        "这一行 {w:.1} 超过预算 {budget:.1}，渲染时会被收成省略号（win_w={win_w}）"
                    );
                }
            }
            let orig_lines = grid_display_lines(&[(orig.to_string(), false)], budget, 12).len();
            assert_eq!(orig_lines, 1, "原名不该折行（win_w={win_w}）");
        }
        // 84 个字符在 12.5px YaHei 下约 556px：1441 宽窗口的列只有 550px，
        // 就是放不下，必须折成两行（以前按 Segoe 估成 470px 才误判为一行）。
        let narrow = batch_wrap_budget(batch_col_w(1441.0, 0.0));
        assert!(grid_display_lines(&[(new.to_string(), false)], narrow, 12).len() >= 2);
        let wide = batch_wrap_budget(batch_col_w(1900.0, 0.0));
        assert_eq!(grid_display_lines(&[(new.to_string(), false)], wide, 12).len(), 1);
    }

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

    /// 图标视图的名称切行不能因为「此电脑」就把高亮段丢掉：
    /// 从「此电脑」发起的全局搜索，结果条目原名 == 显示名，必须保留高亮。
    #[test]
    fn grid_row_lines_keep_highlight_unless_translated() {
        let parts = vec![("Video".to_string(), true), ("Assets".to_string(), false)];
        let layout = grid_display_lines(&parts, 200.0, 3);
        let kept = grid_row_lines(&layout, "VideoAssets", "VideoAssets", 200.0);
        assert_eq!(kept, layout, "显示名未变时不该重排");
        assert!(
            kept
                .iter()
                .any(|l| l.iter().any(|(t, hl)| *hl && t.contains("Video"))),
            "高亮标记丢了: {kept:?}"
        );
        // 卷标被翻译：段文本对不上，按纯文本重排。
        let tr = grid_row_lines(&layout, "本地磁盘 (C:)", "OS (C:)", 200.0);
        assert!(
            tr.iter().all(|l| l.iter().all(|(_, hl)| !*hl)),
            "翻译后的名字不该带旧的高亮段: {tr:?}"
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
    if let Some(app) = SESSION.with(|slot| slot.borrow().clone()) {
        let mut app = app.borrow_mut();
        app.refresh_after_index_ready();
        // 无论是否重查都同步一次状态栏：把「全盘索引构建中...」摘掉。
        app.sync_status();
        // 设置页「重新扫描驱动器」留下的「正在重新扫描…」在此换成完成信息。
        app.finish_index_rescan();
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
    // 吸附窗正在输入：索引到位后补刷一次，让它出现「全盘文件夹」那一轨。
    // 必须在 `app` 借用之外调用（`SESSION` 与 `JUMP` 是两套状态，别叠着借）。
    if jump_query_active() {
        jump_refresh();
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

    /// 重建标签模型并把状态同步给界面。
    ///
    /// 每次都会 `set_tabs(新的 ModelRc)` —— Slint 的 Repeater 据此销毁并重建
    /// **全部**标签实例，悬停态被清零、点击落在被销毁的实例上。正常操作一秒
    /// 最多几次；一秒十几次就说明有东西在空转，专门记一行取证（带调用点）。
    #[track_caller]
    fn sync_tabs(&self) {
        let tabs: Vec<TabInfo> = self
            .state
            .tabs
            .iter()
            .map(|t| {
                // 标签图标角标：设置页齿轮 / 此电脑 / 回收站 / 盘符字母（大写）。
                // 取路径首组件（如 "C:"）的首字符，非字母（网络路径等）回落文件夹符号。
                // 注意：标签图标底色由 Slint 统一用主题色，Rust 不再算颜色。
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
        // 指纹去重：内容没变就**不**换模型。
        //
        // `set_tabs` 传入新 ModelRc 会让 Slint 的 Repeater 销毁并重建全部标签
        // 实例：悬停态被清零、点击落在被销毁的实例上、标签栏整条重新布局。
        // 恢复会话时每个标签的列举结果都会各自调一次 `sync_tabs`，10 个标签就是
        // 10 次重建 —— 日志里那条 `!! [tab] 标签模型 1s 内重建 10 次` 就是这么来的。
        // 内容没变时换模型纯属副作用，跳过它。
        // 标签上的加载圆点只在「这一栏还没有任何内容」时点亮。
        // 刷新、被文件操作重载、恢复会话时的重复列举都会短暂把 loading 置真，
        // 那时列表里明明已经有内容，圆点却把标签图标顶掉又换回来 —— 看起来
        // 就是「标签图标闪了一下圆点」。没有内容时才需要这个提示。
        let active_pane_now = self.state.active().active();
        let loading_flag = active_pane_now.loading && active_pane_now.display.is_empty();
        let settings_open = self.state.active().kind == crate::state::TabKind::Settings;
        // 标题栏实时显示当前聚焦标签的名称（切换标签、导航、设置子页都会走这里）。
        let window_title = format!(
            "{} - Rove",
            i18n::text(&self.config.language, &self.state.active().title())
        );
        let sig = {
            use std::hash::{Hash, Hasher};
            let mut h = std::collections::hash_map::DefaultHasher::new();
            for t in &tabs {
                t.id.hash(&mut h);
                t.title.as_str().hash(&mut h);
                t.icon_text.as_str().hash(&mut h);
                t.active.hash(&mut h);
                t.is_settings.hash(&mut h);
            }
            self.state.active_tab.hash(&mut h);
            loading_flag.hash(&mut h);
            settings_open.hash(&mut h);
            window_title.hash(&mut h);
            h.finish()
        };
        // 圆点必须**无条件**写一次：`reload_pane` 里有直写通道，若这里因指纹
        // 相同提前返回，那个 true 就再也没人去清，圆点会一直闪。
        self.ui.set_tab_loading(loading_flag);
        if self.last_tabs_sig.replace(sig) == sig {
            return;
        }
        {
            use std::sync::atomic::Ordering;
            let now = now_ms();
            if now.saturating_sub(SYNC_TABS_WINDOW_MS.load(Ordering::Relaxed)) > 1000 {
                SYNC_TABS_WINDOW_MS.store(now, Ordering::Relaxed);
                SYNC_TABS_COUNT.store(0, Ordering::Relaxed);
            } else {
                let n = SYNC_TABS_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
                if n >= 10
                    && now.saturating_sub(SYNC_TABS_LOGGED_MS.load(Ordering::Relaxed)) >= 2000
                {
                    SYNC_TABS_LOGGED_MS.store(now, Ordering::Relaxed);
                    logln!(
                        "!! [tab] 标签模型 1s 内重建 {n} 次（悬停会闪、点击会丢），最近一次来自 {}",
                        std::panic::Location::caller()
                    );
                }
            }
        }
        self.ui.set_tabs(ModelRc::new(VecModel::from(tabs)));
        self.ui.set_active_tab(self.state.active_tab as i32);
        self.ui.set_tab_loading(loading_flag);
        self.ui.set_settings_open(settings_open);
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
        // 表头只在详细信息视图存在：切走就没人能再操作这个下拉了。
        self.close_type_filter_menu();
        self.ui.set_view_mode(mode.into());
        // 切视图后格子布局全变，旧的悬停浮层不能再指下去。
        self.ui.invoke_clear_path_tip(0);
        self.ui.invoke_clear_path_tip(1);
        self.visible_cache = None;
        self.visible_cache_right = None;
        self.sync_file_list();
    }

    /// 全局应用排序并持久化。同时刷新所有已打开的面板，确保多标签/分屏统一。
    /// 「此电脑」「回收站」各有独立排序状态：在这两个视图里点排序只改它们自己，
    /// 不碰全局；在普通目录里点排序也只作用于普通目录，不会带走其余两者。
    fn apply_sort(&mut self, key: SortKey, ascending: bool) {
        // 显式重排优先：丢掉重命名后临时钉住的位置。
        self.rename_pin = None;
        let active = self.state.active().active();
        let view = ViewSort::of_pane(active.path.as_deref(), !active.filter.is_empty());
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
                if ViewSort::of_pane(pane.path.as_deref(), !pane.filter.is_empty()) != view {
                    continue;
                }
                pane.sort.key = key;
                pane.sort.ascending = ascending;
                // 重排时把拼音缓存一起搬过去，不用重算。
                pane.resort();
                pane.rebuild_display();
            }
        }

        self.visible_cache = None;
        self.visible_cache_right = None;
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
        let want = |path: Option<&std::path::Path>, searching: bool| match ViewSort::of_pane(path, searching) {
            ViewSort::ThisPc => this_pc,
            ViewSort::Recycle => recycle,
            ViewSort::Normal => global,
        };
        let mut changed = false;
        for tab in &mut self.state.tabs {
            if tab.kind == crate::state::TabKind::Settings { continue; }
            for pane in &mut tab.panes {
                let s = want(pane.path.as_deref(), !pane.filter.is_empty());
                if pane.sort.key == s.key && pane.sort.ascending == s.ascending {
                    continue;
                }
                pane.sort = s;
                // 列表结果还没到（loading 中）时不动旧数据：worker 会用新排序重算。
                if !pane.loading && !pane.entries.is_empty() {
                    pane.resort();
                    pane.rebuild_display();
                }
                changed = true;
            }
        }
        if changed {
            self.visible_cache = None;
            self.visible_cache_right = None;
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
            tab.active_mut().invalidate_pinyin();
            tab.active_mut().filter.clear();
            tab.active_mut().search_total = None;
            tab.active_mut().display = (0..n).collect();
            return;
        }
        // 索引还没加载（启动不再预热）：交给后台线程读盘，先把这次查询记下来，
        // 加载完成后补跑（见 `run_pending_global_search`）—— 输入不会卡住。
        // 「没加载」与「正在构建」要分开：构建中照旧直接查已有的部分结果
        // （最后会由「索引就绪」补查一次），只有真的没加载才去读盘。
        if !crate::search::index::has_data() && !crate::search::index::is_building() {
            logln!("index: first global search -> lazy load {query:?}");
            self.pending_global_search = Some(query.to_string());
            crate::search::index::warm_up_async();
            return;
        }
        self.pending_global_search = None;
        crate::search::index::touch();
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
            .map(|h| FileEntry::from_path(&h.path, h.is_dir, 0, None, false))
            .collect();
        let shown = entries.len();
        let tab = self.state.active_mut();
        tab.active_mut().entries = entries;
        tab.active_mut().invalidate_pinyin();
        tab.active_mut().filter = query.to_string();
        // 命中数超过展示上限时记录总数，供状态栏提示截断。
        tab.active_mut().search_total = (shown < total).then_some(total);
        tab.active_mut().rebuild_display();
    }

    /// 索引加载完成后补跑那次「当时还没加载完」的全盘搜索。
    ///
    /// 为什么不全靠“索引就绪通知”：那条通知是异步投递的，漏一次用户看到的
    /// 就是「首次搜索没有结果，得再敲一下才出来」。这里由 40ms 轮询直接看
    /// 状态，最多晚 40ms；与通知重复触发时只是多跑一次同样的查询。
    fn run_pending_global_search(&mut self) {
        let Some(query) = self.pending_global_search.clone() else {
            return;
        };
        if !crate::search::index::has_data() {
            return;
        }
        self.pending_global_search = None;
        let (is_root, filter) = {
            let tab = self.state.active();
            (tab.active().path.is_none(), tab.active().filter.trim().to_string())
        };
        // 用户已经离开「此电脑」或者又改了搜索词：这一条过期了，丢掉。
        if !is_root || filter != query {
            return;
        }
        logln!("index: run pending global search {query:?}");
        self.run_global_search(&query);
        self.visible_cache = None;
        self.visible_cache_right = None;
        self.sync_file_list();
        self.sync_status();
    }

    /// 长时间没做全盘搜索：把索引内存还给系统（磁盘缓存保留）。
    fn maybe_unload_index(&mut self) {
        // 有一次搜索还在等索引加载：这时候卸载等于把它掐掉。
        if self.pending_global_search.is_some() {
            return;
        }
        // 吸附窗正在搜：它依赖索引里的文件夹那一轨，别在人家搜的时候清空。
        if jump_query_active() {
            return;
        }
        // 从没被用过（比如只是打开了索引设置页）→ 立刻可卸。
        let idle = crate::search::index::idle_for().unwrap_or(Duration::from_secs(u64::MAX / 2));
        if idle < Duration::from_secs(INDEX_UNLOAD_IDLE_MINS * 60) {
            return;
        }
        // 正在看「搜索与索引」看板：那里显示的就是真实占用，别让它突然变 0。
        let on_search_settings = {
            let tab = self.state.active();
            tab.kind == crate::state::TabKind::Settings
                && tab.settings_subpage == crate::state::SettingsSubpage::SearchAndIndex
        };
        if on_search_settings {
            return;
        }
        crate::search::index::unload();
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
        // 这次补查和 `run_pending_global_search` 是两条路，已经查了就别让
        // 40ms 轮询再查一遍（同一句话扫两遍十几万条，纯浪费）。
        self.pending_global_search = None;
        self.run_global_search(&query);
        // 必须清虚拟列表缓存，否则列表命中旧窗口不重渲染（只有状态栏会变）。
        self.visible_cache = None;
        self.visible_cache_right = None;
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
    /// 返回值：`Some(true)` 有数据 / `Some(false)` 正在构建 / `None` 未加载（已卸载或还没用过）。
    ///
    /// 不能只看 `ready`：索引闲置卸载之后 `ready` 也会是 false，那样状态栏
    /// 会一直挂着一句根本不存在的「全盘索引构建中...」。
    fn index_build_state() -> Option<bool> {
        if crate::search::index::is_building() {
            return Some(false);
        }
        if crate::search::index::has_data() {
            return Some(true);
        }
        None
    }

    /// 手动「重新扫描驱动器」扫完后的收尾：把状态栏那句「正在重新扫描…」换成结果。
    ///
    /// 只在状态栏仍然挂着那句提示时才覆盖：扫描期间用户可能已做过别的操作，
    /// 那些结果比这条通知更该留在右下角。
    fn finish_index_rescan(&mut self) {
        if !std::mem::take(&mut self.index_rescan_status) {
            return;
        }
        let scanning = self.tr("正在重新扫描驱动器并构建索引...");
        if self.ui.get_status_job() != scanning {
            return;
        }
        let count = crate::search::index::try_shared()
            .and_then(|i| i.try_read().ok().map(|g| g.len()))
            .unwrap_or(0);
        logln!("index: manual rescan finished, {count} entries");
        self.set_status_job(self.trf("索引重建完成：{} 项", &[count.to_string()]));
    }

    fn sync_status(&self) {
        // 工具栏粘贴按钮与右键菜单同源。
        //
        // 这里只问一句「剪贴板里有没有文件列表」，**不枚举路径**：sync_status
        // 调用很频繁（选中、刷新、切换目录都走它），枚举要为剪贴板里每个文件做
        // 两次 Win32 调用，并且在别的程序占着剪贴板时会重试到上百毫秒——那正是
        // 界面卡顿的来源。真正的粘贴仍由 paste_clipboard 走 paste_state() 重新读。
        // 回收站内的剪切从不写系统剪贴板，所以保留内部缓存这条兜底。
        let can_paste = clipboard::has_file_list()
            || (self.state.clipboard_from_recycle && !self.state.clipboard_paths.is_empty());
        self.ui.set_can_paste(can_paste);
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
            // 带上「扫了多少条」：网络盘上命中先出完、后面几十秒一条都没有，
            // 只写「正在搜索」看起来就是卡死了，有个递增的数字才看得出还在跑。
            let scanned = fmt_scan_count(tab.active().deep_scanned);
            left.push_str(&if en {
                format!(" · Searching subfolders… scanned {scanned}")
            } else {
                format!(" · 正在搜索子文件夹…已扫 {scanned} 项")
            });
        } else if tab.active().deep_capped {
            // 上限掐停 ≠ 扫完了：明说「只给了前 N 个」，免得用户以为磁盘上就这么多。
            left.push_str(if en {
                " · Result cap reached, narrow the search"
            } else {
                " · 结果已达上限，请缩小搜索范围"
            });
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

    /// 请求后台重新探测整批侧栏数据（盘符 / 回收站 / 网络位置）。
    ///
    /// 只排队一个请求：15s 定时器、刷新按钮、缩略图符号刷新很容易挤在一起。
    /// 结果到达后由 40ms 轮询统一应用（并重绘侧栏）。
    fn request_sidebar_probe(&mut self) {
        if self.sidebar_probe_busy {
            return;
        }
        if self.sidebar_probe_tx.send(()).is_ok() {
            self.sidebar_probe_busy = true;
        }
    }

    /// 用持有数据重建侧栏模型（不再做任何系统调用，数据来自后台探测）。
    fn sync_sidebar(&mut self) {
        let stats = self.recycle_stats;
        // 用户自己添加的（`网络位置` 分区，显示别名）。
        let network: Vec<(String, String)> = self
            .config
            .network_locations
            .iter()
            .map(|n| (n.label.clone(), n.path.clone()))
            .collect();
        // 资源管理器里本来就有、但用户没加到 Rove 的（独立 `网络位置(系统)` 分区）。
        let mut detected: Vec<(String, String)> = Vec::new();
        for remote in self.net_wnet.clone() {
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
        for (label, target) in self.net_places.clone() {
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
        // 纳入界面语言：分区标题（快速访问 / 此电脑 / 网络位置…）是 Rust 侧
        // 用 `tr()` 生成后下发到模型的字符串，不是 Slint 绑定，切语言必须重建。
        fingerprint = fingerprint
            .wrapping_mul(31)
            .wrapping_add(!i18n::is_chinese(&self.config.language) as u64);
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

        // 侧栏图标统一采用 Windows 标准 32px 规格：
        // 彻底解决启动阶段 scale 由 1.0 变 1.5 导致请求 key(20) 与判定 key(30) 脱节的 Bug！
        // 尺寸常量统一定义在模块级（见 `SIDEBAR_ICON_PX`）：缩略图「想要」集合
        // 靠这个值区分侧栏图标和列表行，两处不能各写一份。

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
                    self.thumb_for(Path::new("shell:RecycleBinFolder"), None, 0, SIDEBAR_ICON_PX)
                        .0
                } else {
                    self.thumb_for(&_path, None, 0, SIDEBAR_ICON_PX).0
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
                    // 网络路径不在这里 stat：掉线的共享会让界面线程卡住（还会在
                    // 每次刷新侧栏时重复卡）。这类条目照常显示，点进去打不开时
                    // 再由导航流程报错。
                    if !self.path_looks_remote(&path)
                        && !path.exists()
                        && !crate::fs::dir::is_unc_server_root(&path)
                    {
                        continue;
                    }
                    #[cfg(windows)]
                    let icon_img = self.thumb_for(&path, None, 0, SIDEBAR_ICON_PX).0;
                    #[cfg(not(windows))]
                    let icon_img = slint::Image::default();

                    // UNC 共享根没有 file_name()，统一走 leaf_name 取最后一段。
                    let label = crate::fs::dir::leaf_name(&path);
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
                        icon_color: rgb(NETWORK_ICON_COLOR),
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
                        icon_color: rgb(NETWORK_ICON_COLOR),
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
                    network_active_id(
                        &s,
                        network
                            .iter()
                            .map(|(_, t)| t.as_str())
                            .chain(detected.iter().map(|(_, t)| t.as_str())),
                    )
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
                name: crate::fs::dir::leaf_name(&s.path).into(),
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
        self.ui.set_staging_tray_visible(self.staging_tray_enabled());
    }

    /// 「添加到暂存盘」动作是否还开着。
    ///
    /// 关掉后没有任何入口能往暂存盘里加东西，左侧那块「临时暂存盘」
    /// 就只是个永远空的盒子，索性整块不渲染（见 ui/sidebar.slint 的
    /// `staging-visible`）。动作缺失时按「开」处理，与菜单默认一致。
    fn staging_tray_enabled(&self) -> bool {
        self.actions
            .iter()
            .find(|a| a.id == "builtin.stage")
            .map(|a| a.enabled)
            .unwrap_or(true)
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
            type_active0,
            type_active1,
            empty0,
            empty1,
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
                // 表头「类型」箭头的变色按栏判定（分屏时两栏可以各筛各的）。
                !panes[0].type_filter.is_empty(),
                panes.get(1).is_some_and(|p| !p.type_filter.is_empty()),
                // 空列表占位文案：搜索中要换成「正在搜索…已扫 N 项」，
                // 否则整盘搜一个小词时「此文件夹为空」会挂在那里几十秒，
                // 看起来像搜完了、什么都没有。
                (panes[0].deep_searching, panes[0].deep_scanned),
                panes.get(1).map(|p| (p.deep_searching, p.deep_scanned)),
            )
        };
        // 多选时不弹名称浮层（左右栏各自判定）。
        self.ui.set_file_single_selection(single0);
        self.ui.set_file_single_selection_right(single1);
        // 「所在目录」两套呈现，条件相同（搜索态或回收站 —— 两者条目来源各异，
        // 路径是唯一的来源线索），具体用哪套由视图决定：
        //  · 图标视图：悬停在格子上弹浮层；
        //  · 详细信息：`路径` 列（名称与修改时间之间）；
        //  · 列表：每行下面挂一行路径（列表没有列）。
        let (path_tip0, path_tip1, path_row0, path_row1) = {
            let panes = &self.state.active().panes;
            let search0 = !panes[0].filter.is_empty();
            let search1 = panes.get(1).is_some_and(|p| !p.filter.is_empty());
            (
                is_recycle0 || search0,
                is_recycle1 || search1,
                is_recycle0 || search0,
                is_recycle1 || search1,
            )
        };
        let (search_col0, search_col1) = (path_row0, path_row1);
        self.ui.set_file_path_tip(path_tip0);
        self.ui.set_file_path_tip_right(path_tip1);
        self.ui.set_file_path_row(path_row0);
        self.ui.set_file_path_row_right(path_row1);
        self.ui.set_file_search_active(search_col0);
        self.ui.set_file_search_active_right(search_col1);
        self.ui.set_type_filter_active(type_active0);
        self.ui.set_type_filter_active_right(type_active1);
        self.ui.set_sort_key(sort_key.into());
        self.ui.set_sort_ascending(sort_asc);
        self.ui.set_sort_key_right(sort_key_right.unwrap_or(sort_key).into());
        self.ui
            .set_sort_ascending_right(sort_asc_right.unwrap_or(sort_asc));
        self.ui.set_file_loading(loading0);
        self.ui.set_file_total(total0);
        self.ui.set_file_empty_text(empty0.0.then(|| self.trf("正在搜索…已扫 {} 项", &[fmt_scan_count(empty0.1)])).unwrap_or_else(|| self.tr("此文件夹为空").to_string()).into());
        if let Some((searching, scanned)) = empty1 {
            self.ui.set_file_empty_text_right(searching.then(|| self.trf("正在搜索…已扫 {} 项", &[fmt_scan_count(scanned)])).unwrap_or_else(|| self.tr("此文件夹为空").to_string()).into());
        }
        // 「路径」列：回收站一直有，搜索结果同样有（只在详细信息视图显示，
        // 见 Slint）。「此电脑」的全局搜索结果也是普通文件，照样加这一列。
        self.ui.set_show_path_column(search_col0 || is_recycle0);
        self.ui
            .set_show_path_column_right(search_col1 || is_recycle1);
        // 视图形态（此电脑无修改时间列、大小列表头叫剩余）也按栏判定，
        // 否则分屏时「此电脑 + 普通目录」会把普通目录的修改时间列一起藏掉。
        self.ui.set_file_is_computer_view(is_computer0);
        self.ui.set_file_is_computer_view_right(is_computer1);
        self.ui.set_is_split(is_split);
        self.ui.set_file_loading_right(loading1);
        self.ui.set_file_total_right(total1);
        self.refresh_visible_rows();
        // 目录/标签变了就要重新告诉 Shell「哪个窗口在显示哪个目录」，
        // 否则「打开文件所在位置」会找不到我们（见 sys::shell_windows）。
        self.publish_shell_windows();
    }

    /// 把当前所有窗格显示的目录交给 Shell 窗口登记器。
    ///
    /// 内容没变时登记器自己会跳过，所以放在 `sync_file_list` 末尾是安全的。
    fn publish_shell_windows(&self) {
        #[cfg(not(windows))]
        {
            // 非 Windows 宿主只编译不运行，登记器是空实现。
            return;
        }
        #[cfg(windows)]
        {
            let Some(hwnd) = get_window_hwnd(&self.ui) else {
                return;
            };
            let mut panes = Vec::new();
            for (tab_index, tab) in self.state.tabs.iter().enumerate() {
                for (pane_index, pane) in tab.panes.iter().enumerate() {
                    let Some(path) = pane.path.as_ref() else {
                        continue;
                    };
                    // 回收站/搜索结果是虚拟视图（`recycle://`），Shell 没有对应目录。
                    if path.to_string_lossy().contains("://") {
                        continue;
                    }
                    panes.push(crate::sys::shell_windows::PaneRegistration {
                        key: ((tab_index as u64) << 8) | pane_index as u64,
                        path: path.clone(),
                        hwnd,
                    });
                }
            }
            crate::sys::shell_windows::publish(panes);
        }
    }

    /// 找出正在显示 `folder` 的窗格：先看当前标签，再看其他标签。
    ///
    /// 选中请求可能来自「另一个标签已经打开了这个目录」，此时应该切过去，
    /// 而不是再开一个重复的标签。
    fn locate_folder(&self, folder: &Path) -> Option<(usize, usize)> {
        let active = self.state.active_tab;
        let mut tabs: Vec<usize> = vec![active];
        tabs.extend(
            (0..self.state.tabs.len()).filter(|index| *index != active),
        );
        for tab_index in tabs {
            let Some(tab) = self.state.tabs.get(tab_index) else {
                continue;
            };
            if tab.kind != crate::state::TabKind::Explorer {
                continue;
            }
            if let Some(pane_index) = tab.panes.iter().position(|pane| {
                pane.path
                    .as_ref()
                    .is_some_and(|current| crate::fs::dir::same_path_loose(current, folder))
            }) {
                return Some((tab_index, pane_index));
            }
        }
        None
    }

    /// 在当前窗格里选中某个路径并滚进可视区（Shell 的「选中这个条目」请求）。
    fn select_path_in_active_pane(&mut self, target: &Path) {
        let mut found = None;
        {
            let pane = self.state.active_mut().active_mut();
            for (pos, &entry_index) in pane.display.iter().enumerate() {
                let Some(entry) = pane.entries.get(entry_index) else {
                    continue;
                };
                if crate::fs::dir::same_path_loose(&entry.path, target) {
                    found = Some((pos, entry.path.clone()));
                    break;
                }
            }
            if let Some((_, path)) = found.as_ref() {
                pane.selection.clear();
                pane.selection.insert(path.clone());
                pane.anchor = Some(path.clone());
            }
        }
        let Some((pos, path)) = found else {
            // 列表还没加载完（冷启动时最典型：Shell 的选中请求比目录列举先到），
            // 或者条目在当前过滤/隐藏规则下看不见。不要丢掉这次请求：转成
            // 「列表回来后选中并滚进视口」，与接管打开的那条链路共用同一套机制。
            logln!(
                "[shell-windows] 列表里还没有这个条目，等列举完成后选中：{target:?}"
            );
            self.arm_reveal(Some(target.to_path_buf()));
            return;
        };
        logln!("[shell-windows] 选中 pos={pos} {path:?}");
        self.scroll_to_pane = None;
        self.scroll_to_index = Some(pos);
        self.visible_cache = None;
        self.visible_cache_right = None;
        self.refresh_visible_rows();
        self.sync_status();
    }

    /// 重建虚拟列表可见窗口（滚动/选择/数据变化时调用）。
    /// 取某路径的缩略图：命中缓存直接返回；否则对候选路径发起后台加载
    /// （去重），返回空图（UI 显示占位色块，加载完成后经事件循环刷新）。
    /// 当前视图对应的缩略图边长。
    #[cfg(windows)]
    fn thumb_size_for_view(&mut self) -> u32 {
        // 获取当前窗口的真实物理缩放比例（如 125% -> 1.25, 150% -> 1.5）
        //
        // 必须用 `ui_scale()`（会话内只增不减）：`show()` 之后、窗口真正落到显示器
        // 之前 `scale_factor()` 是 1.0，同一批行里先建的按 132 取图、后建的按 264，
        // 结果一半图标是低分辨率放大出来的（日志里 `req=132` / `req=264` 混着出现）。
        let scale = self.ui_scale();

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
    fn drag_icon_px(&mut self) -> u32 {
        let scale = self.ui_scale();
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
    ) -> (slint::Image, u64) {
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
        // 1. 内存 LRU 命中：只更新「最近使用序号」（一次哈希查找 + 写一个 u64）。
        //
        // 已缓存的行不需要再登记「想要」：它没有在途请求，登记了也只是让
        // worker 多比较一次。所以这里直接返回，连 ThumbKey 的克隆都省掉。
        if let Some(entry) = self.thumb_cache.get_mut(&key) {
            let img = entry.img.clone();
            let epoch = entry.epoch;
            self.thumb_use_seq += 1;
            entry.last_used = self.thumb_use_seq;
            return (img, epoch);
        }

        // 2. 负缓存命中：本会话内该图确实取不到，直接空白。
        if self.thumb_failed.contains(&key) {
            crate::logv!(
                "[thumb] negative-cached (will stay blank) {} size={}",
                path.display(),
                size
            );
            return (slint::Image::default(), 0);
        }

        // 从现在起就登记「想要」：key 立刻进集合（worker 可能瞬间就取到任务，
        // 不能等到本轮行模型全部建完才发布），同时留给本轮结束时整体替换。
        // 位置在缓存命中之后：已缓存的行没有在途请求，登记它只是白克隆一次 key。
        // 位置在 pending 判定**之前**：上一帧排队中的行这一帧仍然属于窗口，
        // 不登记就会被 worker 当成「已经滚走」跳过，白白重来一轮。
        self.window_wanted.insert(key.clone());
        thumb_want(&key);

        // 3. 已在 pending：等它出图即可。
        //
        // 不需要再"提到优先池"：只有一条队列，且 worker 取活前会查「想要」集合
        // （见 `thumb_is_wanted`）——跳到新位置时，上一个位置残留的请求会被瞬间
        // 跳过，本窗口的请求按入队顺序排在前面。滚回来的行如果曾被判定过期，
        // 它也在本轮的「想要」集合里，40ms 轮询重建时会重新发。
        if self.thumb_pending.contains(&key) {
            return (slint::Image::default(), 0);
        }

        // 4. 未在 pending 中：先查磁盘缓存，查不到再交给后台 Shell 解码。
        {
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
                            let epoch = self.insert_thumb_cache(key, img.clone());
                            return (img, epoch);
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

            // 磁盘未命中，发送给后台去解码。全体请求一条队列，
            // 顺序 = `thumb_order_key` 的三段式（可见自上而下 → 下方自上而下 → 上方自下而上）。
            if self.thumb_tx.send(key.clone()).is_err() {
                // 通道断开：不能把 key 永远留在 pending 里，否则该行永久空白。
                crate::logln!(
                    "[thumb] !! send to worker channel FAILED {} size={}",
                    path.display(),
                    size
                );
                self.thumb_pending.remove(&key);
                self.mark_thumb_failed(key);
            }
            // 缩略图诊断（ROVE_LOG=1）：本窗口前 24 条「真的发出去了」的请求。
            // 顺序 = `[thumb] window ... order=[...]` 里的顺序。
            if self.thumb_diag_left > 0 {
                self.thumb_diag_left -= 1;
                crate::logv!("[thumb] send size={} {}", size, path.display());
            }
        }
        // 未在 pending 时的收尾：不记日志。此处在热路径上（每行每次刷新都会调用），
        // 1538 张图加载期间可达 6 万行、占日志 79%。
        (slint::Image::default(), 0)
    }

    /// 写入 LRU 缓存（内存），按字节预算 + 项数双上限淘汰。
    ///
    /// 淘汰依据是每项自己的 `last_used` 序号，不再维护一条顺序表：
    /// 命中路径因此是 O(1)，只有真正要淘汰时才扫一遍找最久未用的那条
    /// （每次插入最多触发一次）。
    fn insert_thumb_cache(&mut self, key: ThumbKey, img: slint::Image) -> u64 {
        let sz = img.size();
        // RGBA8 估算：每像素 4 字节。
        let bytes = (sz.width * sz.height * 4) as usize;

        if let Some(old) = self.thumb_cache.remove(&key) {
            self.thumb_cache_bytes = self.thumb_cache_bytes.saturating_sub(old.bytes);
        }

        self.thumb_epoch_next += 1;
        let epoch = self.thumb_epoch_next;
        self.thumb_use_seq += 1;
        self.thumb_cache.insert(
            key,
            CachedThumb {
                img,
                bytes,
                epoch,
                last_used: self.thumb_use_seq,
            },
        );
        self.thumb_cache_bytes += bytes;

        // 淘汰：每次挑一条最久未用的，直到符合双上限。
        while self.thumb_cache.len() > THUMB_CACHE_MAX_ITEMS
            || self.thumb_cache_bytes > THUMB_CACHE_MAX_BYTES
        {
            let Some(victim) = lru_victim(&self.thumb_cache, |v| v.last_used) else {
                break;
            };
            if let Some(evicted) = self.thumb_cache.remove(&victim) {
                self.thumb_cache_bytes = self.thumb_cache_bytes.saturating_sub(evicted.bytes);
            }
        }
        epoch
    }

    #[cfg(not(windows))]
    fn thumb_for(
        &mut self,
        _path: &Path,
        _mtime: Option<SystemTime>,
        _file_size: u64,
        _size: u32,
    ) -> (slint::Image, u64) {
        (slint::Image::default(), 0)
    }

    #[cfg(not(windows))]
    fn thumb_size_for_view(&mut self) -> u32 {
        44
    }

    /// 取图/拖拽图用的缩放系数：**会话内只增不减**。
    ///
    /// `window().scale_factor()` 在 `show()` 之后、窗口真正落到显示器之前仍是 1.0，
    /// 同一批行里先建的会按 132 取图、后建的按 264，日志里就是 `req=132` 与
    /// `req=264` 混着出现——那一半图标是低分辨率放大出来的（发虚）。
    /// 单调取最大值保证「第一次取图就取到够用的分辨率」；把窗口拖到更低 DPI 的
    /// 显示器上只是多留几像素，没有副作用。
    #[cfg(windows)]
    fn ui_scale(&mut self) -> f32 {
        let now = self.ui.window().scale_factor() as f32;
        if now > self.ui_scale {
            self.ui_scale = now;
        }
        self.ui_scale.max(1.0)
    }

    /// 登记一条缩略图失败。超过上限时按插入顺序淘汰最老的几条。
    fn mark_thumb_failed(&mut self, key: ThumbKey) {
        if !self.thumb_failed.insert(key.clone()) {
            return;
        }
        self.thumb_failed_order.push_back(key);
        while self.thumb_failed.len() > THUMB_FAILED_MAX_ITEMS {
            let Some(old) = self.thumb_failed_order.pop_front() else {
                break;
            };
            self.thumb_failed.remove(&old);
        }
    }

    /// 标记缩略图失败（清理 pending + 负缓存，本会话不再重试）。
    fn fail_thumbnail(&mut self, key: ThumbKey) {
        // 诊断（ROVE_LOG=1）：到图（含失败）顺序，与 send 顺序对照就能看出
        // 哪几行是最后才被解出来的。
        self.log_thumb_got("FAIL", &key.path, key.px_size);
        crate::logv!(
            "[thumb] FAILED -> negative cache {} size={} (row stays blank)",
            key.path.display(),
            key.px_size
        );
        self.thumb_pending.remove(&key);
        self.mark_thumb_failed(key);
        // 负缓存已经挡住后续请求，这一行不会再自己变；立刻重画一次，
        // 否则它会一直空着直到下次窗口尺寸/排序变化。
        self.thumb_needs_refresh = true;
    }

    /// worker 丢弃了一条已过期（用户已滚走/跳走）的请求：
    /// 只把 key 从 pending 里拿出来（滚回来时会重新请求），**不进负缓存**。
    fn cancel_thumbnail(&mut self, key: ThumbKey) {
        self.thumb_pending.remove(&key);
        // 万一用户又滚回来了，下一轮 40ms 轮询重建行时会重新发请求。
        self.thumb_needs_refresh = true;
        self.thumb_cancel_n += 1;
        if self.thumb_cancel_n >= 8
            || self.thumb_cancel_t.elapsed() >= std::time::Duration::from_secs(1)
        {
            let n = self.thumb_cancel_n;
            self.thumb_cancel_n = 0;
            self.thumb_cancel_t = std::time::Instant::now();
            crate::logv!("[thumb] cancelled {n} stale requests (scrolled away)");
        }
    }

    /// 写入缩略图缓存（事件循环线程）：只打批量刷新标记，由 poll_timer 统一刷新。
    fn store_thumbnail(&mut self, key: ThumbKey, img: slint::Image) {
        self.log_thumb_got("ok", &key.path, key.px_size);
        self.thumb_pending.remove(&key);
        let is_sidebar_icon = key.px_size == SIDEBAR_ICON_PX;
        self.insert_thumb_cache(key, img);
        // 核心修复：不再每次 store 都全量 refresh，只置位，由主线程 40ms 批量刷新。
        self.thumb_needs_refresh = true;
        // 侧栏图标（32px）也走这条管道：单独置位，避免列表缩略图每批都重建侧栏模型。
        if is_sidebar_icon {
            self.sidebar_icon_dirty = true;
        }
    }

    /// 缩略图诊断（ROVE_LOG=1）：本窗口前 24 条到图记录。
    fn log_thumb_got(&mut self, tag: &str, path: &Path, px: u32) {
        if self.thumb_diag_got >= 24 {
            return;
        }
        self.thumb_diag_got += 1;
        let n = self.thumb_diag_got;
        crate::logv!("[thumb] got#{n} {tag} size={px} {}", path.display());
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

    /// 视口几何的常用组合：`(视口高, 每行列数, 行高, 应渲染行数)`。
    ///
    /// 启动首帧布局未落定时用占位值兜底（视口 600 / 默认行高），否则只能渲染
    /// 前两行（表现为「此电脑」部分盘符空白）。
    fn view_metrics(&self) -> (f32, usize, f32, usize) {
        let raw_h = self.ui.get_list_height();
        let list_height = if raw_h > 1.0 { raw_h } else { 600.0 };
        let cells = self.ui.get_cells_per_row().max(1) as usize;
        let row_h = if self.ui.get_row_height_out() > 0.0 {
            self.ui.get_row_height_out()
        } else {
            ROW_HEIGHT
        };
        let rows_visible = (list_height / row_h).ceil() as usize + OVERSCAN * 2;
        (list_height, cells, row_h, rows_visible)
    }

    /// 某栏此刻应该渲染的条目窗口 `(first, count)`（与 `refresh_visible_rows` 同源）。
    /// 只用于判断「渲染窗口变了没有」：行是绝对定位的，窗口没变就不必重建整表。
    fn pane_window_now(&self, pane_index: usize) -> Option<(usize, usize)> {
        let (_, cells, row_h, rows_visible) = self.view_metrics();
        let pane = self.state.active().panes.get(pane_index)?;
        Some(pane_view_range(
            pane.display.len(),
            cells,
            row_h,
            rows_visible,
            pane.scroll_y.max(0.0),
        ))
    }

    fn refresh_visible_rows(&mut self) {
        let (list_height, cells_per_row, row_h, rows_visible) = self.view_metrics();

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
                self.scroll_to_pane
                    .take()
                    .unwrap_or(tab.active_pane)
                    .min(tab.panes.len().saturating_sub(1))
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

        // 左栏始终绑定 pane 0
        // 先清空「想要」集合：本轮两个分屏的屏幕内容都进去，最后一起发布。
        self.window_wanted.clear();
        let batch0 =
            self.build_rows_for_pane(self.state.active_tab, 0, first0, count0, list_height);
        let rows0 = &batch0.rows;
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
            // 诊断（ROVE_LOG=1）：哪些条目还没图。只在「空白集合」变化时打一行，
            // 避免每 40ms 刷屏。看 first_blank 是否从 0 跳到了 3/4 行。
            let blank: Vec<i32> = rows0
                .iter()
                .filter(|r| r.thumb.size().width == 0)
                .map(|r| r.index)
                .take(24)
                .collect();
            let sig = (blank.first().copied().unwrap_or(-1), empty_thumbs);
            if self.thumb_blank_sig != Some(sig) {
                self.thumb_blank_sig = Some(sig);
                // 详细级：滚动/加载期间每 0.5s 一行，只用于排查“哪几行没出图”。
                let expect_px = self.thumb_size_for_view();
                crate::logv!(
                    "[rows] EMPTY THUMBS {}/{} blank={:?} | geom list_h={} (raw={}) cells={} row_h={} win_rows={} | total={} first={} count={} scroll={:.1} mode={} is_split={} expect_px={} cache_len={} pending={} failed={}",
                    empty_thumbs,
                    rows0.len(),
                    blank,
                    list_height,
                    self.ui.get_list_height(),
                    cells_per_row,
                    row_h,
                    rows_visible,
                    total0,
                    first0,
                    count0,
                    scroll0,
                    self.config.view_mode,
                    is_split,
                    expect_px,
                    self.thumb_cache.len(),
                    self.thumb_pending.len(),
                    self.thumb_failed.len()
                );
            }
        } else if self.thumb_blank_sig.is_some() && !rows0.is_empty() {
            self.thumb_blank_sig = None;
            crate::logv!("[rows] all {} visible thumbs ready", rows0.len());
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
                self.ui.get_list_height(),
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
                // 良性：重命名框在根层固定浮层里（不受行窗口影响），用户把目标
                // 条目滚出可视窗口是正常操作，不需要把它当成故障。
                logv!(
                    "[rename] rename row outside visible window rename-path={} rows={} first={} count={} total={}",
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
                // 重命名期间行模型被改写时，把“重命名行换了槽位 / 离开了窗口”记一笔。
                // 这只是排查线索：重命名框已搬到根层固定浮层（详见 file_list.slint
                // 的「固定重命名浮层」），行怎么滚都不会销毁输入框，所以这些情况
                // 全部是良性的，只放详细级。
                let rp = self.ui.get_rename_path();
                let slot_of = |rows: &[FileRow]| {
                    rows.iter()
                        .position(|r| r.path.as_str() == rp.as_str())
                        .map(|i| i as i32)
                        .unwrap_or(-1)
                };
                let new_slot = slot_of(rows0);
                let old_slot = self
                    .row_diff_left
                    .as_ref()
                    .map(|st| {
                        st.stamps
                            .iter()
                            .position(|s| s.path_text.as_str() == rp.as_str())
                            .map(|i| i as i32)
                            .unwrap_or(-1)
                    })
                    .unwrap_or(-1);
                let prev_total = self.rows_sig.map(|s| s.3).unwrap_or(0);
                if new_slot < 0 {
                    // 良性：目标行滚出了窗口（浮层按绝对行号定位，仍在原位）。
                    logv!(
                        "[rename] rename row left window (old_slot={} total={}->{}) sig={:?} prev={:?}",
                        old_slot,
                        prev_total,
                        total0,
                        sig,
                        self.rows_sig
                    );
                } else if old_slot < 0 && total0 > prev_total {
                    // 良性：新建/粘贴刚把这一行加进来，它以前不存在，没有输入框可销毁。
                    crate::logv!(
                        "[rename] new row appeared at slot {} during rename (input created) sig={:?}",
                        new_slot,
                        sig
                    );
                } else if old_slot != new_slot {
                    logv!(
                        "[rename] rename row moved slot {} -> {} (list scrolled) sig={:?} prev={:?}",
                        old_slot,
                        new_slot,
                        sig,
                        self.rows_sig
                    );
                } else {
                    crate::logv!(
                        "[rename] rows rewritten but rename row kept slot {} (input kept) sig={:?} prev={:?}",
                        new_slot,
                        sig,
                        self.rows_sig
                    );
                }
            }
            // 写可见行：内容没变的行原地更新，只有变了的行才通知 Slint，
            // 因此即便这里“重建”了，行内 TextInput 也不会被销毁（见 write_rows）。
            self.write_rows(0, batch0);
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
            let batch1 =
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
                self.write_rows(1, batch1);
                self.rows_sig_right = Some(sig_right);
                if renaming && rename_pane == 1 {
                    self.arm_rename_scroll_guard();
                }
            }
        } else {
            self.ui.set_file_total_right(0);
            self.row_diff_right = None;
            if self.ui.get_file_rows_right().row_count() != 0 {
                self.ui
                    .set_file_rows_right(ModelRc::new(VecModel::from(Vec::<FileRow>::new())));
            }
            self.rows_sig_right = None;
        }

        // 行已经就绪：没有手动列宽记录的目录，这里按最长条目自适应一次。
        #[cfg(windows)]
        autofit_column_widths_if_needed(self);

        // 发布本轮窗口的 key 集合：worker 取到不属于「现在这一屏」的旧请求时直接跳过。
        publish_thumb_wanted(std::mem::take(&mut self.window_wanted));

        // 记下两栏本帧真正写进行模型的窗口：滚动回调据此判断“窗口没变，不必重建”。
        self.visible_cache = Some((first0, count0));
        self.visible_cache_right = if is_split {
            self.pane_window_now(1)
        } else {
            None
        };

        self.sync_expand_targets();
    }

    /// 下发给 Slint：正在重命名的条目在当前栏行模型里的行号（-1 = 无）。
    ///
    /// 详细信息 / 列表 / 小图标的行是虚拟化的，重命名输入框必须放在根层
    /// 唯一实例里才不会随滚动被销毁（否则表现为“浮层还在却打不进字”）。
    /// 那层浮层要按行号定位，所以要这个索引；坐标系与 `FileRow.index` 一致
    /// （= `display_entry` 的索引，见 `build_rows_for_pane`）。
    fn sync_rename_row(&mut self) {
        let rename = self.ui.get_rename_path().to_string();
        let rename_pane = self.ui.get_rename_pane() as usize;
        let prev = self.ui.get_file_rename_row();
        let mut row = -1i32;
        if !rename.is_empty() {
            let want = std::path::PathBuf::from(&rename);
            let tab = self.state.active();
            if let Some(pane) = tab.panes.get(rename_pane) {
                // 先用上次的行号验一下：这一行上还是同一个条目，就说明排序 / 过滤
                // 都没把它挪走（滚动不改变显示顺序），直接沿用 —— 一次比较搞定。
                // 大目录里这是常态：以前每帧都要把整个显示列表扫一遍（上万条路径
                // 比对），现在只剩第一次。
                let cached_ok = prev >= 0
                    && pane
                        .display_entry(prev as usize)
                        .is_some_and(|e| e.path == want);
                if cached_ok {
                    row = prev;
                } else if let Some(pos) = (0..pane.display.len())
                    .find(|&di| pane.display_entry(di).is_some_and(|e| e.path == want))
                {
                    row = pos as i32;
                    crate::logv!(
                        "[rename] rescan row: scanned {} entries -> {}",
                        pane.display.len(),
                        pos
                    );
                } else if prev >= 0 {
                    // 找不到就保留上一次的行号：宁可能差一行，也不能把输入框
                    // 整个撤掉——撤掉就等于没法继续输入，也没法回车/Esc 退出。
                    row = prev;
                }
            }
        }
        if prev != row {
            self.ui.set_file_rename_row(row);
            crate::logv!("[rename] fixed box row={}", row);
        }
    }

    /// 算出图标视图“展开名称层”的目标：正在重命名的条目，或本栏只选中一个条目时。
    ///
    /// 给出的是**绝对显示索引**（与 `FileRow.index` 同一坐标系）。
    /// 只在中/大/超大图标视图生效——小图标视图是横排单行，保持原样。
    /// 另外顺带下发“重命名行号”（详细信息 / 列表 / 小图标的固定浮层要用）。
    fn sync_expand_targets(&mut self) {
        self.sync_rename_row();
        let icons_big = is_grid_icons_view(&self.config.view_mode);
        if !icons_big {
            self.ui.set_file_expand_index(-1);
            self.ui.set_file_expand_name(SharedString::default());
            self.ui.set_file_expand_box_w(0.0);
            self.ui.set_file_expand_line_ws(empty_lengths());
            self.ui
                .set_file_expand_lines(ModelRc::new(VecModel::from(Vec::<GridLine>::new())));
            self.ui.set_file_expand_index_right(-1);
            self.ui.set_file_expand_name_right(SharedString::default());
            self.ui.set_file_expand_box_w_right(0.0);
            self.ui.set_file_expand_line_ws_right(empty_lengths());
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
            self.ui.set_file_hover_expand_line_ws(empty_lengths());
            self.ui.set_file_hover_expand_index_right(-1);
            self.ui.set_file_hover_expand_lines_right(ModelRc::new(VecModel::from(
                Vec::<GridLine>::new(),
            )));
            self.ui.set_file_hover_expand_box_w_right(0.0);
            self.ui.set_file_hover_expand_line_ws_right(empty_lengths());
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
        // 定位缓存取到本地：下面 `p` 借用着 `self.state`，不能再借 `self`。
        let mut cache = std::mem::take(&mut self.expand_row_cache);
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
            // 悬停层：指针下的格子，它的显示下标本来就是已知的（hovers[pane]），
            // 不必再去显示列表里搜一遍。与选中项相同时下面会跳过重复绘制。
            let hover_hit = if hovers[pane] >= 0
                && p.display_entry(hovers[pane] as usize).is_some()
            {
                Some(hovers[pane])
            } else {
                None
            };
            // 选中 / 重命名层：先用上一帧的行号验一次，对不上才扫整个显示列表。
            let sel_hit = target
                .as_ref()
                .and_then(|w| expand_locate(p, w.as_path(), &mut cache[pane]));
            let rows = if pane == 1 {
                self.ui.get_file_rows_right()
            } else {
                self.ui.get_file_rows()
            };
            let resolve = |di: Option<i32>| -> Option<(i32, String, Vec<(String, bool)>)> {
                let di = di?;
                let e = p.display_entry(di as usize)?;
                let name = e.name.clone();
                // 该条目的完整高亮分段：行模型里存的就是未截断的原始分段，
                // 直接复用，避免在这里重跑一遍搜索命中计算。
                let key = e.path.to_string_lossy();
                let mut parts: Vec<(String, bool)> = Vec::new();
                for i in 0..rows.row_count() {
                    let Some(r) = rows.row_data(i) else { continue };
                    if r.path.as_str() == key.as_ref() {
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
            if let Some(t) = resolve(sel_hit) {
                targets[pane] = t;
            }
            if let Some(t) = resolve(hover_hit) {
                hover_targets[pane] = t;
            }
        }
        self.expand_row_cache = cache;
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
        // 每项：(框宽, 逐行分段, 逐行文字宽, 名称, 悬停框宽, 悬停逐行分段, 悬停逐行文字宽)
        #[allow(clippy::type_complexity)]
        let mut plans: [Option<(f32, GridLines, Vec<f32>, SharedString, f32, GridLines, Vec<f32>)>;
            2] = [None, None];
        for (pane, t) in targets.iter().enumerate() {
            let ht = &hover_targets[pane];
            // 悬停项与选中/重命名项是同一个格子：不重复画（Slint 侧也会跳过）。
            let hover_same = ht.0 < 0 || ht.0 == t.0;
            let mut hover_sig = (-1i32, 0u32, 0u64);
            let mut hover_plan = (0.0f32, GridLines::new(), Vec::new());
            if !hover_same {
                let (needs, box_w, lines) = expand_layout(&self.ui, &ht.2);
                let index = if needs { ht.0 } else { -1 };
                hover_sig = (index, box_w.to_bits(), hash_lines(&lines));
                let ws = line_widths(&lines);
                hover_plan = (box_w, lines, ws);
            }
            if t.0 < 0 {
                sigs[pane] = (-1, 0, 0, 0, hover_sig.0, hover_sig.1, hover_sig.2);
                plans[pane] = Some((
                    0.0,
                    Vec::new(),
                    Vec::new(),
                    SharedString::default(),
                    hover_plan.0,
                    hover_plan.1,
                    hover_plan.2,
                ));
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
            let line_ws = line_widths(&lines);
            plans[pane] = Some((
                box_w,
                lines,
                line_ws,
                t.1.as_str().into(),
                hover_plan.0,
                hover_plan.1,
                hover_plan.2,
            ));
        }
        if self.expand_sig.as_ref() == Some(&sigs) {
            return;
        }
        self.expand_sig = Some(sigs);

        for (pane, plan) in plans.into_iter().enumerate() {
            let (box_w, lines, line_ws, name, hv_box_w, hv_lines, hv_line_ws) = match plan {
                Some(v) => v,
                None => (
                    0.0f32,
                    Vec::new(),
                    Vec::new(),
                    SharedString::default(),
                    0.0f32,
                    Vec::new(),
                    Vec::new(),
                ),
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
                self.ui
                    .set_file_expand_line_ws(ModelRc::new(VecModel::from(line_ws)));
                self.ui.set_file_expand_lines(model);
                self.ui.set_file_hover_expand_index(hv_index);
                self.ui.set_file_hover_expand_box_w(hv_box_w);
                self.ui
                    .set_file_hover_expand_line_ws(ModelRc::new(VecModel::from(hv_line_ws)));
                self.ui.set_file_hover_expand_lines(hv_model);
            } else {
                self.ui.set_file_expand_index_right(index);
                self.ui.set_file_expand_name_right(name);
                self.ui.set_file_expand_box_w_right(box_w);
                self.ui
                    .set_file_expand_line_ws_right(ModelRc::new(VecModel::from(line_ws)));
                self.ui.set_file_expand_lines_right(model);
                self.ui.set_file_hover_expand_index_right(hv_index);
                self.ui.set_file_hover_expand_box_w_right(hv_box_w);
                self.ui
                    .set_file_hover_expand_line_ws_right(ModelRc::new(VecModel::from(hv_line_ws)));
                self.ui.set_file_hover_expand_lines_right(hv_model);
            }
        }
    }

    /// 为指定面板构建可视窗口的行模型（左栏 pane 0 / 右栏 pane 1）。
    ///
    /// 修改时间文本（带记忆表，见 `time_text_memo`）。
    fn time_text(&mut self, time: Option<SystemTime>) -> SharedString {
        if let Some(hit) = self.time_text_memo.get(&time) {
            return hit.clone();
        }
        let text: SharedString = fs::format_time(time).into();
        if self.time_text_memo.len() >= 4096 {
            self.time_text_memo.clear();
        }
        self.time_text_memo.insert(time, text.clone());
        text
    }

    /// 把一屏行写进 Slint 行模型，能原地更新就原地更新。
    ///
    /// `set_file_rows(ModelRc::new(..))` 是整表换模型：Slint 会把 `for` 里每一行
    /// 元素销毁重建，行内 TextInput、悬停态、动画全部重来。缩略图连片到达时
    /// 40ms 轮询一路都在换表，代价很大。
    ///
    /// 这里改成：上下文没变、行数没变时，只对内容真的变了的行调 `set_row_data`。
    /// Slint 收到 RowDataChanged 只是更新那一行元素的属性，元素本身保留
    /// （`RepeaterTracker::row_changed` → `comp.update`）。
    fn write_rows(&mut self, pane: usize, batch: RowBatch) {
        let ctx = {
            let tab = self.state.active();
            let Some(p) = tab.panes.get(pane) else { return };
            RowCtx {
                tab: self.state.active_tab,
                path: p.path.clone(),
                generation: p.generation,
                filter: p.filter.clone(),
                view_mode: self.config.view_mode.clone(),
                name_w_q: batch.name_w_q,
                total: p.display.len(),
                computer: p.path.is_none(),
                split: tab.panes.len() > 1,
            }
        };
        let prev = if pane == 0 {
            self.row_diff_left.take()
        } else {
            self.row_diff_right.take()
        };
        let model = if pane == 0 {
            self.ui.get_file_rows()
        } else {
            self.ui.get_file_rows_right()
        };
        // 此电脑视图不做增量：只有几行盘符，而且行内容会随剩余容量等外部
        // 信息变化，直接换表最省心。
        let patch = if ctx.computer {
            None
        } else {
            rows_to_patch(prev.as_ref(), &ctx, &batch.stamps, model.row_count())
        };
        let RowBatch { rows, stamps, .. } = batch;
        let mut patched = 0usize;
        match patch {
            Some(idx) => {
                patched = idx.len();
                for i in idx {
                    if let Some(row) = rows.get(i) {
                        model.set_row_data(i, row.clone());
                    }
                }
            }
            None => {
                let model_rc = ModelRc::new(VecModel::from(rows));
                if pane == 0 {
                    self.ui.set_file_rows(model_rc);
                } else {
                    self.ui.set_file_rows_right(model_rc);
                }
                // 整表换模型：悬停路径浮层记的是旧内容里的行号与路径，先清掉，
                // 否则鼠标不动时会继续显示上一批结果的路径。
                self.ui.invoke_clear_path_tip(pane as i32);
            }
        }
        // 诊断（ROVE_LOG=1）：一行一行补更新时打一行，便于确认「缩略图到达只动
        // 那一行」，而不是整屏重写。
        if patched > 0 {
            crate::logv!("[rows] patched {patched}/{} rows (pane={pane})", stamps.len());
        }
        let state = RowDiffState { ctx, stamps };
        if pane == 0 {
            self.row_diff_left = Some(state);
        } else {
            self.row_diff_right = Some(state);
        }
    }

    /// 两遍构建：第一遍在 `self.state` 的不可变借用下收集条目/选择/高亮/文本，
    /// 第二遍释放借用后再调 `self.thumb_for` 填缩略图（可能需要 &mut self）。
    fn build_rows_for_pane(
        &mut self,
        tab_index: usize,
        pane_index: usize,
        first: usize,
        count: usize,
        list_height: f32,
    ) -> RowBatch {
        // 名称区宽度（量化到 0.5px）供增量更新判断“排版口径有没有变”。
        // 下面阶段一必定会赋值（早退分支不会走到这里）。
        let name_w_q: i32;
        let collected: Vec<PreRow> = {
            let tab = &self.state.tabs[tab_index];
            let Some(pane) = tab.panes.get(pane_index) else {
                return RowBatch::empty();
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
            name_w_q = (name_area_w * 2.0).round() as i32;
            // 小图标视图：HorizontalLayout 里的名称 Text 宽度 = item-w - 58px
            // （导出值已按视图算好，这里只在导出为 0 时退回按格宽估算）。
            let small_name_w = if name_area_w > 24.0 {
                name_area_w
            } else {
                (self.ui.get_cell_w_out() as f32 - 66.0).max(24.0)
            };
            let details_name_w = name_area_w;
            let grid_name_w = name_area_w;
            // 只有竖排图标网格真正渲染 `grid-lines`：details/list 用单行 elide，
            // icons-small 用行内 name-parts。其余档位以前也在算三行切分，白做。
            let grid_view = is_grid_icons_view(&view_mode);
            // 剥离盘符前缀（如 c:），避免在文件名中误匹配 c: 里的无关字符
            let (_, clean_filter) = crate::search::parse_drive_prefix(&pane.filter);
            let hl_query = clean_filter.trim().to_lowercase();
            let cut = &self.state.cut_paths;
            let mut hl_matcher = nucleo_matcher::Matcher::new(nucleo_matcher::Config::DEFAULT);
            let mut hl_bufs = (Vec::new(), Vec::new(), Vec::new());
            let collected: Vec<PreRow> = (first..first + count)
                    .filter_map(|di| {
                        let entry = pane.display_entry(di)?;
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
                        // 此电脑视图的驱动器条目：显示名里的类型标签（"本地磁盘"）走
                        // 字典翻译，卷标保持原样（与侧边栏 / 面包屑的 tr_drive_label
                        // 一致）。只改显示名；排序、打开、重命名仍用 entry.name/path。
                        let display_name: SharedString = if computer_view {
                            self.tr_drive_label(&entry.name)
                        } else {
                            entry.name.clone().into()
                        };
                        // 图标网格的 name-area-w 恰好 = item-w-8 = cell-w-16，与渲染
                        // 预算同源，所以上面那次截断探针的切行结果可以直接当
                        // grid-lines 用，不必再按同一预算算第二遍。
                        // 此电脑视图的显示名被翻译过（探针用的是原名），单独再算一次。
                        let grid_lines = if !grid_view {
                            Vec::new()
                        } else {
                            grid_row_lines(&layout, &display_name, &entry.name, grid_name_w)
                        };
                        // 文字宽度按该视图的字号换算：估算表是按图标视图的 11.5px
                        // 定的，详细信息/列表用 13px、小图标用 12px。
                        let name_font_scale = if small_icons {
                            12.0 / 11.5
                        } else if view_mode == "details" || view_mode == "list" {
                            13.0 / 11.5
                        } else {
                            1.0
                        };
                        // 必须用**实际渲染出来的那一行**（`layout` / `grid_lines`）来量宽，
                        // 不能用 `name_parts`：没有搜索高亮时 `tip_parts` 是空的，
                        // `name_parts` 也就空 → 宽度算成 0 → 详细信息 / 列表 / 小图标
                        // 里「单选后再点名字」整片都判不进重命名。
                        let name_line_ws: Vec<f32> = if grid_lines.is_empty() {
                            layout.iter().map(|l| est_text_w(l) * name_font_scale).collect()
                        } else {
                            grid_lines.iter().map(|l| est_text_w(l)).collect()
                        };
                        Some(PreRow {
                            index: di as i32,
                            thumb_path: entry.path.clone(),
                            modified: entry.modified,
                            size: entry.size,
                            name: display_name,
                            grid_lines,
                            // 「路径」列显示删除前的原始路径，而不是内部唯一 Shell id。
                            path_text: entry
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
                                fs::format_size(entry)
                            }
                            .into(),
                            type_text: self.tr_type(&entry.type_text),
                            icon_color: icon_color(entry),
                            icon_text: icon_text_for(entry).into(),
                            selected,
                            cut,
                            name_parts,
                            tip_parts,
                            name_truncated,
                            name_line_ws,
                        })
                    })
                    .collect();
            collected
        };
        // 缩略图请求顺序（见 `thumb_order_key`）：屏幕内**自上而下**，缓冲行排后面。
        //
        // 病灶：可视窗口 = 可视行 + 2*OVERSCAN（icons-xl 下 5 列，约 100 个条目）。
        // 按索引顺序发时，先发的是窗口**顶部**那一批（含上方缓冲）；而旧窗口
        // 已经排过成千上万个已滚走的任务，新请求只能排在它们后面，于是
        // 看到的是「从顶部一个个慢慢加载」。曾经用「按离视口中心距离」排序解决：
        // 中心先出图（跳转到中间时整屏同时出现）；但滚动条在顶部时“中心”就是
        // 第 3/4 行，于是第 1/2 行反而最后才出图（用户看到的就是“从第 3 行开始渲染”）。
        //
        // 现在的规则同时兼顾两件事：
        // 1. **屏幕内的行**（不含 overscan）永远排最前，且自上而下——顶部时第一行
        //    第一个出图；
        // 2. 屏幕**下方**的缓冲行自上而下、屏幕**上方**的缓冲行自下而上，
        //    都排在屏幕内之后——突然滚到中间时依旧先把眼前这一屏填满。
        //
        // 注意：可见区间必须用**条目下标**算，不能用 `scroll / row_h`。
        // `scroll_y` 不是按行高换算的像素值（图标视图里两者不同步），
        // 除出来会得到一个偏离真实位置的行号，结果屏幕区间永远算错。
        // `first` 是本帧窗口的**首个条目下标**，但它包含了上方 OVERSCAN 行缓冲
        // （见 pane_view_range：`first_row = scroll/row_h - OVERSCAN`）。
        // 真实屏幕的首项要跳过「实际存在的」上方缓冲行。
        //
        // 关键修复：窗口在顶部时 pane_view_range 会把 first_row clamp 到 0，
        // 此时 first 已经是屏幕首行、**没有任何上方缓冲**。以前这里无条件
        // 跳过 OVERSCAN(=4) 行，等于把屏幕最前 4 行算成了缓冲行 —— 表现就是
        // 「滚动条在开头，缩略图却从第 4 行才开始渲染」。改为按 clamp 前的行数算实际缓冲。
        let row_h = if self.ui.get_row_height_out() > 0.0 {
            self.ui.get_row_height_out()
        } else {
            ROW_HEIGHT
        };
        let cells_per_row = (self.ui.get_cells_per_row().max(1)) as usize;
        let scroll = self.state.tabs[tab_index].panes[pane_index]
            .scroll_y
            .max(0.0);
        // 真实屏幕覆盖的行数＝**与视口相交**的行数，不是 `ceil(视口高/行高)`。
        // 滚动位置几乎从不对齐行高：比如 scroll=6429.5、row_h=144、视口 659 →
        // 顶行 44.65、底边 49.23，屏幕上能看到第 44–49 共 6 行（首尾各被截一半）。
        // 用 ceil(659/144)=5 会让**最底下那半行**（第 49 行）落进缓冲区，
        // 排到整屏之后——表现就是“整屏很快就出来了，就最底下那行要等好几秒”。
        let visible_rows = visible_row_count(scroll, list_height, row_h);
        let raw_first_row = (scroll / row_h).floor() as isize - OVERSCAN as isize;
        let effective_above = if raw_first_row < 0 {
            (OVERSCAN as isize + raw_first_row).max(0) as usize
        } else {
            OVERSCAN
        };
        // 真实屏幕的条目下标区间（不含上下缓冲）。
        let screen_first = (first + effective_above * cells_per_row).min(first + count);
        // 至少保证一个格子落在「屏幕内」：滚到底时 scroll_y 可能超过内容高度，
        // 这时 screen_first 已被夹到 first+count，若不再兜底，屏幕区间会是空的，
        // 整屏都会被当成缓冲行（旧代码就存在这个隐患）。
        let screen_last = (screen_first + visible_rows * cells_per_row)
            .min(first + count)
            .max((screen_first + 1).min(first + count));
        let core_first = screen_first;
        let core_last = screen_last;
        let screen_row_first = screen_first / cells_per_row;
        let screen_row_last = screen_last
            .div_ceil(cells_per_row)
            .max(screen_row_first + 1);
        if effective_above != OVERSCAN {
            // 顶部钳制：窗口贴着列表开头时 `first` 里已经没有上方缓冲行。
            // 不单独刷日志（每次重建都会命中，加载期间会刷屏）；
            // 需要时看下面那条 `[thumb] window ... above=0`。
            logv!(
                "[thumb] top-clamped window: first={} count={} scroll={:.1} above={} screen_first={} core=[{},{})",
                first, count, scroll, effective_above, screen_first, core_first, core_last
            );
        }

        let thumb_px = self.thumb_size_for_view();
        // 关键诊断（ROVE_LOG=1）：验收「存在截断就弹浮层」时记录一次汇总。
        // 按签名节流，避免滚动/40ms 轮询每次都刷日志。
        let truncated_count = collected.iter().filter(|p| p.name_truncated).count();
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
        ordered.sort_by_key(|p| {
            thumb_order_key(
                p.index.max(0) as usize / cells_per_row,
                screen_row_first,
                screen_row_last,
            )
        });
        // 缩略图诊断（ROVE_LOG=1）：窗口一变就打印一次「这一帧按什么顺序发请求」。
        // `vis` = 屏幕内行，`buf` = 屏幕外缓冲行。
        // 想确认「顶部时是不是先出第 3/4 行」，看这里的序号顺序即可。
        let diag_sig = (mode.clone(), pane_index, first, count, cells_per_row);
        if self.thumb_diag_sig.as_ref() != Some(&diag_sig) {
            self.thumb_diag_sig = Some(diag_sig);
            self.thumb_diag_left = 24;
            self.thumb_diag_got = 0;
            let head: Vec<String> = ordered
                .iter()
                .take(16)
                .map(|p| {
                    let i = p.index.max(0) as usize;
                    let tag = if p.index >= core_first as i32 && i < core_last {
                        "vis"
                    } else {
                        "buf"
                    };
                    format!("{i}:{tag}")
                })
                .collect();
            logv!(
                "[thumb] window mode={} pane={} first={} count={} rows={} cells={} rows_vis={} row_h={:.1} scroll={:.1} above={} screen=[{},{}) screen_rows=[{},{}) | cache={} pending={} skipped={} failed={} | order={:?}",
                mode,
                pane_index,
                first,
                count,
                ordered.len(),
                cells_per_row,
                visible_rows,
                row_h,
                scroll,
                effective_above,
                core_first,
                core_last,
                screen_row_first,
                screen_row_last,
                self.thumb_cache.len(),
                self.thumb_pending.len(),
                thumb_skipped().lock().map(|v| v.len()).unwrap_or(0),
                self.thumb_failed.len(),
                head,
            );
        }
        // 同排名称行数对齐：Slint 没有循环，扫不了「这一排最长几行」，只能在这里
        // 先按 `index / cells_per_row` 分组取最大行数，再逐行写回 `row_name_lines`。
        // 详情/列表视图 grid_lines 为空（= 1 行），整排恒为 1，等价于旧行为。
        let mut row_line_max: std::collections::HashMap<i32, i32> = std::collections::HashMap::new();
        for p in &ordered {
            let slot = p.index.max(0) / cells_per_row.max(1) as i32;
            let lines = (p.grid_lines.len() as i32).clamp(1, 3);
            let e = row_line_max.entry(slot).or_insert(1);
            if lines > *e {
                *e = lines;
            }
        }
        let mut stamps: Vec<RowStamp> = Vec::with_capacity(ordered.len());
        let rows: Vec<FileRow> = ordered
            .into_iter()
            .map(|p| {
                let (thumb, thumb_epoch) =
                    self.thumb_for(&p.thumb_path, p.modified, p.size, thumb_px);
                // 同一批条目每次重建都会重新格式化一遍（`format_local` 里有
                // Win32 的时区换算），走记忆表；指纹复用同一份结果，避免第二次格式化。
                let modified_text = self.time_text(p.modified);
                // 指纹只留判等需要的字段：文案类多克隆一次 SharedString
                // （短名是内联存储，长名只是引用计数 +1），比重建整行便宜得多。
                stamps.push(RowStamp {
                    index: p.index,
                    name: p.name.clone(),
                    path_text: p.path_text.clone(),
                    type_text: p.type_text.clone(),
                    size_text: p.size_text.clone(),
                    modified_text: modified_text.clone(),
                    is_dir: p.is_dir,
                    is_hidden: p.is_hidden,
                    selected: p.selected,
                    cut: p.cut,
                    truncated: p.name_truncated,
                    size: p.size,
                    modified: p.modified,
                    thumb: thumb_epoch,
                });
                FileRow {
                    index: p.index,
                    name: p.name,
                    grid_lines: if p.grid_lines.is_empty() {
                        // details / list / icons-small 不渲染 grid-lines：给空模型，
                        // 省掉每行一次三行切分（搜索高亮那几段在这里也没人读）。
                        // 注意用显式空表而不是 `ModelRc::default()`——后者是 null 模型
                        // （`Self(None)`），并非「空列表」。
                        ModelRc::new(VecModel::from(Vec::<GridLine>::new()))
                    } else {
                        ModelRc::new(VecModel::from(
                            p.grid_lines
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
                    dir_path: dir_path_text(&p.path_text).into(),
                    path: p.path_text,
                    is_dir: p.is_dir,
                    is_hidden: p.is_hidden,
                    size_text: p.size_text,
                    modified_text,
                    type_text: p.type_text,
                    icon_color: p.icon_color,
                    icon_text: p.icon_text,
                    selected: p.selected,
                    cut: p.cut,
                    thumb,
                    name_parts: ModelRc::new(VecModel::from(
                        p.name_parts
                            .into_iter()
                            .map(|(text, hl)| NamePart { text: text.into(), hl })
                            .collect::<Vec<_>>(),
                    )),
                    tip_parts: ModelRc::new(VecModel::from(
                        p.tip_parts
                            .into_iter()
                            .map(|(text, hl)| NamePart { text: text.into(), hl })
                            .collect::<Vec<_>>(),
                    )),
                    name_truncated: p.name_truncated,
                    name_line_ws: ModelRc::new(VecModel::from(p.name_line_ws)),
                    row_name_lines: *row_line_max
                        .get(&(p.index.max(0) / cells_per_row.max(1) as i32))
                        .unwrap_or(&1),
                }
            })
            .collect();
        RowBatch { rows, stamps, name_w_q }
    }

    /// 切换标签后的完整同步。
    fn sync_active_tab(&mut self) {
        // 类型下拉属于上一个标签的那个窗格，切标签就关掉。
        self.close_type_filter_menu();
        // 打开保护伞：切换标签时，Slint 会因内容高度变化对 Flickable 自动 Clamp，
        // 触发残留的 scroll-changed 事件；此刻置位可避免其把旧标签的巨大坐标
        // 写进新标签的 scroll_y（否则表现为切标签白屏）。
        self.syncing_tab = true;

        // 切换标签时取消挂起的延迟重命名：否则切走再切回原标签，原标签选择集可能
        // 仍满足「唯一选中该路径」，导致几百毫秒后跨标签冒出重命名输入框。
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
        self.visible_cache_right = None;
        self.sync_file_list();
        self.sync_sidebar_active();

        // 关闭保护伞
        self.syncing_tab = false;

        // 上一个标签如果因网络抖动/共享掉线读失败了，切回来的这一刻补一次：
        // 网络路径不在 mtime 轮询范围内（stat 会无限超时），没有任何定时器
        // 会替它重试，不补就会一直停在失败时的那一份内容上。
        self.retry_failed_panes();

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
        // 搜索框马上要被改写成新面板的值：先把旧面板挂起的搜索词结算掉。
        self.flush_pending_search();
        let mut request = None;
        // 关分屏会直接删掉右栏：挂在右栏上的类型下拉必须先收，
        // 否则遮罩层还盖在左栏上、看起来像卡死了。
        self.close_type_filter_menu();
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
        self.visible_cache_right = None;
        self.sync_split_state();
        self.ui.set_split_ratio_want(ratio);
        self.sync_nav();
        self.sync_breadcrumbs();
        self.sync_status();
        self.sync_file_list();
        self.sync_tabs();
        // 聚焦面板随分屏切换（关闭分屏时右栏整个消失）：侧栏高亮要跟着换，
        // 否则右栏停在 D:\ 时收回分屏，侧栏还亮着 D:\。
        self.sync_sidebar_active();
        // 打开/关闭分屏后左栏视口宽度变了：立刻按左栏内容重新自适应一次列宽，
        // 否则名称列仍按分屏前的宽视口估算，右栏挤进来后会出现横向滚动条。
        #[cfg(windows)]
        if !self.state.active().panes.first().map(|p| p.loading).unwrap_or(true) {
            refit_columns_now(self, 0);
        }
    }

    /// 关闭当前标签；当前标签正处于分屏时只退出分屏（与资源管理器一致）。
    ///
    /// 这是 Ctrl+W 的动作：分屏时按一次先收回第二栏，再按一次才关整个标签。
    /// 关掉最后一个标签 = 结束会话，与点标签上的 × 完全同一条路。
    fn close_tab_or_unsplit(&mut self) {
        let (kind, is_split, id) = {
            let tab = self.state.active();
            (tab.kind, tab.is_split, tab.id)
        };
        // 这一栏马上要没了：挂起的"延迟重命名"必须取消，否则它到点后会按
        // 「依然只有一个同路径选中项」的旧条件误进改名。
        self.cancel_rename_defer();
        if kind == crate::state::TabKind::Explorer && is_split {
            logv!("[tab] close_tab: 分屏中，先退出分屏");
            self.toggle_split();
            return;
        }
        self.close_tab_by_id(id);
    }

    /// 关闭指定标签（标签 × 与 Ctrl+W 共用这一个出口）。
    ///
    /// 关得掉就同步新标签；关不掉（只剩最后一个）则结束会话、仅保留守护进程，
    /// 并丢弃标签快照 —— 下次启动只开一个「此电脑」，不把这一轮捞回来。
    fn close_tab_by_id(&mut self, id: u32) {
        logv!("[tab] close tab id={} tabs={}", id, self.state.tabs.len());
        if self.state.close_tab(id) {
            self.sync_active_tab();
            return;
        }
        if self.state.tabs.len() == 1 {
            self.discard_tab_snapshot();
            schedule_destroy();
        }
    }

    /// 聚焦某一分屏面板（0 / 1），顶部地址栏与工具栏随之切换数据源。
    fn focus_pane(&mut self, index: usize) {
        // 类型下拉挂在某一栏上：焦点换栏后它列的是旧栏的类型，关掉。
        self.close_type_filter_menu();
        // 搜索框跟着聚焦面板走：切走之前先把旧面板挂起的搜索词结算掉。
        self.flush_pending_search();
        let active_tab = self.state.active_tab;
        let mut request = None;
        {
            let Some(tab) = self.state.tabs.get_mut(active_tab) else {
                return;
            };
            if index >= tab.panes.len() {
                return;
            }
            if tab.active_pane == index {
                // 状态已经在这一栏，但界面可能还停在旧值上（某条路径漏了同步）：
                // 用户点这一栏时顺手纠正一次，代价只有几次属性写。
                self.sync_split_state();
                self.sync_sidebar_active();
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
        self.visible_cache_right = None;
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
        // 两栏可能停在不同的盘 / 网络位置：侧栏高亮跟着聚焦面板走。
        self.sync_sidebar_active();
    }

    /// 把分屏状态同步到 UI（开关 / 当前聚焦面板）。
    fn sync_split_state(&self) {
        let tab = self.state.active();
        logv!(
            "[split] sync 聚焦栏={} is_split={}",
            tab.active_pane,
            tab.is_split
        );
        self.ui.set_is_split(tab.is_split);
        self.ui.set_active_pane(tab.active_pane as i32);
    }

    fn sync_sidebar_active(&mut self) {
        // 仅更新激活项（避免整表重建）。
        let active_id = match self.state.active().active().path.as_ref() {
            None => SharedString::default(),
            Some(path) => {
                let s = path.to_string_lossy().into_owned();
                // 盘符大小写不敏感：手输 / 从别处粘来的路径可能是 `c:\`，
                // 按 `starts_with("C:")` 比较会整条高亮都消失。
                let letter = s
                    .as_bytes()
                    .get(1)
                    .filter(|b| **b == b':')
                    .and_then(|_| s.chars().next())
                    .map(|c| c.to_ascii_uppercase());
                let drive = letter.and_then(|c| {
                    self.drives
                        .iter()
                        .find(|d| d.letter.to_ascii_uppercase() == c)
                });
                if let Some(drive) = drive {
                    format!("drive:{}", drive.letter).into()
                } else if s.starts_with("\\\\") {
                    // 网络位置按祖先匹配（见 `network_active_id`）。
                    let model = self.ui.get_sidebar_items();
                    let targets: Vec<String> = (0..model.row_count())
                        .filter_map(|i| model.row_data(i).map(|it| it.id.to_string()))
                        .filter_map(|id| id.strip_prefix("net:").map(str::to_string))
                        .collect();
                    network_active_id(&s, targets.iter().map(|t| t.as_str())).into()
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

/// UNC 路径该选中哪个侧栏网络位置：取「当前路径的祖先里最深的那一条」。
///
/// 以前是全等比较（`net:{当前路径}`），于是在 `\\nas\Video\bsf` 里侧栏整列
/// 都没有高亮，只有正好停在 `\\nas\Video` 时才亮一下——用户会以为"没进网络位置"。
/// 这里按**路径边界**做前缀匹配：`\\nas\Video` 是 `\\nas\Video\bsf` 的祖先，
/// 而 `\\nas\V` 不算（否则会把 `\\nas\Video` 误判成子目录）。
/// 多个目标都命中时取最深的那个（共享目录可以嵌套登记）。
fn network_active_id<'a, I>(path: &str, targets: I) -> String
where
    I: Iterator<Item = &'a str>,
{
    let cur = path.to_lowercase();
    let mut best: Option<(usize, String)> = None;
    for target in targets {
        if target.is_empty() || target.eq_ignore_ascii_case("none") {
            continue;
        }
        // 去掉登记值末尾的反斜杠：`\\nas\media\` 和 `\\nas\media` 必须等价，
        // 否则带尾巴的那条永远匹配不到子目录。
        let lower = target.to_lowercase();
        let lower = lower.trim_end_matches('\\');
        if lower.is_empty() {
            continue;
        }
        let hit = cur == lower || cur.starts_with(&format!("{lower}\\"));
        if hit && best.as_ref().is_none_or(|(len, _)| lower.len() > *len) {
            best = Some((lower.len(), format!("net:{target}")));
        }
    }
    best.map(|(_, id)| id).unwrap_or_default()
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

/// 是否「竖排图标网格」视图（中 / 大 / 超大图标）。
///
/// 只有这一档渲染 `FileRow.grid-lines`（竖排最多 3 行）：小图标是横排单行、
/// details/list 是单行 elide，都不读这个字段，也就不用算。
fn is_grid_icons_view(view_mode: &str) -> bool {
    matches!(view_mode, "icons-medium" | "icons-large" | "icons-xl")
}

/// 一行「内容指纹」：只包含行模型里其余字段的真正输入。
///
/// 增量更新时逐个比对：完全相同的行不必重写（Slint 那边连属性都不会重算）。
/// 不追求“冗余”，关键是**任何会让行外观变化的输入都在里面**：
/// - `index` / `name` / `path_text`：条目本身（重命名、排序、回收站原始路径）
/// - `type_text` / `size_text` / `modified_text`：文案（语言切换、目录体积统计陆续到达）
/// - `is_dir` / `is_hidden`：图标、角标、隐藏态（同名条目被删掉后换成别的类型）
/// - `selected` / `cut` / `truncated`：用户操作与省略号状态
/// - `size` / `modified`：条目自身的体积与 mtime
/// - `thumb`：缩略图版本号（0 = 还没图）
///
/// 漏掉任何一个输入，都会表现为「那一行的变化永远不刷新」。
#[derive(Clone, Debug, PartialEq)]
struct RowStamp {
    index: i32,
    name: SharedString,
    path_text: SharedString,
    type_text: SharedString,
    size_text: SharedString,
    modified_text: SharedString,
    is_dir: bool,
    is_hidden: bool,
    selected: bool,
    cut: bool,
    truncated: bool,
    size: u64,
    modified: Option<SystemTime>,
    thumb: u64,
}

/// 一屏行的「公共上下文」：这些一变，整屏内容都得重算，不做增量。
///
/// `filter` 单独放进来是因为搜索词变化时条目和名字可能完全没变（同一条命中的
/// 高亮分段不同），只看 `RowStamp` 会漏掉高亮更新。
#[derive(Clone, Debug, PartialEq)]
struct RowCtx {
    tab: usize,
    path: Option<PathBuf>,
    generation: u64,
    filter: String,
    view_mode: String,
    name_w_q: i32,
    total: usize,
    computer: bool,
    split: bool,
}

/// 上一帧写进行模型的内容（用于下一次增量更新）。
struct RowDiffState {
    ctx: RowCtx,
    stamps: Vec<RowStamp>,
}

/// `build_rows_for_pane` 的产物：行、指纹、以及算指纹用的名称区宽度。
struct RowBatch {
    rows: Vec<FileRow>,
    stamps: Vec<RowStamp>,
    name_w_q: i32,
}

impl RowBatch {
    fn empty() -> Self {
        Self { rows: Vec::new(), stamps: Vec::new(), name_w_q: 0 }
    }
}

/// 挑出最久未用的缓存项（LRU 淘汰用）。
///
/// 命中路径不再维护顺序表，所以「谁最久没用」只能淘汰时现算一次。
/// 抽成独立函数是为了能脱离 UI 单测。
fn lru_victim<K: Clone, V>(
    map: &std::collections::HashMap<K, V>,
    last_used: impl Fn(&V) -> u64,
) -> Option<K> {
    map.iter()
        .min_by_key(|(_, v)| last_used(v))
        .map(|(k, _)| k.clone())
}

/// 在显示列表里定位某个路径的行号。
///
/// 先用上一帧的结果验一次：那一行上还是同一个条目就直接返回（一次比较）。
/// 滚动不会改变显示顺序，所以常态下走的就是这条快路径；只有排序 / 过滤 /
/// 刷新真的把它挪走了才扫整个显示列表（大目录里那是上万次路径比较）。
fn expand_locate(
    pane: &crate::state::PaneState,
    want: &std::path::Path,
    cache: &mut Option<(PathBuf, i32)>,
) -> Option<i32> {
    if let Some((path, idx)) = cache.as_ref() {
        if *idx >= 0
            && path.as_path() == want
            && pane
                .display_entry(*idx as usize)
                .is_some_and(|e| e.path.as_path() == want)
        {
            return Some(*idx);
        }
    }
    let pos = (0..pane.display.len())
        .find(|&di| pane.display_entry(di).is_some_and(|e| e.path.as_path() == want))?
        as i32;
    *cache = Some((want.to_path_buf(), pos));
    Some(pos)
}

/// 决定这次要怎么写行模型。
///
/// 返回 `None` = 整表换掉（上下文变了 / 行数变了）；返回 `Some(下标)` = 只重写
/// 这几行。整表换掉会让 Slint 销毁重建每一个行元素（行内 TextInput 一起没），
/// 所以能不换就不换。
fn rows_to_patch(
    prev: Option<&RowDiffState>,
    ctx: &RowCtx,
    stamps: &[RowStamp],
    model_len: usize,
) -> Option<Vec<usize>> {
    let prev = prev?;
    if prev.ctx != *ctx || prev.stamps.len() != stamps.len() || model_len != stamps.len() {
        return None;
    }
    Some(
        stamps
            .iter()
            .enumerate()
            .filter(|(i, stamp)| prev.stamps[*i] != **stamp)
            .map(|(i, _)| i)
            .collect(),
    )
}

/// `build_rows_for_pane` 第一阶段的产物：与条目相关的文本 / 标志一次算好。
///
/// 以前这里直接把整个 `FileEntry` 克隆进元组，只为在第二阶段（要 `&mut self`
/// 去取缩略图）不必再借用 pane；一次重建 60 行就是 60 次「name/path/ext/
/// type_text/extra/original_path」全克隆，而其中 `ext` 和 `extra` 的行模型根本
/// 用不到。现在只保留真的会用到的值，并且文本全部在第一阶段（共享借用下）
/// 就算完，第二阶段只剩取缩略图。
struct PreRow {
    index: i32,
    /// 缩略图键用的真实路径（回收站条目是 Shell 条目 id，不是显示用的原始路径）。
    thumb_path: PathBuf,
    modified: Option<SystemTime>,
    size: u64,
    name: SharedString,
    /// 图标网格专用切行；其余视图留空（渲染器不读这个字段）。
    grid_lines: GridLines,
    path_text: SharedString,
    is_dir: bool,
    is_hidden: bool,
    size_text: SharedString,
    type_text: SharedString,
    icon_color: Color,
    icon_text: SharedString,
    selected: bool,
    cut: bool,
    name_parts: Vec<(String, bool)>,
    tip_parts: Vec<(String, bool)>,
    name_truncated: bool,
    /// 图标视图逐行文字宽度（与 `grid_lines` 一一对应）；其余视图只有一个元素。
    /// 用来把「再次点击名称进入重命名」限定在文字上：名称区是整格宽，
    /// 点在文字旁边的空白不该算「点击名称」。
    name_line_ws: Vec<f32>,
}

/// 完整路径去掉文件名后剩下的目录部分（图标视图路径浮层、详细视图行内路径行、
/// 回收站「路径」列都用它）。
///
/// 它不带文件名 —— 名字已经显示在格子/行上了，重复一遍没有意义。
fn dir_path_text(full: &str) -> String {
    let trimmed = full.trim_end_matches(['\\', '/']);
    if trimmed.len() <= 2 {
        // "C:" / "C:\" 这类没有父级，原样返回。
        return full.to_string();
    }
    match trimmed.rfind(['\\', '/']) {
        // 盘根下的条目："C:\b" 的目录是 "C:\"（只留到反斜杠）。
        Some(2) if trimmed.as_bytes().get(1) == Some(&b':') => trimmed[..3].to_string(),
        Some(i) if i >= 2 => {
            let dir = &trimmed[..i];
            // UNC 的共享根本身（\\服务器\共享）再往上只剩服务器名，那不是
            // 可用目录（"\\nas" 少一层）；这种情况原样返回，别造一个假路径。
            let unc_root = (dir.starts_with("\\\\") || dir.starts_with("//"))
                && !dir[2..].contains(['\\', '/']);
            if unc_root {
                full.to_string()
            } else {
                dir.to_string()
            }
        }
        _ => full.to_string(),
    }
}

/// 状态栏里的扫描量：一万以上换「万」，免得一串数字挤掉别的信息。
fn fmt_scan_count(n: usize) -> String {
    if n < 10_000 {
        n.to_string()
    } else {
        format!("{:.1}万", n as f64 / 10_000.0)
    }
}

/// 单个字符的估算像素宽（与 file_list.slint 的 11.5px 字号对齐）。
///
/// ASCII 不能再统一按 6px 估：大写与数字明显更宽（「EXID-Up&Down.mp4」这类
/// 名字会被低估，行宽超出容器后 HorizontalLayout 会压缩末段，把 `.mp4` 的
/// 尾部裁掉）。这里按字宽细分，宁可估宽一点也不裁字。
/// 可打印 ASCII（32..=126）在 **Microsoft YaHei UI 11.5px** 下的实测步进宽度。
///
/// 字体必须是 YaHei —— 那就是 `Theme.font-family`，界面所有文字都用它。这张表
/// 早先照 Segoe UI 测过，YaHei 的拉丁字形平均宽 8.7%（`a` 6.36 vs 5.85、
/// `m` 10.78 vs 9.91、`W` 11.70 vs 10.74），于是每行都算出「刚好放得下」、
/// 实际却超 8.7%，行尾被 `overflow: elide` 收成「…」。
///
/// 也不能按字符类拍脑袋（小写一律 5.9、大写一律 7.4）：同类里 `i`(3.06) 和
/// `m`(10.78) 差 3.5 倍，估错一个字就多折一行。
const ASCII_W: [f32; 95] = [
    3.40, 3.59, 5.01, 7.34, 6.74, 10.23, 10.01, 2.95, 3.84, 3.84,
    5.23, 8.53, 2.77, 4.98, 2.77, 4.91, 6.74, 6.74, 6.74, 6.74,
    6.74, 6.74, 6.74, 6.74, 6.74, 6.74, 2.77, 2.77, 8.53, 8.53,
    8.53, 5.55, 11.86, 8.09, 7.22, 7.69, 8.76, 6.32, 6.11, 8.55,
    8.89, 3.38, 4.55, 7.30, 5.90, 11.24, 9.35, 9.37, 7.04, 9.37,
    7.51, 6.64, 6.59, 8.59, 7.78, 11.70, 7.42, 6.94, 7.13, 3.84,
    4.78, 3.84, 8.53, 5.15, 3.39, 6.36, 7.34, 5.77, 7.36, 6.52,
    3.99, 7.36, 7.08, 3.06, 3.07, 6.26, 3.06, 10.78, 7.09, 7.31,
    7.34, 7.36, 4.39, 5.32, 4.28, 7.09, 6.04, 9.08, 5.83, 6.09,
    5.65, 3.84, 3.09, 3.84, 8.53,
];

fn est_char_w(c: char) -> f32 {
    // 与 file_list.slint 的 font-size 一致。
    const WIDE_W: f32 = 11.50;
    // 符号/emoji 按更保守的 14px 估：连续多个装饰字符（❀♡❤▶）各自回退到
    // 符号字体时，若仍按 12px 计就会累积超宽，整行被水平裁掉两端。
    const SYMBOL_W: f32 = 14.0;
    let code = c as u32;
    if (32..=126).contains(&code) {
        return ASCII_W[(code - 32) as usize];
    }
    if c.is_ascii() {
        return 3.6;
    }
    if is_wide_cjk(c) {
        WIDE_W
    } else {
        SYMBOL_W
    }
}

/// 一段（可能拆成多段的）文字的估算像素宽。
fn est_text_w(parts: &[(String, bool)]) -> f32 {
    parts
        .iter()
        .flat_map(|(t, _)| t.chars())
        .map(est_char_w)
        .sum()
}

/// 图标视图一格的名称切行。
///
/// `layout` 是按条目**原名**切出来的（带搜索高亮段）。显示名和原名一致时直接
/// 复用；「此电脑」里驱动器卷标会被翻译（`OS (C:)` → `本地磁盘 (C:)`），段文本
/// 就对不上显示名了，只能按翻译后的纯文本重排。
///
/// 曾经的写法是「只要是此电脑就按纯文本重排」——于是从「此电脑」发起全局搜索时，
/// 中/大/超大图标视图拿到的段全部没有高亮标记，命中词不高亮（详细信息 / 列表 /
/// 小图标走 `name-parts`，所以看着正常）。
fn grid_row_lines(
    layout: &GridLines,
    display_name: &str,
    raw_name: &str,
    width_px: f32,
) -> GridLines {
    if display_name == raw_name {
        layout.clone()
    } else {
        grid_display_lines(&[(display_name.to_string(), false)], width_px, 3)
    }
}

/// 把「带高亮分段的名字」按可用宽度预切成最多 `max_lines` 行，超出时末行以
/// `…` 收尾，返回逐行的片段列表。
///
/// 为什么按行拆成片段而不是交给 Slint：`Text` 不支持富文本，搜索命中高亮必须
/// 拆成多个 `Text`；而多行 `Text` 用 `overflow:elide` 时，文件名里的 `❀` 等
/// 装饰符号会触发字体回退多断一行，elide 按完整文本高度定位就会把首行整行
/// 裁掉。行数由 Rust 定死后，多行、首行完整、末行省略号、命中高亮可以同时成立。
fn grid_display_lines(parts: &[(String, bool)], width_px: f32, max_lines: usize) -> GridLines {
    // 留整整一个宽字符余量：估算与真实 shaping 总有偏差，留一格缓冲。
    let budget = (width_px - 11.5).max(11.5 * 2.0);
    let width_of = est_char_w;

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
    let limit = (budget - 11.5).max(11.5);
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

/// 逐行文字宽度（与 `grid_display_lines` 的切行一一对应）。
///
/// 用来把「再次点击名称进入重命名」限制在**每一行**的文字上：浮层框固定
/// 1.05 倍格宽、格子的名称区是整格宽，名字折行后后面几行往往短很多，
/// 只按「最宽一行」算，那些行右侧的空白就会被当成文字。
fn line_widths(lines: &GridLines) -> Vec<f32> {
    lines.iter().map(|line| est_text_w(line)).collect()
}

/// 空的「逐行文字宽」模型（`ModelRc::default()` 是 null 模型，不能当空表用）。
fn empty_lengths() -> ModelRc<f32> {
    ModelRc::new(VecModel::from(Vec::<f32>::new()))
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

/// 深搜批次去重：同一个路径只留一条。
///
/// 走查是边扫边读（每个目录的 `ReadDir` 迭代器跨批次保留），文件在扫描途中被
/// 移动或复制时，旧目录和新目录会各报一遍 —— 列表里就是两个一模一样的文件。
/// `seen` 里带上本地条目和已有深搜结果，跨批次也不会重复。
fn dedupe_deep_batch(
    items: Vec<crate::search::index::DeepItem>,
    seen: &mut std::collections::HashSet<PathBuf>,
) -> Vec<crate::search::index::DeepItem> {
    items
        .into_iter()
        .filter(|i| seen.insert(i.entry.path.clone()))
        .collect()
}

/// 面包屑「往上跳」之后该单选哪个条目：被点那一段的**后一段**。
///
/// 不能直接拿当前目录当孩子 —— 连跳两级时当前目录并不在被点目录的下一层，
/// 揭示永远落空。虚拟位置（回收站 `recycle://`、Shell 命名空间 `::{GUID}`）
/// 不在任何普通列表里，白等一轮没有意义，直接跳过。
fn crumb_child_of(segments: &[String], target: Option<&std::path::Path>) -> Option<PathBuf> {
    let idx = segments.iter().position(|seg| {
        let seg_path = if seg.is_empty() {
            None
        } else {
            Some(std::path::Path::new(seg.as_str()))
        };
        seg_path == target
    })?;
    let next = segments.get(idx + 1)?;
    if next.is_empty() || next.starts_with("::") || next == "recycle://" {
        return None;
    }
    Some(PathBuf::from(next))
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
    let pane = app.state.active().active_pane;
    app.expect_new_entries(paths, target, pane);
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
        // 第 5 项对应「路径」列（仅回收站显示）。双击它的分隔符就是「恢复默认宽」：
        // 站内每行来源目录各不相同，逐条算「最长路径」既贵又没有代表值。
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
    app.cur_col_widths[pane] = [w[0], w[1], w[2], w[3], w[4]];
    app.ui
        .invoke_apply_col_widths(pane as i32, w[0], w[1], w[2], w[3], w[4]);
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
        cur[4],
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

/// 与视口**相交**的行数（首尾被截一半的行也算一行）。
///
/// 不能用 `ceil(视口高/行高)`：滚动位置几乎从不对齐行高，那样会漏掉
/// 视口底部那半行，让它在缩略图调度里被当成缓冲区（排到整屏之后）。
fn visible_row_count(scroll: f32, list_height: f32, row_h: f32) -> usize {
    if row_h <= 0.0 {
        return 1;
    }
    let top = (scroll / row_h).floor();
    let bottom = ((scroll + list_height - 0.5) / row_h).ceil();
    (bottom - top).max(1.0) as usize
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

/// 缩略图请求顺序（键越小越先发），严格三段：
///
/// 1. 屏幕内的行（`[screen_row_first, screen_row_last)`）：**自上而下**，永远第一优先；
/// 2. 屏幕下方还没渲染的行：自**上**而下（越靠近屏幕越先）；
/// 3. 屏幕上方还没渲染的行：自**下**而上（越靠近屏幕越先）。
///
/// 为什么不按「离屏幕远近交替」发：那样上下两侧会同时出图，用户往下滚时看到
/// 的是「上下两头发力」的乱序；而列表的自然阅读方向是自上而下，下方先铺满才
/// 符合预期。上方留到最后，是因为它只在用户往回滚时才需要，那时会重新排序
/// 成「屏幕内」，优先级自然就上去了。
fn thumb_order_key(row: usize, screen_row_first: usize, screen_row_last: usize) -> (i64, i64) {
    if row >= screen_row_first && row < screen_row_last {
        (0, (row - screen_row_first) as i64)
    } else if row >= screen_row_last {
        (1, (row - screen_row_last) as i64)
    } else {
        (2, (screen_row_first - 1 - row) as i64)
    }
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

/// 驱动器图标的原版缤纷色板（Pulse 那套）。
///
/// 亮暗两个主题共用同一套颜色：深色下的收敛交给 `.slint`（透明度/底色分支），
/// Rust 侧只提供"这个盘是什么颜色"。占用 >90% 切换为告警红。

fn drive_pulse_color(index: usize, drive: &DriveInfo) -> Color {
    if drive.usage() > 0.90 {
        return Color::from_rgb_u8(0xE0, 0x52, 0x52); // 空间告急：红
    }
    const PALETTE: [(u8, u8, u8); 6] = [
        (0x25, 0x7A, 0xFD), // 鲜亮蓝 (C:)
        (0x10, 0xB9, 0x81), // 翠绿 (D:)
        (0xF5, 0x9E, 0x0B), // 琥珀橙 (E:)
        (0xA8, 0x55, 0xF7), // 紫罗兰 (F:)
        (0xEF, 0x44, 0x44), // 珊瑚红 (G:)
        (0x14, 0xB8, 0xA6), // 薄荷青 (O:)
    ];
    let (r, g, b) = PALETTE[index % PALETTE.len()];
    Color::from_rgb_u8(r, g, b)
}

/// 网络位置图标（侧栏 "N"）的青绿色。
const NETWORK_ICON_COLOR: (u8, u8, u8) = (0x14, 0xB8, 0xA6);

fn rgb(t: (u8, u8, u8)) -> Color {
    Color::from_rgb_u8(t.0, t.1, t.2)
}
// ---------------------------------------------------------------------------
// 主题
// ---------------------------------------------------------------------------

fn apply_theme(ui: &MainWindow, choice: ThemeChoice) {
    let effective = resolve_theme(choice);
    apply_theme_to(&ui.global::<Theme>(), effective);
    // 原生右键菜单是 Windows 自己画的：只有把进程偏好也切过去它才会变深。
    crate::sys::menu::set_dark(effective == ThemeChoice::Dark);
}

/// 把「跟随系统」落到具体主题上（其余原样返回）。
fn resolve_theme(choice: ThemeChoice) -> ThemeChoice {
    match choice {
        ThemeChoice::System => {
            if crate::sys::theme::system_dark() {
                ThemeChoice::Dark
            } else {
                ThemeChoice::Light
            }
        }
        other => other,
    }
}

/// 主题只往传入的 Theme 全局里写，这样主窗口和批量重命名窗口可以共用一套色值。
fn apply_theme_to(theme: &Theme, choice: ThemeChoice) {
    match choice {
        ThemeChoice::Light => {
            theme.set_bg_main(Color::from_rgb_u8(0xF3, 0xF4, 0xF5));
            theme.set_bg_sidebar(Color::from_rgb_u8(0xEB, 0xEC, 0xEE));
            theme.set_bg_list(Color::from_rgb_u8(0xFF, 0xFF, 0xFF));
            theme.set_bg_toolbar(Color::from_rgb_u8(0xF6, 0xF7, 0xF8));
            theme.set_bg_tabstrip(Color::from_rgb_u8(0xE7, 0xE8, 0xEB));
            theme.set_bg_hover(Color::from_rgb_u8(0x00, 0x00, 0x00).with_alpha(0.043));
            theme.set_bg_selected(Color::from_rgb_u8(0x0E, 0x8A, 0x4A).with_alpha(0.14));
            theme.set_bg_statusbar(Color::from_rgb_u8(0xF6, 0xF7, 0xF8));
            theme.set_bg_popup(Color::from_rgb_u8(0xFF, 0xFF, 0xFF));
            theme.set_bg_active_tab(Color::from_rgb_u8(0xFF, 0xFF, 0xFF));
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
        // 深色 = 黑曜石炭灰底色 + 柔紫强调（去除了 Dracula 的蓝紫脏调）。
        // 关键：次要/禁用文字改用中性冷灰，避免大面积小字发蓝发糊；
        // 底色层级压低色偏，输入框比面板深一档做出凹陷层次。
        ThemeChoice::Dark => {
            theme.set_bg_main(Color::from_rgb_u8(0x22, 0x22, 0x2A));
            theme.set_bg_sidebar(Color::from_rgb_u8(0x1B, 0x1B, 0x22));
            theme.set_bg_list(Color::from_rgb_u8(0x22, 0x22, 0x2A));
            theme.set_bg_toolbar(Color::from_rgb_u8(0x1E, 0x1E, 0x25));
            theme.set_bg_tabstrip(Color::from_rgb_u8(0x15, 0x15, 0x1A));
            theme.set_bg_hover(Color::from_rgb_u8(0xFF, 0xFF, 0xFF).with_alpha(0.05));
            theme.set_bg_selected(Color::from_rgb_u8(0xA7, 0x8B, 0xFA).with_alpha(0.14));
            theme.set_bg_statusbar(Color::from_rgb_u8(0x1B, 0x1B, 0x22));
            theme.set_bg_popup(Color::from_rgb_u8(0x27, 0x27, 0x31));
            theme.set_bg_active_tab(Color::from_rgb_u8(0x24, 0x24, 0x2D));
            theme.set_bg_input(Color::from_rgb_u8(0x16, 0x16, 0x1C));
            theme.set_bg_badge(Color::from_rgb_u8(0xFF, 0xFF, 0xFF).with_alpha(0.07));
            theme.set_text_primary(Color::from_rgb_u8(0xEF, 0xEF, 0xF4));
            theme.set_text_secondary(Color::from_rgb_u8(0x9A, 0x9A, 0xA6));
            theme.set_text_disabled(Color::from_rgb_u8(0x5E, 0x5E, 0x6B));
            theme.set_border(Color::from_rgb_u8(0x32, 0x32, 0x3E));
            theme.set_divider(Color::from_rgb_u8(0x29, 0x29, 0x33));
            theme.set_accent(Color::from_rgb_u8(0xA7, 0x8B, 0xFA));
            theme.set_accent_hover(Color::from_rgb_u8(0xB8, 0xA0, 0xFC));
            theme.set_accent_text(Color::from_rgb_u8(0x16, 0x15, 0x1E));
            theme.set_danger(Color::from_rgb_u8(0xF2, 0x6D, 0x6D));
            theme.set_dark(true);
        }
        // 已在 [`resolve_theme`] 里折成 Light / Dark。
        ThemeChoice::System => {}
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
    /// 这条路径「访问它可能要等网络」吗（UNC 或**映射的**网络驱动器）。
    ///
    /// 只看缓存里的驱动器列表，不访问磁盘。用来决定要不要在界面线程上 stat：
    /// 掉线的共享一次 `exists()` 就能把界面卡住好几秒。
    fn path_looks_remote(&self, path: &Path) -> bool {
        if crate::fs::dir::is_network_path_text(&path.to_string_lossy()) {
            return true;
        }
        let letter = match path.components().next() {
            Some(std::path::Component::Prefix(prefix)) => match prefix.kind() {
                std::path::Prefix::Disk(drive) => (drive as char).to_ascii_uppercase(),
                _ => return false,
            },
            _ => return false,
        };
        self.drives
            .iter()
            .any(|d| d.letter == letter && d.drive_type == crate::fs::drives::DriveType::Network)
    }

    /// 用**已加载的列表**判断某项是不是目录（不碰磁盘）。
    ///
    /// 右键菜单、固定到快速访问都要判断这个；原来直接 `is_dir()`，命中掉线的
    /// 网络盘会把界面线程卡住。返回 None = 这条路径不在当前列表里（比如从侧栏
    /// 发起的操作），调用方按「不知道」处理。
    fn loaded_is_dir(&self, path: &Path) -> Option<bool> {
        let pane = self.state.active().active();
        pane.entries
            .iter()
            .find(|e| e.path == path)
            .map(|e| e.is_dir)
            // 搜索结果里的条目不在 entries 而在 deep_entries，同样带着 is_dir。
            .or_else(|| {
                pane.deep_entries
                    .iter()
                    .find(|e| e.path == path)
                    .map(|e| e.is_dir)
            })
    }

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
        // 压缩关联项的标签里带 `{name}`（如 `解压到"{name}"\`）：随选中项实时替换。
        let action_label = |a: &crate::ops::actions::ActionEntry,
                            paths: &[std::path::PathBuf]| {
            let stem = paths
                .first()
                .and_then(|p| p.file_stem())
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
            let text = self.tr(&a.label).to_string();
            if text.contains("{name}") || text.contains("{stem}") {
                text.replace("{name}", &stem).replace("{stem}", &stem)
            } else {
                text
            }
        };

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
                    item(CMD_SELECT_ALL, self.tr("全选").as_str(), true),
                    item(CMD_INVERT_SELECTION, self.tr("反选").as_str(), true),
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
                    // 压缩关联两条由末尾那段单独渲染（要带分隔线压在最底下），
                    // 这里都排除掉，否则菜单里会出现两份。
                    .filter(|a| {
                        a.id != "builtin.reveal_explorer"
                            && !crate::ops::actions::is_archive_action(&a.id)
                    })
                    // 「是不是文件夹」用已加载的条目快照回答：右键时再去
                    // stat 网络路径会卡住界面线程（整个窗口未响应）。
                    .filter(|a| {
                        a.enabled
                            && a.matches_selection(paths, |p| self.loaded_is_dir(p))
                            && !dup(&a.id)
                            // 重命名一次只改一个条目（行内编辑）：多选时直接不出现，
                            // 比留一个点不动的灰项干净。
                            && !(a.id == "builtin.rename" && paths.len() != 1)
                    })
                {
                    if !groups.contains_key(&a.group) {
                        order.push(a.group.clone());
                    }
                    groups.entry(a.group.clone()).or_default().push(a);
                }

                for a in groups.remove("").unwrap_or_default() {
                    let mut label = action_label(a, paths);
                    let enabled = true;
                    // 终端：非目录时改标签。判断用已加载的条目表，
                    // 不在界面线程上 stat（网络路径的 is_dir() 会卡住）。
                    if a.id == "builtin.terminal" {
                        let first_is_dir = paths
                            .first()
                            .and_then(|p| self.loaded_is_dir(p))
                            .unwrap_or(false);
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
                        enabled,
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
                                label: action_label(a, paths),
                                enabled: true,
                                checked: false,
                            })
                            .collect(),
                    });
                }

                // 压缩软件关联项：永远带一条分隔线压在最底下。
                // 顺序按 ARCHIVE_IDS 固定（加压缩包 → 解压文件 → 解压到当前文件夹
                // → 解压到"{name}"\），与探测到的是哪一款无关；某一项不适用时
                // 由 matches_selection 挡掉。
                let mut packed: Vec<MenuNode> = Vec::new();
                for id in crate::ops::actions::ARCHIVE_IDS {
                    let Some(a) = self.actions.iter().find(|a| a.id == id) else {
                        continue;
                    };
                    if !a.enabled || !a.matches_selection(paths, |p| self.loaded_is_dir(p)) {
                        continue;
                    }
                    packed.push(MenuNode::Item {
                        key: format!("act:{}", a.id),
                        label: action_label(a, paths),
                        enabled: true,
                        checked: false,
                    });
                }
                if !packed.is_empty() {
                    nodes.push(MenuNode::Separator);
                    nodes.extend(packed);
                }
                nodes
            }
            MenuScope::Background => {
                // 实时读系统剪贴板：资源管理器等外部程序随时可能覆盖它，
                // 不能用进程内的陈旧缓存判断文字与可用性。
                let (paste_paths, paste_move) = self.paste_state();
                // 这几条只对真实文件夹有意义；虚拟根（此电脑 / 回收站）拿不到
                // path，点了只会提示「请先进入一个文件夹」。
                let mut nodes: Vec<MenuNode> = Vec::new();
                // 撤消 / 重做摆最顶上：栈里有东西才出现，撤消永远在恢复上面。
                // 回收站走的是上面的专用菜单，拿不到这里。
                if let Some(kind) = self.undo.peek_undo() {
                    nodes.push(item("undo", self.tr(kind.undo_label()).as_str(), true));
                }
                if let Some(kind) = self.undo.peek_redo() {
                    nodes.push(item("redo", self.tr(kind.redo_label()).as_str(), true));
                }
                if !nodes.is_empty() {
                    nodes.push(MenuNode::Separator);
                }
                nodes.extend([
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
                    item(CMD_NEW_TXT, self.tr("新建文本文档").as_str(), true),
                    MenuNode::Separator,
                    item(CMD_REFRESH, self.tr("刷新").as_str(), true),
                    item(CMD_SELECT_ALL, self.tr("全选").as_str(), true),
                    item(CMD_INVERT_SELECTION, self.tr("反选").as_str(), true),
                    // 当前文件夹自己的属性页（选中项的那条在 Selection 菜单里）。
                    item(CMD_BG_PROPERTIES, self.tr("属性").as_str(), true),
                    item(CMD_MANAGE_ACTIONS, self.tr("管理右键动作").as_str(), true),
                    item(CMD_TERMINAL, self.tr("在此处打开终端").as_str(), true),
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
                ]);
                nodes
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
        // 搜索结果与回收站一样有「路径」这一列（只在详细信息视图里显示），
        // 所以排序菜单里也要能选它。
        let is_recycle = self.state.active().active().path.as_deref()
            == Some(crate::fs::drives::recycle_virtual_path().as_path());
        let searching = !self.state.active().active().filter.is_empty();
        use crate::sys::menu::MenuNode;
        let item = |key: &str, label: &str, checked: bool| MenuNode::Item {
            key: format!("sort:{key}"),
            label: self.tr(label).to_string(),
            enabled: true,
            checked,
        };
        // 首项跟表头一致：「此电脑」平时是「盘符」，进入搜索后变成「名称」。
        let mut nodes = vec![if is_computer && !searching {
            item("drive", "盘符", sort.key == SortKey::Drive)
        } else {
            item("name", "名称", sort.key == SortKey::Name)
        }];
        // 「路径」列：回收站与搜索结果都有，紧跟在名称后面（与表头列顺序一致）。
        if is_recycle || searching {
            nodes.push(item("path", "路径", sort.key == SortKey::Path));
        }
        // 其他列只列该视图真实存在的（此电脑没有修改时间列）。
        if !is_computer {
            nodes.push(item("modified", "修改日期", sort.key == SortKey::Modified));
        }
        nodes.push(item("type", "类型", sort.key == SortKey::Type));
        // 「此电脑」那一列叫「剩余」（磁盘容量），文案跟表头保持一致；
        // 搜索结果里没有这一列，菜单也就不列它。
        if !(is_computer && searching) {
            nodes.push(item(
                "size",
                if is_computer { "剩余" } else { "大小" },
                sort.key == SortKey::Size,
            ));
        }
        nodes.push(MenuNode::Separator);
        nodes.push(item("asc", "递增", sort.ascending));
        nodes.push(item("desc", "递减", !sort.ascending));
        nodes
    }

    /// 打开右键菜单：Shell 菜单（系统项）+ Rove 自己的项。
    fn open_menu(&mut self, target: MenuScope) {
        #[cfg(windows)]
        {
            match &target {
                MenuScope::Selection(paths) => logln!(
                    "[menu] open selection n={} first={:?} pane={}",
                    paths.len(),
                    paths.first(),
                    self.state.active().active_pane
                ),
                MenuScope::Background => logln!(
                    "[menu] open background pane={} path={:?}",
                    self.state.active().active_pane,
                    self.state.active().active().path
                ),
            }
            // 「系统菜单」关着：一次都不去请求 Shell 菜单（也就不给外壳扩展
            // 执行的机会），右键只显示 Rove 自己的内建项。
            let mut native_target = if self.config.system_menu {
                self.shell_menu_target(&target)
            } else {
                crate::sys::menu::Target::AppOnly
            };
            // 系统菜单刚把菜单线程拖死过：这次只弹 Rove 自己的项。
            // 卡死的源头是第三方外壳扩展（一直在 `QueryContextMenu` 里不出来），
            // 不请它上场就不会再卡，代价是暂时没有「打开方式 / 发送到」这些系统项。
            if crate::sys::menu::shell_unhealthy() && !matches!(native_target, crate::sys::menu::Target::AppOnly) {
                logln!("[menu] 系统菜单刚卡过，本次只显示 Rove 自己的菜单项");
                native_target = crate::sys::menu::Target::AppOnly;
            }
            let shell_present = !matches!(native_target, crate::sys::menu::Target::AppOnly);
            let nodes = self.build_app_nodes(&target, shell_present);
            // 系统菜单卡死时要立刻退成「只含 Rove 自己项」的那一套。两套都在
            // UI 线程一次算好交过去 —— 退回是菜单模块自己做的（1 秒看门狗），
            // 那时再来问 UI 线程就太晚了。
            let fallback = if shell_present {
                self.build_app_nodes(&target, false)
            } else {
                Vec::new()
            };
            // Shift + 右键 = 扩展动词（「以管理员身份运行」这类）。
            let extended = key_down(windows_sys::Win32::UI::Input::KeyboardAndMouse::VK_SHIFT as i32);
            if !self.open_native_menu(native_target.clone(), nodes, fallback, extended) {
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
        fallback: Vec<crate::sys::menu::MenuNode>,
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
        crate::sys::menu::open(target, nodes, fallback, options, owner, sink)
    }

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

    /// 逐个包解压（每个包一个子进程：坏包弹自己的错，不影响别的包）。
    fn run_archive_extract(
        &mut self,
        archives: Vec<std::path::PathBuf>,
        dest: Option<String>,
        entry: &crate::ops::actions::ActionEntry,
        skipped: usize,
    ) {
        let mut result = Ok(());
        for one in &archives {
            let one = std::slice::from_ref(one);
            result = match &dest {
                Some(d) => crate::ops::actions::run_action_into_dir(entry, one, d),
                None => crate::ops::actions::run_action(entry, one),
            };
            if result.is_err() {
                break;
            }
        }
        match result {
            Ok(()) => {
                // 跳过后续分卷时说一句，别让用户以为少解了东西。
                if skipped > 0 {
                    self.set_status_job(
                        self.trf("已跳过 {} 个后续分卷", &[skipped.to_string()]),
                    );
                } else {
                    self.set_status_job(format!(
                        "{}：{}",
                        self.tr("已执行"),
                        self.tr(&entry.label)
                    ));
                }
                self.reload_active();
            }
            Err(e) => self.set_status_job(self.trf("执行失败：{}", &[e.to_string()])),
        }
    }

    /// 保存当前 actions 并重新加载（合并内置动作、校验自定义命令）。
    fn save_actions_and_reload(&mut self) {
        if let Err(e) = crate::ops::actions::save(&self.actions) {
            self.set_status_job(e);
            return;
        }
        let raw = crate::ops::actions::load_or_create(Some(self.preferred_archive_tool()));
        self.actions = crate::ops::actions::heal_full(&raw);
    }

    /// 设置页选的默认压缩软件（缺省 WinRAR）。
    fn preferred_archive_tool(&self) -> crate::sys::archive::ToolKind {
        crate::sys::archive::ToolKind::from_id(&self.config.archive_tool)
            .unwrap_or(crate::sys::archive::ToolKind::WinRar)
    }

    /// 默认压缩软件在设置页下拉框里的下标（0 WinRAR / 1 7-Zip / 2 Bandizip）。
    fn archive_tool_index(&self) -> i32 {
        match self.config.archive_tool.as_str() {
            "7zip" => 1,
            "bandizip" => 2,
            _ => 0,
        }
    }
    /// 重新探测压缩软件（设置页「更新全部」= 整组，`None` 表示全部项）。
    fn rescan_archive(&mut self) {
        self.rescan_archive_for(None);
    }

    /// 重新探测压缩软件。`only = Some(id)` 时扫完只更新那一项（单条「更新」）。
    fn rescan_archive_for(&mut self, only: Option<String>) {
        self.set_status_job(self.tr("正在查找压缩软件…"));
        self.archive_rescan_id = only;
        match crate::sys::archive::scan_async(Some(self.preferred_archive_tool())) {
            Some(gen) => {
                self.archive_scan_gen = gen;
                self.archive_scan_pending = true;
            }
            None => {
                self.archive_scan_pending = false;
                self.archive_rescan_id = None;
                self.set_status_job(
                    self.tr("未检测到压缩软件（WinRAR / 7-Zip / Bandizip）").to_string(),
                );
            }
        }
    }

    /// 把扫描结果并回配置：刷新程序/参数，并同步开关。
    ///
    /// 找到了 → 保留用户原来的开关（他关掉的不要擅自打开）；
    /// 没找到 → 一律关掉，因为程序路径已经不可用了。
    fn apply_archive_tool(&mut self, tool: Option<crate::sys::archive::Tool>) {
        // 单条「更新」只并回那一项；「更新全部」才整组覆盖。
        let only = self.archive_rescan_id.take();
        let fresh = crate::ops::actions::archive_actions(tool.as_ref());
        if tool.is_none() {
            // 三款软件都被卸载了：所有压缩项一起关掉，别留指向已删除 exe 的开关。
            crate::ops::actions::disable_all_archive_entries(&mut self.actions);
        }
        crate::ops::actions::merge_archive_entries(&mut self.actions, fresh, only.as_deref());
        self.save_actions_and_reload();
        self.sync_settings();
        let text = match tool {
            Some(t) => self.trf("已更新压缩软件关联：{}", &[t.kind.name().to_string()]),
            None => self
                .tr("未检测到压缩软件（WinRAR / 7-Zip / Bandizip）")
                .to_string(),
        };
        self.set_status_job(text);
    }

    /// 菜单健康检查：看门狗。
    ///
    /// 必须在 UI 线程的周期回调里（40ms 心跳）。菜单跑在子进程里，所以这里
    /// 不用再操心「进程被扩展拖坏」—— 扩展代码从来没在主进程里跑过。
    /// 唯一要管的是「1 秒还没弹出来」：杀掉那个子进程，改弹只含 Rove 自己
    /// 项的菜单（见 `sys::menu::tick`）。
    fn check_menu_health(&mut self) {
        crate::sys::menu::tick();
    }

    /// `ROVE_LOG=2` 内存明细（节流 2 秒）。
    ///
    /// 分项打印常驻占用，用来对比优化前后到底是哪一块变小了——任务管理器
    /// 只给一个总数，看不出索引 / 缩略图 / 标签页各占多少。
    ///
    /// 三个采样点：启动完成、每次进入/刷新目录、空闲时每 15 秒。
    /// 翻目录时最多每 2 秒一条 —— 静默级别下不产生任何开销，看内存时才开。
    fn log_memory_snapshot(&mut self) {
        if !log_enabled(2) {
            return;
        }
        let now = std::time::Instant::now();
        if self
            .last_mem_log
            .is_some_and(|last| now.duration_since(last) < Duration::from_secs(2))
        {
            return;
        }
        self.last_mem_log = Some(now);
        let where_txt = self
            .state
            .tabs
            .get(self.state.active_tab)
            .and_then(|t| t.panes.get(t.active_pane))
            .and_then(|p| p.path.as_ref())
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "—".to_string());

        let (idx_entries, idx_heap, idx_ready) = crate::search::index::try_shared()
            .and_then(|i| {
                i.try_read()
                    .ok()
                    .map(|g| (g.len(), g.heap_bytes, g.ready))
            })
            .unwrap_or((0, 0, false));
        let disk = crate::search::index::cache_file_size().unwrap_or(0);
        let mut tab_count = 0usize;
        let mut pane_count = 0usize;
        let mut entry_total = 0usize;
        let mut pinyin_total = 0usize;
        let mut deep_total = 0usize;
        let mut history_total = 0usize;
        let mut memo_total = 0usize;
        for tab in &self.state.tabs {
            tab_count += 1;
            for pane in &tab.panes {
                pane_count += 1;
                entry_total += pane.entries.len();
                pinyin_total += pane.pinyin_cache.len();
                deep_total += pane.deep_entries.len();
                history_total += pane.history.len();
                memo_total += pane.path_scroll_memo.len();
            }
        }
        let (ws, priv_bytes) = crate::sys::mem::usage().unwrap_or((0, 0));
        crate::logv!(
            "[mem] {where_txt} | 进程 工作集 {:.1}MB / 私有 {:.1}MB | 索引 {} 条 映射 {:.1}MB（就绪={} 磁盘 {:.1}MB） | 缩略图 {} 张 {:.1}MB（负缓存 {}） | 标签 {} 个 / 窗格 {}，条目共 {} 拼音共 {} 深度 {} | 滚动记忆 {} 历史 {} 页",
            ws as f64 / (1024.0 * 1024.0),
            priv_bytes as f64 / (1024.0 * 1024.0),
            idx_entries,
            idx_heap as f64 / (1024.0 * 1024.0),
            idx_ready,
            disk as f64 / (1024.0 * 1024.0),
            self.thumb_cache.len(),
            self.thumb_cache_bytes as f64 / (1024.0 * 1024.0),
            self.thumb_failed.len(),
            tab_count,
            pane_count,
            entry_total,
            pinyin_total,
            deep_total,
            memo_total,
            history_total,
        );
    }

    /// 搜索索引看板指标：优先 `try_read`，绝不在 UI 线程阻塞等待构建写锁。
    ///
    /// `with_drives` 为 false 时只刷新高频指标（条目数 / 大小 / 状态），
    /// 避免周期刷新重建驱动器模型打断悬停高亮与开关动画。
    fn sync_search_stats(&self, with_drives: bool) {
        let snapshot = crate::search::index::try_shared().and_then(|i| {
            i.try_read()
                .ok()
                .map(|g| (g.ready, g.len(), g.built_at, g.heap_bytes))
        });
        let (is_ready, entry_count, built_at, heap_bytes) = snapshot.unwrap_or((false, 0, None, 0));

        self.ui.set_search_ready(is_ready);
        let status_text = if is_ready {
            self.tr("索引已就绪")
        } else if crate::search::index::is_building() {
            self.tr("正在构建索引...")
        } else {
            self.tr("索引未构建")
        };
        self.ui.set_search_status_text(status_text);
        self.ui.set_search_items_count(format!("{entry_count}").into());
        // 这张卡显示的是**索引文件大小**：索引现在是映射在磁盘上的定长记录，
        // 真正驻留多少页由系统按需决定（远小于文件）。
        let real_bytes = if heap_bytes > 0 {
            heap_bytes as u64
        } else {
            (entry_count * INDEX_ENTRY_BYTES) as u64
        };
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
        self.ui.set_explorer_takeover(self.config.explorer_takeover);
        self.ui.set_system_menu(self.config.system_menu);
        self.ui.set_archive_tool_index(self.archive_tool_index());
        // 开机自启动没有配置项：注册表 Run 键就是唯一真源，进设置页按实况显示，
        // 这样用户在任务管理器里停用/删掉之后，开关不会骗人。
        self.ui.set_autostart(crate::sys::autostart::enabled());

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
            let groups: Vec<SettingsShortcutGroup> =
                ["文件操作", "导航与查看", "编辑与选择", "标签页"]
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
            return;
        }

        let mut groups_map: std::collections::BTreeMap<String, Vec<SettingsActionItem>> =
            std::collections::BTreeMap::new();
        let mut order: Vec<String> = Vec::new();
        let mut packed: Vec<SettingsActionItem> = Vec::new();
        for a in &self.actions {
            if crate::ops::actions::is_archive_action(&a.id) {
                continue;
            }
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
                special: false,
            };
            groups_map.entry(g).or_default().push(item);
        }
        // 压缩关联项在菜单里是一级项，但设置页单独成一组（组头有「更新全部」，
        // 每行的删除按钮换成「更新」）。顺序固定，与文件里的先后无关。
        for id in crate::ops::actions::ARCHIVE_IDS {
            let Some(a) = self.actions.iter().find(|a| a.id == id) else {
                continue;
            };
            packed.push(SettingsActionItem {
                id: a.id.clone().into(),
                // 显示当前语言的模板（`{name}` 原样留着，用户改的就是这段）。
                label: self.tr(&a.label).into(),
                target: a.target.clone().into(),
                extensions: a.extensions.join(", ").into(),
                program: a.program.clone().into(),
                enabled: a.enabled,
                builtin: a.builtin,
                special: true,
            });
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
                    rescan_all: false,
                    items: ModelRc::new(VecModel::from(groups_map.remove(&g).unwrap_or_default())),
                }
            })
            .collect();
        let mut groups = groups;
        if !packed.is_empty() {
            // 压缩关联组压在最底下，组头带「更新全部」。
            groups.push(SettingsActionGroup {
                name: self.tr(crate::ops::actions::ARCHIVE_GROUP).into(),
                expanded: true,
                rescan_all: true,
                items: ModelRc::new(VecModel::from(packed)),
            });
        }
        let groups_model = VecModel::from(groups);
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
            if !self.open_native_menu(crate::sys::menu::Target::AppOnly, nodes, Vec::new(), false) {
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
        // 地址栏命令：输入 `cmd` 回车 = 在当前目录打开终端
        // （与右键菜单「在此处打开终端」同一条实现）。
        if is_address_bar_command(trimmed) {
            self.open_terminal_here();
            return;
        }
        // 虚拟根：地址栏在“此电脑”下会以标签作为初始文本，回车视为回到此电脑
        if trimmed == "此电脑" || trimmed.eq_ignore_ascii_case("this pc") {
            self.navigate_active(None);
            return;
        }

        // 核心修复：使用 expand_path 展开环境变量和 ~
        let p = Self::expand_path(trimmed);

        // 校验放到后台线程：`exists()` 命中掉线的网络盘 / WSL 共享能卡好几秒，
        // 在界面线程上就是整窗未响应（和启动终端同一个坑）。
        // `\\host` 不是文件系统路径（GetFileAttributes 打不开），但能列出共享，
        // 所以不能拿 exists() 卡它 —— 这条规则同样在后台判断。
        self.path_probe_seq += 1;
        let seq = self.path_probe_seq;
        let label = trimmed.to_string();
        let from = self.state.active().active().path.clone();
        let tx = self.path_probe_tx.clone();
        let probe_path = p.clone();
        let spawned = std::thread::Builder::new()
            .name("rove-path-probe".into())
            .spawn(move || {
                let ok = probe_path.exists() || crate::fs::dir::is_unc_server_root(&probe_path);
                let outcome = if ok { Ok(probe_path) } else { Err(()) };
                // 接收端已销毁（退出中）时忽略即可。
                let _ = tx.send(PathProbe {
                    seq,
                    label,
                    from,
                    outcome,
                });
            });
        if spawned.is_err() {
            // 极端情况：线程都建不起来。退回同步校验 —— 宁可慢一点，
            // 也不能让回车彻底没反应。
            logln!("!! 路径校验线程创建失败，退回同步校验");
            if p.exists() || crate::fs::dir::is_unc_server_root(&p) {
                self.navigate_active(Some(p));
            } else {
                self.set_status_job(self.trf("路径不存在：{}", &[trimmed.to_string()]));
            }
        }
    }

    /// 在当前聚焦面板的目录里打开终端（地址栏 `cmd` 用）。
    fn open_terminal_here(&mut self) {
        let dir = self.state.active().active().path.clone();
        self.open_terminal_at(dir);
    }

    /// 在 `dir` 里开终端（右键菜单 / 地址栏 `cmd` 共用）。
    ///
    /// `dir` 允许是**文件**（右键选中文件时开在它所在的目录），判断交给后台
    /// 线程做 —— 网络盘 / WSL 共享上的 `is_dir()` 能卡住，不能在界面线程上问。
    ///
    /// 此电脑（无路径）与回收站这类虚拟目录没有可打开的目录，只给一句状态
    /// 提示，绝不把 `recycle://` 丢给系统去启动进程（那只会得到 os error 267）。
    fn open_terminal_at(&mut self, dir: Option<PathBuf>) {
        let is_virtual = dir
            .as_deref()
            .is_some_and(|p| p == fs::drives::recycle_virtual_path().as_path());
        match dir {
            Some(dir) if !is_virtual => ops::open_terminal(&dir),
            _ => self.set_status_job(self.tr("当前视图没有可打开的目录")),
        }
    }

    /// 侧栏各行的 (顶 y, 行高)（内容坐标）。必须与 `ui/sidebar.slint` 完全一致：
    /// header 30 / drive 46 / 其余 32，行间距 2，顶部内边距 8。
    /// 两侧不同步时，拖拽指示线与落点判断会按驱动器数量累积偏差。
    fn sidebar_row_metrics(&self) -> Vec<(f32, f32)> {
        let model = self.ui.get_sidebar_items();
        let n = model.row_count();
        if n == 0 {
            return Vec::new();
        }
        // 优先用 Slint 每行**实际上报**的几何（窗口坐标，含 Flickable 滚动）。
        // 行高和间距只写在 `.slint` 里，Rust 这边再抄一份常量迟早会跟丢——本次
        // 「快速访问拖动指示线错位」就是布局改成 34/48 而这里还写着 32/46，
        // 每行累加 2px 漂移出来的。换算回内容坐标（第 0 行 = padding-top 8px）
        // 之后与滚动无关，落点判定和指示线都对得上。
        let ids: Vec<String> = (0..n)
            .filter_map(|i| model.row_data(i).map(|r| r.id.to_string()))
            .collect();
        if ids.len() == n {
            let rects = self.sidebar_rects.borrow();
            if let Some(&(_, base_y, _, _)) = rects.get(ids[0].as_str()) {
                let mut v = Vec::with_capacity(n);
                for id in &ids {
                    match rects.get(id.as_str()) {
                        Some(&(_, y, _, h)) if h > 0.0 => v.push((y - base_y + 8.0, h)),
                        // 有任意一行还没上报就先整体退回常量，避免半新半旧混着算。
                        _ => {
                            v.clear();
                            break;
                        }
                    }
                }
                if v.len() == n {
                    return v;
                }
            }
        }
        // 兜底（首帧几何还没上报时）：与 `ui/sidebar.slint` 里的行高保持一致。
        let mut metrics = Vec::with_capacity(n);
        let mut y = 8.0_f32;
        for i in 0..n {
            let h = model
                .row_data(i)
                .map(|it| match it.kind.as_str() {
                    "header" => 30.0,
                    "drive" => 48.0,
                    _ => 34.0,
                })
                .unwrap_or(34.0);
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
        self.open_native_menu(crate::sys::menu::Target::AppOnly, nodes, Vec::new(), false);
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
        self.open_native_menu(crate::sys::menu::Target::AppOnly, nodes, Vec::new(), false);
    }

    #[cfg(not(windows))]
    fn show_quick_item_menu(&mut self, _key: &str) {}

    /// 标签页右键菜单：关闭全部 / 关闭其他 / 左侧 / 右侧。
    ///
    /// 没有可关的项时对应菜单项置灰（左侧在第一页时、右侧在最后一页时），
    /// 避免点了没反应。
    #[cfg(windows)]
    fn show_tab_context_menu(&mut self, id: u32) {
        let Some(index) = self.state.tab_by_id(id) else {
            return;
        };
        let total = self.state.tabs.len();
        let item = |key: String, label: String, enabled: bool| crate::sys::menu::MenuNode::Item {
            key,
            label,
            enabled,
            checked: false,
        };
        let nodes = vec![
            item(
                format!("tab:close-all:{id}"),
                self.tr("关闭全部").to_string(),
                true,
            ),
            item(
                format!("tab:close-others:{id}"),
                self.tr("关闭其他").to_string(),
                total > 1,
            ),
            item(
                format!("tab:close-left:{id}"),
                self.tr("关闭左侧").to_string(),
                index > 0,
            ),
            item(
                format!("tab:close-right:{id}"),
                self.tr("关闭右侧").to_string(),
                index + 1 < total,
            ),
        ];
        self.open_native_menu(crate::sys::menu::Target::AppOnly, nodes, Vec::new(), false);
    }

    #[cfg(not(windows))]
    fn show_tab_context_menu(&mut self, _id: u32) {}

    /// 执行标签栏右键菜单命令：`tab:close-{all|others|left|right}:<锚点 id>`。
    fn run_tab_menu_action(&mut self, id: &str) -> bool {
        use crate::state::TabCloseScope;
        let Some(rest) = id.strip_prefix("tab:close-") else {
            return false;
        };
        if let Some(v) = rest.strip_prefix("all:") {
            let Ok(anchor) = v.parse::<u32>() else {
                return false;
            };
            self.close_all_tabs(anchor);
            return true;
        }
        let (scope, anchor) = if let Some(v) = rest.strip_prefix("others:") {
            (TabCloseScope::Others, v)
        } else if let Some(v) = rest.strip_prefix("left:") {
            (TabCloseScope::Left, v)
        } else if let Some(v) = rest.strip_prefix("right:") {
            (TabCloseScope::Right, v)
        } else {
            return false;
        };
        let Ok(anchor) = anchor.parse::<u32>() else {
            return false;
        };
        let closed = self.state.close_tabs_scope(anchor, scope);
        if closed > 0 {
            logv!("[tab] 批量关闭 {scope:?} 锚点={anchor} 关闭 {closed} 个");
            self.sync_active_tab();
        }
        true
    }

    /// 关闭全部标签：等价于「一个不留地把窗口关掉」。
    ///
    /// 与点窗口的 X 的区别只体现在下次启动：这等于用户明确表示「这一轮的
    /// 标签不要了」，所以顺手把「上次的标签页」快照清空 —— 否则选了
    /// 「启动时打开上次的标签页」，下次又会把这些标签原样拉回来。
    fn close_all_tabs(&mut self, _anchor: u32) {
        logv!("[tab] 关闭全部 {} 个标签", self.state.tabs.len());
        self.discard_tab_snapshot();
        // 会话销毁（窗口关闭、守护进程常驻）。借用在调用方手里，不能直接销毁，
        // 交给事件循环排定。
        schedule_destroy();
    }

    /// 记下「不要把当前标签留到下次启动」并立刻落盘（不依赖销毁流程的顺序）。
    fn discard_tab_snapshot(&mut self) {
        self.tabs_discarded = true;
        if self.config.last_tabs.is_empty() {
            return;
        }
        self.config.last_tabs.clear();
        self.config.save();
    }

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
            // 只认「明确是文件」的：判断来自已加载的列表，网络路径不在这 stat。
            if self.loaded_is_dir(p) == Some(false)
                || *p == fs::drives::recycle_virtual_path()
            {
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
            let label = crate::fs::dir::leaf_name(&p);
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

    /// 读失败的面板过 3 秒自动重试一次（最多 3 次）。
    ///
    /// 网络共享慎一下是最常见的失败原因，用户没理由为它手动 F5。
    /// 重试仍失败就把标记留着，等切标签时由 retry_failed_panes 再补。
    fn maybe_retry_failed_listing(&mut self) {
        let Some((tab_index, pane, at, tries)) = self.list_retry else {
            return;
        };
        if at.elapsed() < Duration::from_millis(3000) {
            return;
        }
        self.list_retry = None;
        if tries > 3 {
            logal!(
                "!! [list] giving up auto retry after {tries} attempts: tab={tab_index} pane={pane}"
            );
            return;
        }
        let still_failed = self
            .state
            .tabs
            .get(tab_index)
            .and_then(|t| t.panes.get(pane))
            .is_some_and(|p| p.list_failed);
        if !still_failed {
            return;
        }
        self.state.tabs[tab_index].panes[pane].list_failed = false;
        logal!("[list] auto retry failed read (attempt {tries}): tab={tab_index} pane={pane}");
        // 登记不清掉、次数不回零：这次重试如果再失败，apply_list_result 会
        // 在它基础上 +1，才能真正收敛到「放弃」；成功了则由成功分支清空。
        self.list_retry = Some((tab_index, pane, Instant::now(), tries));
        self.reload_pane(tab_index, pane);
    }

    /// 切回某个标签时，给它「上次读失败」的面板补发一次列举。
    ///
    /// 只重试带真实路径的面板（此电脑 / 回收站不需要）。标记先清掉再发请求，
    /// 否则一次失败就会变成每次切标签都重发。
    fn retry_failed_panes(&mut self) {
        let tab_index = self.state.active_tab;
        let panes: Vec<usize> = self.state.tabs[tab_index]
            .panes
            .iter()
            .enumerate()
            .filter(|(_, p)| p.list_failed && p.path.is_some())
            .map(|(i, _)| i)
            .collect();
        for pane in panes {
            self.state.tabs[tab_index].panes[pane].list_failed = false;
            logal!(
                "[list] retry after failed read: tab={} pane={}",
                tab_index, pane
            );
            self.reload_pane(tab_index, pane);
        }
    }

    /// 兜底清掉标签拖拽状态（正常由 `up` 的 phase 2 收尾）。
    ///
    /// 只要那一次 `up` 送不到（按住期间标签模型被重建、指针在窗口外松开、
    /// 原生菜单或别的窗口抢走鼠标捕获、窗口失焦），Rust 侧就会一直认为“还在拖”，
    /// 界面上的落点竖线（z: 10）和半透明标签就永远叠在标签上。
    fn cancel_tab_drag(&mut self, why: &str) {
        let Some((index, start_x, moved)) = self.tab_drag.take() else {
            return;
        };
        self.ui.set_tab_drag_index(-1);
        self.ui.set_tab_drag_dx(0.0);
        self.ui.set_tab_drop_slot(-1);
        if moved {
            // 无条件写入：这是「标签栏上多出一条竖条」的现场证据。
            logal!(
                "!! [tab] stuck drag state cleared ({why}): index={index} start_x={start_x:.1} tabs={} left_pressed={}",
                self.state.tabs.len(),
                self.ui.get_mouse_left_pressed()
            );
        } else {
            logv!("[tab] dropped stale drag start ({why}): index={index}");
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
                logv!("[tab] down index={index} x_in_row={x_in_row:.1} pointer_x={pointer_x:.1}");
                self.tab_drag = Some((index, pointer_x, false));
            }
            1 => {
                let Some((from, start_x, moved)) = self.tab_drag else {
                    return;
                };
                if from != index {
                    return;
                }
                // 抖动不算拖动：4px 太敏感（手抖 / 高 DPI / 触摸板轻扫都会越过），
                // 于是一次普通点击会先把标签画成「拖起」状态，看着像误触发拖动。
                let moved = moved || (pointer_x - start_x).abs() > 10.0;
                let was_moved = moved && self.tab_drag.is_some_and(|(_, _, m)| !m);
                self.tab_drag = Some((from, start_x, moved));
                if !moved {
                    return;
                }
                if was_moved {
                    logv!(
                        "[tab] drag armed from={from} travel={:.1} pointer_x={pointer_x:.1}",
                        pointer_x - start_x
                    );
                }
                // 这里只切"视觉拖动态"。**不要**在这里丢弃 clicked：
                // 松手时位移可能仍然不足（见 phase 2 的距离门槛），那时用户其实只想点标签。
                // 用「被拖标签的视觉中心点」判槽位：抓在标签左缘还是右缘都不影响，
                // 必须把标签拖过邻居中线才让位，不会一点边缘就乱跳。
                let dragged_center_x = from as f32 * 153.0 + 75.0 + (pointer_x - start_x);
                self.ui.set_tab_drag_index(from as i32);
                self.ui.set_tab_drag_dx(pointer_x - start_x);
                self.ui.set_tab_drop_slot(slot_of(dragged_center_x) as i32);
            }
            2 => {
                let Some((from, start_x, moved)) = self.tab_drag.take() else {
                    return;
                };
                self.ui.set_tab_drag_index(-1);
                self.ui.set_tab_drag_dx(0.0);
                self.ui.set_tab_drop_slot(-1);
                if from != index || !moved {
                    return;
                }
                // 真正重排需要明显位移（约 1/4 个标签宽）：落点用 floor 划槽，
                // 点在标签右边缘时左边一点抖动就会跨到下一槽，于是"点一下就换了位置"。
                // 位移不足时按点击处理：不重排，也不抑制 clicked。
                if (pointer_x - start_x).abs() < 40.0 {
                    logv!(
                        "[tab] drag ignored: travel={:.1}px < 40px from={}",
                        pointer_x - start_x,
                        from
                    );
                    return;
                }
                self.suppress_tab_click = Some(std::time::Instant::now());
                let dragged_center_x = from as f32 * 153.0 + 75.0 + (pointer_x - start_x);
                let to = slot_of(dragged_center_x);
                logv!(
                    "[tab] drag release from={from} to={to} travel={:.1} tabs={}",
                    pointer_x - start_x,
                    self.state.tabs.len()
                );
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
                // 若改名前它本来就不在选中集里（新建文件夹流程：条目刚创建，从没被选中过），
                // 提交后要把它选中 —— 否则重命名框一关列表里没有任何选中项。
                let was_selected = {
                    let tab = self.state.active_mut();
                    let hit = tab.active_mut().selection.remove(path);
                    if hit {
                        tab.active_mut().selection.insert(new_path.clone());
                    }
                    hit
                };
                if !was_selected {
                    // 复用「新条目就位后滚进可视区并选中」的既有机制
                    // （见 apply_list_result 里的 scroll_to_paths / scroll_select）。
                    self.scroll_to_paths = vec![new_path.clone()];
                    self.scroll_select = true;
                    self.reveal_pane = Some(self.state.active().active_pane);
                }
                self.state.cut_paths.remove(path);
                // 记账**不能**挂在下面「找到显示位置」那个 if 里：改名时条目
                // 可能根本不在当前列表里（搜索 / 类型筛选过滤掉了，或者列举还
                // 没到），那样这次改名就悄悄进不了撤消栈。
                self.undo.record(crate::ops::undo::Kind::Rename {
                    pairs: vec![(path.to_path_buf(), new_path.clone())],
                });
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
    fn expect_new_entries(&mut self, sources: &[PathBuf], dest_dir: &Path, pane: usize) {
        // 揭示只在**这个面板**的列举结果上生效：跨分屏拖放时目标栏不是聚焦栏，
        // 而揭示的消费条件以前写死成“聚焦栏”，于是新文件既不滚进视野也不选中。
        self.reveal_pane = Some(pane);
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
        let pane = self.state.active().active_pane;
        self.expect_new_entries(&sources, &dest, pane);
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
        // 回收站条目没有「可改的名字」：站内路径是内部 Shell id，显示的原始
        // 路径只是线索。F2、单击文件名、右键重命名都收口在这里，一律拦掉。
        if self.pane_is_recycle() {
            return;
        }
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

    /// 当前聚焦的那一栏是不是回收站。
    fn pane_is_recycle(&self) -> bool {
        self.state.active().active().path.as_deref()
            == Some(crate::fs::drives::recycle_virtual_path().as_path())
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

    /// 延迟进入重命名：先起定时器留出双击窗口。
    ///
    /// 窗口内若来第二下按下（双击打开），调用方会 `cancel_rename_defer` 取消它。
    fn schedule_rename(&mut self, path: PathBuf, from: &str) {
        logv!("[rename] schedule via {} path={}", from, path.display());
        let Some(weak) = self.self_weak.clone() else {
            self.begin_rename(&path);
            return;
        };
        self.rename_defer_timer.stop();
        self.rename_defer_timer.start(
            TimerMode::SingleShot,
            Duration::from_millis(effective_double_click_ms() + 50),
            move || {
                let Some(app) = weak.upgrade() else { return };
                let Ok(mut app) = app.try_borrow_mut() else { return };
                // 已经有一个重命名输入框打开（F2 / 右键菜单 / 另一行的挂起改名）：
                // 不要再抢一次，否则会把用户正在编辑的文本重置成原名。
                if !app.ui.get_rename_path().is_empty() {
                    logv!("[rename] deferred rename skipped: already renaming");
                    return;
                }
                let sel = app.state.active().active().selected_paths();
                if sel.len() == 1 && sel[0] == path {
                    logv!("[rename] deferred rename firing for {:?}", path);
                    app.begin_rename(&path);
                }
            },
        );
    }

    /// 取消挂起的延迟重命名（双击打开、点空白、切目录时调用）。
    fn cancel_rename_defer(&mut self) {
        self.rename_defer_timer.stop();
    }

    /// 名称浮层当前指向的（栏号, 条目路径）。
    ///
    /// 选中/重命名层与悬停层是两套属性，只有 `hover-expand-index` 或
    /// `expand-index` 有值时才有浮层；悬停优先（鼠标下的那一层才是用户看的那层）。
    /// 空白处右键按下：记快照 + 置背景菜单范围（菜单推迟到抬手才弹）。
    ///
    /// 抽出来是因为名称浮层盖住空白时也要走同一套：那里没有行元素，
    /// 事件只能由浮层转交。
    fn background_right_press(&mut self) {
        logln!(
            "[menu] background right-press pane={} path={:?}",
            self.state.active().active_pane,
            self.state.active().active().path
        );
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
        self.menu_scope = Some(MenuScope::Background);
        self.deferred_bg_menu = Some(MenuScope::Background);
        // 行内右键挂起的那份也作废：两种菜单不可能同时算数。
        self.pending_row_menu = None;
    }

    /// 某一栏第 `index` 个显示条目对应的路径。
    fn overlay_hit_row(&self, pane: usize, index: usize) -> Option<PathBuf> {
        self.state
            .active()
            .panes
            .get(pane)
            .and_then(|p| p.display_entry(index))
            .map(|e| e.path.clone())
    }

    /// 在某一栏的某一行上「右键」：没选中就先单选它，然后弹出选中集菜单。
    fn row_right_click(&mut self, pane: usize, index: usize) {
        if self.state.active().active_pane != pane {
            self.focus_pane(pane);
        }
        let Some(path) = self.overlay_hit_row(pane, index) else {
            return;
        };
        logln!(
            "[menu] row right-click pane={} idx={} path={} sel={}",
            pane,
            index,
            path.display(),
            self.state.active().active().selection.len()
        );
        // 上一次空白右键若没等到抬手（菜单把 up 吃掉了），这里必须作废，
        // 否则它会在下一次空白抬手时冒出来。挂着的行内右键同理。
        self.deferred_bg_menu = None;
        self.pending_row_menu = None;
        self.last_left_press_row = None;
        if !self.state.active().active().selection.contains(&path) {
            let tab = self.state.active_mut().active_mut();
            tab.selection.clear();
            tab.selection.insert(path.clone());
            tab.anchor = Some(path.clone());
        }
        self.menu_anchor_path = Some(path);
        let target = MenuScope::Selection(self.state.active().active().selected_paths());
        self.menu_scope = Some(target.clone());
        self.visible_cache = None;
        self.visible_cache_right = None;
        self.refresh_visible_rows();
        self.sync_status();
        self.open_menu(target);
    }

    /// 在某一栏的某一行上「单击」：只做单选，不进重命名。
    fn row_left_click_select(&mut self, pane: usize, index: usize) {
        if self.state.active().active_pane != pane {
            self.focus_pane(pane);
        }
        let Some(path) = self.overlay_hit_row(pane, index) else {
            return;
        };
        logln!(
            "[click] overlay selects neighbour pane={} idx={} path={}",
            pane,
            index,
            path.display()
        );
        {
            let tab = self.state.active_mut().active_mut();
            tab.selection.clear();
            tab.selection.insert(path.clone());
            tab.anchor = Some(path.clone());
        }
        self.pending_single_click = None;
        self.last_left_press_row = Some(path.clone());
        self.last_click = Some((path, std::time::Instant::now()));
        self.cancel_rename_defer();
        self.visible_cache = None;
        self.visible_cache_right = None;
        self.refresh_visible_rows();
        self.sync_status();
    }

    /// 浮层相关的两个索引（选中层 / 悬停层），只用于诊断日志。
    fn overlay_indices(&self, pane: usize) -> (i32, i32) {
        if pane == 1 {
            (
                self.ui.get_file_expand_index_right(),
                self.ui.get_file_hover_expand_index_right(),
            )
        } else {
            (
                self.ui.get_file_expand_index(),
                self.ui.get_file_hover_expand_index(),
            )
        }
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
    /// 彻底删除：交给 Shell 的 `delete` 动词，确认框由系统自己弹。
    ///
    /// 不自己画确认框的原因见 `sys::menu::invoke_shell_verb`：只有 Shell 弹的
    /// 那个才和资源管理器长得一样（图标、按钮、默认按钮、「不再提示」勾选框）。
    /// 用户点「否」时命令也可能返回失败，所以结果按「条目还在不在」判定。
    fn purge_items_with_confirm(&mut self, paths: Vec<PathBuf>) {
        if paths.is_empty() {
            return;
        }
        logln!("[recycle] purge request n={}", paths.len());
        let invoked =
            crate::sys::shell::purge_items(&paths, crate::sys::shell::current_owner_hwnd());
        // 结果只认「条目还在不在」：Shell 的 delete 动词在用户点「否」时也可能
        // 返回成功，光看 HRESULT 分不出删没删。
        let removed = fs::drives::recycle_remaining(&paths)
            .map(|left| paths.len().saturating_sub(left));
        match removed {
            Some(n) if n > 0 => {
                logln!("[recycle] purge done n={}", n);
                self.set_status_job(self.trf("已彻底删除 {} 项", &[n.to_string()]));
                self.reload_active();
                self.sidebar_fingerprint = 0;
            }
            Some(_) if invoked => {
                // 命令投递成功但条目还在 = 用户点了「否」：什么都不做，也不报错。
                logln!("[recycle] purge cancelled");
            }
            Some(_) => {
                logln!("[recycle] purge failed");
                self.set_status_job(self.tr("删除失败"));
            }
            None => {
                // 列举失败，判断不了：刷新一遍让列表自己说话，不写状态。
                logln!("[recycle] purge result unknown (trash list failed)");
                self.reload_active();
                self.sidebar_fingerprint = 0;
            }
        }
    }

    fn delete_selection(&mut self, to_trash: bool) {
        let paths = self.state.active().active().selected_paths();
        // 回收站里「删除」= 永久删除（Delete 与 Shift+Delete 都是），
        // 走 Shell 自己的确认框，跟资源管理器一致。
        if !paths.is_empty()
            && self.state.active().active().path.as_deref()
                == Some(fs::drives::recycle_virtual_path().as_path())
        {
            self.purge_items_with_confirm(paths);
            return;
        }
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

    /// 反选：已选中的取消，未选中的选上。
    ///
    /// 只反转**当前显示**的条目（搜索/过滤时看不见的保持原状）。
    fn invert_selection(&mut self) {
        let displayed: Vec<PathBuf> = self.state.active().active().displayed_paths();
        {
            let pane = self.state.active_mut().active_mut();
            for path in displayed {
                if !pane.selection.remove(&path) {
                    pane.selection.insert(path);
                }
            }
        }
        self.visible_cache = None;
        self.visible_cache_right = None;
        self.refresh_visible_rows();
        self.sync_status();
    }

    /// 立刻把某条目设为唯一选中项并滚进可视区（不重载列表）。
    ///
    /// 给「Esc 取消新建文件夹的重命名」用：条目已经在列表里，只是从没被选中过。
    /// 只认**已加载快照**里的路径，绝不碰磁盘（`is_file()` 在掉线的网络盘上会卡界面）。
    /// 找不到（例如被搜索过滤掉）就返回 false，什么都不做。
    fn select_path_now(&mut self, path: &Path) -> bool {
        let pos = {
            let pane = self.state.active().active();
            pane.display
                .iter()
                .position(|&ei| pane.entries.get(ei).is_some_and(|e| e.path == path))
        };
        let Some(pos) = pos else {
            // 列表还没到位（异步列举竞态）：交给列举结果落地时再选中。
            self.scroll_to_paths = vec![path.to_path_buf()];
            self.scroll_select = true;
            self.reveal_pane = Some(self.state.active().active_pane);
            return false;
        };
        {
            let pane = self.state.active_mut().active_mut();
            pane.selection.clear();
            pane.selection.insert(path.to_path_buf());
            pane.anchor = Some(path.to_path_buf());
        }
        // 新建文件夹留下的「等旧名字出现」的挂起匹配作废，否则下次列举还会拿它去滚。
        self.scroll_to_paths.clear();
        self.scroll_select = false;
        // 这里滚的是聚焦面板，不能留着上一次（可能是另一栏）的记录。
        self.reveal_pane = None;
        self.scroll_to_pane = None;
        self.scroll_to_index = Some(pos);
        self.visible_cache = None;
        self.visible_cache_right = None;
        self.refresh_visible_rows();
        self.sync_status();
        true
    }

    /// 全选当前显示项。
    fn select_all(&mut self) {
        let selected: HashSet<PathBuf> =
            self.state.active().active().displayed_paths().into_iter().collect();
        self.state.active_mut().active_mut().selection = selected;
        self.visible_cache = None;
        self.visible_cache_right = None;
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

        // 0.05 标签栏批量关闭（不依赖选中项）。
        if self.run_tab_menu_action(id) {
            return;
        }

        // 0.1 侧栏「快速访问」取消固定（不依赖文件选择）。
        if let Some(key) = id.strip_prefix("quick:unpin:") {
            let key = key.to_string();
            self.unpin_quick_item(&key);
            return;
        }

        // 0.2 空白处右键菜单最顶上的撤消 / 重做（不依赖选中项和当前目录）。
        if id == "undo" {
            self.history_action(false);
            return;
        }
        if id == "redo" {
            self.history_action(true);
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
                // 路径（仅回收站有这一列）：从根往后读，默认升序。
                "path" => { key = SortKey::Path; asc = true; }
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
                "builtin.copy_path" => {
                    if let MenuScope::Selection(paths) = &target {
                        if paths.is_empty() {
                            return;
                        }
                        let text = ops::clipboard::paths_text(paths);
                        match ops::clipboard::set_text(&text) {
                            Ok(()) => {
                                // 系统剪贴板已被改写为纯文本：把内部那份「回收站内剪切」
                                // 兜底缓存一起清掉，否则接下来的「粘贴」会莫名其妙把
                                // 回收站里的条目搬出去（那是上一次剪切留下的语义）。
                                self.state.clipboard_paths.clear();
                                self.state.clipboard_from_recycle = false;
                                self.state.cut_paths.clear();
                                self.visible_cache = None;
                                self.visible_cache_right = None;
                                self.refresh_visible_rows();
                                self.set_status_job(if paths.len() > 1 {
                                    self.trf("已复制 {} 项路径", &[paths.len().to_string()])
                                } else {
                                    self.tr("已复制路径").to_string()
                                })
                            }
                            Err(e) => self.set_status_job(e),
                        }
                    }
                    return;
                }
                "builtin.batch_rename" => {
                    // 独立顶层窗口：可同时开多个，各自记住自己的选项。
                    if let MenuScope::Selection(paths) = &target {
                        let paths = paths.clone();
                        self.open_batch_rename(paths);
                    }
                    return;
                }
                "builtin.copy" => {
                    self.copy_selection(false);
                    return;
                }
                "builtin.move" => {
                    self.copy_selection(true);
                    return;
                }
                // 与 Del 键同一条路径（删除到回收站）。系统菜单那一项不记账，
                // 所以默认藏掉它、改用这一条，撤消栈才能覆盖删除。
                "builtin.trash" => {
                    self.delete_selection(true);
                    return;
                }
                // 属性 / 重命名：系统菜单也有这两项，但自己实现才能保证
                // 系统菜单被关键词过滤掉以后功能还在（重命名还要接 F2 那条路）。
                "builtin.properties" => {
                    if let MenuScope::Selection(paths) = &target {
                        // Shell 的 properties 动词一次只处理一个条目，多选时显示第一项。
                        if let Some(p) = paths.first() {
                            if let Err(e) = crate::sys::shell::show_properties(p) {
                                self.set_status_job(self.trf("无法显示属性：{}", &[e.to_string()]));
                            }
                        }
                    }
                    return;
                }
                "builtin.rename" => {
                    if let MenuScope::Selection(paths) = &target {
                        if paths.len() == 1 {
                            let path = paths[0].clone();
                            // 与 F2 同一条路：先清拖拽上睾锁，否则输入框接管鼠标后
                            // TouchArea 收不到 up 事件，滚轮/滚动会永久冻住。
                            self.drag_pending = None;
                            self.ui.invoke_set_drag_armed(false);
                            self.begin_rename(&path);
                        }
                    }
                    return;
                }
                "builtin.open_with" => {
                    if let MenuScope::Selection(paths) = &target {
                        // Shell 的选择器一次只处理一个文件（与「属性」同规矩）。
                        if let Some(first) = paths.first().cloned() {
                            // ShellExecuteEx 冷启动 OpenWith.exe 要几十毫秒到几百
                            // 毫秒，不放 UI 线程上赌运气。
                            std::thread::spawn(move || {
                                if let Err(e) = crate::sys::shell::show_open_with(&first) {
                                    crate::logal!("!! open_with {:?} failed: {}", first, e);
                                    let msg = e.to_string();
                                    let _ = slint::invoke_from_event_loop(move || {
                                        let Some(app) =
                                            SESSION.with(|slot| slot.borrow().clone())
                                        else {
                                            return;
                                        };
                                        let app = app.borrow();
                                        let text = app.trf(
                                            "无法打开「打开方式」：{}",
                                            &[msg],
                                        );
                                        app.set_status_job(text);
                                    });
                                }
                            });
                        }
                    }
                    return;
                }
                "builtin.create_shortcut" | "builtin.send_to_desktop" => {
                    let to_desktop = action_id == "builtin.send_to_desktop";
                    if let MenuScope::Selection(paths) = &target {
                        let paths = paths.clone();
                        // 「- 快捷方式」这半截跟着界面语言走（资源管理器也是用系统语言）。
                        let suffix = self.tr("快捷方式").to_string();
                        // 回调回来时面板可能已经被切走（设置页 / 别的目录）：
                        // 记住点击那一刻的目录，只有还停在那儿才重列举。
                        let keep_path = self.state.active().active().path.clone();
                        // 建 .lnk = 碰磁盘 + COM，绝不能跑在 UI 线程上。
                        std::thread::spawn(move || {
                            let result =
                                crate::sys::shell::create_shortcuts(&paths, to_desktop, &suffix);
                            let _ = slint::invoke_from_event_loop(move || {
                                let Some(app) = SESSION.with(|slot| slot.borrow().clone()) else {
                                    return;
                                };
                                let mut app = app.borrow_mut();
                                match result {
                                    Ok(n) => {
                                        let msg = app
                                            .trf("已创建 {} 个快捷方式", &[n.to_string()]);
                                        app.set_status_job(msg);
                                        // 「创建快捷方式」会往当前目录里放新条目，
                                        // 重新列举才能看到它。设置页没有目录可刷，
                                        // 切走的标签也不该被点亮“加载中”。
                                        if app.state.active().kind
                                            == crate::state::TabKind::Explorer
                                            && app.state.active().active().path == keep_path
                                        {
                                            app.reload_active();
                                        }
                                    }
                                    Err(e) => {
                                        crate::logal!("!! shortcut failed: {e}");
                                        let msg =
                                            app.trf("创建快捷方式失败：{}", &[e.to_string()]);
                                        app.set_status_job(msg);
                                    }
                                }
                            });
                        });
                    }
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
                        MenuScope::Selection(paths) => paths.first().cloned(),
                        MenuScope::Background => self.state.active().active().path.clone(),
                    };
                    self.open_terminal_at(dir);
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
                // 三个解压项：多选时逐个包跑一遍（WinRAR / 7-Zip / Bandizip
                // 都不接受「一次给多个包」——多出来的参数会被当成包内文件名，
                // 实测 WinRAR 直接卡在对话框上不出结果）。
                // 分卷只交第一卷：整组递过去同样会卡住。
                "builtin.archive_extract_files"
                | "builtin.archive_extract_here"
                | "builtin.archive_extract" => {
                    let MenuScope::Selection(paths) = &target else {
                        return;
                    };
                    let archives = crate::ops::actions::extractable_archives(paths);
                    let skipped = crate::ops::actions::skipped_volumes(paths);
                    if archives.is_empty() {
                        // 只有后续卷 / 只选了 part2 这类：缺第一卷本来解不开。
                        if skipped > 0 {
                            self.set_status_job(
                                self.tr("选中的分卷缺少第一卷，没法解压").to_string(),
                            );
                        }
                        return;
                    }
                    let entry = self.actions.iter().find(|a| a.id == action_id).cloned();
                    let Some(entry) = entry else { return };
                    // 「解压文件」先问一次目录，全部包都解到那儿。
                    // 选择框必须放后台线程弹：同步 Show 会开嵌套消息循环堵死
                    // Slint 的定时器，吸附窗就渲染不出来（搜索排除目录那边同理）。
                    if action_id == "builtin.archive_extract_files" {
                        if self.archive_extract_picking {
                            return;
                        }
                        self.archive_extract_picking = true;
                        let title = self.tr("选择解压目标文件夹").to_string();
                        std::thread::spawn(move || {
                            let dest = crate::sys::pick::pick_folder_titled(&title);
                            let _ = slint::invoke_from_event_loop(move || {
                                let Some(app) =
                                    SESSION.with(|slot| slot.borrow().clone())
                                else {
                                    return;
                                };
                                let mut app = app.borrow_mut();
                                app.archive_extract_picking = false;
                                let Some(dest) = dest else { return };
                                let entry = app
                                    .actions
                                    .iter()
                                    .find(|a| a.id == "builtin.archive_extract_files")
                                    .cloned();
                                if let Some(entry) = entry {
                                    app.run_archive_extract(
                                        archives,
                                        Some(dest),
                                        &entry,
                                        skipped,
                                    );
                                }
                            });
                        });
                        return;
                    }
                    self.run_archive_extract(archives, None, &entry, skipped);
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
                    self.purge_items_with_confirm(paths.clone());
                }
            }
            "recycle-empty" => {
                // 走系统 API：确认框 / 进度条 / 提示音都由它提供（见 `empty_recycle_bin`）。
                match fs::drives::empty_recycle_bin(crate::sys::shell::current_owner_hwnd()) {
                    fs::drives::EmptyResult::Emptied | fs::drives::EmptyResult::AlreadyEmpty => {
                        self.set_status_job(self.tr("回收站已清空"));
                    }
                    // 用户点了「否」：列表没变，不刷新也不报错。
                    fs::drives::EmptyResult::Cancelled => return,
                    fs::drives::EmptyResult::Failed => {
                        self.set_status_job(self.tr("清空回收站失败"));
                        return;
                    }
                }
                // 条目真的没了（或本来就是空的）：重新列举，否则列表还挂着旧条目。
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
                    MenuScope::Selection(paths) => paths.first().cloned(),
                    MenuScope::Background => self.state.active().active().path.clone(),
                };
                self.open_terminal_at(dir);
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
                            self.reveal_pane = Some(self.state.active().active_pane);
                            self.visible_cache = None;
                            self.visible_cache_right = None;
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
            CMD_NEW_TXT => self.create_text_file(),
            CMD_BG_PROPERTIES => self.show_background_properties(),
            CMD_REFRESH => self.user_refresh_active(),
            CMD_SELECT_ALL => {
                let tab = self.state.active_mut().active_mut();
                let all = tab.displayed_paths();
                tab.selection = all.into_iter().collect();
                self.refresh_visible_rows();
                self.sync_status();
            }
            CMD_INVERT_SELECTION => self.invert_selection(),
            _ => {}
        }
    }

    /// 打开文件/目录。
    fn open_path(&mut self, path: &Path) {
        // 网络 / WSL 共享上的 stat 能卡十几秒，而这里跑在 UI 线程上：
        // 一卡就整条消息循环停住（连托盘图标都会「未响应」）。
        // 远端路径一律丢后台问，回来再投递；本地盘 stat 是微秒级，保持同步。
        if crate::fs::dir::is_remote_path(path) {
            let path = path.to_path_buf();
            std::thread::spawn(move || {
                let is_dir = std::fs::metadata(&path).map(|m| m.is_dir()).unwrap_or(false);
                let _ = slint::invoke_from_event_loop(move || {
                    let Some(app) = SESSION.with(|slot| slot.borrow().clone()) else {
                        return;
                    };
                    let mut app = app.borrow_mut();
                    if is_dir {
                        app.navigate_active(Some(path));
                    } else if let Err(e) = crate::sys::shell::shell_open(&path) {
                        crate::logal!("!! open {:?} failed: {}", path, e);
                        let msg = app.trf("打开失败：{}", &[e.to_string()]);
                        app.set_status_job(msg);
                    }
                });
            });
            return;
        }
        let is_dir = std::fs::metadata(path).map(|m| m.is_dir()).unwrap_or(false);
        if is_dir {
            self.navigate_active(Some(path.to_path_buf()));
        } else if let Err(e) = crate::sys::shell::shell_open(path) {
            crate::logal!("!! open {:?} failed: {}", path, e);
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
            self.visible_cache_right = None;
            self.refresh_visible_rows();
            self.set_status_job(self.trf(
                "已剪切 {} 个回收站项目（粘贴即可移动到目标文件夹）",
                &[self.state.clipboard_paths.len().to_string()],
            ));
            return;
        }

        // 常规目录正常写入系统剪贴板
        self.state.clipboard_from_recycle = false;
        // 记耗时：这条路径在界面线程上跑，一旦又混进磁盘/网络调用，
        // 这里会立刻显示出几百毫秒以上的「卡一下」。
        let started = std::time::Instant::now();
        let count = paths.len();
        let written = clipboard::set_files(&paths, is_move);
        logv!(
            "[clipboard] 写 CF_HDROP {} 项 move={} 耗时 {:?}",
            count,
            is_move,
            started.elapsed()
        );
        match written {
            Ok(()) => {
                self.state.clipboard_paths = paths.clone();
                self.state.clipboard_operation = if is_move { "move".into() } else { "copy".into() };
                if is_move {
                    self.state.cut_paths = paths.into_iter().collect();
                } else {
                    self.state.cut_paths.clear();
                }
                self.visible_cache = None;
                self.visible_cache_right = None;
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
                self.visible_cache_right = None;
            }
            return;
        }

        // 场景 2：从回收站剪切后粘贴到常规目录 -> 先还原，再移动到目标目录。
        if self.state.clipboard_from_recycle && !self.state.clipboard_paths.is_empty() {
            let paths = std::mem::take(&mut self.state.clipboard_paths);
            self.state.clipboard_from_recycle = false;
            self.state.cut_paths.clear();
            self.visible_cache = None;
            self.visible_cache_right = None;

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
                let pane = self.state.active().active_pane;
                self.expect_new_entries(&to_move, &dest, pane);
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
                self.reveal_pane = Some(self.state.active().active_pane);
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
        let pane = self.state.active().active_pane;
        self.expect_new_entries(&paths, &dest, pane);
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
                // 这是应用自己建的，不是 Shell 新建动词的结果：作废那份快照，
                // 否则随后到达的列举会对着旧快照做 diff。
                self.shell_new_watch = None;

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
                self.undo.record(crate::ops::undo::Kind::Create {
                    dirs: vec![crate::ops::undo::CreatedDir {
                        id: crate::ops::undo::capture_dir_id(&path),
                        path: path.clone(),
                        is_file: false,
                    }],
                });
                self.scroll_to_paths = vec![path.clone()];
                self.scroll_select = false;
                self.reveal_pane = Some(self.state.active().active_pane);
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

    /// 空白处「新建文本文档」：建一个 0 字节的 txt，然后直接进重命名。
    ///
    /// 与「新建文件夹」同一条路：记账 → 让新条目滚进可视区 → 起输入框。
    fn create_text_file(&mut self) {
        let Some(dir) = self.state.active().active().path.clone() else {
            self.set_status_job(self.tr("请先进入一个文件夹"));
            return;
        };
        // 用 create_new 而不是 File::create：后者会**截断**同名文件。
        // exists() 查完到真正建之间有个窗口，这个窗口里冒出来的同名文件
        // 必须换下一个名字，不能把别人的内容清成 0 字节。
        let mut counter = 1u32;
        let made = loop {
            let name = if counter == 1 {
                "新建文本文档.txt".to_string()
            } else {
                format!("新建文本文档 ({counter}).txt")
            };
            let candidate = dir.join(name);
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&candidate)
            {
                Ok(_) => break candidate,
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    counter += 1;
                    if counter > 9999 {
                        self.set_status_job(self.trf("创建文件失败：{}", &[e.to_string()]));
                        return;
                    }
                }
                Err(e) => {
                    self.set_status_job(self.trf("创建文件失败：{}", &[e.to_string()]));
                    return;
                }
            }
        };
        let path = made;
        // 与新建文件夹一致：清拖拽上膛锁，作废 Shell 新建快照。
        self.drag_pending = None;
        self.ui.invoke_set_drag_armed(false);
        self.shell_new_watch = None;
        // 撤消这次新建 = 把这个**还是空**的文件删掉（见 ops::jobs 的
        // Kind::Create：写了内容就不删）。
        self.undo.record(crate::ops::undo::Kind::Create {
            dirs: vec![crate::ops::undo::CreatedDir {
                id: crate::ops::undo::capture_dir_id(&path),
                path: path.clone(),
                is_file: true,
            }],
        });
        self.scroll_to_paths = vec![path.clone()];
        self.scroll_select = false;
        self.reveal_pane = Some(self.state.active().active_pane);
        self.reload_active();
        self.begin_rename(&path.clone());
    }

    /// 空白处「属性」：显示当前文件夹自己的属性页。
    fn show_background_properties(&mut self) {
        let Some(dir) = self.state.active().active().path.clone() else {
            self.set_status_job(self.tr("请先进入一个文件夹"));
            return;
        };
        if let Err(e) = crate::sys::shell::show_properties(&dir) {
            self.set_status_job(self.trf("无法显示属性：{}", &[e.to_string()]));
        }
    }

    /// 提交后台文件任务；返回是否真的提交成功（已有任务在进行时为 false）。
    fn submit_job(&mut self, kind: JobKind) -> bool {
        // 作废「Shell 系统菜单新建」快照：我们自己发起的复制/移动/删除同样会
        // 往目录里放新条目，而粘贴多文件时中间的列举可能刚好只看到 1 个新文件，
        // 于是被 diff 当成「用 Shell 新建了一个文件」→ 把刚粘贴的文件抢进重命名。
        self.shell_new_watch = None;
        self.shell_op_running = true;
        self.shell_key_suppressed_logged = false;
        let ok = self.jobs.submit(kind);
        if !ok {
            self.set_status_job(self.tr("有任务正在进行，请稍候"));
        }
        ok
    }

    /// Ctrl+Z / Ctrl+Y：撤消 / 重做最近一次文件操作。
    ///
    /// 撤消本身也是一次文件任务（要动磁盘、可能弹 Shell 对话框），所以走同一条
    /// 任务队列，跟复制 / 删除互斥。栈顶那条记录要等任务**真的提交成功**才移动，
    /// 否则「有任务在跑」时按一下 Ctrl+Z 就把记录弄丢了。
    fn history_action(&mut self, redo: bool) {
        let entry = if redo {
            self.undo.peek_redo().cloned()
        } else {
            self.undo.peek_undo().cloned()
        };
        let Some(entry) = entry else {
            logv!("[undo] 栈是空的 redo={}", redo);
            self.set_status_job(self.tr(if redo {
                "没有可重做的操作"
            } else {
                "没有可撤消的操作"
            }));
            return;
        };
        logv!(
            "[undo] submit redo={} kind={:?} undo_left={} redo_left={}",
            redo,
            entry,
            self.undo.can_undo(),
            self.undo.can_redo()
        );
        if !self.submit_job(JobKind::History { redo, entry }) {
            return;
        }
        // 提交成功才搬记录：失败时记录留在原处，下次还能再试。
        if redo {
            self.undo.take_redo();
        } else {
            self.undo.take_undo();
        }
    }

    fn set_status_job(&self, message: impl Into<SharedString>) {
        let message = message.into();
        // 危险提示用红色：失败、名称超限、删除（含删除取消）等需要一眼看到。
        self.ui.set_status_danger(status_is_danger(message.as_str()));
        self.ui.set_status_job(message);
    }

    /// 任务完成后的处理：刷新受影响目录。
    fn on_job_finished(&mut self, mut summary: JobSummary) {
        self.shell_op_running = false;
        // 撤消栈：普通操作成功就记一笔；撤消 / 重做任务只有失败时才把记录退回原处
        // （成功时提交那一步已经搬过了）。
        if let Some(kind) = summary.recorded.take() {
            self.undo.record(kind);
        }
        if summary.clear_undo {
            self.undo.clear();
        }
        if let Some((redo, _)) = summary.history.take() {
            if summary.failed > 0 && !summary.cancelled {
                // 用户取消：什么都没动，记录放回去还能再试。
                // 真正的失败（比如回收站里已经找不到那批文件）就把它扔掉 ——
                // 留在栈顶会让之后每一次 Ctrl+Z 都重试同一条、次次失败，
                // 更早的历史等于永远撤不到。
                logv!("[undo] 这次记录失败，作废 redo={}", redo);
                if redo {
                    self.undo.drop_undo();
                } else {
                    self.undo.drop_redo();
                }
            } else if summary.failed > 0 {
                if redo {
                    self.undo.restore_redo();
                } else {
                    self.undo.restore_undo();
                }
            }
        }
        // 复制/移动是逐个落地的：任务期间发起的列举可能只含一部分新文件，
        // 它们的结果可能在这个函数之后才到。给一小段保护期，只拦“不完整”的
        // 列举；完整的那次（本函数稍后发起的重载）照常立即消费。
        self.reveal_grace_until = Some(std::time::Instant::now() + std::time::Duration::from_millis(400));
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

        // 搜索面板：子树里有东西被动了（复制/移动/删除/改名/新建），结果整体
        // 作废重跑。不重跑的话，被移动的那一条会「旧位置 + 新位置」各留一份，
        // 列表里就是两个一模一样的文件。只重跑搜索面板，普通目录本来就走重列举。
        {
            let mut rerun = Vec::new();
            for (index, tab) in self.state.tabs.iter().enumerate() {
                if tab.kind != crate::state::TabKind::Explorer {
                    continue;
                }
                for (pane_idx, pane) in tab.panes.iter().enumerate() {
                    if pane.filter.trim().is_empty() {
                        continue;
                    }
                    let Some(path) = pane.path.as_deref() else {
                        continue;
                    };
                    if path == fs::drives::recycle_virtual_path() {
                        continue;
                    }
                    // 只认「被改动的目录在搜索子树里」。上级目录被改动（比如从
                    // 树外面搬进来一个文件）只会影响根目录那一层，那由重新列举
                    // 负责，深搜不必白扫一趟。
                    if affected.iter().any(|a| a == path || a.starts_with(path)) {
                        rerun.push((index, pane_idx));
                    }
                }
            }
            let rerun_any = !rerun.is_empty();
            for (ti, pi) in rerun {
                logv!("[deep] 文件任务改动了子树，重跑搜索 tab={ti} pane={pi}");
                self.start_deep_search_for(ti, pi);
                // 这条面板不会走「列举结果到达」那条重建路径（它的目录本身没变），
                // 必须自己把显示表重算一遍，否则 display 还指着已被清掉的深搜条目。
                if let Some(pane) = self.state.tabs[ti].panes.get_mut(pi) {
                    pane.rebuild_display();
                }
            }
            if rerun_any {
                self.visible_cache = None;
                self.visible_cache_right = None;
                self.sync_file_list();
                self.sync_status();
            }
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
            self.visible_cache_right = None;
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

/// 待打开的资源管理器请求上限（防止异常情况下无限制堆积）。
const SHELL_OPEN_QUEUE_MAX: usize = 32;
/// 借用冲突时的重试次数与间隔：约 1.5 秒后放弃（每次都记日志）。
const SHELL_OPEN_RETRIES: u32 = 30;
const SHELL_OPEN_RETRY_MS: u64 = 50;

/// 排空「资源管理器转来的新标签打开」队列，借用拿不到就推迟再试。
///
/// 早先这里拿不到借用就直接丢弃请求 —— 用户点了文件夹却什么都不发生，
/// 也没有任何提示。现在改成退避重试，只有在长时间被占用时才放弃。
fn schedule_shell_drain(weak: Weak<RefCell<App>>, attempt: u32) {
    let Some(app) = weak.upgrade() else { return };
    let mut done = false;
    if let Ok(mut app) = app.try_borrow_mut() {
        app.drain_shell_tabs();
        done = true;
    }
    if done {
        return;
    }
    if attempt >= SHELL_OPEN_RETRIES {
        logln!("!! 接管资源管理器：App 长时间被借用，放弃 {} 条打开请求", {
            app.try_borrow().map(|a| a.shell_open_queue.len()).unwrap_or(0)
        });
        if let Ok(mut app) = app.try_borrow_mut() {
            app.shell_open_queue.clear();
        }
        return;
    }
    // 队列留在 App 上，只重新排一次定时器。
    let app2 = app.clone();
    app.borrow().shell_open_timer.stop();
    app.borrow().shell_open_timer.start(
        TimerMode::SingleShot,
        Duration::from_millis(SHELL_OPEN_RETRY_MS),
        move || {
            if let Ok(app) = app2.try_borrow() {
                // 定时器要给下一轮用：这里只是借出克隆，不能在被借用时 start。
                let weak = app.self_weak.clone();
                drop(app);
                if let Some(weak) = weak {
                    schedule_shell_drain(weak, attempt + 1);
                }
            }
        },
    );
}

impl App {
    /// 当前标签导航到路径（压入历史）。
    fn navigate_active(&mut self, path: Option<PathBuf>) {
        self.navigate_active_reveal(path, None);
    }

    /// 同上，顺便记下加载完要选中的条目（「在文件夹中显示」）。
    fn navigate_active_reveal(&mut self, path: Option<PathBuf>, select: Option<PathBuf>) {
        self.flush_pending_search();
        self.state.active_mut().active_mut().navigate(path);
        self.sync_after_navigate();
        self.arm_reveal(select);
    }

    /// 排队一个「新标签打开」请求：当前 App 借用还没释放，不能在这里直接导航。
    ///
    /// 请求存进 `shell_open_queue`（App 字段），不要每次新建闭包捕获：连点两个
    /// 文件夹时，前一次的定时器会被后一次 `start` 直接覆盖，闭包里那条请求也
    /// 跟着消失（表现为「第二个打开了、第一个没了」）。
    fn queue_shell_tab(&mut self, path: Option<PathBuf>, select: Option<PathBuf>) {
        if self.shell_open_queue.len() >= SHELL_OPEN_QUEUE_MAX {
            logln!("!! 接管资源管理器：待打开请求过多，丢弃最旧的一条");
            self.shell_open_queue.remove(0);
        }
        self.shell_open_queue.push((path, select));
        let Some(weak) = self.self_weak.clone() else {
            // 会话刚建好还没登记 self_weak：此时肯定没有别的借用，直接执行。
            self.drain_shell_tabs();
            return;
        };
        self.shell_open_timer.stop();
        self.shell_open_timer.start(
            TimerMode::SingleShot,
            Duration::from_millis(0),
            move || schedule_shell_drain(weak.clone(), 0),
        );
    }

    /// 依次打开队列里的全部请求。
    fn drain_shell_tabs(&mut self) {
        // 先取出来：open_shell_tab 里若同步再排队，也不会打断这一轮。
        let queued = std::mem::take(&mut self.shell_open_queue);
        for (path, select) in queued {
            self.open_shell_tab(path, select);
        }
    }

    /// 资源管理器转来的打开请求：新开一个标签再导航，不覆盖用户正在看的目录。
    ///
    /// `select` 是「在文件夹中显示」带来的条目：列举结果回来时选中它并滚进
    /// 可视区（浏览器下载完点「打开所在文件夹」就是这条路）。
    fn open_shell_tab(&mut self, path: Option<PathBuf>, select: Option<PathBuf>) {
        // 路径已经不存在（刚被删/断开网盘）：不要开一个空标签，直接忽略。
        // 注意只对显式路径做检查；None 是「此电脑」，本来就该能打开。
        // 网络路径一律不在这里 stat：`\\wsl.localhost\...` 或断线的共享
        // 盘上 `is_dir()` 能把 UI 线程卡几十秒（窗口「未响应」），
        // 真不存在时列举自己会报错，和普通导航一致。
        if let Some(p) = path.as_ref() {
            // `is_remote_path` 同时覆盖 UNC 和**映射盘**（`O:\Video`）：它只用
            // `GetDriveTypeW` 查本机设备表，不碰网络，所以在界面线程上判断安全。
            if !crate::fs::dir::is_remote_path(p) && !p.is_dir() {
                logln!("!! 接管资源管理器：目标目录不可用，已忽略 {:?}", p);
                return;
            }
        }
        // 新标签继承的排序会被 sync_view_sort 按目标视图（普通/此电脑/回收站）
        // 覆盖成对应视图自己的状态，这里只用当前排序占个位。
        // 当前标签可能是设置页（没有目录），此时用全局排序做种子。
        let sort = self.state.active().active().sort;
        // inherit=false：path 为 None 就是真的要进「此电脑」视图，
        // 不能因为「新标签继承当前目录」的默认习惯被悄悄换成当前文件夹。
        self.state.new_tab_ex(path, sort, false);
        logv!(
            "接管资源管理器：新标签打开 {:?}（选中 {:?}）",
            self.state.active().active().path,
            select
        );
        // 与「普通导航」走完全相同的链路：sync_after_navigate 内部会
        // sync_view_sort → sync_tabs/nav/... → reload_active。
        // 这里不能用 sync_active_tab：那是「切换已有标签」用的，不发起列举，
        // 新标签会一直停在空状态（表现为「此文件夹为空」）。
        self.sync_after_navigate();
        self.arm_reveal(select);
        self.ui.invoke_scroll_tabs_to_end();
    }

    /// 记下「列表加载完要选中并滚进可视区」的条目。
    ///
    /// 列举是异步的：这里只存路径，`apply_list_result` 里换算成显示索引。
    /// 用 `scroll_to_paths` + `scroll_select` 这同一套机制（粘贴/新建文件夹
    /// 已经在用），不另开一条代码路径。
    fn arm_reveal(&mut self, select: Option<PathBuf>) {
        let Some(target) = select else { return };
        logv!("[reveal] 选中 {:?}", target);
        self.scroll_to_paths = vec![target];
        self.scroll_select = true;
        // 这类揭示都由聚焦面板发起；写死面板，免得沿用上一次跨栏拖放留下的记录。
        self.reveal_pane = Some(self.state.active().active_pane);
    }

    fn sync_after_navigate(&mut self) {
        // 换目录了：类型下拉里列的是上一个目录的类型，必须关掉。
        self.close_type_filter_menu();
        // 导航时取消挂起的延迟重命名：导航会换目录/选择集，原路径可能不再有效，
        // 否则几百毫秒后会在新页面里误弹出重命名。
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
        self.visible_cache_right = None;

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
        self.flush_pending_search();
        let tab = self.state.active();
        if !tab.active().can_go_back() {
            return;
        }
        let index = tab.active().history_index - 1;
        let before = tab.active().path.clone();
        let pane_index = tab.active_pane;
        let tab_id = tab.id;
        self.state.active_mut().active_mut().jump_to(index);
        self.after_history_jump(before.clone());
        // 后退落点正好是刚离开的那个目录的父目录：把那个目录选中。
        // 双击进去的目录一定在可视范围内（用户就在那一格上双击的），
        // 所以这里只做「选中 + 最小位移滚进视野」，不会跳顶。
        if let (Some(left), Some(now)) = (before, self.state.active().active().path.clone()) {
            if left
                .parent()
                .is_some_and(|p| crate::fs::dir::same_path_loose(p, &now))
            {
                self.pending_history_select = Some((tab_id, pane_index, left));
            }
        }
    }

    fn nav_forward(&mut self) {
        self.flush_pending_search();
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
        self.visible_cache_right = None;
        self.sync_nav();
        self.sync_status();
        self.sync_tabs();
        self.sync_file_list();
        push_scroll(&self.ui, pane_index, scroll);
    }

    /// 点面包屑跳转：回到某个上级目录后，把「我是从哪个文件夹上来的」
    /// 单选出来并滚进可视区 —— 与「上一级」(`nav_up`) 完全同一套收尾。
    ///
    /// 孩子取的是「被点那一段的后一段」，不是当前目录：连跳两级时当前目录
    /// 并不在被点目录的下一层，拿它去揭示永远选不中。
    fn navigate_breadcrumb(&mut self, target: Option<PathBuf>) {
        // 点的就是当前那一格：什么都不做（不重载、不滚、不压历史）。
        // 例外：正处在搜索页 —— 那时这一下以前会「重新进这个目录并清掉搜索词」，
        // 行为不变，别把用户退出搜索的路给堵掉。
        {
            let active = self.state.active().active();
            if target == active.path && active.filter.trim().is_empty() {
                return;
            }
        }
        let crumbs = self.ui.get_breadcrumbs();
        let segs: Vec<String> = (0..crumbs.row_count())
            .filter_map(|i| crumbs.row_data(i))
            .map(|seg| seg.path.to_string())
            .collect();
        let child = crumb_child_of(&segs, target.as_deref());
        if let Some(child) = child.as_ref() {
            logv!("[nav] crumb: select {:?}", child);
        }
        self.navigate_active_reveal(target, child);
    }

    fn nav_up(&mut self) {
        let Some(parent) = self.state.active().active().parent_path() else {
            return;
        };
        // 回到上级后，把「刚才所在的那个文件夹 / 盘符」单选出来并滚进可视区：
        // 连按向上键时视点不会丢，也顺手告诉用户自己是从哪儿上来的。
        // 列举是异步的，这里只挂目标（arm_reveal），由 apply_list_result 消费。
        let child = self.state.active().active().path.clone();
        if let Some(child) = child.as_ref() {
            logv!("[nav] up: select {:?}", child);
        }
        self.navigate_active_reveal(parent, child);
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

/// 全局物理按键状态（不管那个按键消息进了哪个窗口的队列）。
///
/// `key_down`（GetKeyState）只能看到本线程队列里的键；窗口刚被别的程序把
/// 焦点扔回来时，刚才那次按下是别人消费的，只能靠这个函数判断「键还按着没」。
#[cfg(windows)]
fn async_key_down(vk: i32) -> bool {
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;
    (unsafe { GetAsyncKeyState(vk) } as u16) & 0x8000 != 0
}

#[cfg(not(windows))]
fn async_key_down(_vk: i32) -> bool {
    false
}

/// 进程级时间基准（毫秒计数用）。
fn uptime_ms() -> u64 {
    static EPOCH: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    EPOCH
        .get_or_init(std::time::Instant::now)
        .elapsed()
        .as_millis() as u64
}

/// 最近一次「前台不是本进程」的时刻（ms，见 [`note_foreign_foreground`]）。
static LAST_FOREIGN_FG_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// 记一笔：此刻前台是别人的窗口。
///
/// 键盘事件的先后顺序不能信：启动器（stools）里按回车启动程序，它 `hide()`
/// 自己，Windows 把前台还给 Rove——但那次回车的**重复键**比 WM_ACTIVATE 更早
/// 进到我们的事件队列（实测日志：导航 19:48:01.492，Focused 却 19:48:01.493），
/// 屏障来不及挂上。所以改成看「刚刚前台还是别人吗」：40ms 轮询一直在采样，
/// 判断时序就与现实一致了。
#[cfg(windows)]
fn note_foreign_foreground() {
    use windows_sys::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowThreadProcessId};
    let hwnd = unsafe { GetForegroundWindow() };
    if hwnd.is_null() {
        return;
    }
    let mut pid: u32 = 0;
    unsafe { GetWindowThreadProcessId(hwnd, &mut pid) };
    if pid != std::process::id() {
        LAST_FOREIGN_FG_MS.store(uptime_ms(), std::sync::atomic::Ordering::Relaxed);
    }
}

#[cfg(not(windows))]
fn note_foreign_foreground() {}

/// 距上一次「前台是别人」是不是在 `ms` 毫秒之内。
fn foreign_foreground_within(ms: u64) -> bool {
    let last = LAST_FOREIGN_FG_MS.load(std::sync::atomic::Ordering::Relaxed);
    last != 0 && uptime_ms().saturating_sub(last) <= ms
}

const VK_SHIFT: i32 = 0x10;
const VK_CONTROL: i32 = 0x11;
const VK_MENU: i32 = 0x12;
/// 「提交键」：按下去就会触发动作的那几个（启动器/对话框里确认用）。
const VK_COMMIT: [i32; 4] = [0x0D, 0x20, 0x1B, 0x09];

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
                    self.visible_cache_right = None;
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
                    // 已经真正开始拖出：不要再塌缩选择集，也不要事后弹重命名。
                    self.pending_single_click = None;
                    self.cancel_rename_defer();
                    self.ui.invoke_set_drag_armed(false);
                    self.begin_drag_out(paths);
                }
            }
            _ => {}
        }
        // 行内右键：按下时只把菜单对象准备好，抬手才弹。菜单是独立窗口，
        // 会吃掉这次按下的抬手事件 —— 在按下时就弹，Slint 记下的「这一下
        // 落在哪个元素上」永远清不掉，之后任意一次按下都被派给同一行
        // （右键 A 却弹出 B 的菜单，而且换哪一行都一样）。
        if event.kind == PointerEventKind::Up && event.button == PointerEventButton::Right {
            if let Some(target) = self.pending_row_menu.take() {
                logln!("[menu] row right-up open");
                self.open_menu(target);
            }
            return;
        }
        if event.kind == PointerEventKind::Cancel {
            self.pending_row_menu = None;
        }
        if event.kind != PointerEventKind::Down {
            return;
        }
        // 点到条目就打断「空白双击」链条，免得「行上一下 + 空白一下」被凑成双击。
        self.blank_click = None;
        let Some(path) = self.state.active().active().display_entry(index).map(|e| e.path.clone()) else {
            return;
        };
        match event.button {
            PointerEventButton::Right => {
                logln!(
                    "[menu] row right-press idx={} path={} sel={}",
                    index,
                    path.display(),
                    self.state.active().active().selection.len()
                );
                // 右键打断「同一行连续按下」链条：菜单之后随手点一下名称只算第一次
                // 按下，不会直接进重命名。
                self.last_left_press_row = None;
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
                self.visible_cache_right = None;
                self.refresh_visible_rows();
                self.sync_status();
                self.pending_row_menu = Some(target);
            }
            PointerEventButton::Left => {
                self.pending_row_menu = None;
                // 这一下是不是「同一行的连续第二次按下」（见 `last_left_press_row`）。
                let second_press_on_row =
                    self.last_left_press_row.as_deref() == Some(path.as_path());
                self.last_left_press_row = Some(path.clone());
                // 新的一次按下：丢掉上一次未完成的「延迟塌缩单选」。
                self.pending_single_click = None;
                // 双击检测在 Rust 侧做（有效双击间隔内同一路径二次按下）：
                // 不依赖 Slint 的 double-clicked，行模型重建也不会打断判定。
                let now = std::time::Instant::now();
                if self.last_click.as_ref().is_some_and(|(p, t)| {
                    *p == path && t.elapsed().as_millis() < effective_double_click_ms() as u128
                }) {
                    // 第二次按下 = 双击 → 打开（并撤掉可能已挂起的延迟改名）。
                    self.last_click = None;
                    self.cancel_rename_defer();
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
                // 单选后（非双击、非 Ctrl/Shift）点击文件名 → 进入重命名，但要求这是
                // **同一行的连续第二次按下**（见 `last_left_press_row`）：
                // 第一次按下只是「选中并记下这一行」，双击打开在上面的双击判定里已经返回，
                // 所以走到这里的第二次按下才是改名意图，但仍要等一个双击间隔
                // （`schedule_rename`），否则快速双击名称会被抢成改名而打不开文件。
                // 因此「右键菜单之后随手点一下名称」永远是安全的。
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
                    if second_press_on_row {
                        self.schedule_rename(path.clone(), "cell-row");
                        return;
                    }
                    logv!(
                        "[rename] name press without previous press on this row, select only: {}",
                        path.display()
                    );
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
                    // 锚点在**显示序列**里找：`entries` 只有本地条目，搜索时
                    // 深层结果在 `deep_entries` 里，用 `entries` 会找不到锚点，
                    // Shift 点击就退化成单击（真机：搜索后按住 Shift 点第二个
                    // 文件毫无反应，Ctrl 点击却正常）。
                    let anchor = tab.anchor.clone();
                    let anchor_pos = anchor.and_then(|anchor| {
                        (0..tab.display.len())
                            .find(|&di| tab.display_entry(di).is_some_and(|e| e.path == anchor))
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
                self.visible_cache_right = None;
                self.refresh_visible_rows();
                self.sync_status();
            }
            _ => {}
        }
    }

    /// 任务 2.3：发起 OLE 拖放循环（阻塞至释放）。
    fn begin_drag_out(&mut self, paths: Vec<PathBuf>) {
        logln!(
            "[drag] begin n={} first={:?} pane={} sel={}",
            paths.len(),
            paths.first(),
            self.state.active().active_pane,
            self.state.active().active().selection.len()
        );
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

        // 「松开左键」那条消息被 OLE 的模态循环吃掉了（DoDragDrop 自己捕获鼠标直到
        // 松手），Slint 收不到 up，按下状态就一直卡着：拖完之后点哪儿，那一次点击都
        // 被当成「上一下的释放」——焦点切不过去，而且拖出前的多选会被塌缩成拖的
        // 那一行。补一条合成的 up 把状态复位（不影响拖放结果，up 里也不改选择集）。
        #[cfg(windows)]
        {
            reset_pointer_press(&self.ui);
            logv!("[drag] 合成 up 复位按下状态");
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
        let mut dropped_on_dir = false;
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
                dropped_on_dir = true;
            }
        }

        // 4. 防止原地拖放或拖入自身子目录
        if paths.iter().all(|p| p.parent() == Some(target_dir.as_path())) {
            return; // 同一目录下移动/复制，直接忽略
        }
        // 搜索状态下落到本栏空白处 = 目标就是搜索根目录：结果里子文件夹中的
        // 文件会被搬到根上，属于误操作，直接拒绝。落到具体文件夹行（上面
        // 命中 dir）仍然允许；别的分屏、别的程序完全不受影响。
        if !dropped_on_dir
            && self
                .state
                .active()
                .panes
                .get(target_pane)
                .is_some_and(|p| !p.filter.is_empty())
            && paths.iter().any(|p| p.starts_with(&target_dir))
        {
            logln!(
                "[drag] !! 搜索中拖回本栏被拒：{} 项 -> {:?}",
                paths.len(),
                target_dir
            );
            self.set_status_job(self.tr("搜索中不能拖放到当前分屏，请拖到具体的文件夹上"));
            return;
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
        // 拖放的目标栏由光标位置决定（见上面的 target_pane），揭示必须跟着它。
        self.expect_new_entries(&paths, &target_dir, target_pane);
        let kind = if is_move {
            JobKind::Move { sources: paths, dest_dir: target_dir }
        } else {
            JobKind::Copy { sources: paths, dest_dir: target_dir }
        };
        self.submit_job(kind);
    }

    /// 空白处单击的收尾：判定双击。规则与行上的双击一致（同一套系统间隔、
    /// 同一套 4px 抖动容差），并且只有「按下列抬」之间没怎么动才算点击 ——
    /// 拖着框选一大片之后松手不该触发导航。
    fn on_blank_click(&mut self, x: f32, y: f32) {
        const SLOP: f32 = 4.0;
        let moved = match self.blank_press.take() {
            Some((px, py)) => (px - x).abs() > SLOP || (py - y).abs() > SLOP,
            // 没收到按下（比如从行上按下、空白处松开）：不当作点击。
            None => true,
        };
        if moved {
            self.blank_click = None;
            return;
        }
        // 正在改名时不导航：那会儿空白处的点击常是「放弃改名」，跳走会连着丢上下文。
        if !self.ui.get_rename_path().is_empty() {
            self.blank_click = None;
            return;
        }
        let now = std::time::Instant::now();
        let pane = self.state.active().active_pane;
        let is_double = self.blank_click.as_ref().is_some_and(|(t, lx, ly, lp)| {
            *lp == pane
                && t.elapsed().as_millis() < effective_double_click_ms() as u128
                && (*lx - x).abs() <= SLOP
                && (*ly - y).abs() <= SLOP
        });
        if is_double {
            self.blank_click = None;
            logv!("[nav] blank double-click at=({x:.0},{y:.0}) pane={pane} -> up");
            self.nav_up();
        } else {
            self.blank_click = Some((now, x, y, pane));
        }
    }

    fn background_pointer_event(&mut self, event: &PointerEvent, x: f32, y: f32) {
        // 左键抬手：判「双击空白 = 回上一级」。只在这里判，是因为空白层在
        // Slint 侧被 pointer-event 全接管了，`double-clicked` 不会触发。
        if event.kind == PointerEventKind::Up && event.button == PointerEventButton::Left {
            self.on_blank_click(x, y);
            return;
        }
        if event.kind != PointerEventKind::Down {
            return;
        }
        if event.button == PointerEventButton::Left {
            self.blank_press = Some((x, y));
        }
        // 每次新按下都复位：上一下的兜底标记不能留到下一轮。
        self.bg_hit_row = false;
        // 兜底：按下点其实正压在某个条目上，却落到了空白层。
        //
        // 实测（ROVE_LOG=2）：原生右键菜单关闭后的第一次点击、以及排序后刷新
        // 完的第一次点击，会绕过行自己的 TouchArea 直接进到这里 —— 日志里
        // `at=(860,378)` 落在第 12 行内，紧接着同一位置的第二次点击才记
        // `[select] row down index=12`。玩家观感就是「第一次点击被吞了」。
        // 这里按单击选中处理，行为与行上的点击一致（不含拖出与改名意图）。
        if event.button == PointerEventButton::Left {
            if let Some(di) = self.row_index_at(x, y) {
                let Some(path) = self
                    .state
                    .active()
                    .active()
                    .display_entry(di)
                    .map(|e| e.path.clone())
                else {
                    return;
                };
                // 双击判定必须和行上的点击共用同一套状态：否则「被吃掉的第一下
                // + 立刻双击」的第二下会被当成第一次按下，只是选中而不打开。
                self.last_left_press_row = Some(path.clone());
                self.bg_hit_row = true;
                // 这一下是「行」的点击（只是事件落到了空白层），别让它参与空白双击判定。
                self.blank_press = None;
                self.blank_click = None;
                let now = std::time::Instant::now();
                if self.last_click.as_ref().is_some_and(|(p, t)| {
                    *p == path && t.elapsed().as_millis() < effective_double_click_ms() as u128
                }) {
                    self.last_click = None;
                    self.cancel_rename_defer();
                    logv!("[select] background hit row {} at=({:.0},{:.0}) -> 双击打开", di, x, y);
                    self.activate_row(di);
                    return;
                }
                self.last_click = Some((path.clone(), now));
                self.pending_single_click = None;
                let ctrl = key_down(VK_CONTROL);
                let tab = self.state.active_mut().active_mut();
                if ctrl {
                    if !tab.selection.remove(&path) {
                        tab.selection.insert(path.clone());
                    }
                    tab.anchor = Some(path);
                } else {
                    tab.selection.clear();
                    tab.selection.insert(path.clone());
                    tab.anchor = Some(path);
                }
                self.visible_cache = None;
                self.visible_cache_right = None;
                logv!(
                    "[select] background hit row {} at=({:.0},{:.0}) ctrl={} sel={}",
                    di,
                    x,
                    y,
                    ctrl,
                    self.state.active().active().selection.len()
                );
                self.refresh_visible_rows();
                self.sync_status();
                return;
            }
        }
        // 点了空白：连续按下链断了，下次点名称只算第一次按下；挂起的改名也撤掉。
        self.last_left_press_row = None;
        self.cancel_rename_defer();
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
                    "[select] rubber-band down additive={} base={} at=({:.0},{:.0}) rows={} view={}",
                    additive,
                    self.rubber_band_base.as_ref().map_or(0, |b| b.len()),
                    x,
                    y,
                    self.state.active().active().display.len(),
                    self.config.view_mode
                );
                if !additive {
                    let tab = self.state.active_mut().active_mut();
                    tab.selection.clear();
                    tab.anchor = None;
                    self.visible_cache = None;
                    self.visible_cache_right = None;
                    self.refresh_visible_rows();
                    self.sync_status();
                }
            }
            PointerEventButton::Right => {
                self.background_right_press();
                // 菜单推迟到**抬手**再弹（`open_deferred_background_menu`）。
                //
                // 在按下这一刻就弹菜单的话：菜单是独立窗口，它会吃掉属于这次
                // 按下的抬手事件，Slint 记下的「这一下落在哪个元素上」就再也
                // 清不掉 —— 之后任意一次按下都会被派给空白层，表现为右键菜单
                // （排序方式在这里）之后的第一下点击不选中、第一下拖动变框选，
                // 而且等多久都不会自愈。
                self.deferred_bg_menu = Some(MenuScope::Background);
            }
            _ => {}
        }
    }

    /// 空白处右键抬手：真正弹出推迟的那份菜单。
    fn open_deferred_background_menu(&mut self) {
        let Some(target) = self.deferred_bg_menu.take() else {
            return;
        };
        // 原生菜单返回即「菜单已收起」，此时 Shell 的新建动词多半已经执行
        // （或即将通过事件循环执行），所以推迟 250ms 主动重载一次目录 ——
        // 比等 1 秒的 mtime 轮询更快，也不依赖目录 mtime 是否被 Windows 及时更新。
        self.open_menu(target);
        if self.shell_new_watch.is_some() {
            self.shell_new_reload_at =
                Some(std::time::Instant::now() + std::time::Duration::from_millis(250));
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

    /// 点 (x, y)（flick 内容坐标）落在哪个显示条目上。
    fn row_index_at(&self, x: f32, y: f32) -> Option<usize> {
        let geom = RubberGeom::read(&self.ui, self.state.active().active_pane == 1);
        let tab = self.state.active().active();
        for di in 0..tab.display.len() {
            let (ix, iy, iw, ih) = geom.rect(di);
            if ix <= x && x < ix + iw && iy <= y && y < iy + ih {
                return Some(di);
            }
        }
        None
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
        // 这一下按下其实压在条目上（走了兜底），刚刚已经选中它了：
        // 空白层的「单击＝点空白取消选择」不能在这里再跑一遍。
        if !dragged && self.bg_hit_row {
            self.bg_hit_row = false;
            self.rubber_band_base = None;
            return;
        }
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
            self.visible_cache_right = None;
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
            self.visible_cache_right = None;
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
    if let Some(app) = SESSION.with(|slot| slot.borrow().clone()) {
        if let Ok(mut app) = app.try_borrow_mut() {
            app.run_pending_global_search();
        }
    }

    // 采样「前台是不是别人」：给按键处理判「这次按键是不是刚被别的程序扔过来的」。
    note_foreign_foreground();

    // 标签拖拽没人收尾时兜底复位。只在「物理左键已松开」时动手：正常拖拽
    // 松手时 phase 2 会先把状态取走，这里就不会重复动作；反过来，如果那一次
    // `up` 根本没送到，界面上会一直留着落点竖线 + 半透明标签。
    {
        let mut app = app.borrow_mut();
        if app.tab_drag.is_some() && !app.ui.get_mouse_left_pressed() {
            app.cancel_tab_drag("poll");
        }
    }

    // 读失败的目录（网络共享慎一下）自动重试，同样搭这趟 40ms 轮询。
    app.borrow_mut().maybe_retry_failed_listing();

    // 右键菜单的健康检查（只读几个原子量）。搭这趟 40ms 轮询，是为了被卡死的
    // 扩展拖坏进程后能在一秒内自我修复，而不是等 15 秒那条周期定时器。
    app.borrow_mut().check_menu_health();

    // 压缩软件「更新」：后台线程扫完就把结果并回配置。搭这趟 40ms 轮询，
    // 不额外起定时器，也不让界面线程去碰开始菜单。
    if app.borrow().archive_scan_pending {
        if let Some(found) = crate::sys::archive::take_result(app.borrow().archive_scan_gen) {
            let mut app = app.borrow_mut();
            app.archive_scan_pending = false;
            app.apply_archive_tool(found);
        }
    }

    // 搜索框防抖：攒够 150ms 再跑一次全目录匹配。
    // 用 40ms 轮询判到期，省一个单独的定时器；到期前用户又敲了字会重新计时。
    let pending_search = app.borrow().pending_search.clone();
    if let Some((text, at)) = pending_search {
        if at.elapsed() >= Duration::from_millis(150) {
            let mut app = app.borrow_mut();
            app.pending_search = None;
            // 期间导航/切标签可能已经把搜索框改写掉了：文本对不上就丢弃。
            if app.ui.get_filter_text() == text {
                app.apply_search_query(&text);
            }
        }
    }

    // 后台线程启动终端 / 自定义动作失败（目录不存在、进程起不来）在这里显示。
    // 逐条借一次 App，避免和下面 `borrow_mut` 抢借用。
    for message in ops::take_launch_errors() {
        app.borrow().set_status_job(message);
    }

    // 主窗 HWND 登记、无边框拖拽钩子、最大化还原、首次启动居中，已全部移至
    // on_winit_window_event 的首次 RedrawRequested（窗口 realize 后）统一应用，
    // 见 apply_deferred_geometry（由 setup_keyboard 注册）。
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
                // 右键菜单收起后主动重载一次目录（见 shell_new_reload_at）：
                // 「新建 → 文本文档」是 Shell 自己执行的，Rove 只能靠列举 diff 认出它。
                if let Some(at) = app.shell_new_reload_at {
                    if std::time::Instant::now() >= at {
                        app.shell_new_reload_at = None;
                        app.reload_active();
                    }
                }
                while let Ok(result) = app.list_rx.try_recv() {
                    app.apply_list_result(result);
                }
                while let Ok(result) = app.deep_rx.try_recv() {
                    app.apply_deep_result(result);
                }
                // 拼音缓存：先收后台结果，再给还没算好的面板派活。
                // 每 40ms 只做几次标志位判断，不需要时才不产生任何开销。
                loop {
                    match app.pinyin_rx.try_recv() {
                        Ok(result) => app.apply_pinyin_result(result),
                        Err(mpsc::TryRecvError::Empty) => break,
                        Err(mpsc::TryRecvError::Disconnected) => break,
                    }
                }
                app.request_pinyin_jobs();
                while let Ok(result) = app.dir_size_rx.try_recv() {
                    app.apply_dir_size_result(result);
                }
                loop {
                    match app.mtime_probe_rx.try_recv() {
                        Ok(probed) => app.apply_dir_mtime_probe(probed),
                        Err(mpsc::TryRecvError::Empty) => break,
                        // 探测线程已经没了：解掉「在跑」标记，否则目录自动刷新永久停摆。
                        Err(mpsc::TryRecvError::Disconnected) => {
                            logln!("!! mtime 探测线程已退出");
                            app.mtime_probe_busy = false;
                            break;
                        }
                    }
                }
                // 地址栏路径校验结果：只认最后一次回车，且用户中途没换过目录。
                while let Ok(probe) = app.path_probe_rx.try_recv() {
                    if probe.seq != app.path_probe_seq {
                        continue;
                    }
                    let still_here =
                        app.state.active().active().path.as_ref() == probe.from.as_ref();
                    match probe.outcome {
                        Ok(path) if still_here => app.navigate_active(Some(path)),
                        // 用户已经在别的目录里翻文件了，别把他拽回来。
                        Ok(_) => {}
                        Err(()) => {
                            let label = probe.label;
                            app.set_status_job(app.trf("路径不存在：{}", &[label]));
                        }
                    }
                }

                // 侧栏后台探测结果（盘符容量 / 回收站 / 网络位置）：应用到内存并重绘。
                loop {
                    match app.sidebar_probe_rx.try_recv() {
                        Ok(probe) => {
                            app.sidebar_probe_busy = false;
                            app.drives = probe.drives;
                            app.recycle_stats = probe.stats;
                            app.net_wnet = probe.wnet;
                            app.net_places = probe.places;
                            app.sidebar_fingerprint = 0;
                            app.sync_sidebar();
                        }
                        Err(mpsc::TryRecvError::Empty) => break,
                        // 探测线程已经没了（理论上不会）：别让「在跑」标记永久卡住。
                        Err(mpsc::TryRecvError::Disconnected) => {
                            logln!("!! 侧栏探测线程已退出");
                            app.sidebar_probe_busy = false;
                            break;
                        }
                    }
                }

                // 被 worker 判定过期而跳过的请求：在这里一次性从 pending 里摘掉。
                // 留着会让滚回来的行以为「还在解」而不再发请求（永久空白）。
                let skipped = take_thumb_skipped();
                let skip_overflow =
                    THUMB_SKIP_OVERFLOW.swap(false, std::sync::atomic::Ordering::AcqRel);
                if !skipped.is_empty() {
                    for k in skipped {
                        app.cancel_thumbnail(k);
                    }
                }
                if skip_overflow {
                    // 兜底：批次被丢过，逐个 key 清已经不可靠 —— 直接按「当前窗口」
                    // 重建 pending（不在窗口里的都不算在解，滚回来会重新发请求）。
                    let wanted = thumb_wanted()
                        .lock()
                        .map(|w| w.clone())
                        .unwrap_or_default();
                    app.thumb_pending
                        .retain(|k| k.px_size == SIDEBAR_ICON_PX || wanted.contains(k));
                    app.thumb_needs_refresh = true;
                    logln!("[thumb] !! 跳过批次溢出，按窗口重建 pending");
                }
                // 核心修复：缩略图批量到达后，统一在主线程刷新一次，避免 653 张
                // 连续 store 触发 653 次全量行重建导致掉帧。
                if app.thumb_needs_refresh {
                    app.thumb_needs_refresh = false;
                    app.visible_cache = None;
                    app.visible_cache_right = None;
                    app.refresh_visible_rows();
                }
                // 侧栏图标（32px）也走同一条缩略图管道：只有它真的更新了才重绘侧栏。
                // 以前这里无条件 `sidebar_fingerprint = 0; sync_sidebar()`，
                // 每批缩略图（最多 25 次/秒）都要跑一遍盘符/回收站/WNet 枚举。
                if app.sidebar_icon_dirty {
                    app.sidebar_icon_dirty = false;
                    app.sidebar_fingerprint = 0;
                    app.sync_sidebar();
                }

                // 核心修复：用户空闲时静默重排，绝不打断当前操作。
                //
                // 两件事共用同一套防抖（拖拽中 / 列表加载中 / 800ms 内点过东西
                // 都算「正在操作」）：
                //   · 目录大小回传后按大小重排；
                //   · 搜索流式结果的重排（期间列表只追加，见 `deep_sort_pending`）。
                if app.size_sort_pending
                    || app
                        .state
                        .tabs
                        .iter()
                        .any(|t| t.panes.iter().any(|p| p.deep_sort_pending))
                {
                    let is_interacting = app.drag_pending.is_some()
                        || app.ui.get_file_loading()
                        || app
                            .last_click
                            .as_ref()
                            .is_some_and(|(_, t)| t.elapsed().as_millis() < 800)
                        || app
                            .last_scroll
                            .is_some_and(|t| t.elapsed().as_millis() < 600);

                    if !is_interacting {
                        let mut resorted = false;
                        if app.size_sort_pending {
                            app.size_sort_pending = false;
                            let tab = app.state.active_mut();
                            for pane in &mut tab.panes {
                                if pane.sort.key == SortKey::Size {
                                    pane.resort();
                                    pane.rebuild_display();
                                    resorted = true;
                                }
                            }
                        }
                        // 搜索：把流式期间追加的结果按得分排一次。
                        for tab in &mut app.state.tabs {
                            for pane in &mut tab.panes {
                                if pane.deep_sort_pending {
                                    pane.deep_sort_pending = false;
                                    pane.rebuild_display();
                                    resorted = true;
                                }
                            }
                        }
                        if resorted {
                            app.visible_cache = None;
                            app.visible_cache_right = None;
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
                    // 探测在线程里跑：有变化才会经 40ms 轮询重绘侧栏。
                    app.request_sidebar_probe();
                    app.maybe_idle_rescan();
                    app.maybe_unload_index();
                    app.log_memory_snapshot();
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

        // 窗口焦点变化。
        //
        // 获得焦点时，键盘上可能还按着「提交键」：启动器里按回车启动程序、
        // 资源管理器里双击后回车、别程序的窗口把自己藏起来把焦点还回来……
        // 那次按下属于上一个窗口，不能让 Rove 当成自己的命令。
        // 实测：启动器（stools）里按回车启动程序，焦点回到 Rove，
        // 那次回车直接把之前选中的文件夹打开了。
        //
        // 只在「焦点回来的一瞬确实有提交键还按着」时才上锁：否则会无端
        // 吞掉关对话框后立刻敲下的第一个键。解锁靠那一次松键（或 400ms 兜底），
        // 鼠标点进来也立即解锁。
        if let WindowEvent::Focused(focused) = event {
            // 取证：窗口一秒钟内反复失焦/获焦时，Slint 会清空悬停态、点击还会
            // 落到正在重建的元素上 —— 表现就是「按钮闪烁 + 点了没反应」。
            // 只在真的抖起来时写一行（level 1）。
            {
                use std::sync::atomic::Ordering;
                let now = now_ms();
                if crate::sys::menu::recently_active(1_500) {
                    // 原生菜单刚抢过 / 还过前台：这时的失焦/获焦是用户右键了，
                    // 不是窗口在抖，重新起窗别算进来。
                    FOCUS_WINDOW_MS.store(now, Ordering::Relaxed);
                    FOCUS_COUNT.store(0, Ordering::Relaxed);
                } else if now.saturating_sub(FOCUS_WINDOW_MS.load(Ordering::Relaxed)) > 3000 {
                    FOCUS_WINDOW_MS.store(now, Ordering::Relaxed);
                    FOCUS_COUNT.store(0, Ordering::Relaxed);
                } else {
                    let n = FOCUS_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
                    if n >= 4
                        && now.saturating_sub(FOCUS_LOGGED_MS.load(Ordering::Relaxed)) >= 3000
                    {
                        FOCUS_LOGGED_MS.store(now, Ordering::Relaxed);
                        logln!(
                            "!! [focus] 窗口 3s 内失焦/获焦 {n} 次（悬停会闪、点击会丢），刚收到 focused={focused}"
                        );
                    }
                }
            }
            if let Some(focus_app) = key_app.upgrade() {
                let mut app = focus_app.borrow_mut();
                if *focused {
                    let held: Vec<i32> = VK_COMMIT
                        .iter()
                        .copied()
                        .filter(|vk| async_key_down(*vk))
                        .collect();
                    // 提交键还按着 —— 典型现场。没按着也给 120ms 的短屏障：
                    // 人从点鼠标到敲下一个键至少也要 200ms，短屏障不会误伤，
                    // 却能拦住“焦点刚回来紧跟一个按键”的任何变体。
                    let ms = if held.is_empty() { 120 } else { 400 };
                    app.keys_block_until =
                        Some(std::time::Instant::now() + std::time::Duration::from_millis(ms));
                    app.keys_block_logged = false;
                    if !held.is_empty() {
                        logln!(
                            "[keys] focused with commit key held (vk={:?}), block keys until release",
                            held
                        );
                    }
                } else {
                    // 失焦：取消挂起的延迟重命名，否则切到别的应用后后台突然弹出 rename 输入框。
                    app.cancel_rename_defer();
                    // 窗口失焦时左键的「松开」可能送到别的窗口，这里先清掉按住态，
                    // 否则拖选贴边自动滚动的定时器会在后台一直推进选中范围。
                    app.ui.set_mouse_left_pressed(false);
                    // 同一原因：标签拖拽也收不到那一次 `up`，状态得一并清掉。
                    app.cancel_tab_drag("blur");
                }
            }
            return EventResult::Propagate;
        }

        // 鼠标左键按住状态回填给 UI：Slint 的 TextInput 拖选期间抓住鼠标，
        // 任何 TouchArea 都收不到事件，重命名输入框的「拖选贴边自动滚动」
        // 定时器只能从这里判断拖选是否仍在进行（松开即停）。
        // 同时：用户用鼠标点进来就是主动接管键盘，立刻解除激活屏障。
        if let WindowEvent::MouseInput {
            state,
            button,
            ..
        } = event
        {
            if let Some(btn_app) = key_app.upgrade() {
                let mut app = btn_app.borrow_mut();
                if matches!(state, ElementState::Pressed) {
                    app.keys_block_until = None;
                    // 鼠标点进窗口 = 用户亲自在用这台窗口，此后敲的键都是给它的：
                    // 清掉「前台刚是别人」的标记，免得紧接着的回车被当成上一家的尾巴。
                    LAST_FOREIGN_FG_MS.store(0, std::sync::atomic::Ordering::Relaxed);
                }
                if *button == MouseButton::Left {
                    app.ui
                        .set_mouse_left_pressed(matches!(state, ElementState::Pressed));
                }
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
                drop(app);
                // 尺寸变化后强制下一轮 40ms 轮询重建可见行：行模型里的
                // name-truncated（浮层弹出依据）是按旧几何算的，行内文字的
                // 省略号由 Slint 实时画、两者会割裂（表现为「浮层按旧窗口宽
                // 判断截断」）。置哨兵值让轮询的几何比较必定命中一次。
                evt_app.borrow_mut().list_geom = (-1.0, 0, -1.0, false, -1.0, -1.0);
            }
            return EventResult::Propagate;
        }

        // 松键：屏障靠这一次松键解除（用户按住的那个键终于抬起来了）。
        if let WindowEvent::KeyboardInput {
            event: KeyEvent {
                state: ElementState::Released,
                ..
            },
            ..
        } = event
        {
            if let Some(up_app) = key_app.upgrade() {
                let mut app = up_app.borrow_mut();
                if app.keys_block_until.is_some() {
                    app.keys_block_until = None;
                    app.keys_block_logged = false;
                    logv!("[keys] key released, block lifted");
                }
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
        // 主屏障：焦点刚从别的程序手里回来（≤250ms）时，回车 / 空格不算我们的。
        //
        // 用 40ms 轮询采到的前台信息判定，而不是 winit 的 Focused 事件：
        // 实测（stools 里按回车启动程序）那次回车的重复键比 Focused 更早进队列，
        // 屏障根本来不及挂上，文件夹就被打开了。
        if matches!(
            logical_key,
            Key::Named(slint::winit_030::winit::keyboard::NamedKey::Enter)
                | Key::Named(slint::winit_030::winit::keyboard::NamedKey::Space)
        ) && foreign_foreground_within(150)
        {
            logln!("!! [keys] swallow commit key released by another app's foreground");
            return EventResult::PreventDefault;
        }
        // 激活屏障期内（刚被外部程序把焦点扔过来）不接受任何按键：
        // 这个回车/空格属于上一个窗口那次操作，不是给 Rove 的。
        if let Some(until) = app.keys_block_until {
            if std::time::Instant::now() < until {
                if !app.keys_block_logged {
                    app.keys_block_logged = true;
                    logln!("!! [keys] swallow key right after focus gain");
                }
                return EventResult::PreventDefault;
            }
            app.keys_block_until = None;
            app.keys_block_logged = false;
        }
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

        // 类型筛选下拉开着：Esc 只关下拉（资源管理器同款），
        // 不要继续落到「Esc 清空选择」这类全局动作上。
        if app.type_filter_pane.is_some()
            && matches!(
                logical_key,
                Key::Named(slint::winit_030::winit::keyboard::NamedKey::Escape)
            )
        {
            app.close_type_filter_menu();
            return EventResult::PreventDefault;
        }

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
                "open" | "delete" | "delete_perm" | "rename" | "paste" | "copy" | "cut" | "undo" | "redo"
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
            "nav_back" => app.nav_back(),
            "nav_forward" => app.nav_forward(),
            "nav_up" => app.nav_up(),
            "refresh" => app.user_refresh_active(),
            "focus_search" => {
                // 进入搜索框前取消挂起的延迟重命名，否则 rename 输入框稍后弹出、
                // 抢走搜索框焦点。
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
            "undo" => app.history_action(false),
            "redo" => app.history_action(true),
            "select_all" => app.select_all(),
            "close_tab" => app.close_tab_or_unsplit(),
            _ => return EventResult::Propagate,
        }

        EventResult::PreventDefault
    });
}

/// 延后重试的上限与间隔。
///
/// 借用被占用的时间通常只是「这一帧的 sync 阶段」，几毫秒内必然放开；
/// 留 60 次 × 16ms ≈ 1 秒余量，超了就放弃本次处理（丢的只是折叠状态或
/// 一次搜索词重算，下一次交互还会再来）。**不能**无限 0ms 重试：那会在
/// 借用长期被占时变成忙等 + 日志刷屏。
const DEFER_TRIES: u32 = 60;
const DEFER_DELAY_MS: u64 = 16;

/// Slint → Rust 回调里安全地拿 App 可变借用。
///
/// 为什么不能直接 `borrow_mut()`：写界面属性会**同步**派发 Slint 的 `changed`
/// 回调，而这些写操作常常发生在 40ms 轮询定时器持着 `borrow_mut()` 的区间里
/// （那个闭包从取借用一直用到结尾，中间还有一大堆 `sync_*` 界面写操作）。
/// 一旦撞上就是 RefCell 双借用 → panic → 进程 abort，用户看到的是
/// 「启动就崩、连窗口都不出」。取不到借用就延后重试，而不是 abort。
fn defer_app_mut(
    what: &'static str,
    app: &Rc<RefCell<App>>,
    action: impl Fn(&mut App) + 'static,
) {
    attempt_app_mut(what, app, Rc::new(action), DEFER_TRIES);
}

fn attempt_app_mut<F: Fn(&mut App) + 'static>(
    what: &'static str,
    app: &Rc<RefCell<App>>,
    action: Rc<F>,
    tries: u32,
) {
    if let Ok(mut app) = app.try_borrow_mut() {
        action(&mut app);
        return;
    }
    if tries == 0 {
        logln!("!! {what} 回调连续 {} 次命中 App 借用中，放弃本次处理", DEFER_TRIES);
        return;
    }
    if tries == DEFER_TRIES {
        // 只记第一次：借用占着的那段时间里每次重试都会失败，逐次记就是刷屏。
        // 延后重试是**设计行为**（Slint 写属性会同步派发回调），不是异常，
        // 所以留在详细级 —— 只有「重试到底还是失败」才值得报警（见上面那条）。
        logv!("[defer] {what} 回调命中 App 借用中，延后重试");
    }
    let weak = Rc::downgrade(app);
    slint::Timer::single_shot(Duration::from_millis(DEFER_DELAY_MS), move || {
        if let Some(app) = weak.upgrade() {
            attempt_app_mut(what, &app, action, tries - 1);
        }
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
            app.borrow_mut().flush_pending_search();
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
                    logv!(
                        "[tab] click id={} dropped ({}ms after drag)",
                        id,
                        at.elapsed().as_millis()
                    );
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
                // 切标签会换掉搜索框的内容：旧标签挂起的搜索词先结算。
                app.flush_pending_search();
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
            // 与 Ctrl+W 同一条路：最后一个标签 = 结束会话（仅保留守护进程）。
            app.close_tab_by_id(id as u32);
        });
    }

    {
        let app = Rc::downgrade(app);
        upgrade().on_tab_context_requested(move |id| {
            let Some(app) = app.upgrade() else { return };
            app.borrow_mut().show_tab_context_menu(id as u32);
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
            app.borrow_mut().navigate_breadcrumb(target);
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
        upgrade().on_crumb_dbg(move |view_w, content_w, content_x, n, pref_w| {
            logv!(
                "[crumb] n={} view={:.1} content={:.1} x={:.1} pref={:.1}",
                n, view_w, content_w, content_x, pref_w
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
        let overlay_right_app = app.clone();
        let hover_app = app.clone();
        let overlay_hover_app = app.clone();
        upgrade().on_file_row_pointer_event(move |index, event, x, y, on_name| {
            let Some(app) = app.upgrade() else { return };
            app.borrow_mut()
                .row_pointer_event(index as usize, &event, x, y, on_name);
        });
        upgrade().on_file_name_overlay_right_clicked(move |pane, owner, hit, x, y| {
            let Some(app) = overlay_right_app.upgrade() else { return };
            let mut app = app.borrow_mut();
            let pane = pane.max(0) as usize;
            // `hit` 是浮层下面那一刻真正压着的行（-1 = 空白）。浮层向下伸出
            // 自己的格子是常态，落在别处时要按**那里的**行/空白处理，
            // 不能一律当成浮层主人那一行（那正是「右键别的文件却弹出它的菜单」）。
            let (expand, hover) = app.overlay_indices(pane);
            logln!(
                "[menu] overlay right-click pane={} owner={} hit={} expand={} hover={} at=({:.0},{:.0})",
                pane,
                owner,
                hit,
                expand,
                hover,
                x,
                y
            );
            if hit < 0 {
                // 浮层盖住空白：按「空白」处理。这个回调本身就在抬手时刻，
                // 所以直接弹（不再推迟到下一次抬手）。
                // 先切到浮层所在的那一栏：空白菜单的目录快照 / 「新建」都按
                // 活动栏算，栏没切就会拿错目录。
                if app.state.active().active_pane != pane {
                    app.focus_pane(pane);
                }
                app.background_right_press();
                app.open_deferred_background_menu();
                return;
            }
            // 主人是谁由 Slint 直接告诉我们（`owner`）。原先是回调到达时按
            // 「悬停优先」再推一遍，而那一刻悬停索引可能已经被清成 -1 或换了人 ——
            // 推错就是「右键 A 弹出 B」。
            let under = app.overlay_hit_row(pane, hit as usize);
            if owner != hit {
                // 落点压在别的格子上：完全按行内右键走。
                if under.is_some() {
                    app.row_right_click(pane, hit as usize);
                }
                return;
            }
            let Some(path) = app.overlay_hit_row(pane, owner as usize) else {
                return;
            };
            // 浮层吃掉了这次按下：行那边不会收到 Up，残留的拖拽手势要在这里清掉，
            // 否则它会在之后某次移动里被当成「拖出这个文件」。
            app.drag_pending = None;
            app.ui.invoke_set_drag_armed(false);
            // 菜单的操作对象是「活动栏的选中集」，所以先把浮层所在的栏切过去。
            if app.state.active().active_pane != pane {
                app.focus_pane(pane);
            }
            // 右键打断「同一行连续按下」链条（与行内右键一致）。
            app.last_left_press_row = None;
            if !app.state.active().active().selection.contains(&path) {
                let tab = app.state.active_mut().active_mut();
                tab.selection.clear();
                tab.selection.insert(path.clone());
                tab.anchor = Some(path.clone());
            }
            app.deferred_bg_menu = None;
            app.menu_anchor_path = Some(path.clone());
            let target = MenuScope::Selection(app.state.active().active().selected_paths());
            logln!(
                "[menu] overlay own-row right-click path={} sel={}",
                path.display(),
                app.state.active().active().selection.len()
            );
            app.menu_scope = Some(target.clone());
            app.visible_cache = None;
            app.visible_cache_right = None;
            app.refresh_visible_rows();
            app.sync_status();
            app.open_menu(target);
        });
        upgrade().on_file_name_overlay_clicked(move |pane, owner, hit, x, y| {
            let Some(app) = overlay_app.upgrade() else { return };
            let mut app = app.borrow_mut();
            let pane = pane.max(0) as usize;
            let (expand, hover) = app.overlay_indices(pane);
            logln!(
                "[click] overlay click pane={} owner={} hit={} expand={} hover={} at=({:.0},{:.0})",
                pane,
                owner,
                hit,
                expand,
                hover,
                x,
                y
            );
            // 同右键：落点在别的格子上时按那一行处理（单击选中），不做重命名。
            if hit < 0 {
                logln!("[click] overlay click on blank, ignored");
                return;
            }
            let under = app.overlay_hit_row(pane, hit as usize);
            if owner != hit {
                if under.is_some() {
                    app.row_left_click_select(pane, hit as usize);
                }
                return;
            }
            // 浮层里的 TouchArea 吃掉了点击，格子那边收不到，所以这里要自己处理：
            //  · 该文件还没被选中 → 先选中它（单击选中，与点图标一致）；
            //  · 已经是唯一选中项 → 进入重命名（原有行为）。
            // 不做这一步的话，仅因悬停而弹出的浮层被单击后会毫无反应。
            //
            // 主人用 Slint 传来的 `owner`（理由同右键那条：事后重推会推错）。
            let target = owner.max(0) as usize;
            let Some(path) = app.overlay_hit_row(pane, target) else {
                return;
            };
            if app.state.active().active_pane != pane {
                app.focus_pane(pane);
            }
            // 双击检测：浮层盖在名称区上，格子自己的 TouchArea 收不到这些点击，
            // 所以双击打开必须在这里自己判（与 row_pointer_event 同一套机制：
            // 有效双击间隔内同一路径第二次点击 = 打开）。
            // 不做的话，长文件名在图标视图里双击会先选中、再进重命名，
            // 而短文件名双击是打开文档，两者不一致。
            let now = std::time::Instant::now();
            if app.last_click.as_ref().is_some_and(|(p, t)| {
                *p == path && t.elapsed().as_millis() < effective_double_click_ms() as u128
            }) {
                app.last_click = None;
                if app.state.active().active().selection.len() != 1
                    || !app.state.active().active().selection.contains(&path)
                {
                    let tab = app.state.active_mut().active_mut();
                    tab.selection.clear();
                    tab.selection.insert(path.clone());
                    tab.anchor = Some(path.clone());
                }
                app.cancel_rename_defer();
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
                tab.anchor = Some(path.clone());
                // 这一下也算「在本行按下」：否则浮层上的第一次点击不记帐，
                // 下一次点击仍被判成第一次按下，要点三下才进重命名。
                app.last_left_press_row = Some(path.clone());
                app.visible_cache = None;
                app.visible_cache_right = None;
                app.refresh_visible_rows();
                app.sync_status();
                return;
            }
            // 已经是唯一选中项：不要当场进重命名。与行内的名称点击一致，
            // 先挂一个延时，留出双击窗口：窗口内再来一击 = 双击打开（在上面
            // 已处理），否则窗口过后才进重命名。
            // 当场 start-rename 的话，重命名输入框会立刻顶掉浮层，
            // 第二次点击落到输入框上，双击打开就永远做不到。
            // 与行内点击同一规则：只认「同一行的连续第二次按下」。
            let second_press = app.last_left_press_row.as_deref() == Some(path.as_path());
            app.last_left_press_row = Some(path.clone());
            if second_press {
                app.schedule_rename(path, "name-overlay");
            } else {
                logv!(
                    "[rename] overlay press without previous press on this row, select only: {}",
                    path.display()
                );
            }
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
            // 排序会改列表顺序，下拉跟着关掉（否则它挂在一个已经变了的位置上）。
            app.borrow_mut().close_type_filter_menu();
        });
    }
    {
        let app = Rc::downgrade(app);
        upgrade().on_file_type_filter_open(move |pane| {
            let Some(app) = app.upgrade() else { return };
            app.borrow_mut().open_type_filter_menu(pane.max(0) as usize);
        });
    }
    {
        let app = Rc::downgrade(app);
        upgrade().on_file_type_filter_pick(move |pane, index, toggle| {
            let Some(app) = app.upgrade() else { return };
            app.borrow_mut().type_filter_pick(pane.max(0) as usize, index, toggle);
        });
    }
    {
        let app = Rc::downgrade(app);
        upgrade().on_file_type_filter_close(move || {
            let Some(app) = app.upgrade() else { return };
            app.borrow_mut().close_type_filter_menu();
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
            // 像素级滚动（同一行内的微调）不需要重建行模型：行在 Slint 里是按
            // `index * row-h` 绝对定位的，视口偏移由 Flickable 自己负责。
            // 只有渲染窗口真的换了（跨过一行 / 内容变了）才重建。
            if app.visible_cache.is_some() && app.visible_cache == app.pane_window_now(0) {
                return;
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
            // 也记一笔给「空闲时静默重排」用（别在人家滚的时候重排）。
            app.last_scroll = Some(std::time::Instant::now());
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
            // 同左栏：窗口没变就不重建。
            if app.visible_cache_right.is_some()
                && app.visible_cache_right == app.pane_window_now(1)
            {
                return;
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
        let app_r = app.clone();
        upgrade().on_file_background_pointer_event(move |event, x, y| {
            let Some(app) = app.upgrade() else { return };
            app.borrow_mut()
                .background_pointer_event(&event, x, y);
        });
        upgrade().on_file_background_right_up(move || {
            let Some(app) = app_r.upgrade() else { return };
            app.borrow_mut().open_deferred_background_menu();
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
            // 取消只有 Esc 一条路（回车、点别处走的都是提交），先取出目标路径。
            let cancelled = PathBuf::from(app.ui.get_rename_path().to_string());
            app.ui.set_rename_path(SharedString::default());
            // 退出重命名后，图标视图的展开层回到“只显示名字 / 隐藏”。
            app.sync_expand_targets();
            // 核心修复：取消重命名时也必须解锁，否则锁残留 → 滚轮冻结。
            app.drag_pending = None;
            app.ui.invoke_set_drag_armed(false);
            // 新建文件夹流程：条目刚创建、从没被选中过 → 退出重命名后把它选中，
            // 否则（尤其按 Esc 保留默认名时）列表里没有任何选中项。
            // 普通重命名的目标本来就在选中集里，选择集不动。
            if !cancelled.as_os_str().is_empty()
                && !app.state.active().active().selection.contains(&cancelled)
            {
                app.select_path_now(&cancelled);
            }
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
            defer_app_mut("staging-expanded", &app, move |app| {
                // 延后重试期间用户可能又点了几次：只认界面上的最新值，
                // 否则旧的那次会把新状态覆盖掉。
                if app.ui.get_staging_expanded() != expanded {
                    return;
                }
                if app.config.staging_expanded == expanded {
                    return;
                }
                app.config.staging_expanded = expanded;
                app.config.save();
            });
        });
    }

    // ---- 工具栏 ----
    {
        let app = Rc::downgrade(app);
        upgrade().on_filter_edited(move |text| {
            let Some(app) = app.upgrade() else { return };
            // 搜索框的文本也会被 Rust 自己回写（导航/标签切换/清空），
            // 那时可能正在某个 `borrow_mut()` 区间里 —— 不能直接借。
            defer_app_mut("filter", &app, move |app| {
                // 同上：延后期间可能已有新的一次输入生效，旧文本不能覆盖新结果。
                if app.ui.get_filter_text() != text {
                    return;
                }
                // 清空立即生效：要马上恢复搜索前的滚动位置，不能让用户干等。
                if text.trim().is_empty() {
                    app.pending_search = None;
                    app.apply_search_query(&text);
                    return;
                }
                // 其余情况攒 150ms 再算（逐字触发全目录打分太贵），由 40ms 轮询到点应用。
                app.pending_search = Some((text.to_string(), std::time::Instant::now()));
            });
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
            app.request_sidebar_probe();
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
            if subpage == crate::state::SettingsSubpage::SearchAndIndex {
                // 看板要显示真实条目数：进这个子页才把索引加载起来（后台线程）。
                crate::search::index::warm_up_async();
            }
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
            let mut app = app.borrow_mut();
            // 登记一次：扫完由 finish_index_rescan 把这句话换成完成信息。
            app.index_rescan_status = true;
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
        let app = Rc::downgrade(app);
        // 供下面两个下拉回调共用（第一个闭包会移走 app）。
        let lang_app = app.clone();
        let startup_app = app.clone();
        let theme_app = app.clone();
        let auto_edit_app = app.clone();
        upgrade().on_auto_edit_path_on_new_tab_toggled(move |enabled| {
            let Some(app) = auto_edit_app.upgrade() else { return };
            let mut app = app.borrow_mut();
            app.config.auto_edit_path_on_new_tab = enabled;
            app.config.save();
            app.ui.set_auto_edit_path_on_new_tab(enabled);
        });
        // 接管资源管理器：改开关立刻写/还原注册表，失败只回退开关并记日志。
        let takeover_app = app.clone();
        // 「系统菜单」开关：只影响下一次右键（不重建菜单线程、不用重启）。
        let sysmenu_app = app.clone();
        upgrade().on_system_menu_toggled(move |enabled| {
            let Some(app) = sysmenu_app.upgrade() else { return };
            let mut app = app.borrow_mut();
            app.config.system_menu = enabled;
            app.config.save();
            app.ui.set_system_menu(enabled);
            // 关掉时「隐藏的系统菜单项」卡片随之隐藏，顺带重算页面内容高度。
            app.sync_settings();
        });
        // 默认压缩软件：存配置后整组重扫，四条压缩项按新的那一款重填程序/参数。
        let archtool_app = app.clone();
        upgrade().on_archive_tool_changed(move |index| {
            let Some(app) = archtool_app.upgrade() else { return };
            let id = match index {
                1 => "7zip",
                2 => "bandizip",
                _ => "winrar",
            };
            {
                let mut app = app.borrow_mut();
                if app.config.archive_tool == id {
                    return;
                }
                app.config.archive_tool = id.to_string();
                app.config.save();
                app.ui.set_archive_tool_index(index);
            }
            app.borrow_mut().rescan_archive();
        });
        upgrade().on_explorer_takeover_toggled(move |enabled| {
            let Some(app) = takeover_app.upgrade() else { return };
            let mut app = app.borrow_mut();
            // 菜单文字跟随界面语言（写进注册表，资源管理器直接显示这个字符串）。
            let label = i18n::text(&app.config.language, "用 Rove 打开");
            if let Err(err) = crate::sys::integration::sync(enabled, &label) {
                logln!("!! 接管资源管理器{}失败：{err}", if enabled { "开启" } else { "关闭" });
                // 回退开关到真实状态（配置没变，界面也必须跟配置一致）。
                app.ui.set_explorer_takeover(app.config.explorer_takeover);
                return;
            }
            app.config.explorer_takeover = enabled;
            app.config.save();
            app.ui.set_explorer_takeover(enabled);
            // Win+E 的键盘钩子跟着开关走：关掉时立刻卸下，一次按键都不多吞。
            crate::sys::win_e::sync(enabled);
        });
        // 开机自启动：写/删 HKCU 的 Run 键，成败都以注册表实况回填开关。
        let autostart_app = app.clone();
        upgrade().on_autostart_toggled(move |enabled| {
            let Some(app) = autostart_app.upgrade() else { return };
            let app = app.borrow();
            if let Err(err) = crate::sys::autostart::set_enabled(enabled) {
                logln!("!! 开机自启动{}失败：{err}", if enabled { "开启" } else { "关闭" });
                app.set_status_job(app.trf("开机自启动未生效：{}", &[err]));
            } else {
                logv!(
                    "开机自启动{}",
                    if enabled { "已开启" } else { "已关闭" }
                );
            }
            // 无论成败都按注册表实况回填（可能被别的程序占用而维持原状）。
            let actual = crate::sys::autostart::enabled();
            app.ui.set_autostart(actual);
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
            // 吸附窗不在主窗口的 i18n 绑定里，语言要单独同步。
            set_jump_language(&app.config.language);
            app.jobs
                .set_english(!i18n::is_chinese(&app.config.language));
            // Slint 里绑定的文案会自动刷新；由 Rust 生成的标签要重新下发。
            app.sync_settings();
            app.sync_tabs();
            app.sync_sidebar();
            app.sync_breadcrumbs();
            app.sync_status();
        });
        // 通用设置：主题色（跟随系统 / 亮 / 暗）——即时换全套色值 + 原生菜单深浅。
        upgrade().on_theme_changed(move |index| {
            let Some(app) = theme_app.upgrade() else { return };
            let (choice, ui) = {
                let mut app = app.borrow_mut();
                app.config.theme = match index {
                    1 => ThemeChoice::Light,
                    2 => ThemeChoice::Dark,
                    _ => ThemeChoice::System,
                };
                app.config.save();
                (app.config.theme, slint::ComponentHandle::clone_strong(&app.ui))
            };
            apply_theme(&ui, choice);
            // 侧栏 / 标签栏的图标颜色分亮暗两套，必须跟着重算一遍，否则切主题后
            // 盘符和 "N" 图标还是旧主题那套色。
            if let Some(app) = theme_app.upgrade() {
                if let Ok(mut app) = app.try_borrow_mut() {
                    app.sync_sidebar();
                    app.sync_tabs();
                }
            }
            retheme_extra_windows(choice);
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
                // 「添加到暂存盘」开关同时控制侧栏里那块托盘的显隐。
                app.ui.set_staging_tray_visible(app.staging_tray_enabled());
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
            // 内容高度由 Slint 侧 `preferred-height` 实测，这里只换模型。
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
            app.ui.set_settings_editor_label_locked(false);
            // 弹窗是同一个实例：上一条是压缩项时留下的「受管」标记必须清掉，
            // 否则新增动作也只剩 名称 / 执行路径 / 参数模板。
            app.ui.set_settings_editor_managed(false);
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
                // 压缩关联项的名称是固定语义（`解压到"{name}"\`），不许改。
                let locked = crate::ops::actions::is_archive_action(&a.id);
                app.ui.set_settings_editor_label_locked(locked);
                app.ui.set_settings_editor_managed(locked);
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
        // 压缩关联项：单条「更新」只刷这一项；组头「更新全部」才刷整组。
        let app = Rc::downgrade(app);
        upgrade().on_settings_action_rescan(move |id| {
            let Some(app) = app.upgrade() else { return };
            app.borrow_mut().rescan_archive_for(Some(id.to_string()));
        });
    }
    {
        let app = Rc::downgrade(app);
        upgrade().on_settings_archive_rescan_all(move || {
            let Some(app) = app.upgrade() else { return };
            app.borrow_mut().rescan_archive();
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
            // 压缩关联项：名称与 tool/builtin 标记沿用原值，只让用户改程序/参数。
            let existing = app.actions.iter().find(|a| a.id == id).cloned();
            let is_archive = crate::ops::actions::is_archive_action(&id);
            let (label, tool, builtin) = match (&existing, is_archive) {
                (Some(old), true) => (old.label.clone(), old.tool.clone(), old.builtin),
                _ => (label.to_string(), String::new(), false),
            };
            let action = crate::ops::actions::ActionEntry {
                id: id.clone(),
                label,
                program: program.to_string(),
                args: args.to_string(),
                working_dir: "{target_dir}".into(),
                target: target.to_string(),
                extensions,
                // 压缩关联项跟着原开关走，别因为「编辑」就被强行打开。
                enabled: existing.as_ref().map(|a| a.enabled).unwrap_or(true),
                group: group.to_string(),
                tool,
                builtin,
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
    {
        // 参数模板按钮：把 `"{token}"` 插到参数框的光标处（有选中则替换选中），
        // 再把光标放回插入内容的后面。与批量重命名的正则片段插入同一套算法。
        let app = Rc::downgrade(app);
        upgrade().on_settings_editor_token_inserted(move |token, caret, anchor| {
            let Some(app) = app.upgrade() else { return };
            // 先算完再写 Slint 属性：写属性会同步派发 changed 回调，
            // 不能带着 App 的 RefCell 借用进去。
            let (new_text, new_caret) = {
                let text = app.borrow().ui.get_settings_editor_args();
                insert_arg_token(&text, &token, caret, anchor)
            };
            app.borrow().ui.set_settings_editor_args(new_text.into());
            app.borrow().ui.invoke_put_caret(new_caret);
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
            app.visible_cache_right = None;
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

/// 补一条合成的「松开左键」消息，复位卡住的按下状态。
///
/// OLE 的 `DoDragDrop` 会捕获鼠标并吃掉松手消息，Slint 永远收不到 up。之后第一次
/// 点击会被当成上一下的释放：焦点不切换、多选塌缩成被拖的那一行。winit 收到
/// `WM_LBUTTONUP` 就会派发一个正常的 MouseInput(Released)，按下状态随之复位。
#[cfg(windows)]
fn reset_pointer_press(ui: &MainWindow) {
    use windows_sys::Win32::Foundation::POINT;
    // ScreenToClient 在 windows-sys 里挂在 Gdi 模块下（虽属 user32）。
    use windows_sys::Win32::Graphics::Gdi::ScreenToClient;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetCursorPos, PostMessageW, WM_LBUTTONUP,
    };
    let Some(hwnd) = get_window_hwnd(ui) else {
        return;
    };
    if hwnd == 0 {
        return;
    }
    let mut pt = POINT { x: 0, y: 0 };
    let mut lparam = 0isize;
    unsafe {
        // 用真实光标位置构造 lparam：Slint 会把释放事件的坐标当作当前鼠标位置。
        if GetCursorPos(&mut pt) != 0 && ScreenToClient(hwnd as _, &mut pt) != 0 {
            lparam = ((pt.y as isize) << 16) | (pt.x as isize & 0xFFFF);
        }
        PostMessageW(hwnd as _, WM_LBUTTONUP, 0, lparam);
    }
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

/// `WM_SETTINGCHANGE` 的 lParam 是否指向 `"ImmersiveColorSet"`（系统换深浅色）。
///
/// 这条消息是**跨进程广播**：lParam 里的指针是在发送方地址空间里有效的，
/// 在我们进程里未必可读，直接解引用就是野指针（能直接把程序带崩）。
/// 所以用 `ReadProcessMemory` 读自己的地址空间——地址无效时它只会返回
/// `FALSE`，不会触发访问违例。
#[cfg(windows)]
fn is_immersive_color_set(lparam: windows_sys::Win32::Foundation::LPARAM) -> bool {
    use windows_sys::Win32::System::Diagnostics::Debug::ReadProcessMemory;
    use windows_sys::Win32::System::Threading::GetCurrentProcess;

    const NEEDLE: &str = "ImmersiveColorSet";
    if lparam == 0 {
        return false;
    }
    let mut buf = [0u16; NEEDLE.len()];
    let bytes = buf.len() * std::mem::size_of::<u16>();
    let mut read = 0usize;
    unsafe {
        let ok = ReadProcessMemory(
            GetCurrentProcess(),
            lparam as *const core::ffi::c_void,
            buf.as_mut_ptr() as *mut core::ffi::c_void,
            bytes,
            &mut read,
        );
        if ok == 0 || read < bytes {
            return false;
        }
    }
    buf.iter().copied().eq(NEEDLE.encode_utf16())
}

/// 系统深浅色变了：只有「跟随系统」需要重新配色（其余是用户钉死的选择）。
///
/// 走 `invoke_from_event_loop` 而不是同步执行：这里还在窗口过程里，
/// 直接写 Slint 属性会重入事件循环；而且 WM_SETTINGCHANGE 一次会来好几条。
#[cfg(windows)]
fn resync_follow_system_theme() {
    let _ = slint::invoke_from_event_loop(|| {
        let app = SESSION.with(|slot| slot.borrow().clone());
        let Some(app) = app else { return };
        let Ok(mut app) = app.try_borrow_mut() else { return };
        if app.config.theme != ThemeChoice::System {
            return;
        }
        let ui = slint::ComponentHandle::clone_strong(&app.ui);
        apply_theme(&ui, ThemeChoice::System);
        // 侧栏 / 标签栏图标色分亮暗两套：先把主题写进 Slint（sync_* 读的是
        // Theme.dark），再重建这两个模型。
        app.sync_sidebar();
        app.sync_tabs();
        drop(app);
        retheme_extra_windows(ThemeChoice::System);
        crate::logv!("[theme] 系统深浅色变化，已按「跟随系统」重算配色");
    });
}

/// 无边框窗口的 `WM_NCHITTEST` 子类化。
///
/// **这里不再判定标题栏拖拽区**。历史做法是把标签栏右侧的空白算成
/// `HTCAPTION`（目的只有一个：手指拖窗口走系统原生路径），但那个矩形是从
/// Slint 布局**轮询**来的副本，新建标签 / 最大化 / 改缩放的一瞬间副本会落后
/// 于真实布局 —— 偏小时就把标签本身判成标题栏，表现为「点标签变成拖窗口、
/// 关闭按钮闪烁」，而且只在偶发时机出现，抓不到也复现不了。
///
/// 现在整段一律 `HTCLIENT`：命中的裁决交给 Slint 自己的元素命中测试（同步、
/// 精确、不存在滞后），拖窗口走 `WindowMoveArea` → winit 发
/// `WM_NCLBUTTONDOWN/HTCAPTION` → 系统原生移动循环（吸附、从最大化拖出时
/// 自动还原都还在）。代价只有两个：手指拖窗口失效、空白处双击最大化改由
/// Slint 的 `double-clicked` 自己处理。
///
/// 仍然必须留在子类化里的只有三件事：
/// - 无边框窗口的**边缘缩放**（系统不知道 `resize-border-width`）；
/// - 右上角窗口按钮区压制缩放边（否则点关闭按钮的上沿会变成拉高窗口）；
/// - 自己发起 OLE 拖拽期间把一切压成 `HTCLIENT`，别让缩放光标盖掉拖放光标。
#[cfg(windows)]
mod drag_hittest {
    use std::sync::atomic::{AtomicI64, Ordering};

    /// 前一窗口过程（`SetWindowLongPtrW` 的返回值）。
    static PREV_WNDPROC: AtomicI64 = AtomicI64::new(0);
    /// 钩子已安装到的 HWND（0 = 未安装）。
    ///
    /// 守护进程常驻时会多次建会话、每次都是新窗口；只有"同一个 HWND 已装过"
    /// 才允许跳过，否则第二个窗口永远拿不到钩子。
    static INSTALLED_HWND: AtomicI64 = AtomicI64::new(0);

    /// 标题栏高度（逻辑像素）。只有落在这一带里的右上角才是窗口按钮区。
    const TITLE_BAND_H: f32 = 38.0;
    /// 右上角窗口按钮组的宽度（设置 36 + 最小化 44 + 最大化 44 + 关闭 46 + 余量）。
    const CONTROLS_W: f32 = 172.0;
    /// 无边框窗口的边缘缩放厚度（逻辑像素）。滚动条按 7px 内缩躲开它。
    const BORDER: f32 = 6.0;

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
            CallWindowProcW, HTBOTTOM, HTBOTTOMLEFT, HTBOTTOMRIGHT, HTCLIENT, HTLEFT, HTRIGHT,
            HTTOP, HTTOPLEFT, HTTOPRIGHT, IsZoomed, SC_MOVE, WNDPROC, WM_GETMINMAXINFO,
            WM_NCHITTEST, WM_NCLBUTTONDOWN, WM_NCRBUTTONDOWN, WM_SETTINGCHANGE, WM_SYSCOMMAND,
        };

        // 非客户区点击 / 系统发起的移动：编辑态与重命名必须先退出。
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
        // 系统「个性化 → 颜色」一变，Windows 就广播 WM_SETTINGCHANGE 且
        // lParam 指向 "ImmersiveColorSet"。只在「跟随系统」时重算配色，
        // 否则用户选的是固定主题，系统怎么变都与我们无关。
        if msg == WM_SETTINGCHANGE && super::is_immersive_color_set(lparam) {
            super::resync_follow_system_theme();
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

            // 物理像素 → 逻辑像素用窗口自己的 DPI：本进程是 per-monitor v2，
            // 换屏或改缩放后窗口宽必须按当前显示器算，不能沿用启动时的 scale。
            let dpi = windows_sys::Win32::UI::HiDpi::GetDpiForWindow(hwnd) as f32 / 96.0;
            let scale = if dpi > 0.01 { dpi } else { 1.0 };
            let sx = (lparam as i32 & 0xFFFF) as i16 as i32;
            let sy = ((lparam as i32 >> 16) & 0xFFFF) as i16 as i32;
            let mut pt = windows_sys::Win32::Foundation::POINT { x: sx, y: sy };
            windows_sys::Win32::Graphics::Gdi::ScreenToClient(hwnd as _, &mut pt);
            let lx = pt.x as f32 / scale;
            let ly = pt.y as f32 / scale;

            let mut rect = windows_sys::Win32::Foundation::RECT {
                left: 0,
                top: 0,
                right: 0,
                bottom: 0,
            };
            windows_sys::Win32::UI::WindowsAndMessaging::GetClientRect(hwnd, &mut rect);
            let win_w = (rect.right - rect.left) as f32 / scale;
            let win_h = (rect.bottom - rect.top) as f32 / scale;

            // 右上角窗口控制按钮的独占区：任何缩放或窗口尺寸下都回报客户区，
            // 否则点在按钮上沿会变成「拉高窗口」（窄窗口下尤其明显）。
            if (0.0..=TITLE_BAND_H).contains(&ly) && lx >= win_w - CONTROLS_W {
                return HTCLIENT as isize;
            }

            if IsZoomed(hwnd) == 0 {
                // 1. 顶部边缘（宽 8px 防漏网）
                if ly <= 8.0 {
                    if lx <= BORDER {
                        return HTTOPLEFT as isize;
                    }
                    if lx >= win_w - BORDER {
                        return HTTOPRIGHT as isize;
                    }
                    return HTTOP as isize;
                }
                // 2. 底部边缘
                if ly >= win_h - BORDER {
                    if lx <= BORDER {
                        return HTBOTTOMLEFT as isize;
                    }
                    if lx >= win_w - BORDER {
                        return HTBOTTOMRIGHT as isize;
                    }
                    return HTBOTTOM as isize;
                }
                // 3. 左右边缘
                if lx <= BORDER {
                    return HTLEFT as isize;
                }
                if lx >= win_w - BORDER {
                    return HTRIGHT as isize;
                }
            }
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
            if current == 0 || current == wndproc as *const () as isize || is_installed(hwnd) {
                return;
            }
            PREV_WNDPROC.store(current as i64, Ordering::Relaxed);
            let _ = SetWindowLongPtrW(hwnd as _, GWLP_WNDPROC, wndproc as *const () as isize);
            INSTALLED_HWND.store(hwnd as i64, Ordering::Relaxed);
            logv!("drag hittest: hooked hwnd={hwnd:#x} prev={current:#x}");
        }
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
    /// 本次会话是否「只建不显示」（`--background` 开机自启动）。
    /// create_session 消费一次后复位，所以之后由托盘/资源管理器拉起的窗口照常显示。
    static START_HIDDEN: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// 绑定 IPC 管道服务。每次都是全新的闭包（`spawn_server` 只接一次）。
fn bind_ipc_server() -> bool {
    crate::daemon::spawn_server(move |request| {
        // 闭包只捕获 request（Send），会话槽在事件循环线程的 thread_local 里。
        let _ = slint::invoke_from_event_loop(move || {
            wake_session(request);
        });
    })
}

/// 等目标进程真的退出（最多 `timeout`）。返回 `true` = 已退出
/// （拿不到句柄也算，说明它已经不在了）。
#[cfg(windows)]
fn wait_for_pid_exit(pid: u32, timeout: Duration) -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0};
    use windows_sys::Win32::System::Threading::{
        OpenProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE,
    };
    unsafe {
        let handle = OpenProcess(PROCESS_SYNCHRONIZE, 0, pid);
        if handle.is_null() {
            return true;
        }
        let waited =
            WaitForSingleObject(handle, timeout.as_millis().min(u32::MAX as u128) as u32);
        CloseHandle(handle);
        waited == WAIT_OBJECT_0
    }
}

#[cfg(not(windows))]
fn wait_for_pid_exit(_pid: u32, _timeout: Duration) -> bool {
    true
}

/// 静默重启的交接标记文件（`%LOCALAPPDATA%\Rove\restart.ok`）。
fn restart_marker_path() -> PathBuf {
    rove_data_dir().join("restart.ok")
}

/// 菜单线程卡到上限后，借「用户关窗口」这个安全时刻换一个新进程。
///
/// 卡死在第三方菜单扩展里的宿主线程回收不了，只能靠换进程一次性清干净。
/// 顺序必须是「先起新进程 → 新进程绑上管道并落标记 → 旧进程才退出」：
/// 管道是单实例的，反过来就会两边都绑不上，托盘图标直接消失。
/// 握手拿不到标记就把子进程杀掉，继续用旧进程 —— 宁可不重启，也不能把守护弄丢。
/// 立刻终止本进程。`std::process::exit` 要跑完所有 atexit / DLL 卸载，
/// 在已经卡死的进程里可能**永远不返回**（真机日志：exit 之后 3 秒都没走掉，
/// 于是新进程抢不到管道，托盘图标也还挂在这个僵尸身上）。
fn hard_exit() -> ! {
    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::System::Threading::{GetCurrentProcess, TerminateProcess};
        let _ = TerminateProcess(GetCurrentProcess(), 0);
    }
    std::process::exit(0);
}

/// 盯住会话销毁：卡死就硬退出，绝不留下「窗口没了但进程还赖着」的僵尸。
///
/// 僵尸的危害不只是占内存：管道、托盘图标都在它手里，用户再点快捷方式只会
/// 转发给这个已经废掉的进程 —— 表现为「新启动的 Rove 右键菜单打不开」，
/// 只有重启电脑才能救。
fn spawn_destroy_guard(job_running: bool) {
    let _ = std::thread::Builder::new()
        .name("rove-close-guard".into())
        .spawn(move || {
            for _ in 0..80 {
                std::thread::sleep(Duration::from_millis(100));
                // 正常走完的销毁会把 CLOSING_AT 清掉。
                if CLOSING_AT.load(std::sync::atomic::Ordering::Acquire) == 0 {
                    return;
                }
            }
            if job_running {
                // 有复制/移动在跑：任务线程是进程级的，会话销毁后它还在走。
                // 硬终止会把它拦腰砍断（半截文件、移动做到一半），宁可与僵尸共存。
                logal!("[close] 销毁会话 8 秒没完成，但还有任务在跑；不硬终止");
                return;
            }
            logal!("[close] 销毁会话 8 秒没完成，硬终止本进程（免得霸占 IPC 和托盘图标）");
            if !QUITTING.load(std::sync::atomic::Ordering::Acquire) {
                let marker = restart_marker_path();
                if spawn_handoff_child().is_some() {
                    // 给接班进程两秒走到交接点，之后再硬退出（退出本身可能卡死）。
                    for _ in 0..20 {
                        std::thread::sleep(Duration::from_millis(100));
                        if marker.exists() {
                            break;
                        }
                    }
                }
            }
            hard_exit();
        });
}

/// 只负责拉起接班进程并等它就位（不等本进程退出）。返回子进程句柄。
fn spawn_handoff_child() -> Option<std::process::Child> {
    let Ok(exe) = std::env::current_exe() else {
        logal!("[restart] 拿不到自身路径，放弃重启");
        return None;
    };
    let marker = restart_marker_path();
    let _ = std::fs::remove_file(&marker);
    let child = std::process::Command::new(&exe)
        .arg("--background")
        .arg("--await-pipe")
        .arg("--await-pid")
        .arg(std::process::id().to_string())
        .spawn()
        .ok()?;
    Some(child)
}

fn restart_handoff() {
    let marker = restart_marker_path();
    let Some(mut child) = spawn_handoff_child() else {
        logal!("[restart] 子进程启动失败，继续用旧进程");
        return;
    };
    for _ in 0..80 {
        std::thread::sleep(Duration::from_millis(100));
        if marker.exists() {
            // 此刻只知道「子进程起来并走到交接点」，真正绑上管道要等本进程退出之后
            // （管道单实例），所以这里说的是「就位」而不是「已接管」。
            logal!("[restart] 新进程已就位，退出旧进程以释放 IPC（回收卡死的菜单线程）");
            hard_exit();
        }
        if matches!(child.try_wait(), Ok(Some(_))) {
            break;
        }
    }
    let _ = child.kill();
    let _ = std::fs::remove_file(&marker);
    logal!("[restart] 新进程没能接管 IPC，继续用旧进程");
}

/// 用户主动「退出」时置上：这条路径绝不能再拉起接班进程，否则退出会把应用又拖回来。
static QUITTING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// 会话是否存在。给看门狗用：`SESSION` 是 thread_local，从别的线程读永远是空。
static SESSION_ALIVE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// 会话销毁从哪一刻开始的（毫秒时间戳，0 = 没在销毁）。卡住时看门狗靠它判死。
static CLOSING_AT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// 已经安排过一次「换进程」。避免卡死期间每秒钟都去拉一个新进程。
static HANDOFF_DONE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// 托盘「退出」在销毁会话前调用。
pub fn note_quitting() {
    QUITTING.store(true, std::sync::atomic::Ordering::Release);
}

/// UI 线程心跳（毫秒，进程启动起算）。由 UI 线程上的 500ms 定时器更新。
static UI_BEAT_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// 进程启动起的毫秒数。
fn proc_ms() -> u64 {
    static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    START
        .get_or_init(std::time::Instant::now)
        .elapsed()
        .as_millis()
        .min(u64::MAX as u128) as u64
}

/// UI 线程心跳定时器：窗口活着就每 500ms 跳一次。
///
/// 这不是为了性能，是为了**留证据**：消息循环卡住时它自然就停了，
/// 看门狗据此判断「窗口多久没动」，并在日志里写清当时的菜单/会话状态。
pub(crate) fn install_ui_heartbeat() {
    let timer = slint::Timer::default();
    timer.start(
        slint::TimerMode::Repeated,
        std::time::Duration::from_millis(500),
        || {
            UI_BEAT_MS.store(proc_ms(), std::sync::atomic::Ordering::Release);
        },
    );
    // 定时器必须活得和进程一样久，否则一 drop 就停。
    std::mem::forget(timer);
}

/// 卡死看门狗：UI 线程超过 5 秒没心跳就写一条无条件日志。
///
/// 消息循环本身**不能**设超时（阻塞在哪一行，就只能等那一行返回），
/// 所以这里只做两件事：判死 + 记录现场，方便下次直接定位是哪个环节。
pub fn start_ui_watchdog() {
    std::thread::Builder::new()
        .name("rove-watchdog".into())
        .spawn(|| {
            let mut stalled_for = 0u64;
            let mut reported = false;
            let mut no_beat_reported = false;
            // 首次观察到「会话已存在」的时刻，用于给心跳留出安装时间。
            let mut session_seen: Option<u64> = None;
            // 上一轮是否正处在模态系统对话框里（见下方豁免）。
            let mut modal_seen = false;
            loop {
                std::thread::sleep(std::time::Duration::from_millis(1000));
                let beat = UI_BEAT_MS.load(std::sync::atomic::Ordering::Acquire);
                if beat == 0 {
                    // 心跳还没装（会话未创建），不算卡。
                    // 但如果会话已经存在 8 秒仍然一次心跳都没有，说明事件循环压根没在转：
                    // 此时托盘点击会被投递进去、然后永远没人处理（表现就是「点了没反应」）。
                    if SESSION_ALIVE.load(std::sync::atomic::Ordering::Acquire) {
                        let seen = *session_seen.get_or_insert_with(proc_ms);
                        if proc_ms().saturating_sub(seen) > 8000 && !no_beat_reported {
                            no_beat_reported = true;
                            crate::logal!("[watchdog] !! UI 心跳从未开始（事件循环没在跑）");
                        }
                    }
                    continue;
                }
                session_seen = None;
                // 会话已经销毁（常驻托盘、窗口没了）：事件循环不再跑，心跳当然
                // 停了 —— 这不是卡死。以前这里照样每 30 秒记一条「UI 线程无响应
                // session=false」，把真正的问题埋在了噪音里。
                if !SESSION_ALIVE.load(std::sync::atomic::Ordering::Acquire) {
                    // 销毁会话卡住了（真机：一个第三方组件把销毁路径堵住，
                    // 停在 `drop(session)` 里再没回来）。这时窗口没了、事件
                    // 循环不转、托盘点了没反应，除任务管理器没有出路 —— 自己换一个
                    // 新进程，用户至少还能继续用。
                    let since = CLOSING_AT.load(std::sync::atomic::Ordering::Acquire);
                    if since != 0
                        && !QUITTING.load(std::sync::atomic::Ordering::Acquire)
                        && !HANDOFF_DONE.load(std::sync::atomic::Ordering::Acquire)
                        && now_ms().saturating_sub(since) > 10_000
                    {
                        HANDOFF_DONE.store(true, std::sync::atomic::Ordering::Release);
                        crate::logal!(
                            "[watchdog] !! 销毁会话卡了 {}ms（多半是菜单扩展占着不放），换一个新进程",
                            now_ms().saturating_sub(since)
                        );
                        let _ = std::thread::Builder::new()
                            .name("rove-restart".into())
                            .spawn(restart_handoff);
                    }
                    stalled_for = 0;
                    reported = false;
                    continue;
                }
                let lag = proc_ms().saturating_sub(beat);
                // 模态系统对话框（确认框 / 清空回收站）本来就占着 UI 线程，
                // 那不是卡死，不该记 `!!`。
                if crate::sys::shell::modal_active() {
                    modal_seen = true;
                    continue;
                }
                // 对话框刚关掉的那一轮同样要跳过：心跳还停在对话框弹出之前，
                // 算出来的 lag 全是用户按对话框的时间，等下一拍心跳才是真的。
                if modal_seen {
                    modal_seen = false;
                    reported = false;
                    stalled_for = 0;
                    continue;
                }
                if lag > 5000 {
                    // 每多卡 30 秒再记一条：`reported` 只表示「上一档已经记过」。
                    let next_step = lag / 30_000 > stalled_for / 30_000;
                    stalled_for = lag;
                    if !reported || next_step {
                        reported = true;
                        let menu = crate::sys::menu::busy_for_ms();
                        crate::logal!(
                            "[watchdog] UI 线程无响应 {}ms menu_busy={:?} session={}",
                            lag,
                            menu,
                            SESSION_ALIVE.load(std::sync::atomic::Ordering::Acquire)
                        );
                    }
                } else if reported && lag < 1500 {
                    crate::logal!("[watchdog] UI 线程恢复（最长卡顿 {}ms）", stalled_for);
                    reported = false;
                    stalled_for = 0;
                }
            }
        })
        .ok();
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
        // 菜单子进程会把前台权带走：收回主窗口的焦点，否则键盘事件没人接。
        // Shell 任务正在跑时不抢（会把它的确认框/进度框压下去），并且延到
        // 本次借用释放之后再动 Slint —— 回调里再 borrow_mut 会直接 panic。
        MenuAction::Closed => {
            if !app.shell_op_running {
                let ui = app.ui.as_weak();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = ui.upgrade() {
                        focus_window(&ui);
                    }
                });
            }
        }
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
// ---------------------------------------------------------------------------
// 文件对话框吸附窗（独立于会话常驻）
// ---------------------------------------------------------------------------
//
// 早期实现把吸附窗挂在 App 里，于是「关掉主窗口 → 会话销毁 → 吸附窗跟着没了」，
// 文件对话框吸附静默失效（这正是 `--background` 开机常驻最需要的功能）。
// 现在窗口、被吸附的对话框、几何、界面语言全部搬到进程级的 `JUMP` 里：
// 会话有没有、主窗口开没开都不影响它，主窗口只负责它自己的东西。

/// 吸附窗常驻状态（进程级，不随会话销毁）。
struct JumpState {
    window: DialogJumpWindow,
    /// 被吸附的对话框 HWND + 当时目录（`None` 表示当前没有吸附目标）。
    target: Option<(usize, Option<PathBuf>)>,
    /// 最近一次吸附几何 (x, y, w, placement)，供结果数 / 焦点变化时重新锚定。
    geom: Option<(i32, i32, i32, String)>,
    /// 界面语言快照：会话销毁后就没有 App 可以问了。
    language: String,
    /// 后台常驻时的索引空闲卸载定时器（见 `init_dialog_jump`）。
    /// 必须存在这里：`slint::Timer` 一析构就停，放局部变量等于从没启动。
    unload_timer: slint::Timer,
}

thread_local! {
    static JUMP: RefCell<Option<JumpState>> = const { RefCell::new(None) };
}

/// 吸附窗输入框里是否有内容（有内容才值得等全盘索引那一轨）。
fn jump_query_active() -> bool {
    with_jump(|state| !state.window.get_query().is_empty()).unwrap_or(false)
}

/// 只读/可变访问吸附窗状态；未初始化（非 Windows 或初始化失败）时返回 None。
fn with_jump<R>(f: impl FnOnce(&mut JumpState) -> R) -> Option<R> {
    JUMP.with(|slot| slot.borrow_mut().as_mut().map(f))
}

/// 切换界面语言后同步吸附窗文案（它不随主窗口的 i18n 绑定刷新）。
pub(crate) fn set_jump_language(language: &str) {
    with_jump(|state| state.language = language.to_string());
}

/// 主题变了：吸附窗独立的 Theme 全局也要跟着换。
fn set_jump_theme(theme: ThemeChoice) {
    with_jump(|state| apply_theme_to(&state.window.global::<Theme>(), resolve_theme(theme)));
}

/// 主题变了：把**独立组件树**的窗口也刷一遍（主窗口之外还有吸附窗、批量重命名）。
///
/// 这些窗口各自持有一份 `Theme` 全局，建窗时下发过、之后不会自动同步。
fn retheme_extra_windows(theme: ThemeChoice) {
    set_jump_theme(theme);
    let effective = resolve_theme(theme);
    let windows: Vec<BatchRenameWindow> = BATCH_WINDOWS.with(|list| {
        list.borrow()
            .iter()
            .filter_map(|e| e.ui.upgrade())
            .collect()
    });
    for ui in windows {
        apply_theme_to(&ui.global::<Theme>(), effective);
    }
}

/// 当前会话正在看的目录（没有会话 / App 正被借用时返回 None）。
fn session_current_dir() -> Option<PathBuf> {
    let app = SESSION.with(|slot| slot.try_borrow().ok().and_then(|s| s.clone()))?;
    let app = app.try_borrow().ok()?;
    app.state.active().active().path.clone()
}

/// 「当前目录」快捷项：把 Rove 正在看的目录插到历史列表最前面。
fn with_current_dir_pin(hits: &mut Vec<crate::search::jump::JumpHit>, language: &str) {
    let Some(p) = session_current_dir() else { return };
    if p == crate::fs::drives::recycle_virtual_path() {
        return;
    }
    let name = crate::fs::dir::leaf_name(&p);
    let pin = crate::search::jump::JumpHit {
        name: name.clone(),
        path: p.clone(),
        score: 0,
        name_parts: vec![
            (name, false),
            (format!(" ({})", i18n::text(language, "当前")), true),
        ],
        is_local: true,
        is_history: false,
        last_time: 0,
    };
    hits.retain(|h| h.path != pin.path);
    hits.insert(0, pin);
    hits.truncate(6); // 允许比平时多展示一个
}

/// 重算吸附窗候选列表，并按结果数重新锚定窗口位置。
///
/// 铁律：**持着 JUMP 借用时绝不回调进 Slint**。set_results / show / hide /
/// invoke_clear_query 都可能同步派发 Slint 回调（例如 clear-query 立刻触发
/// text-changed），那时再借 JUMP 就是 RefCell 二次借用 → 进程 abort。
/// 所以先算出结果、释放借用，再动 UI。
///
/// 搜索根目录**每次都从对话框重新提取**：用户完全可能自己换过目录（点侧栏、
/// 敲地址栏、退回上级），缓存旧根目录会让结果继续停在旧范围内。
fn jump_refresh() {
    let snapshot = with_jump(|state| {
        #[cfg(windows)]
        let root = state.target.as_ref().and_then(|(hwnd, _)| {
            crate::sys::dialog_jump::extract_current_folder(*hwnd as _)
        });
        #[cfg(not(windows))]
        let root: Option<PathBuf> = None;
        // 吸附窗的「全盘文件夹」那一轨也吃这份索引。索引是按需加载的，
        // 这里既然用户在搜，就顺手让它去读（幂等；读完 `on_index_ready`
        // 会回来补刷一次结果）。
        if !state.window.get_query().is_empty() {
            crate::search::index::touch();
            if !crate::search::index::has_data() {
                logv!("[jump] 索引未加载，为吸附窗检索触发加载");
            }
            crate::search::index::warm_up_async();
        }
        let hits = if state.window.get_query().is_empty() {
            let mut hits = crate::search::jump::search("", None);
            with_current_dir_pin(&mut hits, &state.language);
            Some(hits)
        } else {
            logv!(
                "[rove:dialog_jump] refresh q={:?} root={:?}",
                state.window.get_query(),
                root
            );
            Some(crate::search::jump::search(
                &state.window.get_query(),
                root.as_deref(),
            ))
        };
        hits.map(jump_results_model)
    })
    .flatten();
    if let Some(model) = snapshot {
        with_jump(|state| state.window.set_results(model));
    }
    // bottom 吸附时窗口高度随结果数变化，需重新锚定底边：
    // bottom 延迟一轮（等 Slint 完成 height 重算后原子锚定），top 立即执行。
    #[cfg(windows)]
    if let Some((gx, gy, gw, gplacement)) = with_jump(|state| state.geom.clone()).flatten() {
        if gplacement == "bottom" {
            reposition_bottom_next_tick(gx, gy, gw, gplacement);
        } else {
            with_jump(|state| position_jump_window(&state.window, gx, gy, gw, &gplacement));
        }
    }
}

/// 建好吸附窗并接线。进程内只做一次（见 `run()`），与主窗口生命周期无关。
pub(crate) fn init_dialog_jump(language: &str, theme: ThemeChoice) {
    let window = DialogJumpWindow::new().unwrap();
    // 吸附窗是独立的组件树，主题全局要单独下发一次（否则深色下它是白的）。
    apply_theme_to(&window.global::<Theme>(), resolve_theme(theme));
    window.set_mouse_left_pressed(false);
    JUMP.with(|slot| {
        *slot.borrow_mut() = Some(JumpState {
            window: window.clone_strong(),
            target: None,
            geom: None,
            language: language.to_string(),
            unload_timer: slint::Timer::default(),
        });
    });

    // 后台常驻（没有主窗口）时没人跑空闲卸载：吸附窗自己看着点。
    // 主窗口在的时候由 `App::maybe_unload_index` 负责，这里直接让路。
    with_jump(|state| {
        state
            .unload_timer
            .start(slint::TimerMode::Repeated, Duration::from_secs(60), || {
                if SESSION.with(|slot| slot.borrow().is_some()) || jump_query_active() {
                    return;
                }
                let idle = crate::search::index::idle_for();
                if idle.is_some_and(|d| d >= Duration::from_secs(INDEX_UNLOAD_IDLE_MINS * 60)) {
                    crate::search::index::unload();
                }
            });
    });

    // 输入变化（含程序化清空）：重算候选。
    window.on_text_changed(move |_query| jump_refresh());

    // 聚焦：一律按对话框的**最新路径**重算。
    //
    // 以前这里只在输入框为空时才刷新，于是「搜了一半、又去对话框里自己换了个
    // 目录、再点吸附窗」时，框里还留着上次的文本，结果却还是旧目录下算出来的。
    //
    // 推迟一拍执行：聚焦与「点中某一条」是同一次鼠标事件里的两件事，
    // 同步重建结果模型会把按下时那个条目实例拆掉，松手的激活就没了
    // （表现为「要点两次才生效」）。
    window.on_focus_changed(|| {
        slint::Timer::single_shot(std::time::Duration::from_millis(0), jump_refresh);
    });

    // 选中一条：把路径写进原生对话框并确认（不依赖会话）。
    window.on_item_activated(|path| {
        let target = with_jump(|state| state.target.clone()).flatten();
        let Some((hwnd, _)) = target else {
            logln!(
                "!! [rove:dialog_jump] 点选被忽略：当前没有吸附目标（{}），先隐藏吸附窗",
                path
            );
            hide_dialog_jump();
            return;
        };
        // 对话框可能已经关掉 / 被重建（跳转后 DirectUI 会重建，旧句柄作废）。
        // 继续往失效句柄写字符串只会静默失败，用户会觉得「点了没反应」。
        #[cfg(windows)]
        if !crate::sys::dialog_jump::target_still_valid(hwnd as _) {
            logln!(
                "!! [rove:dialog_jump] 点选被忽略：目标对话框 {:#x} 已失效，隐藏吸附窗（下次打开对话框会重新吸附）",
                hwnd
            );
            hide_dialog_jump();
            return;
        }
        #[cfg(windows)]
        match crate::sys::dialog_jump::navigate_dialog(hwnd as _, Path::new(path.as_str())) {
            Ok(()) => logln!("[rove:dialog_jump] 跳转已发起 -> {}", path),
            Err(err) => logln!("!! [rove:dialog_jump] 跳转失败：{}（目标 {}）", err, path),
        }
        // 历史记录写盘丢到后台线程：同步磁盘 IO 会破坏输入法的 IME 上下文。
        let recorded = path.to_string();
        std::thread::spawn(move || {
            crate::search::jump::record_history(Path::new(&recorded));
        });
        // 清空输入必须放在释放借用之后：它会同步触发 text-changed。
        with_jump(|state| state.window.clone_strong())
            .map(|window| window.invoke_clear_query());
    });

    // 吸附窗是独立 Window，主窗口的 winit 钩子收不到它的鼠标事件；单独挂一个，
    // 把左键按住状态回填给输入框（拖选贴边自动滚动用）。
    {
        use slint::winit_030::winit::event::{ElementState, MouseButton, WindowEvent};
        use slint::winit_030::{EventResult, WinitWindowAccessor};
        let jw = window.clone_strong();
        window.window().on_winit_window_event(move |_w, event| {
            if let WindowEvent::MouseInput {
                state,
                button: MouseButton::Left,
                ..
            } = event
            {
                jw.set_mouse_left_pressed(matches!(state, ElementState::Pressed));
            }
            // 失焦后「松开」可能收不到，先清掉按住态，避免自动滚动定时器在后台继续跑。
            if matches!(event, WindowEvent::Focused(false)) {
                jw.set_mouse_left_pressed(false);
            }
            EventResult::Propagate
        });
    }

    // 文件对话框吸附轮询：跑在事件循环线程上（钩子线程会被 WINEVENT 洪水饿死）。
    // 定时器必须存活到进程结束，所以由 JUMP 之外的一个进程级 Timer 承载。
    POLL.with(|slot| {
        slot.borrow_mut().replace({
            let timer = Timer::default();
            timer.start(TimerMode::Repeated, Duration::from_millis(500), || {
                #[cfg(windows)]
                crate::sys::dialog_jump::poll_foreground();
            });
            timer
        });
    });
}

thread_local! {
    /// 吸附轮询定时器（进程级保活，`Option` 只是为了让它在事件循环线程上创建）。
    static POLL: RefCell<Option<Timer>> = const { RefCell::new(None) };
}

/// 搜索命中 → Slint 结果模型。
fn jump_results_model(hits: Vec<crate::search::jump::JumpHit>) -> ModelRc<JumpResult> {
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
    ModelRc::new(VecModel::from(results))
}

#[cfg(windows)]
fn position_jump_window(jump: &DialogJumpWindow, x: i32, y: i32, w: i32, placement: &str) {
    let window = jump.window();
    let scale = window.scale_factor();
    let bar: i32 = (40.0 * scale) as i32;

    // 1. 先同步逻辑宽度和吸附方向给 Slint，确保 Slint 视口按正确宽度渲染，
    //    避免 SetWindowPos 设完物理尺寸后又被逻辑宽度回写打桩
    jump.set_window_width((w as f32 / scale) as f32);
    jump.set_placement(placement.into());

    if placement == "top" {
        // 顶边固定：搜索框不动，结果向下生长，resize 不会让搜索框移动
        window.set_position(slint::PhysicalPosition::new(x, y - bar));
    } else {
        // bottom：底边必须固定 → 用 SetWindowPos 原子设定位置 + 大小
        let h = (jump.get_content_height() * scale) as i32;
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
        with_jump(|state| {
            #[cfg(windows)]
            position_jump_window(&state.window, x, y, w, &placement);
            #[cfg(not(windows))]
            let _ = (x, y, w, &placement);
        });
    });
}

/// 呼出文件对话框跳转吸附窗，并贴合到对话框边缘。
///
/// 不依赖任何会话：主窗口关掉之后吸附窗照样要能弹出。
#[cfg(windows)]
pub(crate) fn show_dialog_jump(
    hwnd: usize,
    current_dir: Option<PathBuf>,
    x: i32,
    y: i32,
    w: i32,
    placement: String,
) {
    // 记下目标与几何，然后把借用放掉再动窗口：show() 会同步派发焦点回调，
    // 那时回调还要借 JUMP（RefCell 二次借用会让进程直接 abort）。
    let Some(window) = with_jump(|state| {
        state.target = Some((hwnd, current_dir.clone()));
        state.geom = Some((x, y, w, placement.clone()));
        state.window.clone_strong()
    }) else {
        return;
    };

    // 1. 先 show() 让底层 HWND 创建完毕，否则首次呼出时
    //    jump_window_hwnd 拿不到句柄，SetWindowPos 定位会静默失效
    let _ = window.show();

    // level 1：每次弹出都留一条现场（含被吸附窗口的几何），误吸附 / 位置偏移
    // 这类偶发问题只能靠它定位。
    logln!(
        "[rove:dialog_jump] show dialog={:#x} rect=({},{} w={}) placement={} dir={:?}",
        hwnd, x, y, w, placement, current_dir
    );

    // 2. 再定位（此时必然能取到有效 HWND，底边锚定一次到位）
    with_jump(|state| position_jump_window(&state.window, x, y, w, &placement));

    // 默认不抢焦点：由用户主动点击吸附条再聚焦，避免在原生对话框里打字被劫持。
    // 仅登记吸附窗自身 HWND，供事件钩子豁免「用户点击吸附条」引发的 FOREGROUND 事件。
    use raw_window_handle::HasWindowHandle;
    if let Ok(rh) = window.window().window_handle().window_handle() {
        if let raw_window_handle::RawWindowHandle::Win32(win) = rh.as_raw() {
            let slint_hwnd = win.hwnd.get() as windows_sys::Win32::Foundation::HWND;
            crate::sys::dialog_jump::register_jump_window(slint_hwnd as isize);
        }
    }

    // 按这个对话框的当前目录重算一次候选：吸附窗里可能还留着上次搜的关键词
    // （清空只发生在「选中一条」之后），不重算就会拿旧对话框的旧目录当搜索范围。
    jump_refresh();
}

/// 对话框移动时实时跟随更新吸附窗位置与吸附方向。
#[cfg(windows)]pub(crate) fn update_dialog_jump_position(x: i32, y: i32, w: i32, placement: String) {
    with_jump(|state| {
        state.geom = Some((x, y, w, placement.clone()));
        position_jump_window(&state.window, x, y, w, &placement);
    });
}

/// 对话框失焦 / 销毁时自动隐藏吸附窗。
///
/// 后端的「已吸附」标记一起清掉：本函数也会被吸附窗自己调用（目标失效 /
/// 没有目标），那种情况钩子那边还记着旧对话框，不清就会永久不再吸附。
pub(crate) fn hide_dialog_jump() {
    crate::sys::dialog_jump::forget_current_dialog();
    let had_target = with_jump(|state| state.target.is_some()).unwrap_or(false);
    if had_target {
        logln!("[rove:dialog_jump] hide（本次是真的在隐藏）");
    }
    let window = with_jump(|state| {
        state.target = None;
        state.geom = None;
        state.window.clone_strong()
    });
    if let Some(window) = window {
        let _ = window.window().hide();
        // 不再 clear_query：防止切换输入法/失焦时清空用户输入。
        // 只有用户真正跳转或主动关闭时才清空。
    }
    crate::sys::dialog_jump::register_jump_window(0);
}

pub fn run() {
    logln!("starting, args: {:?}", std::env::args().collect::<Vec<_>>());

    // GUI 子系统没有控制台，panic 信息会直接丢掉（表现为「窗口突然消失」）。
    // 装个钩子把它写进 rove.log，排查崩溃时才有现场。
    std::panic::set_hook(Box::new(|info| {
        let location = info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
            .unwrap_or_else(|| "?".into());
        let payload = info
            .payload()
            .downcast_ref::<&str>()
            .map(|s| (*s).to_string())
            .or_else(|| info.payload().downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "<非字符串 panic>".into());
        // 无条件写：崩溃现场不能因为「没开日志」就丢掉。
        log_write(&format!("!! [panic] {location}: {payload}"));
    }));

    // 菜单子进程：只构建并显示一张原生菜单，然后退出。
    //
    // 必须在**任何**初始化之前分叉出去：单实例管道、托盘、Slint 事件循环
    // 统统不要碰（碰了就会跟已经在跑的主进程抢东西）。
    #[cfg(windows)]
    if std::env::args().any(|a| a == "--menu-host") {
        crate::sys::menu::run_host_child();
    }

    // 守护模式启动：拉起 Windows 文件对话框监听器（非 Windows 平台为空实现）。
    crate::sys::dialog_jump::start_daemon();

    let args: Vec<String> = std::env::args().skip(1).collect();
    // 静默重启的接班进程标记：老进程释放管道后本进程接管，见 run() 的绑定段。
    let await_pipe = args.iter().any(|a| a == "--await-pipe");
    // 交接时老进程的 pid：接班进程要等它真的消失，管道（单实例）才抢得到。
    let await_pid: Option<u32> = args
        .iter()
        .position(|a| a == "--await-pid")
        .and_then(|i| args.get(i + 1))
        .and_then(|v| v.parse().ok());
    // 注意：不在这里清理 `restart.ok`。清理只由发起交接的那一方在 spawn 之前做，
    // 否则「交接进行中恰好又启动一次 rove」会把标记删掉，害得老进程误判失败。
    // 接管资源管理器拉起的这一次进程没有继承用户的 ROVE_LOG（父进程是浏览器/
    // 下载器），命令行就成了盲区 —— 排查这类场景要靠 exe 同目录的 `rove.verbose`
    // 标记文件（见 `log_level`）。顺带记下有没有程序往命令行里塞 `/select`。
    if args.iter().any(|a| {
        a == "--shell-open"
            || a == "--this-pc"
            || a == "--win-e"
            || a.to_ascii_lowercase().starts_with("/select")
    }) || args.iter().any(|a| a.to_ascii_lowercase().starts_with("-select")) {
        // 正常接管打开只是日常操作，留在 level 1：接管起来的这一进程本来就
        // 不带 ROVE_LOG（父进程是浏览器/下载器），排查时用 exe 同目录的
        // `rove.verbose` 标记即可（见 `log_level`）。
        logln!("[takeover] argv={args:?}");
    }
    // 「谁接住了这次启动」：出问题时第一件事就是要分清这条请求由哪个 exe、
    // 哪个进程处理 —— 磁盘上的 rove.exe 被换掉，已经在跑的旧进程不会跟着更新，
    // 它照样会接住转发过来的请求，然后因为里面没有对应的修复而表现异常。
    let exe = std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    // level 1：每次启动都写一行太吵（用户明确要求默认静默），排查时开
    // ROVE_LOG=1 或放 `rove.verbose` 标记就够了 —— 它只是「这份日志是谁写的」。
    logln!(
        "[boot] pid={} exe={exe} args={args:?}",
        std::process::id()
    );
    let request = crate::daemon::parse_args(&args);
    // 登录自启动（Run 键）带的是 --background：不动已经开着的窗口，什么也不显示。
    let background = crate::daemon::wants_background(&args);

    // ---- 单实例：已存在守护则转发路径后立即退出 ----
    // 先交前台许可：本进程是资源管理器拉起的，带着这个权限，窗口却由守护进程显示。
    crate::daemon::hand_over_foreground();
    // 后台启动绝不走转发：转发路径会唤醒并前置窗口，登录瞬间就弹一个窗口。
    // 交班子进程同样绝不转发：它的任务恰恰是「等老进程交出管道，然后自己接管」，
    // 转发给那个快死的老进程只会把自己立刻送回坟墓 —— 老进程等不到交接标记，
    // 修复/重启流程静默失效（真机现象：自我修复从来没成功过）。
    if !background && !await_pipe {
        let forwarded = crate::daemon::try_forward(&request);
        // forwarded=true：请求交给已在运行的守护进程，本进程随即退出，
        // 后续日志在**它的** exe 目录里 —— 排查时据此判断该去哪看。
        logln!("[boot] forwarded={forwarded}");
        if forwarded {
            logln!("forwarded to running daemon, exiting");
            return;
        }
    }

    // 文件对话框吸附窗：进程级常驻，和主窗口完全解耦（关了主窗口它还在）。
    // 放在转发判断之后：转发进程秒退，没必要建这一套。
    let boot_config = crate::config::Config::load();
    crate::app::init_dialog_jump(&boot_config.language, boot_config.theme);

    // Shell 的「选中这个条目」投递口（IShellView::SelectItem → 事件循环）。
    install_shell_select_sink();

    // 「打开文件所在位置」的冷启动：进程刚起来，窗口还没有，但目录已经知道了。
    // 先把「这个目录的窗口马上就来」预定给 Shell（RegisterPending 不需要窗口），
    // 再慢慢建会话 —— 否则 Shell 可能先放弃，表现为选不中 / 半分钟后冒资源管理器。
    // 只对明确带目录的启动做（--this-pc 没有目录，也没什么可选的）。
    if !background {
        if let Some(path) = request.path.clone() {
            crate::sys::shell_windows::reserve(path);
        }
    }

    // ---- 守护模式：管道服务线程（唤醒请求投递到事件循环） ----
    // 静默重启的接班进程，顺序不能反：
    //   1. 先喊一声「我起来了」（老进程看到标记才退出，它退出才会放开管道）；
    //   2. 等老进程真的消失；
    //   3. 才去抢管道（单实例管道，老进程不退出永远抢不到 —— 反过来就是互等）。
    let mut bound = false;
    if await_pipe {
        if let Err(e) = std::fs::write(restart_marker_path(), std::process::id().to_string()) {
            logal!("--await-pipe: 写交接标记失败 {e}");
        }
        if let Some(pid) = await_pid {
            // 只等 3 秒：老进程正常会在 ~100 ms 内退出（`restart_handoff` 每 100 ms
            // 看一次交接标记），只有它自己卡在退出里才需要等满。等太久的表现就是
            // 托盘图标长时间空窗 —— 而抢管道只是**探测**，抢不到也能降级跑，不必陪等。
            if !wait_for_pid_exit(pid, Duration::from_secs(3)) {
                logal!("--await-pipe: 老进程 pid={pid} 3 秒内没退出，先往下走（IPC 可能抢不到）");
            }
        }
        // 3 秒（30 × 100 ms）：抢管道是每次重开一个实例去试，失败不破坏任何东西，
        // 老进程真死了下一次就成；10 秒只是把托盘空窗拖长。
        for _ in 0..30 {
            bound = bind_ipc_server();
            if bound {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        if bound {
            logln!("--await-pipe: 已接管 IPC (pid={})", std::process::id());
        } else {
            // 抢不到就退回单进程模式：托盘和窗口照常，只是「再开一次 rove」找不到本进程。
            logal!("--await-pipe: 管道一直没抢到，退回单进程模式（IPC 不可用）");
        }
    } else {
        bound = bind_ipc_server();
    }
    if !bound && background && !await_pipe {
        // 后台启动时已有实例在跑：什么都不做，直接退出（正是期望行为）。
        logln!("--background: another instance owns the ipc pipe, exiting");
        return;
    }
    if !bound {
        // 竞态失败：短暂重试转发（另一实例正在启动）。
        for _ in 0..15 {
            std::thread::sleep(Duration::from_millis(100));
            if crate::daemon::try_forward(&request) {
                logln!("forwarded after race, exiting");
                return;
            }
        }
        logln!("ipc bind failed; running standalone");
    }

    // 便携版换了目录（Downloads → 别处）后，把 Run 键里的登录启动命令改写到当前路径。
    // 只在真正的守护进程里做，转发的那一次进程不碰注册表。
    crate::sys::autostart::heal();

    if background {
        START_HIDDEN.with(|flag| flag.set(true));
        logln!("starting in background (--background), no window will be shown");
    }

    // 首个会话：若启动参数带目录则直接打开（首个会话没有已有标签，新开标签无意义）。
    wake_session(crate::daemon::WakeRequest {
        path: request.path.clone(),
        new_tab: false,
        select: request.select.clone(),
    });

    // 守护托盘：UI 销毁后的常驻入口（打开/退出）。
    // 托盘图标必须在**自己的线程**上创建：它的右键菜单用的是模态
    // `TrackPopupMenu`，跑在拥有图标窗口的那个线程里 —— 放在 UI 线程上，
    // 菜单一弹就会冻住整个 Slint 事件循环（见 tray::spawn 的注释）。
    #[cfg(windows)]
    crate::tray::spawn();

    // UI 卡死留证据：心跳 + 看门狗（先装定时器再装看门狗）。
    install_ui_heartbeat();
    start_ui_watchdog();

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

/// 用于「双击打开 / 延迟重命名」的有效双击间隔。
///
/// 必须严格等于系统设置（`GetDoubleClickTime`，默认 500ms）。曾经为了「单击重命名
/// 更跟手」把它封顶到 300ms，结果**慢一点的双击（320~500ms）被当成两次单击**：
/// 文件打不开，落在名称列上还会直接进重命名。用户控制面板里设的间隔就是他的
/// 双击速度，不能替他收紧——宁可重命名晚 200ms，也不能双击打不开。
fn effective_double_click_ms() -> u64 {
    double_click_ms()
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
        // 这一帧是「窗口真正 realize」的时刻。登记给 Shell 依赖真实 HWND，
        // 之前每一次 publish 都可能因为拿不到句柄而空转，这里补一次保证
        // 冷启动时尽快出现在 IShellWindows 里。
        app.borrow().publish_shell_windows();
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

/// 地址栏命令别名（输入后回车直接执行，不再当路径解析）。
///
/// 目前只有 `cmd`：配合右键菜单里的「在此处打开终端」，
/// 地址栏里敲两个字母就能在当前目录开终端。
fn is_address_bar_command(text: &str) -> bool {
    matches!(text.trim().to_ascii_lowercase().as_str(), "cmd" | "cmd.exe")
}

/// 初始化路径该「新开标签」还是「占用当前（默认）标签」。
///
/// 只看 `restored_last_tabs`：`create_session` 永远是从空会话开始的，此时
/// `state.tabs` 里除了「恢复出来的用户标签」就只剩那个还没列举过的默认
/// 「此电脑」标签 —— 顶掉它才对。若这里也听 `requested_new_tab`（接管/命令行
/// 传来的新开标签要求），冷启动时会留下一个没列举过的空「此电脑」标签贴在
/// 左边（表现为「最左侧多出一个显示此文件夹为空的此电脑标签」）。
///
/// 会话已存在时的新开标签要求由 `queue_shell_tab` 负责，跟这里无关。
fn initial_path_opens_new_tab(restored_last_tabs: bool) -> bool {
    restored_last_tabs
}

#[cfg(test)]
mod deep_dedupe_tests {
    use super::dedupe_deep_batch;
    use crate::fs::FileEntry;
    use crate::search::PinyinFields;
    use std::collections::HashSet;
    use std::path::PathBuf;

    fn item(path: &str) -> crate::search::index::DeepItem {
        crate::search::index::DeepItem {
            entry: FileEntry::from_path(std::path::Path::new(path), false, 0, None, false),
            fields: PinyinFields::default(),
        }
    }

    /// 同一路径在同一批里出现两次（旧目录、新目录各报一遍）只留一条。
    #[test]
    fn same_path_in_one_batch_kept_once() {
        let mut seen = HashSet::new();
        let out = dedupe_deep_batch(
            vec![item("C:\\r\\sub\\a.txt"), item("C:\\r\\sub\\a.txt")],
            &mut seen,
        );
        assert_eq!(out.len(), 1);
    }

    /// 已有结果（本地条目 + 之前的深搜批次）里的路径也不许再来一遍。
    #[test]
    fn already_seen_paths_are_skipped_across_batches() {
        let mut seen: HashSet<PathBuf> = HashSet::new();
        seen.insert(PathBuf::from("C:\\r\\a.txt"));
        let first = dedupe_deep_batch(vec![item("C:\\r\\a.txt")], &mut seen);
        assert!(first.is_empty(), "本地/上一批已有的路径不该重复");
        let second = dedupe_deep_batch(vec![item("C:\\r\\sub\\b.txt")], &mut seen);
        assert_eq!(second.len(), 1);
        // seen 必须被写回：下一批再来同一条仍然要挡住。
        let again = dedupe_deep_batch(vec![item("C:\\r\\sub\\b.txt")], &mut seen);
        assert!(again.is_empty());
    }

    /// 不同路径（哪怕同名）是真的两条结果，不能误杀。
    #[test]
    fn same_name_in_different_dirs_is_kept() {
        let mut seen = HashSet::new();
        let out = dedupe_deep_batch(
            vec![item("C:\\r\\a\\x.txt"), item("C:\\r\\b\\x.txt")],
            &mut seen,
        );
        assert_eq!(out.len(), 2);
    }
}

#[cfg(test)]
mod crumb_child_tests {
    use super::crumb_child_of;
    use std::path::Path;

    fn segs(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    /// 上一级：孩子就是当前目录（和 nav_up 一致）。
    #[test]
    fn parent_click_selects_current_dir() {
        let s = segs(&["", "C:\\", "C:\\a", "C:\\a\\b"]);
        assert_eq!(
            crumb_child_of(&s, Some(Path::new("C:\\a"))),
            Some(std::path::PathBuf::from("C:\\a\\b"))
        );
    }

    /// 连跳两级：孩子是被点目录的后一段，不是当前目录。
    #[test]
    fn multi_level_click_selects_intermediate_dir() {
        let s = segs(&["", "C:\\", "C:\\a", "C:\\a\\b", "C:\\a\\b\\c"]);
        assert_eq!(
            crumb_child_of(&s, Some(Path::new("C:\\a"))),
            Some(std::path::PathBuf::from("C:\\a\\b"))
        );
    }

    /// 点「此电脑」：孩子是刚才所在的盘符，回到盘符列表后单选它。
    #[test]
    fn this_pc_click_selects_drive() {
        let s = segs(&["", "C:\\", "C:\\a"]);
        assert_eq!(
            crumb_child_of(&s, None),
            Some(std::path::PathBuf::from("C:\\"))
        );
    }

    /// 回收站是虚拟位置：跳回「此电脑」时不能拿 `recycle://` 去揭示。
    #[test]
    fn recycle_click_back_has_no_child() {
        let s = segs(&["", "recycle://"]);
        assert_eq!(crumb_child_of(&s, None), None);
        // 点在回收站那一格上（已在站内）：也没有孩子可揭示。
        assert_eq!(crumb_child_of(&s, Some(Path::new("recycle://"))), None);
    }

    /// 网络路径按 服务器 / 共享 拆段，跳服务器根时孩子是共享目录。
    #[test]
    fn network_click_selects_share() {
        let s = segs(&["", "\\\\srv", "\\\\srv\\share", "\\\\srv\\share\\a"]);
        assert_eq!(
            crumb_child_of(&s, Some(Path::new("\\\\srv"))),
            Some(std::path::PathBuf::from("\\\\srv\\share"))
        );
    }

    /// 点的是最后一段（自己）：回到原位，没有孩子。
    #[test]
    fn last_segment_has_no_child() {
        let s = segs(&["", "C:\\", "C:\\a"]);
        assert_eq!(crumb_child_of(&s, Some(Path::new("C:\\a"))), None);
    }
}

#[cfg(test)]
mod log_fold_tests {
    use super::{l1_admit, l1_tag};

    /// 标签取第一个方括号里的词；没有方括号就按第一个词归堆，
    /// 否则所有无标签的行会挤进同一个桶互相限流。
    #[test]
    fn tag_is_bracketed_word_or_first_token() {
        assert_eq!(l1_tag("[menu] row right-press idx=3"), "menu");
        assert_eq!(l1_tag("[thumb] 慢请求 12ms"), "thumb");
        assert_eq!(l1_tag("index: rebuild"), "index");
        assert_eq!(l1_tag("running persistent event loop"), "running");
        assert_eq!(l1_tag(""), "other");
    }

    /// 同一标签连着灌：只放行开头的少量，其余折叠成计数（不能一条条写盘）。
    #[test]
    fn same_tag_burst_is_folded() {
        let tag = "[test-fold-burst]";
        let mut admitted = 0;
        let mut folded = 0u32;
        for _ in 0..40 {
            match l1_admit(&format!("{tag} x")) {
                Some((_, _)) => admitted += 1,
                None => folded += 1,
            }
        }
        assert!(admitted >= 1, "至少放行一条，否则日志全瞎");
        assert!(admitted <= 8, "一秒内同一标签最多放行一个容量（{admitted}）");
        assert_eq!(folded, 40 - admitted, "其余全部折叠，一条不漏账");
    }

    /// 不同标签各自记账：一个标签被折叠不该连累别的。
    #[test]
    fn other_tags_are_not_starved_by_one_hot_tag() {
        let hot = "[test-fold-hot]";
        for _ in 0..40 {
            let _ = l1_admit(&format!("{hot} x"));
        }
        assert!(
            l1_admit("[test-fold-cold] y").is_some(),
            "热点标签不能把冷门标签一起饿死"
        );
    }
}

#[cfg(test)]
mod address_bar_command_tests {
    use super::is_address_bar_command;

    #[test]
    fn cmd_aliases_are_commands() {
        assert!(is_address_bar_command("cmd"));
        assert!(is_address_bar_command("CMD"));
        assert!(is_address_bar_command("  cmd  "));
        assert!(is_address_bar_command("cmd.exe"));
        assert!(is_address_bar_command("CMD.EXE"));
    }

    /// 真实路径不能误判成命令：`C:\cmd`、`\\server\cmd`、`D:\tools\cmd.exe`
    /// 都必须照旧走「当路径解析」。
    #[test]
    fn paths_are_not_commands() {
        assert!(!is_address_bar_command(r"C:\cmd"));
        assert!(!is_address_bar_command(r"\\server\cmd"));
        assert!(!is_address_bar_command(r"D:\tools\cmd.exe"));
        assert!(!is_address_bar_command("cmd dir"));
        assert!(!is_address_bar_command(""));
    }
}

fn create_session(
    initial_path: Option<PathBuf>,
    jobs: JobManager,
    initial_reveal: Option<PathBuf>,
) {
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

    // 接管资源管理器：按配置同步一次注册表。放在建窗口之前做，
    // 这样 exe 换过路径 / 被别的程序覆盖后，启动时就能自动纠正；
    // 切换界面语言后重启也会顺带把右键菜单文字一起换掉。
    let takeover_label = i18n::text(&config.language, "用 Rove 打开");
    if let Err(err) = crate::sys::integration::sync(config.explorer_takeover, &takeover_label) {
        logln!("!! 接管资源管理器同步失败：{err}");
    }
    // Win+E 的第二层（键盘钩子）。关掉接管时这里什么都不装。
    crate::sys::win_e::sync(config.explorer_takeover);

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
    ui.set_theme_index(match config.theme {
        ThemeChoice::Light => 1,
        ThemeChoice::Dark => 2,
        ThemeChoice::System => 0,
    });
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
    #[cfg(windows)]
    spawn_thumb_worker(thumb_rx);
    #[cfg(not(windows))]
    drop(thumb_rx);

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
        dir_size_tx,
        dir_size_rx,
        watch_tx,
        watch_rx,
    )));
    // 供延迟重命名等定时器在回调里升级回 App。
    app.borrow_mut().self_weak = Some(Rc::downgrade(&app));
    {
        let prefer = app.borrow().preferred_archive_tool();
        let raw = crate::ops::actions::load_or_create(Some(prefer));
        app.borrow_mut().actions = crate::ops::actions::heal_full(&raw);
    }

    // 先入槽再接线：会话槽持有唯一强引用，回调/定时器均为 Weak；
    // 漏掉此行会导致 App 在本函数返回时即被释放（列表收不到数据）。
    SESSION.with(|slot| *slot.borrow_mut() = Some(app.clone()));

    wire_callbacks(&app);
    setup_timers(&app);
    setup_keyboard(&app);

    // 初始数据。
    {
        let mut app = app.borrow_mut();
        app.state.recent = app.config.recent.clone();
        // 是否真的按「上次的标签页」重建了标签（决定初始路径是新增标签还是顶替）。
        let mut restored_last_tabs = false;
        if startup_snapshot.0 == "last_tabs" && !startup_snapshot.1.is_empty() {
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
            restored_last_tabs = true;
        }
        // 命令行 / 接管资源管理器带来的初始路径。
        // 恢复过上次标签页时必须新开标签，不能顶掉用户上次的标签；
        // 否则直接把默认那个「此电脑」标签换成目标目录 —— 早先这里是
        // 「再多 push 一个标签」，默认的「此电脑」标签就既没列举也没人切过去，
        // 表现为最左侧多出一个显示「此文件夹为空」的此电脑标签。
        if let Some(path) = initial_path {
            // 恢复出来的标签里已经有这个目录：切过去就好，别再开一个一模一样的。
            // 热会话的 reveal 走 locate_folder 去重，冷启动这条路径也要一致，
            // 否则同一目录会出现两个标签（用户右键「用 Rove 打开」一个已经
            // 恢复出来的文件夹时最容易撞上）。
            let already_open = if restored_last_tabs {
                app.locate_folder(&path)
            } else {
                None
            };
            if let Some((tab_index, pane_index)) = already_open {
                app.state.active_tab = tab_index;
                app.state.tabs[tab_index].active_pane = pane_index;
            } else if initial_path_opens_new_tab(restored_last_tabs) {
                // inherit=false：路径是明确给的，新标签不该再继承当前目录。
                let sort = app.state.active().active().sort;
                app.state.new_tab_ex(Some(path), sort, false);
            } else {
                app.state.active_mut().active_mut().navigate(Some(path));
            }
        }
        // 目录已经定下来了就把窗格登记给 Shell —— 不要等下面那一串 UI 同步
        // 和 `sync_file_list`：冷启动时「打开文件所在位置」的选中请求正在等
        // 我们出现在 IShellWindows 里，越早越好（窗口尚未 realize 时
        // publish 会自己跳过，最后由 sync_file_list 那一次兜底）。
        app.publish_shell_windows();
        // 「电脑选项」不做事：AppState 初始就是单栏的「此电脑」。
        // 但它的排序必须用「此电脑」自己的状态（主键是盘符），
        // 不能沿用普通目录的全局排序。
        app.sync_view_sort();
        app.sync_tabs();
        // 冷启动恢复出来的分屏标签自带 active_pane（快照里的聚焦栏）。
        // 漏了这一次同步，顶部的聚焦条会一直停在第 0 栏：视觉在左、
        // 键盘/前进后退却作用在右栏。sync_file_list 只写 is-split，
        // 不写 active-pane，所以必须单独同步一次。
        app.sync_split_state();
        app.sync_nav();
        app.sync_breadcrumbs();
        app.sync_status();
        app.sync_sidebar();
        // 盘符容量 / 回收站在后台线程取，结果到了再重绘一次侧栏。
        app.request_sidebar_probe();
        app.sync_staging();
        app.sync_file_list();
        // 「在文件夹中显示」：必须在 reload_active 之前挂上，否则列举结果回来时
        // 没有目标可匹配（结果可能同帧就到，来不及补挂）。
        app.arm_reveal(initial_reveal);
        // 恢复会话时上面已经给每个面板都发过列举请求，这里再刷一次就是同一目录
        // 列两遍（网络盘上等于双倍等待 + 双倍 IO，日志里表现为同一 tab 两条
        // request_listing）。新开的标签/navigate 分支自己会请求，不必兜底。
        if !restored_last_tabs {
            app.reload_active();
        }
        // 启动即预热全盘索引：懒加载会让首次搜索撞上冷启动（等待扫描），
        // 且状态栏空等。shared() 是幂等的（OnceLock），常驻进程重复调用无副作用。
        //
        // 位置很关键：它要读十几万条缓存（实测约 300ms），而上面那次
        // `sync_file_list` 已经把窗格登记给 Shell 了。冷启动时「打开文件所在
        // 位置」的选中请求就在等这一步，绝不能让它挡在登记之前。
        // 索引改为**按需加载**：启动不再把几十万条读进内存。
        // 用不上全盘搜索的人（多数人）不必替它付几十上百兆；第一次搜索或打开
        // 「搜索与索引」设置页时才在后台读缓存（见 `index::warm_up_async`）。
        logln!("[boot] index lazy (loads on first global search / index settings page)");
        // 启动时窗口还没显示，标签条其实一个像素都没布局（视口宽度为 0），
        // 恢复出来的「当前标签」可能落在可视区外，用户看不到自己在哪一页。
        // 这里只丢一个目标下标，TabBar 内部会等布局落定后补滚
        // （见 tab_bar.slint 的 scroll-pending）。已在可视区内则不会移动。
        let active_tab = app.state.active_tab as i32;
        app.ui.invoke_scroll_tabs_to_index(active_tab);
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

    // `--background`（登录自启动）：只把会话建起来，不显示窗口。
    // 托盘、文件对话框吸附、全盘索引预热都依赖 SESSION 存在，所以会话照建，
    // 只是省掉 show() —— 窗口留到用户点托盘 / 双击文件夹 / 再次启动 Rove 时再出现。
    // 注意：HWND 登记与无边框拖拽钩子装在 on_winit_window_event 的 RedrawRequested
    // 里，首次 show() 时会补做，所以这里跳过的不是「只做一次」的初始化。
    if START_HIDDEN.with(|flag| flag.replace(false)) {
        let mut app = app.borrow_mut();
        if first_run {
            app.first_run_pending = true;
        }
        app.window_pending_show = true;
        logln!("session created hidden (--background); tray + dialog hook are live");
        SESSION_ALIVE.store(true, std::sync::atomic::Ordering::Release);
        return;
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

    logln!("[boot] showing window");
    app.borrow().ui.show().unwrap();

    // 强制立即重绘：让首次 RedrawRequested 尽快触发，及时应用首次启动 75% 居中
    // （否则首屏列表/索引等工作会把首帧推迟到数秒后，出现“先默认位置、再居中”的闪烁）。
    app.borrow().ui.window().request_redraw();

    // 首次启动 75% 居中：必须在窗口 realize 后（scale_factor 正确）由首帧 redraw 应用，
    // 见 apply_deferred_geometry（on_winit_window_event 注册）。
    if first_run {
        app.borrow_mut().first_run_pending = true;
    }
    // 登记给 Shell 必须在这里补一次：`get_window_hwnd` 在窗口 show() 之前拿不到
    // 句柄，所以 create_session 内部那些 publish 全是空转。不补这一下，登记要等到
    // 「第一次列举结果回来 → sync_file_list」，网络盘上就是好几秒之后 ——
    // 冷启动时浏览器/下载器的「打开文件所在位置」早就等不及了。
    app.borrow().publish_shell_windows();
    logln!("[boot] shell registration done");

    logln!("session created");
    // 启动就采一条：哪怕只开几秒也能看到「地板」是多少。
    app.borrow_mut().log_memory_snapshot();
    SESSION_ALIVE.store(true, std::sync::atomic::Ordering::Release);
}

/// Shell 通过 `IShellView::SelectItem` 发来的「选中这个条目」请求。
///
/// 浏览器 / 下载器 / 微信的「打开文件所在位置」走的是 `SHOpenFolderAndSelectItems`，
/// 选中项只在 pidl 里流转、**不进命令行**，所以只能由 Shell 回调我们的
/// `IShellView`（见 `sys::shell_windows`）拿到。这个函数在事件循环线程上执行。
pub fn reveal_in_session(target: PathBuf) {
    reveal_in_session_attempt(target, 0);
}

/// 一次选中请求的处理。`attempt` 是「界面正在忙、稍后重来」的次数上限。
fn reveal_in_session_attempt(target: PathBuf, attempt: u32) {
    logln!("[shell-windows] 界面线程收到选中请求 {target:?}");
    let session = SESSION.with(|slot| slot.borrow().clone());
    if let Some(app) = session {
        match try_reveal_in_session(&app, &target) {
            RevealOutcome::Done => {
                let ui = slint::ComponentHandle::clone_strong(&app.borrow().ui);
                let _ = ui.show();
                focus_window(&ui);
                return;
            }
            RevealOutcome::Busy => {
                // 正在处理别的 Slint 回调：稍后重来，别丢掉这次请求。
                //
                // 这条分支里**不能碰 app**：Busy 的含义就是「App 正被可变借用」，
                // 此时再 `app.borrow()` 会立刻 panic（RefCell 双借用 → 进程 abort）。
                // 窗口本来就因为会话存在而处于显示状态，这里只要安排重试即可。
                if attempt < 20 {
                    slint::Timer::single_shot(Duration::from_millis(50), move || {
                        reveal_in_session_attempt(target, attempt + 1);
                    });
                } else {
                    logln!("!! [shell-windows] 界面一直忙，选中请求放弃");
                }
                return;
            }
            RevealOutcome::NotOpen => {}
        }
    }
    // 没有哪个窗格在显示这个目录：与「接管资源管理器」同一条链路
    // （新标签打开 + 列表加载完选中）。
    let folder = target.parent().map(Path::to_path_buf);
    wake_session(crate::daemon::WakeRequest {
        path: folder,
        new_tab: true,
        select: Some(target),
    });
}

enum RevealOutcome {
    /// 已经选中。
    Done,
    /// 没有窗格显示这个目录。
    NotOpen,
    /// App 正被别的回调借用，稍后重试。
    Busy,
}

/// 在当前会话里找到「正在显示该目录」的窗格，切过去并选中。
fn try_reveal_in_session(app: &Rc<RefCell<App>>, target: &Path) -> RevealOutcome {
    let Ok(mut app) = app.try_borrow_mut() else {
        return RevealOutcome::Busy;
    };
    let Some(folder) = target.parent() else {
        return RevealOutcome::NotOpen;
    };
    let Some((tab_index, pane_index)) = app.locate_folder(folder) else {
        return RevealOutcome::NotOpen;
    };
    let changed = tab_index != app.state.active_tab
        || pane_index != app.state.active().active_pane;
    if changed {
        logv!("[shell-windows] 切到已有窗格 tab={tab_index} pane={pane_index}");
        app.state.active_tab = tab_index;
        app.state.tabs[tab_index].active_pane = pane_index;
        app.sync_active_tab();
    }
    app.select_path_in_active_pane(target);
    RevealOutcome::Done
}

/// 装上 Shell 的选中请求投递口（只会成功一次）。
fn install_shell_select_sink() {
    crate::sys::shell_windows::set_select_sink(|path| {
        if slint::invoke_from_event_loop(move || reveal_in_session(path)).is_err() {
            logln!("!! [shell-windows] 事件循环不可用，选中请求丢弃");
        }
    });
}

/// 唤醒：无 UI 则重建会话（冷启动 < 50ms）；有 UI 则前置窗口并导航。
pub fn wake_session(request: crate::daemon::WakeRequest) {
    let existing = SESSION.with(|slot| slot.borrow().clone());
    logv!(
        "[wake] 会话已存在={} new_tab={} path={:?} (pid={})",
        existing.is_some(),
        request.new_tab,
        request.path,
        std::process::id()
    );
    if let Some(app) = existing {
        {
            let mut app = app.borrow_mut();
            let select = request.select.clone();
            // 「在文件夹中显示」没给目录时，用条目的父目录；这里只做字符串截断，
            // 不 stat（UI 线程碰网络路径会卡窗口）。
            let path = request.path.clone().or_else(|| {
                select
                    .as_ref()
                    .and_then(|p| p.parent())
                    .map(std::path::Path::to_path_buf)
            });
            if let Some(p) = path {
                if request.new_tab {
                    // 新标签要在借用释放后由定时器执行（见 queue_shell_tab）。
                    app.queue_shell_tab(Some(p), select);
                } else {
                    app.navigate_active_reveal(Some(p), select);
                }
            } else if request.new_tab {
                // 此电脑：同样新开标签，不动用户当前目录。
                app.queue_shell_tab(None, None);
            }
        }
        // 借用必须先释放：ShowWindow / SetForegroundWindow 会同步派发窗口消息
        // 到我们子类化的窗口过程，Slint 侧回调可能再次借用 App → 进程被 abort。
        let ui = slint::ComponentHandle::clone_strong(&app.borrow().ui);
        // 后台会话（--background）的窗口是「建了但没显示」的：最大化状态、首帧
        // 重绘、拖拽热区都还没落地，这里补一次（与 create_session 里 show 前后那几步一致）。
        // 只读不消费：`show()` 失败时标记必须留着，下一次点击才会再走一遍
        // 最大化/首帧/拖拽热区那几步；否则窗口会永久停在「建了但没显示」。
        let pending_show = app.borrow().window_pending_show;
        if pending_show {
            let maximized = app.borrow().config.window_maximized;
            ui.window().set_maximized(maximized);
            ui.set_win_maximized(!maximized);
        }
        let shown = ui.show();
        if let Err(e) = &shown {
            // show 失败还照样清掉 pending 标记的话，后面每次点击都会走
            // 「已经显示过」的分支，窗口就永远出不来了 —— 必须留下证据。
            logal!("[wake] !! ui.show() 失败：{e}（窗口可能无法显示）");
        }
        logv!(
            "[wake] show ok={} pending_show={} size={:?}",
            shown.is_ok(),
            pending_show,
            ui.window().size()
        );
        if pending_show && shown.is_ok() {
            app.borrow_mut().window_pending_show = false;
            app.borrow().publish_shell_windows();
            // 首帧重绘要显式请求：后台会话没有走过启动时的 request_redraw，
            // 首帧不来的话 HWND 登记与无边框拖拽钩子会一直缺位。
            ui.window().request_redraw();
        }
        focus_window(&ui);
        logv!("woke existing session, path={:?} new_tab={}", request.path, request.new_tab);
        return;
    }
    // JobManager 的任务线程跨会话存活；此处为新会话重建一份管理句柄，
    // 使 jobs 状态与 UI 生命周期解耦。
    let reveal = request.select.clone();
    let initial_path = request.path.clone().or_else(|| {
        request
            .select
            .as_ref()
            .and_then(|p| p.parent())
            .map(std::path::Path::to_path_buf)
    });
    create_session(initial_path, JobManager::new(), reveal);
    // 会话刚建起来就再「显示 + 前置」一次。
    //
    // 托盘上「第一次点击只闪一下、第二次才真的打开」正是缺了这一步：第二次
    // 点击走的是「已有会话 → show + focus」，而第一次只走到 create_session
    // 末尾那次 show —— 长时间没有会话（窗口关掉后放着不动）时，那一次 show
    // 偶尔不能把窗口真正落到屏幕上。这里补的正是用户手动做的第二次点击。
    //
    // 后台会话（--background）不能做：它的窗口本来就应该保持隐藏，
    // `window_pending_show` 就是那个标记。
    if let Some(app) = SESSION.with(|slot| slot.borrow().clone()) {
        let pending_show = app.borrow().window_pending_show;
        if !pending_show {
            let ui = slint::ComponentHandle::clone_strong(&app.borrow().ui);
            logv!("[wake] 补一次 show，窗口尺寸={:?}", ui.window().size());
            let _ = ui.show();
            ui.window().request_redraw();
            focus_window(&ui);
        }
    }
}

/// 销毁当前会话：丢弃窗口（释放 GPU 资源与组件树）与全部状态，
/// 仅保留管道守护线程，常驻内存降到守护水位。
pub fn destroy_session() {
    // 幂等守门：会话只能被销毁一次。窗口关闭按钮、托盘退出、系统关闭请求
    // 可能各自排定一次销毁，重复那次必须在这里直接吞掉——否则会再打一条
    // “destroy_session running” 的假日志，看起来像销毁了两遍。
    let app = SESSION.with(|slot| slot.borrow_mut().take());
    let Some(app) = app else {
        crate::logv!("[session] destroy ignored (already destroyed)");
        return;
    };
    logln!("destroy_session running");
    SESSION_ALIVE.store(false, std::sync::atomic::Ordering::Release);
    // 销毁本该在一瞬间做完。卡住时进程既没有窗口、事件循环也不再转，托盘点了
    // 没反应，用户只能去任务管理器杀 —— 记下开始时刻，看门狗超时后自己换进程。
    CLOSING_AT.store(now_ms().max(1), std::sync::atomic::Ordering::Release);
    // 退出存档：窗口几何（逻辑像素）+ 最大化状态 + 上次标签页快照。
    // 统一在销毁出口完成，保证「窗口 X」与「托盘退出」两条路径都能落盘。
    {
        let mut app = app.borrow_mut();
        // 注意：size() 是物理像素，配置里的窗口尺寸按逻辑像素保存，
        // 否则在高 DPI 下每次会话窗口都会按缩放系数膨胀。
        let window = app.ui.window();
        let scale = window.scale_factor() as f64;
        // `--background` 建的会话可能从未显示过窗口：这时窗口还没真正显示，
        // GetWindowPlacement / IsZoomed 拿到的是初始值（w/h 可能是 0、最大化恒为
        // false），存盘会把用户上次的窗口几何和最大化状态冲掉。宁可不存这些字段。
        // 典型路径：登录后台常驻 → 直接托盘「退出」，全程没开窗口。
        let never_shown = app.window_pending_show;
        // 退出存档的窗口几何统一按「逻辑像素」保存（size()/矩形都是物理像素，
        // 不折算的话高 DPI 下每次会话窗口都会按缩放系数膨胀）。
        // 关键：必须用 GetWindowPlacement 的 rcNormalPosition（恢复正常矩形），
        // 而非 GetWindowRect——后者在最大化/最小化时返回的是「最大化矩形」（会溢出
        // 工作区、left/top 为负）。据此存盘会把正常几何污染成最大化几何，导致下次
        // 启动还原最大化后、再「退出最大化」时落到错误的尺寸/位置。
        #[cfg(windows)]
        if !never_shown {
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
        }
        #[cfg(not(windows))]
        if !never_shown {
            let size = window.size();
            app.config.window_width = (size.width as f64 / scale).round() as u32;
            app.config.window_height = (size.height as f64 / scale).round() as u32;
        }
        // 「上次的标签页(包括分屏)」：记下每个标签各栏的路径。
        // 被「关闭全部 / 关掉最后一个标签」收尾的会话不写：那是用户明确表示
        // 这一轮的标签不要了，下次只该开一个「此电脑」。
        app.config.last_tabs = if app.tabs_discarded {
            Vec::new()
        } else {
            app.state
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
                .collect()
        };
        app.config.last_active_tab = app.state.active_tab;
        logv!(
            "[win-save] w={} h={} x={} y={} maximized={} hwnd_present={} tabs={} never_shown={}",
            app.config.window_width,
            app.config.window_height,
            app.config.window_x,
            app.config.window_y,
            app.config.window_maximized,
            get_window_hwnd(&app.ui).is_some(),
            app.config.last_tabs.len(),
            never_shown
        );
        app.config.save();
    }
    // 显式隐藏后整体 drop：回调均为 Weak，无强引用环，
    // Slint 组件树与 winit 窗口（GPU 纹理、表面）随之释放。
    let ui = app.borrow().ui.as_weak();
    // 先记下 HWND：Slint 的 hide() 是往事件循环里排的一次请求，如果这之后
    // 组件被 drop（或被漏出去的强引用留在内存里），那次 hide 可能压根没执行到，
    // 窗口就会变成「桌面上看不见、却仍占着那块区域吃点击」的透明窗口，
    // 任务栏里还留着能最小化/还原的条目。所以下面既同步 ShowWindow 一次，
    // drop 之后再审计一遍。
    #[cfg(windows)]
    let close_hwnd = get_window_hwnd(&app.borrow().ui);
    if let Some(w) = ui.upgrade() {
        let _ = w.hide();
    }
    #[cfg(windows)]
    if let Some(hwnd) = close_hwnd {
        use windows_sys::Win32::UI::WindowsAndMessaging::{ShowWindow, SW_HIDE};
        unsafe {
            ShowWindow(hwnd as _, SW_HIDE);
        }
        logv!("[close] hide requested + ShowWindow(SW_HIDE) hwnd={:#x}", hwnd);
    }
    // 有原生菜单挂在那儿（用户正开着右键菜单，或者第三方扩展把菜单线程拖住了）
    // 时，销毁会话会撞上它：真机日志停在「dirsize SAVE」之后、`session
    // destroyed` 之前，UI 线程再没回来，托盘也就永远唤不醒窗口。留一条证据。
    if let Some(ms) = crate::sys::menu::busy_for_ms() {
        logal!("!! [close] 有原生菜单挂着 {ms}ms，直接杀掉菜单子进程");
    }
    // 菜单在建在子进程里：关窗时把这个子进程杀掉就干净了，销毁会话不会被它拖住。
    crate::sys::menu::shutdown();
    // 会话销毁时，将累计的目录容量缓存刷写到系统 Temp 目录（重启不丢）。
    if let Ok(mut store) = global_dir_sizes().lock() {
        store.save_to_temp();
    }
    logv!("[close] step 1/4 dirsize saved");
    // 有任务在跑就不换进程：重启会杀掉进行中的复制 / 移动。
    // `ROVE_FORCE_RESTART=1` 只为验证交接流程用。
    let jobs_running = app.borrow().jobs.is_running();
    let restart_wanted = !QUITTING.load(std::sync::atomic::Ordering::Acquire)
        && std::env::var_os("ROVE_FORCE_RESTART").is_some()
        && !jobs_running;
    logv!("[close] step 2/4 restart_wanted={} -> dropping session", restart_wanted);
    // 会话销毁本身也可能卡死（真机：卡在 `drop(session)` 里再也不回来，
    // 于是本进程赖着不放 IPC 和托盘图标，新进程只能转发给这个僵尸）。
    // 交给一个后台线程盯着：真的卡住就硬终止，绝不留下第二个僵尸。
    spawn_destroy_guard(jobs_running);
    drop(app);
    logv!("[close] step 3/4 session dropped");
    // 审计：组件 drop 之后主窗口应该已经不在了（IsWindow 返回 0）。
    // 如果它还在且仍可见，就说明某个强引用把窗口留在了内存里——这正是
    // 「窗口消失了但还盖着一层透明窗口」的成因，强制藏起来并留证据。
    #[cfg(windows)]
    if let Some(hwnd) = close_hwnd {
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            IsWindow, IsWindowVisible, ShowWindow, SW_HIDE,
        };
        unsafe {
            if IsWindow(hwnd as _) != 0 && IsWindowVisible(hwnd as _) != 0 {
                logal!(
                    "!! [close] ghost window still visible after session drop: hwnd={:#x} -> hiding",
                    hwnd
                );
                ShowWindow(hwnd as _, SW_HIDE);
            } else {
                logv!("[close] window gone after drop (hwnd {:#x})", hwnd);
            }
        }
    }
    if restart_wanted {
        logln!("[restart] 菜单线程已达上限，关窗后静默换进程");
        std::thread::Builder::new()
            .name("rove-restart".into())
            .spawn(restart_handoff)
            .ok();
    }
    CLOSING_AT.store(0, std::sync::atomic::Ordering::Release);
    logln!("session destroyed; daemon resident");
}

/// 把窗口前置并聚焦（WSLg/真实 Windows 均适用）。
fn focus_window(ui: &MainWindow) {
    focus_window_attempt(ui, 0);
}

/// 取窗口句柄 + 激活的实体。返回 `false` 表示**句柄还没准备好**。
///
/// 这一点在托盘唤醒上很关键：`ui.show()` 之后 Slint 并不会立刻把 HWND 交出来
/// （实测要等首帧重绘走完，约 0.3–0.4 秒），此时整个函数会静默返回，窗口就
/// 变成「已经显示、但没被激活到前台」——用户看到的就是「点一下只闪一下，
/// 点第二下才真的打开」。
fn activate_window(ui: &MainWindow) -> bool {
    #[cfg(windows)]
    {
        use raw_window_handle::RawWindowHandle;
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            BringWindowToTop, GetForegroundWindow, GetWindowThreadProcessId, IsIconic, IsWindowVisible,
            SetForegroundWindow, ShowWindow, SW_RESTORE, SW_SHOW,
        };
        // 两步获取：HasWindowHandle::window_handle() → rwh WindowHandle → as_raw()。
        if let Ok(h) = raw_window_handle::HasWindowHandle::window_handle(&ui.window().window_handle())
        {
            if let RawWindowHandle::Win32(win) = h.as_raw() {
                let hwnd = win.hwnd.get() as isize as windows_sys::Win32::Foundation::HWND;
                unsafe {
                    let was_iconic = IsIconic(hwnd) != 0;
                    let visible = IsWindowVisible(hwnd) != 0;
                    // 只有「最小化了」才 RESTORE：SW_RESTORE 对最大化窗口会把窗口
                    // 还原成普通大小（表现为「打开一个文件夹，Rove 就退出最大化」）。
                    if was_iconic {
                        ShowWindow(hwnd, SW_RESTORE);
                    } else if !visible {
                        ShowWindow(hwnd, SW_SHOW);
                    }
                    // 抢前台：本进程不是当前前台进程时 Windows 会拒绝，并让任务栏
                    // 按钮闪黄（用户看到的就是「窗口不还原、任务栏一直闪」）。
                    // 标准解法：临时 AttachThreadInput 到前台线程，取得同一条输入队列
                    // 后再激活；结束后立即 detach。首次接管（由 explorer 拉起的新进程
                    // 转发 IPC）拿不到前台权限，走这个兜底才稳。
                    let mut set_ok = SetForegroundWindow(hwnd) != 0;
                    if !set_ok {
                        let foreground = GetForegroundWindow();
                        let foreground_thread = GetWindowThreadProcessId(foreground, std::ptr::null_mut());
                        let self_thread =
                            windows_sys::Win32::System::Threading::GetCurrentThreadId();
                        if foreground_thread != 0 && foreground_thread != self_thread {
                            let attached = windows_sys::Win32::System::Threading::AttachThreadInput(
                                self_thread,
                                foreground_thread,
                                1,
                            ) != 0;
                            if attached {
                                BringWindowToTop(hwnd);
                                set_ok = SetForegroundWindow(hwnd) != 0;
                                let _ = windows_sys::Win32::System::Threading::AttachThreadInput(
                                    self_thread,
                                    foreground_thread,
                                    0,
                                );
                            }
                        }
                    }
                    // 兜底：抢前台被拒时个别 Windows 版本会连还原一起吞掉，
                    // 这里再异步补一次还原，确保窗口不会停在任务栏。
                    if IsIconic(hwnd) != 0 {
                        windows_sys::Win32::UI::WindowsAndMessaging::ShowWindowAsync(
                            hwnd,
                            SW_RESTORE,
                        );
                    }
                    logv!(
                        "[focus] iconic {}->{} visible={} foreground={}",
                        was_iconic,
                        IsIconic(hwnd) != 0,
                        IsWindowVisible(hwnd) != 0,
                        set_ok
                    );
                }
                return true;
            }
        }
        false
    }
    #[cfg(not(windows))]
    {
        let _ = ui;
        true
    }
}

/// 首帧还没落地时句柄是空的：50ms 一轮等它出现，最多 1.5 秒。
fn focus_window_attempt(ui: &MainWindow, attempt: u32) {
    if activate_window(ui) {
        if attempt > 0 {
            logv!("[focus] 等到窗口句柄，第 {} 轮激活", attempt);
        }
        return;
    }
    if attempt >= 30 {
        logln!("!! [focus] 始终拿不到窗口句柄，放弃激活");
        return;
    }
    let weak = ui.as_weak();
    slint::Timer::single_shot(Duration::from_millis(50), move || {
        if let Some(ui) = weak.upgrade() {
            focus_window_attempt(&ui, attempt + 1);
        }
    });
}


// ---------------------------------------------------------------------------
// 批量重命名窗口
// ---------------------------------------------------------------------------

/// 一个待改名的条目（窗口打开时拍下的快照）。
struct BatchItem {
    path: PathBuf,
    name: String,
    is_dir: bool,
    checked: bool,
}

/// 一个批量重命名窗口的全部状态。
struct BatchSession {
    items: Vec<BatchItem>,
    /// 所在目录现有条目名（小写）：判断「目标名已存在」用，避免在界面线程上查盘。
    existing: HashSet<String>,
    options: crate::ops::batch_rename::Options,
    filter: i32,
    /// 最近一次预览的计算结果（下标与 items 平行），供提交时取新名字。
    previews: Vec<crate::ops::batch_rename::Preview>,
    /// 窗口逻辑宽度：决定名字折行的位置。
    win_w: f32,
    /// 名字列宽（折行和 Slint 里的绝对坐标共用同一个数）。
    col_w: f32,
    /// 排好版的可见行（手写窗口化只从这里取视口附近的一小段）。
    laid: Vec<BatchLaidRow>,
    /// 内容总高度。
    laid_h: f32,
    /// 列表视口高度（Slint 每次变化都报上来）。
    view_h: f32,
    /// 滚动偏移，向下为正。
    scroll: f32,
    /// 上次推给界面的行区间 `[start, end)`；区间没变就不重建模型。
    win: (usize, usize),
    lang: String,
}

/// 一行排好版的结果：滚动时直接从这儿取，不重新算预览和折行。
struct BatchLaidRow {
    /// 对应 `BatchSession::items` 的下标（勾选状态从那儿读）。
    item: usize,
    /// 在内容坐标里的顶部位置与高度（绝对定位用）。
    y: f32,
    h: f32,
    /// 折行时用的预算宽度：窗口宽度变了就得重折。
    budget: f32,
    /// 名字列宽（与折行预算同源，Slint 按它摆两列）。
    col_w: f32,
    orig: ModelRc<ModelRc<RenamePart>>,
    new: ModelRc<ModelRc<RenamePart>>,
    /// 折行前的原始片段，用来判断「这一行能不能沿用上一版」。
    orig_raw: Vec<(String, bool)>,
    new_raw: Vec<(String, bool)>,
    state: i32,
    reason: SharedString,
}

struct BatchWindowEntry {
    /// 窗口编号：后台线程回传结果时用它找回来（Rc 不能跨线程）。
    id: u64,
    ui: slint::Weak<BatchRenameWindow>,
    state: Rc<RefCell<BatchSession>>,
}

thread_local! {
    /// 所有打开中的批量重命名窗口。可以同时有多个，各自独立。
    static BATCH_WINDOWS: RefCell<Vec<BatchWindowEntry>> = RefCell::new(Vec::new());
    static BATCH_NEXT_ID: std::cell::Cell<u64> = const { std::cell::Cell::new(1) };
}

/// 按编号找回窗口与其状态（只在界面线程里用）。
fn batch_lookup(id: u64) -> Option<(BatchRenameWindow, Rc<RefCell<BatchSession>>)> {
    BATCH_WINDOWS.with(|list| {
        list.borrow()
            .iter()
            .find(|e| e.id == id)
            .and_then(|e| e.ui.upgrade().map(|ui| (ui, e.state.clone())))
    })
}

/// 打开一次批量重命名窗口。
impl App {
    fn open_batch_rename(&mut self, paths: Vec<PathBuf>) {
        if paths.is_empty() || self.pane_is_recycle() {
            return;
        }
        let pane = self.state.active().active();
        let dir = pane.path.clone();
        // 现有条目名：只在该面板正看着「这些项所在目录」时才有意义；
        // 搜索结果是跨目录的，那种情况下退回空集（提交前还会再查一次磁盘）。
        let same_dir = dir
            .as_ref()
            .is_some_and(|d| paths.iter().all(|p| p.parent() == Some(d.as_path())));
        let existing: HashSet<String> = if same_dir {
            pane.entries
                .iter()
                .map(|e| e.name.to_lowercase())
                .collect()
        } else {
            HashSet::new()
        };
        let is_dir_of = |p: &PathBuf| -> bool {
            pane.entries
                .iter()
                .find(|e| e.path == *p)
                .map(|e| e.is_dir)
                .unwrap_or(false)
        };
        let mut items: Vec<BatchItem> = Vec::with_capacity(paths.len());
        for p in paths {
            let name = p
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            if name.is_empty() {
                continue;
            }
            let is_dir = is_dir_of(&p);
            items.push(BatchItem { path: p, name, is_dir, checked: true });
        }
        if items.is_empty() {
            return;
        }

        // 初始尺寸完全照主窗口首次启动那套：屏的 75%，居中，且不超出工作区。
        // 主窗口的 scale_factor 此时已是真实值（批量窗口刚建还没显示）。
        let scale = self.ui.window().scale_factor().max(1.0);
        let (screen_w, screen_h) = screen_size_logical(scale).unwrap_or((1920.0, 1080.0));
        let mut start_w = (screen_w * 0.75).max(720.0);
        let mut start_h = (screen_h * 0.75).max(460.0);
        if let Some((aw, ah)) = work_area_logical(scale) {
            start_w = start_w.min(aw).max(720.0);
            start_h = start_h.min(ah - 60.0).max(460.0); // 留出标题栏
        }
        let start_x = ((screen_w - start_w) / 2.0).max(0.0);
        let start_y = ((screen_h - start_h) / 2.0).max(0.0);

        let session = Rc::new(RefCell::new(BatchSession {
            previews: vec![Default::default(); items.len()],
            laid: Vec::new(),
            laid_h: 0.0,
            view_h: 0.0,
            scroll: 0.0,
            win: (usize::MAX, usize::MAX),
            items,
            existing,
            filter: 0,
            win_w: start_w,
            col_w: 0.0,
            options: crate::ops::batch_rename::Options {
                search: self.config.batch_rename.search.clone(),
                replace: self.config.batch_rename.replace.clone(),
                use_regex: self.config.batch_rename.use_regex,
                match_all: self.config.batch_rename.match_all,
                case_sensitive: self.config.batch_rename.case_sensitive,
                pinyin: self.config.batch_rename.pinyin,
                apply_to: crate::ops::batch_rename::ApplyTo::from_index(
                    self.config.batch_rename.apply_to,
                ),
            },
            lang: self.config.language.clone(),
        }));

        let ui = match BatchRenameWindow::new() {
            Ok(u) => u,
            Err(err) => {
                logln!("!! [batch-rename] 建窗口失败：{err}");
                return;
            }
        };
        // 文案与主题：新窗口是独立的组件树，两个全局都要各自下发一次。
        ui.global::<I18n>()
            .set_strings(ModelRc::new(VecModel::from(i18n::strings_for(
                &self.config.language,
            ))));
        apply_theme_to(&ui.global::<Theme>(), resolve_theme(self.config.theme));

        let id = BATCH_NEXT_ID.with(|c| {
            let id = c.get();
            c.set(id + 1);
            id
        });
        batch_push_static(&ui, &self.config.language);
        {
            // 上次用过的搜索词/替换词与三个开关也一并恢复。
            let s = session.borrow();
            ui.set_search_text(SharedString::from(s.options.search.as_str()));
            ui.set_replace_text(SharedString::from(s.options.replace.as_str()));
            ui.set_opt_regex(s.options.use_regex);
            ui.set_opt_match_all(s.options.match_all);
            ui.set_opt_case(s.options.case_sensitive);
            ui.set_opt_pinyin(s.options.pinyin);
            ui.set_opt_apply_to(s.options.apply_to.to_index());
        }
        batch_wire(&ui, &session, id);
        batch_refresh(&ui, &session);

        ui.window().on_close_requested(move || {
            // 关闭按钮只让窗口隐藏，不留登记（否则关掉的窗口会一直被引用着）。
            let state = BATCH_WINDOWS.with(|list| {
                let mut list = list.borrow_mut();
                let found = list.iter().position(|e| e.id == id);
                found.map(|i| list.remove(i).state.clone())
            });
            if let Some(state) = state {
                if let Ok(state) = state.try_borrow() {
                    batch_remember(&state);
                }
                batch_save_prefs();
            }
            slint::CloseRequestResponse::HideWindow
        });

        BATCH_WINDOWS.with(|list| {
            list.borrow_mut().push(BatchWindowEntry {
                id,
                ui: ui.as_weak(),
                state: session.clone(),
            })
        });
        // 尺寸必须在 show() 之后再设：Slint 建窗时会先用组件声明的 preferred-*
        // 决定初始大小，show 之前设的会被它覆盖。
        if let Err(err) = ui.show() {
            logln!("!! [batch-rename] 显示窗口失败：{err}");
            return;
        }
        ui.window()
            .set_size(WindowSize::Logical(LogicalSize::new(start_w, start_h)));
        ui.window()
            .set_position(WindowPosition::Logical(LogicalPosition::new(start_x, start_y)));
        // 布局是按窗口真实尺寸折行的，而窗口在 show 之后才拿到最终尺寸。
        // 下一帧再刷一次，列宽/折行才对得上（否则列宽按 0.75 屏宽算，
        // 窗口被系统拉伸后整行会溢出到窗口外面）。
        {
            let weak = ui.as_weak();
            let state = session.clone();
            slint::Timer::single_shot(std::time::Duration::from_millis(180), move || {
                if let Some(ui) = weak.upgrade() {
                    batch_refresh(&ui, &state);
                }
            });
        }
        logln!("[batch-rename] 打开窗口，{} 项", session.borrow().items.len());
    }
}

/// 静态文案（窗口标题、各控件标签、正则片段）。
fn batch_push_static(ui: &BatchRenameWindow, lang: &str) {
    let tr = |zh: &str| i18n::text(lang, zh);
    ui.set_window_title(tr("批量重命名"));
    ui.set_search_label(tr("搜索"));
    ui.set_replace_label(tr("替换为"));
    ui.set_apply_to_label(tr("应用于"));
    ui.set_text_regex(tr("使用正则表达式"));
    ui.set_text_match_all(tr("匹配所有出现的对象"));
    ui.set_text_case(tr("区分大小写"));
    ui.set_text_pinyin(tr("拼音"));
    ui.set_text_original(tr("原始"));
    ui.set_text_renamed(tr("已重命名"));
    ui.set_text_apply(tr("重命名"));
    ui.set_text_show_all(tr("显示全部"));
    ui.set_text_show_renamed(tr("仅显示已重命名"));
    ui.set_apply_to_options(ModelRc::new(VecModel::from(vec![
        tr("文件名 + 扩展名"),
        tr("文件名"),
        tr("扩展名"),
    ])));
    ui.set_regex_tokens(ModelRc::new(VecModel::from(vec![
        SharedString::from("^"),
        SharedString::from("$"),
        SharedString::from("."),
        SharedString::from("+"),
        SharedString::from("?"),
        SharedString::from("*"),
        SharedString::from(r"\d"),
        SharedString::from(r"\D"),
        SharedString::from(r"\w"),
        SharedString::from(r"\s"),
        SharedString::from(r"\S"),
        SharedString::from(r"\b"),
    ])));
}

/// 接线：所有控件回调都作用在自己的 session 上，窗口之间互不干扰。
fn batch_wire(ui: &BatchRenameWindow, state: &Rc<RefCell<BatchSession>>, id: u64) {
    let weak = ui.as_weak();

    {
        let state = state.clone();
        let weak = weak.clone();
        ui.on_search_edited(move |text| {
            state.borrow_mut().options.search = text.to_string();
            batch_refresh_weak(&weak, &state);
        });
    }
    {
        let state = state.clone();
        let weak = weak.clone();
        ui.on_replace_edited(move |text| {
            state.borrow_mut().options.replace = text.to_string();
            batch_refresh_weak(&weak, &state);
        });
    }
    {
        let state = state.clone();
        let weak = weak.clone();
        ui.on_regex_toggled(move |on| {
            state.borrow_mut().options.use_regex = on;
            batch_refresh_weak(&weak, &state);
        });
    }
    {
        let state = state.clone();
        let weak = weak.clone();
        ui.on_match_all_toggled(move |on| {
            state.borrow_mut().options.match_all = on;
            batch_refresh_weak(&weak, &state);
        });
    }
    {
        let state = state.clone();
        let weak = weak.clone();
        ui.on_case_toggled(move |on| {
            state.borrow_mut().options.case_sensitive = on;
            batch_refresh_weak(&weak, &state);
        });
    }
    {
        let state = state.clone();
        let weak = weak.clone();
        ui.on_pinyin_toggled(move |on| {
            state.borrow_mut().options.pinyin = on;
            batch_refresh_weak(&weak, &state);
        });
    }
    {
        let state = state.clone();
        let weak = weak.clone();
        ui.on_apply_to_changed(move |idx| {
            state.borrow_mut().options.apply_to = crate::ops::batch_rename::ApplyTo::from_index(idx);
            batch_refresh_weak(&weak, &state);
        });
    }
    {
        let state = state.clone();
        let weak = weak.clone();
        ui.on_row_toggled(move |id, want| {
            let mut s = state.borrow_mut();
            if let Some(item) = s.items.get_mut(id as usize) {
                item.checked = want;
            }
            drop(s);
            batch_refresh_weak(&weak, &state);
        });
    }
    {
        let state = state.clone();
        let weak = weak.clone();
        ui.on_all_toggled(move |want| {
            let mut s = state.borrow_mut();
            for item in s.items.iter_mut() {
                item.checked = want;
            }
            drop(s);
            batch_refresh_weak(&weak, &state);
        });
    }
    {
        let state = state.clone();
        let weak = weak.clone();
        ui.on_filter_changed(move |f| {
            state.borrow_mut().filter = f;
            batch_refresh_weak(&weak, &state);
        });
    }
    {
        let state = state.clone();
        let weak = weak.clone();
        ui.on_token_clicked(move |token, caret, anchor| {
            let token = token.to_string();
            let mut s = state.borrow_mut();
            let text = s.options.search.clone();
            let (new_text, new_caret) = insert_at_selection(&text, &token, caret, anchor);
            s.options.search = new_text.clone();
            drop(s);
            if let Some(ui) = weak.upgrade() {
                ui.set_search_text(SharedString::from(new_text));
                ui.invoke_put_caret(new_caret);
            }
            batch_refresh_weak(&weak, &state);
        });
    }
    {
        let state = state.clone();
        let weak = weak.clone();
        ui.on_apply_clicked(move || {
            batch_apply(id, &weak, &state);
        });
    }
    {
        // 滚动位置 / 视口高度变了：只换「看得见的那几十行」，不重算预览。
        let state = state.clone();
        let weak = weak.clone();
        ui.on_scrolled(move |offset, view_h| {
            {
                let mut s = state.borrow_mut();
                s.scroll = offset.max(0.0);
                s.view_h = view_h.max(0.0);
            }
            if let Some(ui) = weak.upgrade() {
                batch_window(&ui, &state);
            }
        });
    }
    {
        let state = state.clone();
        let weak = weak.clone();
        ui.on_resized(move |w| {
            // 回调给的值在窗口 realize 之前是**物理像素**（640 逻辑报成 960），
            // 之后又不会再有 resize 事件，宽度就永久停在错值上。这里一律以
            // 窗口自己报的「物理尺寸 ÷ 缩放」为准，拿不到才退回回调值。
            let mut w = w as f32;
            if let Some(win) = weak.upgrade() {
                let sc = win.window().scale_factor().max(0.1);
                let real = win.window().size().width as f32 / sc;
                if real > 120.0 {
                    w = real;
                }
            }
            {
                let mut s = state.borrow_mut();
                // 拖动边缘时每像素都会回调，宽度没实质变化就不重算。
                if (s.win_w - w).abs() < 12.0 {
                    return;
                }
                s.win_w = w;
            }
            batch_refresh_weak(&weak, &state);
        });
    }
}

/// 把窗口当前的选择抄进内存里的配置（不落盘：编辑器里每敲一个字都会走到这里）。
fn batch_remember(s: &BatchSession) {
    let Some(app) = SESSION.with(|slot| slot.borrow().clone()) else {
        return;
    };
    let Ok(mut app) = app.try_borrow_mut() else {
        return;
    };
    let p = &mut app.config.batch_rename;
    p.search.clone_from(&s.options.search);
    p.replace.clone_from(&s.options.replace);
    p.use_regex = s.options.use_regex;
    p.match_all = s.options.match_all;
    p.case_sensitive = s.options.case_sensitive;
    p.pinyin = s.options.pinyin;
    p.apply_to = s.options.apply_to.to_index();
}

/// 关窗时把记住的选项写进 config.json。
fn batch_save_prefs() {
    let Some(app) = SESSION.with(|slot| slot.borrow().clone()) else {
        return;
    };
    let cfg = {
        let Ok(borrowed) = app.try_borrow() else {
            return;
        };
        borrowed.config.clone()
    };
    cfg.save();
}

/// 把 `token` 插到 `[caret, anchor]` 之间（有选中就替换选中内容），返回新的
/// (文本, 新光标字节偏移)。偏移先夹到字符边界，避免切在 UTF-8 中间。
fn insert_at_selection(text: &str, token: &str, caret: i32, anchor: i32) -> (String, i32) {
    let clamp = |v: i32| -> usize {
        let mut v = v.clamp(0, text.len() as i32) as usize;
        while v > 0 && !text.is_char_boundary(v) {
            v -= 1;
        }
        v
    };
    let a = clamp(caret);
    let b = clamp(anchor);
    let (start, end) = if a <= b { (a, b) } else { (b, a) };
    let mut out = String::with_capacity(text.len() + token.len());
    out.push_str(&text[..start]);
    out.push_str(token);
    out.push_str(&text[end..]);
    (out, (start + token.len()) as i32)
}

/// 动作编辑器的「参数模板」按钮：把 `"{token}"` 插进参数文本。
///
/// `caret < 0` 表示参数框从来没被点过：没动过光标时 Slint 报回来的位置是无意义的 (0, 0)，
/// 直接用会把参数插到最前面，所以这种情况下追加到末尾（非空时补一个空格分隔）。
fn insert_arg_token(text: &str, token: &str, caret: i32, anchor: i32) -> (String, i32) {
    let quoted = format!("\"{token}\"");
    if caret >= 0 {
        return insert_at_selection(text, &quoted, caret, anchor);
    }
    let mut t = text.to_string();
    if !t.is_empty() && !t.ends_with(' ') {
        t.push(' ');
    }
    t.push_str(&quoted);
    let end = t.len() as i32;
    (t, end)
}

fn batch_refresh_weak(weak: &slint::Weak<BatchRenameWindow>, state: &Rc<RefCell<BatchSession>>) {
    if let Some(ui) = weak.upgrade() {
        batch_refresh(&ui, state);
    }
}

/// 重算预览并推给界面。
fn batch_refresh(ui: &BatchRenameWindow, state: &Rc<RefCell<BatchSession>>) {
    let mut s = state.borrow_mut();
    use crate::ops::batch_rename as br;
    let compiled = br::compile(&s.options);
    let regex_error = compiled.is_err();
    let re = match &compiled {
        Ok(r) => r.clone(),
        Err(_) => None,
    };
    let lang = s.lang.clone();
    let tr = |zh: &str| i18n::text(&lang, zh);

    let mut previews: Vec<br::Preview> = s
        .items
        .iter()
        .map(|it| br::preview(&it.name, it.is_dir, &s.options, re.as_ref(), &s.existing))
        .collect();

    // 命中项之间互相撞名要单独处理：如果某个「已存在」的名字正好属于本次
    // 也会被改掉的另一项（它马上就搬走了），那不算冲突。
    // 例：把 a.txt、b.txt 都选中，搜索 b 换成 c —— a.txt→b.txt 不该被拦。
    let vacating: HashSet<String> = previews
        .iter()
        .enumerate()
        .filter(|(i, p)| s.items[*i].checked && p.new_name != s.items[*i].name)
        .map(|(i, _)| s.items[i].name.to_lowercase())
        .collect();
    if !vacating.is_empty() {
        let effective: HashSet<String> =
            s.existing.iter().filter(|n| !vacating.contains(*n)).cloned().collect();
        previews = s
            .items
            .iter()
            .map(|it| br::preview(&it.name, it.is_dir, &s.options, re.as_ref(), &effective))
            .collect();
    }

    // 两项算出同一个新名字：都标冲突（否则第二次 rename 会把第一项覆盖掉）。
    let mut targets: HashMap<String, Vec<usize>> = HashMap::new();
    for (i, p) in previews.iter().enumerate() {
        if s.items[i].checked && p.state == br::RowState::Ready {
            targets.entry(p.new_name.to_lowercase()).or_default().push(i);
        }
    }
    for idxs in targets.values() {
        if idxs.len() > 1 {
            for &i in idxs {
                previews[i].state = br::RowState::Conflict;
                previews[i].reason = Some(br::Reason::DuplicateTarget);
            }
        }
    }

    batch_remember(&s);

    // 窗口逻辑宽度**必须从窗口自己取**，不能用 `resized` 回调送来的值：
    // 那个回调在窗口 realize 之前会用物理像素报一次（640 逻辑 → 报 960），
    // 之后再没有 resize 事件，宽度就一直停在错的值上，列宽和折行全都按
    // 一个不存在的窗口宽度算，右列被推到窗口外。
    let win = ui.window();
    let scale = win.scale_factor().max(0.1);
    let real_w = win.size().width as f32 / scale;
    if real_w > 120.0 {
        s.win_w = real_w;
    }

    // 折行预算只跟窗口宽度与正则侧栏有关，所有行共用。
    let rail = if s.options.use_regex { 56.0 } else { 0.0 };
    let col_w = batch_col_w(s.win_w, rail);
    s.col_w = col_w;
    let budget = batch_wrap_budget(col_w);
    let parts = |list: &[(String, bool)]| -> ModelRc<ModelRc<RenamePart>> {
        // 折行直接复用图标视图那套算法（按字符宽度估、优先在分隔符处断行），
        // 只是把字号差换算进可用宽度；不截断，名字完整显示。
        // 12 行只是防御性上限（255 个字符的文件名也到不了）。
        let wrapped = grid_display_lines(list, budget, 12);
        ModelRc::new(VecModel::from(
            wrapped
                .into_iter()
                .map(|line| {
                    ModelRc::new(VecModel::from(
                        line.into_iter()
                            .map(|(text, hot)| RenamePart {
                                text: SharedString::from(text.as_str()),
                                hot,
                            })
                            .collect::<Vec<_>>(),
                    )) as ModelRc<RenamePart>
                })
                .collect::<Vec<_>>(),
        ))
    };

    // 整批行一次排好版：算出每行的绝对 y 和高度，总高度给滚动区。
    // 滚动时只需要从这里截一段推给界面，不再重算预览与折行。
    let mut cache: HashMap<usize, BatchLaidRow> = std::mem::take(&mut s.laid)
        .into_iter()
        .map(|r| (r.item, r))
        .collect();
    let mut laid: Vec<BatchLaidRow> = Vec::with_capacity(previews.len());
    let mut y = 0.0f32;
    for (i, p) in previews.iter().enumerate() {
        if s.filter != 0 && p.new_name == s.items[i].name {
            continue;
        }
        let (code, reason) = match p.state {
            br::RowState::Unchanged => (0, SharedString::default()),
            br::RowState::Ready => (1, SharedString::default()),
            br::RowState::Invalid => (2, tr(p.reason.map(|r| r.zh_text()).unwrap_or("未更改"))),
            br::RowState::Conflict => (3, tr(p.reason.map(|r| r.zh_text()).unwrap_or("未更改"))),
        };
        // 上一版里同一行的折行结果能沿用就沿用：敲一个字符时绝大多数行
        // 并没有命中，没必要把几千个名字重新折一遍、重新建模型。
        let cached = cache.remove(&i);
        let (orig, new_name, h) = match cached {
            Some(c)
                if (c.budget - budget).abs() < 0.5
                    && c.orig_raw == p.orig_parts
                    && c.new_raw == p.new_parts =>
            {
                (c.orig, c.new, c.h)
            }
            _ => {
                let orig = parts(&p.orig_parts);
                let new_name = parts(&p.new_parts);
                // 行高与 ui/batch_rename.slint 的约定一致：行数取两侧较长者，
                // 17px 行距 + 上下各 5px。Rust 算是因为要靠它累计每行的位置。
                let lines = orig.row_count().max(new_name.row_count());
                (orig, new_name, lines as f32 * 17.0 + 10.0)
            }
        };
        laid.push(BatchLaidRow {
            item: i,
            y,
            h,
            budget,
            col_w,
            orig,
            new: new_name,
            orig_raw: p.orig_parts.clone(),
            new_raw: p.new_parts.clone(),
            state: code,
            reason,
        });
        y += h;
    }

    let orig_count = s.items.len() as i32;
    let renamed_count = previews
        .iter()
        .enumerate()
        .filter(|(i, p)| p.new_name != s.items[*i].name)
        .count() as i32;
    let can_apply = previews
        .iter()
        .enumerate()
        .any(|(i, p)| s.items[i].checked && p.state == br::RowState::Ready);
    let all_checked = !s.items.is_empty() && s.items.iter().all(|i| i.checked);
    s.previews = previews;
    s.laid = laid;
    s.laid_h = y;
    // 行集合变了：可见区间要按当前滚动位置重算一次。
    s.win = (usize::MAX, usize::MAX);
    drop(s);

    ui.set_orig_count(orig_count);
    ui.set_renamed_count(renamed_count);
    ui.set_all_checked(all_checked);
    ui.set_can_apply(can_apply);
    ui.set_status_text(if regex_error {
        tr("无效的正则表达式")
    } else {
        SharedString::default()
    });
    batch_window(ui, state);
}

/// 批量重命名：名字列的折行预算（单位是 `grid_display_lines` 的 11.5px 字宽）。
///
/// 右列的实际宽度 = (窗口 - 正则侧栏) - 控制区 288 - 滚动条 9 - 行内边距 12
/// - 两处间距 16 - 勾选框 16，再对半分。`grid_display_lines` 按 11.5px 字号估宽，
/// 本窗口是 12.5px，所以要按 1.087 换算，末尾再留几像素余量。
/// 原先这串常数是揉在一起的 `- 288` 再 `- 22`，谁对应哪块看不出来；拆开写以后
/// 窗口一改布局就能照着对。
fn batch_col_w(win_w: f32, rail: f32) -> f32 {
    ((win_w - rail - 288.0 - 9.0 - 12.0 - 16.0 - 16.0) / 2.0).max(90.0)
}

/// 把列宽换成 `grid_display_lines` 用的折行预算（11.5px 字宽世界）。
///
/// 换算系数就是 12.5 / 11.5 = 1.087：字宽表本身是 11.5px 实测的，本窗口用 12.5px。
/// 表必须是 **Microsoft YaHei UI**（界面字体就是它）。之前那张表是照 Segoe UI 测的，
/// YaHei 的拉丁字形平均宽 8.7%，于是每一行都算出「刚好放得下」而实际超了 8.7%，
/// 行尾就被 `overflow: elide` 收成「…」。
const BATCH_WRAP_SCALE: f32 = 1.087;

fn batch_wrap_budget(col_w: f32) -> f32 {
    col_w / BATCH_WRAP_SCALE - 6.0
}

/// 视口对应的行区间 `[start, end)`：上下各留 `buffer` 像素的缓冲。
///
/// `rows` 按内容顺序排列，`yh` 给出每行的 (顶部偏移, 高度)。
fn visible_range<T>(
    rows: &[T],
    scroll: f32,
    view_h: f32,
    buffer: f32,
    yh: impl Fn(&T) -> (f32, f32),
) -> (usize, usize) {
    let top = (scroll - buffer).max(0.0);
    let bottom = scroll + view_h.max(0.0) + buffer;
    let mut start = 0usize;
    while start < rows.len() {
        let (y, h) = yh(&rows[start]);
        if y + h > top {
            break;
        }
        start += 1;
    }
    let mut end = start;
    while end < rows.len() && yh(&rows[end]).0 < bottom {
        end += 1;
    }
    (start, end)
}

/// 手写窗口化：只把视口上下各 300px 内的行推给界面。
///
/// Slint 的 `ListView` 虚拟化但行高必须统一，`ScrollView` 行高随意但要全部
/// 实例化；一次选几千个文件时后者会卡，所以自己算「谁在视口里」。
/// 区间没变就直接返回，拖滚动条时不会每像素重建模型。
fn batch_window(ui: &BatchRenameWindow, state: &Rc<RefCell<BatchSession>>) {
    /// 视口上下各多放这么多，滚动时不至于露出空白。
    const BUFFER: f32 = 300.0;

    let mut s = state.borrow_mut();
    // 首帧还没有真实视口高度：先按一个保守值铺，Slint 报上来后会重算。
    let view_h = if s.view_h > 1.0 { s.view_h } else { 900.0 };
    // 内容变短（改完名、切换筛选）之后滚动位置可能已经超出底部：先夹住，
    // 否则会算出一个空区间，列表先白一帧再被 Slint 的越界回弹拉回来。
    let scroll = s.scroll.clamp(0.0, (s.laid_h - view_h).max(0.0));
    s.scroll = scroll;
    let (start, end) = visible_range(&s.laid, scroll, view_h, BUFFER, |r| (r.y, r.h));
    if s.win == (start, end) {
        return;
    }
    s.win = (start, end);

    let rows: Vec<RenameRow> = s.laid[start..end]
        .iter()
        .map(|r| RenameRow {
            id: r.item as i32,
            y: r.y,
            h: r.h,
            col_w: r.col_w,
            orig: r.orig.clone(),
            new: r.new.clone(),
            state: r.state,
            reason: r.reason.clone(),
            checked: s.items.get(r.item).map(|i| i.checked).unwrap_or(false),
        })
        .collect();
    let total_h = s.laid_h;
    let col_w = s.col_w;
    // 先放手再碰界面：写属性会同步触发 Slint 回调（可能又回到这里）。
    drop(s);

    // 先给内容高度再换行：反过来，Slint 会先用旧的（可能更小的）高度去夹
    // content-y，把正确的滚动位置夹小，回来又把我们的 scroll 改掉。
    ui.set_content_h(total_h);
    ui.set_col_w(col_w);
    ui.set_rows(ModelRc::new(VecModel::from(rows)));
}

/// 提交：把「勾选且合法」的行放到后台线程改名。
fn batch_apply(
    id: u64,
    weak: &slint::Weak<BatchRenameWindow>,
    state: &Rc<RefCell<BatchSession>>,
) {
    let plan: Vec<(usize, crate::ops::batch_rename::PlanItem)> = {
        let s = state.borrow();
        let mut plan = Vec::new();
        for (i, item) in s.items.iter().enumerate() {
            let Some(p) = s.previews.get(i) else { continue };
            if !item.checked || p.state != crate::ops::batch_rename::RowState::Ready {
                continue;
            }
            let Some(parent) = item.path.parent() else { continue };
            plan.push((
                i,
                crate::ops::batch_rename::PlanItem {
                    from: item.path.clone(),
                    to: parent.join(&p.new_name),
                },
            ));
        }
        plan
    };
    if plan.is_empty() {
        if let Some(ui) = weak.upgrade() {
            let lang = state.borrow().lang.clone();
            ui.set_status_text(i18n::text(&lang, "没有可重命名的项"));
        }
        return;
    }

    let items: Vec<crate::ops::batch_rename::PlanItem> =
        plan.iter().map(|(_, p)| p.clone()).collect();
    // 磁盘操作一律离开界面线程：网络盘上一次 rename 就能卡住整个窗口。
    std::thread::spawn(move || {
        let summary = crate::ops::batch_rename::execute(&items);
        let done = summary.renamed_items.clone();
        let failed = summary.failed.clone();
        let skipped = summary.skipped_existing.len();
        // 这里只能捕获能跨线程的值：Rc 状态回界面线程后按 id 再查一次。
        let _ = slint::invoke_from_event_loop(move || {
            // 主面板先跟着改名结果走：即使用户在改名过程中关掉了批量窗口
            // （此时按 id 已经查不到），磁盘上的名字也变了，列表必须刷新，
            // 否则列表里全是已经不存在的旧名字。
            batch_sync_pane(&done);
            let Some((ui, state)) = batch_lookup(id) else { return };
            let lang = state.borrow().lang.clone();
            // 1. 把成功的项换成新路径/新名字，并同步「现有名字」快照。
            {
                let mut s = state.borrow_mut();
                for done_item in &done {
                    let new_name = done_item
                        .to
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    let idx = s.items.iter().position(|i| i.path == done_item.from);
                    if let Some(idx) = idx {
                        let old = std::mem::take(&mut s.items[idx].name);
                        s.existing.remove(&old.to_lowercase());
                        s.existing.insert(new_name.to_lowercase());
                        s.items[idx].path = done_item.to.clone();
                        s.items[idx].name = new_name;
                    }
                }
            }
            // 2. 状态栏（面板已在上面同步过了）。
            {
                let tr = |zh: &str| i18n::text(&lang, zh);
                let skipped_total = skipped + failed.len();
                let text = if done.is_empty() && skipped_total > 0 {
                    tr("重命名失败").to_string()
                } else {
                    tr("已重命名 {} 项，跳过 {} 项")
                        .to_string()
                        .replacen("{}", &done.len().to_string(), 1)
                        .replacen("{}", &skipped_total.to_string(), 1)
                };
                batch_refresh(&ui, &state);
                ui.set_status_text(SharedString::from(text));
            }
            if let Some((path, err)) = failed.first() {
                logln!("!! [batch-rename] 改名失败 {:?}: {err}", path);
            }
        });
    });
}

/// 主面板跟随改名结果：换掉选中集里的旧路径，然后刷新列表。
fn batch_sync_pane(done: &[crate::ops::batch_rename::PlanItem]) {
    if done.is_empty() {
        return;
    }
    let Some(app) = SESSION.with(|slot| slot.borrow().clone()) else {
        return;
    };
    // 批量改名也进撤消栈：一次 Ctrl+Z 把整批名字改回去。
    app.borrow_mut()
        .undo
        .record(crate::ops::undo::Kind::Rename {
            pairs: done.iter().map(|d| (d.from.clone(), d.to.clone())).collect(),
        });
    let refresh_here = {
        let mut app = app.borrow_mut();
        let tab = app.state.active_mut();
        let pane = tab.active_mut();
        let here = pane.path.clone();
        let in_place = done
            .iter()
            .all(|d| d.from.parent().map(|p| Some(p.to_path_buf())) == Some(here.clone()));
        for d in done {
            if pane.selection.remove(&d.from) {
                pane.selection.insert(d.to.clone());
            }
        }
        // 暂存盘/剪切标记里存的是旧路径，一并跟上。
        for d in done {
            app.state.cut_paths.remove(&d.from);
        }
        in_place
    };
    if refresh_here {
        app.borrow_mut().reload_active();
    }
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
