//! 应用装配：状态机 + UI 回调 + 后台任务。
//!
//! 线程模型：
//! - UI 线程：Slint 事件循环；所有回调与状态变更都在这里发生。
//! - 目录列举线程：`list_directory` / 驱动器枚举在此执行，结果经通道
//!   回传，由轮询定时器合并（代号 generation 过期即丢弃）。
//! - 文件操作线程：复制/移动/删除，进度经共享状态暴露。

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, SystemTime};

use slint::language::{PointerEvent, PointerEventButton, PointerEventKind};
use slint::{
    Color, ComponentHandle, LogicalSize, Model, ModelRc, SharedString, Timer, TimerMode, VecModel,
    WindowSize,
};

use crate::config::{Config, ThemeChoice};
use crate::fs::{
    self, DriveInfo, FileEntry, SortKey, SortState, icon_color, icon_text_for, list_drives,
    list_network_locations, quick_access_dirs, recycle_bin_stats, sort_entries,
};
use crate::ops::{
    self, clipboard, jobs::{JobKind, JobManager, JobSummary, status_text}, shortcut,
};
use crate::state::AppState;

slint::include_modules!();

/// 诊断日志：ROVE_LOG=1 时追加写入 %TEMP%/rove.log。
/// GUI 子系统进程的 stderr 不可见，文件日志是 WSL 下排查的唯一渠道。
#[macro_export]
macro_rules! logln {
    ($($arg:tt)*) => {{
        if std::env::var_os("ROVE_LOG").is_some() {
            use std::io::Write as _;
            // 日志写在 exe 同目录（WSL 文件系统上 Windows 进程可写），
            // 避免依赖 %TEMP% 解析与 WSLENV 传递。
            let path = std::env::current_exe()
                .ok()
                .and_then(|p| p.parent().map(|d| d.join("rove.log")))
                .unwrap_or_else(|| std::env::temp_dir().join("rove.log"));
            if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
                let _ = writeln!(f, "[rove] {}", format!($($arg)*));
            }
        }
    }};
}


/// 虚拟列表行高（与 ui/file_list.slint 的 row-h 保持一致）。
const ROW_HEIGHT: f32 = 30.0;
/// 视口上下过扫描行数。
const OVERSCAN: usize = 4;

// ---- 右键菜单动作 ID ----
const ACT_OPEN: &str = "open";
const ACT_COPY: &str = "copy";
const ACT_CUT: &str = "cut";
const ACT_PASTE: &str = "paste";
const ACT_RENAME: &str = "rename";
const ACT_DELETE: &str = "delete";
const ACT_STAGE: &str = "stage";
const ACT_TERMINAL: &str = "terminal";
const ACT_NEW_FOLDER: &str = "new_folder";
const ACT_REFRESH: &str = "refresh";
const ACT_SELECT_ALL: &str = "select_all";
const ACT_SENDTO_DESKTOP: &str = "sendto_desktop";
const ACT_SENDTO_STARTMENU: &str = "sendto_startmenu";

/// 右键菜单目标。
#[derive(Clone, Debug)]
enum MenuTarget {
    /// 选中项（非空）。
    Files(Vec<PathBuf>),
    /// 列表空白处。
    Background,
}

/// 目录列举请求。
struct ListRequest {
    tab_id: u32,
    generation: u64,
    /// None = "此电脑"视图（列举驱动器）。
    path: Option<PathBuf>,
    sort: SortState,
}

/// 目录列举结果。
struct ListResult {
    tab_id: u32,
    generation: u64,
    entries: Vec<FileEntry>,
}

/// 深度搜索请求：在目录子树中递归查找匹配项（文件夹视图搜索）。
struct DeepRequest {
    tab_id: u32,
    generation: u64,
    deep_generation: u64,
    root: PathBuf,
    query: String,
}

/// 深度搜索结果。
struct DeepResult {
    tab_id: u32,
    generation: u64,
    deep_generation: u64,
    query: String,
    items: Vec<crate::search::index::DeepItem>,
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
    /// 虚拟列表当前窗口 (first, count)。
    visible_cache: Option<(usize, usize)>,
    /// 当前右键菜单目标。
    menu_target: Option<MenuTarget>,
    /// 双击检测：上次左键按下的 (路径, 时刻)。
    last_click: Option<(PathBuf, std::time::Instant)>,
    /// 侧栏分区拖拽：正在拖动的分区 header 行索引。
    sidebar_drag: Option<usize>,
    /// 外部拖入缓冲：winit 对一次投放逐文件发事件，聚合后统一处理。
    drop_buffer: Vec<PathBuf>,
    /// 任务 3（刷新）：各打开目录的 mtime 快照，轮询检测外部/内部变更。
    dir_mtimes: HashMap<PathBuf, SystemTime>,
    /// 拖入聚合定时器（最后一个文件事件后短暂延迟处理）。
    drop_timer: Timer,
    /// 拖出待定手势：(按下时行局部 x, y, 待拖路径)。
    drag_pending: Option<(f32, f32, Vec<PathBuf>)>,
    /// 任务 3.1：QuickLook 预览窗口（独立 Slint Window）。
    preview: PreviewWindow,
    /// 预览窗口是否已 show（首次需 show，之后用 visible 控制）。
    preview_shown: bool,
    /// actions.toml 驱动的自定义动作（压缩/解压/自定义）。
    actions: Vec<crate::ops::actions::ActionEntry>,
    /// 缩略图 LRU（Phase 5）：(路径, 尺寸) → 已生成的 Slint 图像。
    /// 缩略图管线暂未在视图中启用，保留基础设施。
    #[allow(dead_code)]
    thumb_cache: HashMap<(PathBuf, u32), slint::Image>,
    /// 缩略图缓存插入序（容量淘汰用）。
    thumb_order: Vec<(PathBuf, u32)>,
    /// 已请求未完成的缩略图（防重复提交）。
    thumb_pending: HashSet<(PathBuf, u32)>,
    /// 失败负缓存（本会话内不再重试，避免反复 COM 调用）。
    thumb_failed: HashSet<(PathBuf, u32)>,
    #[allow(dead_code)]
    thumb_tx: Sender<(PathBuf, u32)>,
    list_tx: Sender<ListRequest>,
    list_rx: Receiver<ListResult>,
    /// 深度搜索通道（文件夹视图子树搜索，独立线程执行）。
    deep_tx: Sender<DeepRequest>,
    deep_rx: Receiver<DeepResult>,
    poll_timer: Timer,
    sidebar_timer: Timer,
    /// 侧栏周期刷新（驱动器容量/回收站统计/网络位置）。
    /// 必须挂在 App 字段上：局部 Timer 会在函数返回时停止。
    refresh_timer: Timer,
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
        thumb_tx: Sender<(PathBuf, u32)>,
    ) -> Self {
        let state = AppState::new(None);
        Self {
            ui,
            state,
            config,
            jobs,
            drives: Vec::new(),
            sidebar_fingerprint: 0,
            visible_cache: None,
            menu_target: None,
            last_click: None,
            sidebar_drag: None,
            drop_buffer: Vec::new(),
            dir_mtimes: HashMap::new(),
            drop_timer: Timer::default(),
            drag_pending: None,
            preview: PreviewWindow::new().expect("failed to create preview window"),
            preview_shown: false,
            actions: Vec::new(),
            thumb_cache: HashMap::new(),
            thumb_order: Vec::new(),
            thumb_pending: HashSet::new(),
            thumb_failed: HashSet::new(),
            thumb_tx,
            list_tx,
            list_rx,
            deep_tx,
            deep_rx,
            poll_timer: Timer::default(),
            sidebar_timer: Timer::default(),
            refresh_timer: Timer::default(),
        }
    }

    /// 请求列举目录（工作线程执行）。
    fn request_listing(&self, tab_id: u32, generation: u64, path: Option<PathBuf>, sort: SortState) {
        logln!("request_listing tab={tab_id} gen={generation} path={path:?}");
        let _ = self.list_tx.send(ListRequest {
            tab_id,
            generation,
            path,
            sort,
        });
    }

    /// 重新列举指定标签的当前目录。
    fn reload_tab(&self, tab_index: usize) {
        let Some(tab) = self.state.tabs.get(tab_index) else {
            return;
        };
        tab_loading_flag(&self.ui, tab_index == self.state.active_tab, true);
        self.request_listing(tab.id, tab.generation, tab.path.clone(), tab.sort);
    }

    /// 任务 3：轮询各打开目录的 mtime，检测外部/内部变更后实时刷新对应标签。
    fn poll_dir_changes(&mut self) {
        if self.ui.get_settings_open() {
            return;
        }
        let n = self.state.tabs.len();
        let mut to_reload: Vec<usize> = Vec::new();
        for i in 0..n {
            let path = match self.state.tabs[i].path.clone() {
                Some(p) => p,
                None => continue,
            };
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

    /// 重新列举当前标签。
    fn reload_active(&self) {
        self.reload_tab(self.state.active_tab);
    }

    /// 列举结果到达：代号匹配才应用。
    fn apply_list_result(&mut self, result: ListResult) {
        logln!("list_result tab={} gen={} n={}", result.tab_id, result.generation, result.entries.len());
        let Some(tab_index) = self.state.tab_by_id(result.tab_id) else {
            logln!("apply_list_result: tab not found");
            return;
        };
        let tab = &mut self.state.tabs[tab_index];
        if tab.generation != result.generation {
            return; // 过期结果
        }
        tab.entries = result.entries;
        // 拼音缓存随 entries 失效（惰性重建）。
        tab.pinyin_cache.clear();
        // 「此电脑」重载 = 回到驱动器列表，残留的全局搜索词会误过滤空表。
        if tab.path.is_none() {
            tab.filter.clear();
        }
        tab.loading = false;
        tab.rebuild_display();
        // 记录最近目录。
        if let Some(path) = tab.path.clone() {
            self.state.push_recent(&path);
            self.config.recent = self.state.recent.clone();
            self.config.save();
        }
        if tab_index == self.state.active_tab {
            self.visible_cache = None;
            self.sync_file_list();
            self.sync_status();
            self.sync_nav();
        }
        // 标签标题可能变化（"此电脑" 不变，目录名会变）。
        self.sync_tabs();
    }

    /// 统一应用搜索词（搜索框编辑 / 外部唤起预填共用）：
    /// 「此电脑」= 全局索引秒搜；目录视图 = 本地过滤 + 异步深度搜索。
    fn apply_search_query(&mut self, text: &str) {
        logln!("search query: {text:?}");
        let is_root = self.state.active().path.is_none();
        // 「此电脑」视图：搜索框 = 全局拼音秒搜（Phase 5 索引）。
        if is_root {
            let query = text.trim().to_string();
            self.run_global_search(&query);
        } else {
            {
                let tab = self.state.active_mut();
                tab.filter = text.to_string();
            }
            // 深度搜索（子树递归）：异步执行，结果经 poll timer 合并。
            self.start_deep_search();
            self.state.active_mut().rebuild_display();
        }
        self.visible_cache = None;
        self.sync_file_list();
        self.sync_status();
        self.ui.invoke_scroll_list_to_top();
    }

    /// 启动/重置当前标签的深度搜索（filter 变化时调用）。
    /// 空查询仅清空残留结果；有效查询在代号递增后发往工作线程。
    fn start_deep_search(&mut self) {
        let tab = self.state.active_mut();
        tab.clear_deep();
        let (tab_id, generation, deep_generation, root, query) = (
            tab.id,
            tab.generation,
            tab.deep_generation,
            tab.path.clone(),
            tab.filter.trim().to_string(),
        );
        // 「此电脑」/ 回收站虚拟目录没有子树搜索。
        let Some(root) = root else { return };
        if root == fs::drives::recycle_virtual_path() || query.is_empty() {
            return;
        }
        let _ = self.deep_tx.send(DeepRequest {
            tab_id,
            generation,
            deep_generation,
            root,
            query,
        });
        self.state.active_mut().deep_searching = true;
    }

    /// 深度搜索结果到达：标签存在、代号与查询都匹配才应用。
    fn apply_deep_result(&mut self, result: DeepResult) {
        logln!(
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
        if tab.generation != result.generation || tab.deep_generation != result.deep_generation {
            return; // 过期结果
        }
        if tab.filter.trim().to_lowercase() != result.query.trim().to_lowercase() {
            return;
        }
        tab.deep_entries = result.items.iter().map(|i| i.entry.clone()).collect();
        tab.deep_pinyin = result.items.into_iter().map(|i| i.fields).collect();
        tab.deep_searching = false;
        tab.rebuild_display();
        if tab_index == self.state.active_tab {
            self.visible_cache = None;
            self.sync_file_list();
            self.sync_status();
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

/// 启动目录列举工作线程。
fn spawn_list_worker(rx: Receiver<ListRequest>, tx: Sender<ListResult>) {
    std::thread::Builder::new()
        .name("rove-lister".into())
.spawn(move || {
            logln!("lister worker started");
            while let Ok(req) = rx.recv() {
                logln!("worker req tab={} path={:?}", req.tab_id, req.path);
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
                let sent = tx.send(ListResult {
                    tab_id: req.tab_id,
                    generation: req.generation,
                    entries,
                });
                if sent.is_err() {
                    break;
                }
            }
        })
        .expect("spawn rove-lister worker");
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

/// 缩略图工作线程：COM 初始化一次，循环取路径生成缩略图，
/// 经事件循环回写缓存（线程安全：Image 可跨线程发送）。
#[cfg(windows)]
fn spawn_thumb_worker(rx: mpsc::Receiver<(PathBuf, u32)>) {
    std::thread::Builder::new()
        .name("rove-thumbs".into())
        .spawn(move || {
            while let Ok((path, size)) = rx.recv() {
                let result = crate::ops::thumbs::get_thumbnail(&path, size);
                match result {
                    Ok((w, h, rgba)) => {
                        // slint::Image 非 Send，事件循环内再构造。
                        let _ = slint::invoke_from_event_loop(move || {
                            let Some(img) = to_slint_image(w, h, &rgba) else {
                                return;
                            };
                            let Some(app) = SESSION.with(|slot| slot.borrow().clone()) else {
                                return;
                            };
                            app.borrow_mut().store_thumbnail((path, size), img);
                        });
                    }
                    Err(e) => {
                        logln!("thumb failed {}: {}", path.display(), e);
                        let _ = slint::invoke_from_event_loop(move || {
                            let Some(app) = SESSION.with(|slot| slot.borrow().clone()) else {
                                return;
                            };
                            app.borrow_mut().fail_thumbnail((path, size));
                        });
                    }
                }
            }
        })
        .ok();
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

    #[test]
    fn drag_gesture_accumulates_until_threshold() {
        let paths = vec![PathBuf::from("C:\\a.txt")];
        let mut dp = Some((0.0f32, 0.0f32, paths.clone()));

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
}

// ---------------------------------------------------------------------------
// UI 同步
// ---------------------------------------------------------------------------

impl App {
    fn sync_tabs(&self) {
        let tabs: Vec<TabInfo> = self
            .state
            .tabs
            .iter()
            .map(|t| TabInfo {
                id: t.id as i32,
                title: t.title().into(),
                active: t.id == self.state.active().id,
                is_settings: t.kind == crate::state::TabKind::Settings,
            })
            .collect();
        self.ui.set_tabs(ModelRc::new(VecModel::from(tabs)));
        self.ui.set_active_tab(self.state.active_tab as i32);
        self.ui.set_tab_loading(self.state.active().loading);
        self.ui.set_settings_open(self.state.active().kind == crate::state::TabKind::Settings);
    }

    fn sync_nav(&self) {
        let tab = self.state.active();
        self.ui.set_can_go_back(tab.can_go_back());
        self.ui.set_can_go_forward(tab.can_go_forward());
        self.ui.set_can_go_up(tab.parent_path().is_some());
    }

    fn sync_breadcrumbs(&self) {
        let tab = self.state.active();
        let mut segments: Vec<BreadcrumbSegment> = Vec::new();
        segments.push(BreadcrumbSegment {
            label: "此电脑".into(),
            path: SharedString::default(),
        });
        if let Some(path) = &tab.path {
            if *path == fs::drives::recycle_virtual_path() {
                // 回收站视图：单个虚拟段。
                segments.push(BreadcrumbSegment {
                    label: "回收站".into(),
                    path: fs::drives::recycle_virtual_path().to_string_lossy().into_owned().into(),
                });
                self.ui.set_breadcrumbs(ModelRc::new(VecModel::from(segments)));
                return;
            }
            // 驱动器段使用卷标（"Win11 (C:)"），其余为目录名。
            let mut cumulative = PathBuf::new();
            let components: Vec<String> = path
                .components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect();
            for (i, comp) in components.iter().enumerate() {
                // 根目录分隔符已被驱动器段覆盖，跳过（避免单独的 "\" 段）。
                if i > 0 && comp == "\\" {
                    continue;
                }
                if i == 0 {
                    // "C:" → 补上根
                    cumulative = PathBuf::from(format!("{comp}\\"));
                } else {
                    cumulative = cumulative.join(comp);
                }
                let label = if i == 0 {
                    self.drives
                        .iter()
                        .find(|d| cumulative.starts_with(format!("{}:", d.letter).as_str()))
                        .map(|d| d.display_label())
                        .unwrap_or_else(|| format!("{comp}\\"))
                } else {
                    comp.clone()
                };
                segments.push(BreadcrumbSegment {
                    label: label.into(),
                    path: cumulative.to_string_lossy().into_owned().into(),
                });
            }
        }
        self.ui.set_breadcrumbs(ModelRc::new(VecModel::from(segments)));
    }

    /// 切换视图模式并持久化。
    fn set_view_mode(&mut self, mode: &str) {
        if self.config.view_mode == mode {
            return;
        }
        self.config.view_mode = mode.to_string();
        self.config.save();
        self.ui.set_view_mode(mode.into());
        self.visible_cache = None;
        self.state.active_mut().scroll_y = 0.0;
        self.ui.invoke_scroll_list_to_top();
        self.sync_file_list();
        self.refresh_visible_rows();
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
            tab.entries = entries;
            tab.pinyin_cache.clear();
            tab.filter.clear();
            tab.display = (0..n).collect();
            return;
        }
        let index = shared();
        let Ok(guard) = index.read() else {
            return;
        };
        let hits = guard.search(query, 200);
        let entries: Vec<FileEntry> = hits
            .into_iter()
            .map(|(e, _)| FileEntry::from_path(&e.path, e.is_dir, 0, None))
            .collect();
        let tab = self.state.active_mut();
        tab.entries = entries;
        tab.pinyin_cache.clear();
        tab.filter = query.to_string();
        tab.rebuild_display();
    }

    fn sync_status(&self) {
        // 工具栏粘贴按钮可用性（系统剪贴板或内部缓存非空）。
        let has_clipboard = !self.state.clipboard_paths.is_empty() || !clipboard::read_files().paths.is_empty();
        self.ui.set_can_paste(has_clipboard);
        let tab = self.state.active();
        let (dirs, files, selected_size) = tab.stats();
        let total = tab.display.len();
        let mut left = format!("{total} 个项目（{dirs} 个文件夹，{files} 个文件）");
        if tab.deep_searching {
            left.push_str(" · 正在搜索子文件夹…");
        }
        self.ui.set_status_left(left.into());
        let selected = tab.selection_count();
        let right = if selected == 0 {
            String::new()
        } else if selected_size > 0 {
            format!("已选 {selected} 项 · {}", fs::format_size_bytes(selected_size))
        } else {
            format!("已选 {selected} 项")
        };
        self.ui.set_status_right(right.into());
    }

    fn sync_sidebar(&mut self) {
        self.drives = list_drives();
        let stats = recycle_bin_stats();
        let network = list_network_locations();

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
        for n in &network {
            fingerprint = fingerprint.wrapping_mul(31).wrapping_add(n.len() as u64);
        }
        if fingerprint == self.sidebar_fingerprint {
            return;
        }
        self.sidebar_fingerprint = fingerprint;

        let mut items: Vec<SidebarItem> = Vec::new();

        // ---- 各分区内容 ----
        // key → 区块（header + 条目），按 config.sidebar_order 组装。
        let mut quick_items: Vec<SidebarItem> = vec![header("快速访问")];
        for (label, id_suffix, dir) in quick_access_dirs() {
            // 目录不存在时跳过该入口（值本身不使用，仅作存在性探测）。
            let Some(_dir) = dir else { continue };
            let (color, icon) = match label {
                "桌面" => (Color::from_rgb_u8(0xE8, 0xB3, 0x3C), "桌"),
                "下载" => (Color::from_rgb_u8(0x3E, 0x7B, 0xC4), "下"),
                "图片" => (Color::from_rgb_u8(0x4C, 0x9F, 0x70), "图"),
                "文档" => (Color::from_rgb_u8(0x3E, 0x7B, 0xC4), "文"),
                "音乐" => (Color::from_rgb_u8(0xD9, 0x69, 0x4E), "乐"),
                "视频" => (Color::from_rgb_u8(0x8E, 0x6C, 0xC9), "视"),
                _ => (Color::from_rgb_u8(0x8A, 0x8F, 0x98), "回"),
            };
            let detail = if label == "回收站" {
                format!(
                    "{} 项 · {}",
                    stats.items,
                    fs::format_size_bytes(stats.bytes.max(0) as u64)
                )
            } else {
                String::new()
            };
            quick_items.push(SidebarItem {
                id: format!("quick:{id_suffix}").into(),
                label: label.into(),
                detail: detail.into(),
                kind: "quick".into(),
                icon_color: color,
                icon_text: icon.into(),
                usage: -1.0,
            });
        }

        let mut recent_items: Vec<SidebarItem> = Vec::new();
        if !self.state.recent.is_empty() {
            recent_items.push(header("最近使用"));
            for path in self.state.recent.iter().take(8) {
                if !path.exists() {
                    continue;
                }
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
                });
            }
        }

        let mut computer_items: Vec<SidebarItem> = vec![header("此电脑")];
        for drive in &self.drives {
            computer_items.push(SidebarItem {
                id: format!("drive:{}", drive.letter).into(),
                label: drive.display_label().into(),
                detail: format!(
                    "{} 可用，共 {}",
                    fs::format_size_bytes(drive.free_bytes),
                    fs::format_size_bytes(drive.total_bytes)
                )
                .into(),
                kind: "drive".into(),
                icon_color: drive_color(drive),
                icon_text: drive.letter.to_string().into(),
                usage: drive.usage() as f32,
            });
        }

        let mut network_items: Vec<SidebarItem> = vec![header("网络位置")];
        if network.is_empty() {
            network_items.push(SidebarItem {
                id: "net:none".into(),
                label: "暂无网络位置".into(),
                detail: String::new().into(),
                kind: "quick".into(),
                icon_color: Color::from_rgb_u8(0x8A, 0x8F, 0x98),
                icon_text: "网".into(),
                usage: -1.0,
            });
        } else {
            for remote in &network {
                network_items.push(SidebarItem {
                    id: format!("net:{remote}").into(),
                    label: remote.clone().into(),
                    detail: String::new().into(),
                    kind: "network".into(),
                    icon_color: Color::from_rgb_u8(0x3A, 0xA6, 0xA6),
                    icon_text: "网".into(),
                    usage: -1.0,
                });
            }
        }

        // ---- 按 config.sidebar_order 组装 ----
        for key in &self.config.sidebar_order {
            let block = match key.as_str() {
                "quick" => &quick_items,
                "recent" => &recent_items,
                "computer" => &computer_items,
                "network" => &network_items,
                _ => continue,
            };
            items.extend(block.iter().cloned());
        }

        // 当前激活的侧边栏项。
        let active_id = match self.state.active().path.as_ref() {
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
        self.ui
            .set_sort_key(match self.state.active().sort.key {
                SortKey::Name => "name",
                SortKey::Size => "size",
                SortKey::Modified => "modified",
                SortKey::Type => "type",
                SortKey::Path => "path",
            }
            .into());
        self.ui
            .set_sort_ascending(self.state.active().sort.ascending);
        self.ui.set_file_loading(self.state.active().loading);
        self.ui
            .set_file_total(self.state.active().display.len() as i32);
        self.refresh_visible_rows();
    }

    /// 重建虚拟列表可见窗口（滚动/选择/数据变化时调用）。
    /// 取某路径的缩略图：命中缓存直接返回；否则对候选路径发起后台加载
    /// （去重），返回空图（UI 显示占位色块，加载完成后经事件循环刷新）。
    /// 当前视图对应的缩略图边长。
    #[cfg(windows)]
    fn thumb_size_for_view(&self) -> u32 {
        match self.config.view_mode.as_str() {
            "icons-xl" => 132,
            "icons-large" => 88,
            "icons-medium" => 60,
            _ => crate::ops::thumbs::THUMB_SIZE,
        }
    }

    #[cfg(windows)]
    fn thumb_for(&mut self, path: &Path, size: u32) -> slint::Image {
        let key = (path.to_path_buf(), size);
        if let Some(img) = self.thumb_cache.get(&key) {
            return img.clone();
        }
        if self.thumb_failed.contains(&key) {
            return slint::Image::default();
        }
        if !self.thumb_pending.contains(&key) {
            self.thumb_pending.insert(key.clone());
            let _ = self.thumb_tx.send(key);
        }
        slint::Image::default()
    }

    #[cfg(not(windows))]
    fn thumb_for(&mut self, _path: &Path, _size: u32) -> slint::Image {
        slint::Image::default()
    }

    #[cfg(not(windows))]
    fn thumb_size_for_view(&self) -> u32 {
        44
    }

    /// 缩略图容量上限。
    #[allow(dead_code)]
    const THUMB_CACHE_CAP: usize = 512;

    /// 标记缩略图失败（清理 pending + 负缓存，本会话不再重试）。
    #[allow(dead_code)]
    fn fail_thumbnail(&mut self, key: (PathBuf, u32)) {
        self.thumb_pending.remove(&key);
        self.thumb_failed.insert(key);
    }

    /// 写入缩略图缓存（事件循环线程），并刷新可见行。
    #[allow(dead_code)]
    fn store_thumbnail(&mut self, key: (PathBuf, u32), img: slint::Image) {
        self.thumb_pending.remove(&key);
        if self.thumb_cache.contains_key(&key) {
            self.thumb_cache.insert(key.clone(), img);
            return;
        }
        if self.thumb_cache.len() >= Self::THUMB_CACHE_CAP {
            // 简单 FIFO 淘汰。
            while self.thumb_order.len() >= Self::THUMB_CACHE_CAP {
                let oldest = self.thumb_order.remove(0);
                self.thumb_cache.remove(&oldest);
            }
        }
        self.thumb_order.push(key.clone());
        self.thumb_cache.insert(key, img);
        self.visible_cache = None;
        self.refresh_visible_rows();
    }

    fn refresh_visible_rows(&mut self) {
        let list_height = self.ui.get_list_height();
        // 视图几何：details/list 每行 1 条；icons 网格按 cells-per-row 折算。
        let cells_per_row = (self.ui.get_cells_per_row().max(1)) as usize;
        let row_h_px = (self.ui.get_row_height_out() / 1.0f32.max(1.0)).max(ROW_HEIGHT);
        let row_h = if self.ui.get_row_height_out() > 0.0 {
            self.ui.get_row_height_out()
        } else {
            ROW_HEIGHT
        };
        let _ = row_h_px;
        let rows_visible = (list_height / row_h).ceil() as usize + OVERSCAN * 2;
        let total = self.state.active().display.len();
        let total_rows = total.div_ceil(cells_per_row);
        let scroll = self.state.active().scroll_y.max(0.0);
        let mut first_row = (scroll / row_h).floor() as isize - OVERSCAN as isize;
        first_row = first_row.clamp(0, total_rows.saturating_sub(1) as isize);
        let first_row = first_row.max(0) as usize;
        let count_rows = rows_visible.min(total_rows.saturating_sub(first_row)).max(1);

        // 行区间 → 条目区间。
        let first = first_row * cells_per_row;
        let count = (count_rows * cells_per_row).min(total.saturating_sub(first)).max(1);

        logln!("visible_rows h={list_height} total={total} cpr={cells_per_row} first={first} count={count}");
        if self.visible_cache == Some((first, count)) && !self.state.active().loading {
            return;
        }
        self.visible_cache = Some((first, count));

        let tab = self.state.active();
        let computer_view = tab.path.is_none();
        let cut = &self.state.cut_paths;
        // 高亮查询（空 = 不高亮）：取自当前标签过滤词（文件夹视图与
        // 全局搜索都写入 filter），命中区间经拼音反向索引映射回原名。
        let hl_query = tab.filter.trim().to_lowercase();
        let mut hl_matcher = nucleo_matcher::Matcher::new(nucleo_matcher::Config::DEFAULT);
        let mut hl_bufs = (Vec::new(), Vec::new(), Vec::new());
        // 第一遍（借用 tab）：收集除缩略图外的全部行数据；
        // 第二遍（借用 self）：填缩略图 / 发起后台加载请求。
        let collected: Vec<(i32, FileEntry, bool, bool, Vec<(String, bool)>)> =
            (first..first + count)
                .filter_map(|di| {
                    let entry = tab.display_entry(di)?.clone();
                    let selected = tab.selection.contains(&entry.path);
                    let cut = cut.contains(&entry.path);
                    let name_parts = if hl_query.is_empty() {
                        Vec::new()
                    } else {
                        tab.display_parts(di)
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
                    Some((di as i32, entry, selected, cut, name_parts))
                })
                .collect();
        // tab 借用随 collected 完成而结束。
        let rows: Vec<FileRow> = collected
            .into_iter()
            .map(|(index, entry, selected, cut, name_parts)| {
                let thumb = self.thumb_for(&entry.path, self.thumb_size_for_view());
                FileRow {
                    index,
                    name: entry.name.clone().into(),
                    path: entry.path.to_string_lossy().into_owned().into(),
                    is_dir: entry.is_dir,
                    size_text: if computer_view {
                        entry.extra.clone()
                    } else {
                        fs::format_size(&entry)
                    }
                    .into(),
                    modified_text: fs::format_time(entry.modified).into(),
                    type_text: entry.type_text.clone().into(),
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
                }
            })
            .collect();
        self.ui
            .set_file_rows(ModelRc::new(VecModel::from(rows)));
    }

    /// 切换标签后的完整同步。
    fn sync_active_tab(&mut self) {
        // 切换标签后退出地址栏编辑态。
        self.ui.set_path_editing(false);
        self.sync_tabs();
        self.sync_nav();
        self.sync_breadcrumbs();
        self.sync_status();
        self.visible_cache = None;
        self.sync_file_list();
        self.sync_sidebar_active();
    }

    fn sync_sidebar_active(&mut self) {
        // 仅更新激活项（避免整表重建）。
        let active_id = match self.state.active().path.as_ref() {
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
    }
}

fn header(label: &str) -> SidebarItem {
    SidebarItem {
        id: format!("header:{label}").into(),
        label: label.into(),
        detail: String::new().into(),
        kind: "header".into(),
        icon_color: Color::default(),
        icon_text: String::new().into(),
        usage: -1.0,
    }
}

fn drive_color(drive: &DriveInfo) -> Color {
    let (r, g, b) = match drive.drive_type {
        fs::DriveType::Fixed => (0x3E, 0x7B, 0xC4),
        fs::DriveType::Removable => (0x4C, 0x9F, 0x70),
        fs::DriveType::Network => (0x3A, 0xA6, 0xA6),
        fs::DriveType::Cdrom => (0xB0, 0x7A, 0x2E),
        fs::DriveType::Ramdisk => (0x8E, 0x6C, 0xC9),
        fs::DriveType::Unknown => (0x8A, 0x8F, 0x98),
    };
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
fn group_options(actions: &[crate::ops::actions::ActionEntry]) -> Vec<SharedString> {
    let mut opts: Vec<SharedString> = vec!["一级菜单".into()];
    for a in actions {
        let g = a.group.trim();
        if !g.is_empty() && !opts.iter().any(|o| o == g) {
            opts.push(g.into());
        }
    }
    opts
}

impl App {
    /// 依据目标构建右键菜单模型；同时返回内容高度（逻辑像素）。
    /// 自 Sprint 2 起，Files 视图菜单完全由 actions.toml（含内置动作）驱动，
    /// 按 group 字段自动组织一级项与二级子菜单。
    fn build_menu(&self, target: &MenuTarget) -> (Vec<MenuItem>, f32) {
        let mut items: Vec<MenuItem> = Vec::new();
        let sep = || MenuItem {
            id: SharedString::default(),
            label: SharedString::default(),
            icon_text: SharedString::default(),
            icon_color: Color::default(),
            kind: "separator".into(),
            enabled: false,
        };
        let item = |id: &str, label: &str, icon: &str, color: Color, kind: &str| MenuItem {
            id: id.into(),
            label: label.into(),
            icon_text: icon.into(),
            icon_color: color,
            kind: kind.into(),
            enabled: true,
        };
        let accent = Color::from_rgb_u8(0x0E, 0x8A, 0x4A);
        let blue = Color::from_rgb_u8(0x3E, 0x7B, 0xC4);
        let amber = Color::from_rgb_u8(0xE8, 0xB3, 0x3C);
        let gray = Color::from_rgb_u8(0x8A, 0x8F, 0x98);
        let danger = Color::from_rgb_u8(0xC4, 0x2B, 0x1C);

        // 回收站视图：专用菜单（还原 / 彻底删除 / 清空）。
        if self.state.active().path.as_deref() == Some(fs::drives::recycle_virtual_path().as_path()) {
            let mut items: Vec<MenuItem> = Vec::new();
            match target {
                MenuTarget::Files(_) => {
                    items.push(item("recycle-restore", "还原到原位置", "\u{21a9}", accent, "normal"));
                    items.push(sep());
                    items.push(item("recycle-purge", "彻底删除", "✕", danger, "danger"));
                }
                MenuTarget::Background => {
                    items.push(item("recycle-empty", "清空回收站", "🗑", danger, "danger"));
                    items.push(sep());
                    items.push(item(ACT_REFRESH, "刷新", "⟳", gray, "normal"));
                }
            }
            let height = items
                .iter()
                .map(|i| if i.kind == "separator" { 9.0 } else { 28.0 })
                .sum::<f32>()
                + 8.0;
            return (items, height);
        }

        match target {
            MenuTarget::Files(paths) => {
                let single = paths.len() == 1;
                // 从 actions.toml 过滤出适用当前选中的动作，按 group 分组。
                let mut groups: std::collections::BTreeMap<String, Vec<&crate::ops::actions::ActionEntry>> =
                    std::collections::BTreeMap::new();
                let mut root_order: Vec<String> = Vec::new();
                for a in self.actions.iter().filter(|a| a.enabled && a.matches_selection(paths)) {
                    let g = a.group.clone();
                    if !groups.contains_key(&g) {
                        root_order.push(g.clone());
                    }
                    groups.entry(g).or_default().push(a);
                }

                // group="" 的项作为一级菜单项直接输出。
                if let Some(root_items) = groups.remove("") {
                    for a in root_items {
                        let mut m = self.action_menu_item(a);
                        // 重命名在单选外置灰。
                        if a.id == "builtin.rename" && !single {
                            m.enabled = false;
                        }
                        // 终端：非目录时改标签。
                        if a.id == "builtin.terminal" {
                            let first_is_dir = paths.first().map(|p| p.is_dir()).unwrap_or(false);
                            m.label = if first_is_dir { "在此处打开终端".into() } else { "在终端中打开".into() };
                        }
                        items.push(m);
                    }
                }

                // 非空 group 输出为二级子菜单；顺序保留 actions.toml 中首次出现顺序。
                for g in root_order.iter().filter(|g| !g.is_empty()) {
                    let Some(_) = groups.get(g) else { continue };
                    items.push(MenuItem {
                        id: format!("group:{g}").into(),
                        label: g.clone().into(),
                        icon_text: "▣".into(),
                        icon_color: amber,
                        kind: "submenu".into(),
                        enabled: true,
                    });
                }
            }
            MenuTarget::Background => {
                let has_clipboard = !self.state.clipboard_paths.is_empty();
                items.push(MenuItem {
                    id: ACT_PASTE.into(),
                    label: if self.state.clipboard_operation == "move" {
                        "粘贴（移动）"
                    } else {
                        "粘贴"
                    }
                    .into(),
                    icon_text: "⧉".into(),
                    icon_color: blue,
                    kind: "normal".into(),
                    enabled: has_clipboard,
                });
                items.push(item(ACT_NEW_FOLDER, "新建文件夹", "＋", amber, "normal"));
                items.push(sep());
                items.push(item(ACT_TERMINAL, "在此处打开终端", ">_", gray, "normal"));
                items.push(item(ACT_REFRESH, "刷新", "⟳", gray, "normal"));
                items.push(item(ACT_SELECT_ALL, "全选", "▣", gray, "normal"));
                items.push(item(ACT_MANAGE_ACTIONS, "管理右键动作...", "✎", gray, "normal"));
                items.push(item(ACT_VIEW, "视图", "▦", gray, "submenu"));
            }
        }

        let height = items
            .iter()
            .map(|i| if i.kind == "separator" { 9.0 } else { 28.0 })
            .sum::<f32>()
            + 8.0;
        (items, height)
    }

    /// 动作条目 → 菜单项。
    fn action_menu_item(&self, a: &crate::ops::actions::ActionEntry) -> MenuItem {
        let icon = if a.label.contains("解压") { "▾" } else { "▣" };
        MenuItem {
            id: format!("act:{}", a.id).into(),
            label: a.label.clone().into(),
            icon_text: icon.into(),
            icon_color: Color::from_rgb_u8(0xE8, 0xB3, 0x3C),
            kind: "normal".into(),
            enabled: true,
        }
    }

    /// 按子菜单 id 构建二级菜单内容。
    fn build_submenu_for(&self, id: &str) -> (Vec<MenuItem>, f32) {
        if id == ACT_VIEW {
            let current = self.config.view_mode.clone();
            let entry = |vid: &str, label: &str, active: bool| MenuItem {
                id: format!("view:{vid}").into(),
                label: if active {
                    format!("● {label}").into()
                } else {
                    label.to_string().into()
                },
                icon_text: SharedString::default(),
                icon_color: Color::default(),
                kind: "normal".into(),
                enabled: true,
            };
            let items = vec![
                entry("icons-xl", "超大图标", current == "icons-xl"),
                entry("icons-large", "大图标", current == "icons-large"),
                entry("icons-medium", "中等图标", current == "icons-medium"),
                entry("icons-small", "小图标", current == "icons-small"),
                entry("list", "列表", current == "list"),
                entry("details", "详细信息", current == "details"),
            ];
            let height = items.len() as f32 * 28.0 + 8.0;
            return (items, height);
        }

        // 动作分组二级菜单（打开方式 / 发送到 / 压缩与解压 / 自定义分组）。
        if let Some(group_name) = id.strip_prefix("group:") {
            let targets = match &self.menu_target {
                Some(MenuTarget::Files(p)) => p.clone(),
                _ => Vec::new(),
            };
            let amber = Color::from_rgb_u8(0xE8, 0xB3, 0x3C);
            let gray = Color::from_rgb_u8(0x8A, 0x8F, 0x98);

            if group_name == "打开方式" {
                let items = vec![
                    MenuItem {
                        id: "act:builtin.open_with".into(),
                        label: "打开方式...".into(),
                        icon_text: "◈".into(),
                        icon_color: gray,
                        kind: "normal".into(),
                        enabled: true,
                    },
                ];
                let n = items.len();
                return (items, n as f32 * 28.0 + 8.0);
            }

            if group_name == "发送到" {
                let items = vec![
                    MenuItem {
                        id: ACT_SENDTO_DESKTOP.into(),
                        label: "桌面快捷方式".into(),
                        icon_text: "\u{2302}".into(),
                        icon_color: amber,
                        kind: "normal".into(),
                        enabled: true,
                    },
                    MenuItem {
                        id: ACT_SENDTO_STARTMENU.into(),
                        label: "开始菜单快捷方式".into(),
                        icon_text: "\u{229e}".into(),
                        icon_color: amber,
                        kind: "normal".into(),
                        enabled: true,
                    },
                ];
                let n = items.len();
                return (items, n as f32 * 28.0 + 8.0);
            }

            let items: Vec<MenuItem> = self
                .actions
                .iter()
                .filter(|a| a.enabled && a.group == group_name && a.matches_selection(&targets))
                .map(|a| self.action_menu_item(a))
                .collect();
            let n = items.len();
            return (items, n as f32 * 28.0 + 8.0);
        }

        // 默认：发送到。
        self.build_sendto_submenu()
    }

    /// 「发送到」二级菜单兜底。
    fn build_sendto_submenu(&self) -> (Vec<MenuItem>, f32) {
        let amber = Color::from_rgb_u8(0xE8, 0xB3, 0x3C);
        let items = vec![
            MenuItem {
                id: ACT_SENDTO_DESKTOP.into(),
                label: "桌面快捷方式".into(),
                icon_text: "\u{2302}".into(),
                icon_color: amber,
                kind: "normal".into(),
                enabled: true,
            },
            MenuItem {
                id: ACT_SENDTO_STARTMENU.into(),
                label: "开始菜单快捷方式".into(),
                icon_text: "\u{229e}".into(),
                icon_color: amber,
                kind: "normal".into(),
                enabled: true,
            },
        ];
        (items, 2.0 * 28.0 + 8.0)
    }

    /// 打开右键菜单（坐标为 FileList 内坐标，Slint 侧再加布局偏移）。
    fn open_menu(&self, target: MenuTarget, x: f32, y: f32) {
        let (items, height) = self.build_menu(&target);
        self.ui.set_menu_items(ModelRc::new(VecModel::from(items)));
        self.ui.set_menu_items_height(height);
        self.ui
            .invoke_open_menu(x, y);
    }

    fn close_menu(&self) {
        self.ui.invoke_close_menu();
    }
}

/// "发送到" 菜单项 ID（在 build_menu 中使用）。
const ACT_MANAGE_ACTIONS: &str = "manage_actions";
const ACT_VIEW: &str = "view_menu";

// ---------------------------------------------------------------------------
// 其余交互动作（键盘快捷键 / 侧边栏 / 列排序 / 重命名 / 暂存盘）
// ---------------------------------------------------------------------------

impl App {
    /// 打开设置页标签（本地右键菜单子页）。
    fn open_settings_tab(&mut self, subpage: crate::state::SettingsSubpage) {
        self.state.open_settings(subpage);
        self.ui.set_settings_open(true);
        self.ui.set_filter_text(SharedString::default());
        self.sync_tabs();
        self.sync_nav();
        self.sync_breadcrumbs();
        self.sync_file_list();
        self.sync_status();
        self.sync_settings();
    }

    /// 保存当前 actions 并重新加载（自愈压缩软件路径、合并内置动作）。
    fn save_actions_and_reload(&mut self) {
        if let Err(e) = crate::ops::actions::save(&self.actions) {
            self.set_status_job(e);
            return;
        }
        let raw = crate::ops::actions::load_or_create();
        self.actions = crate::ops::actions::heal_full(&raw);
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

        let nav_items = vec![
            SettingsNavItem {
                id: "general".into(),
                label: "通用".into(),
                icon: "⚙".into(),
                selected: current == SettingsSubpage::General,
            },
            SettingsNavItem {
                id: "search".into(),
                label: "搜索与索引".into(),
                icon: "⌕".into(),
                selected: current == SettingsSubpage::SearchAndIndex,
            },
            SettingsNavItem {
                id: "context".into(),
                label: "本地右键菜单".into(),
                icon: "☰".into(),
                selected: current == SettingsSubpage::ContextMenu,
            },
            SettingsNavItem {
                id: "duplicates".into(),
                label: "重复文件".into(),
                icon: "◫".into(),
                selected: current == SettingsSubpage::Duplicates,
            },
            SettingsNavItem {
                id: "quicklook".into(),
                label: "快速预览".into(),
                icon: "⏵".into(),
                selected: current == SettingsSubpage::QuickLook,
            },
        ];
        self.ui.set_settings_nav_items(ModelRc::new(VecModel::from(nav_items)));

        // 子页标题 / 描述 / 新增入口（未实现子页显示占位提示）。
        let (title, desc, show_add) = match current {
            SettingsSubpage::General => ("通用", "应用启动、视图与行为偏好。", false),
            SettingsSubpage::SearchAndIndex => ("搜索与索引", "拼音检索、索引范围与缓存策略。", false),
            SettingsSubpage::ContextMenu => (
                "本地右键菜单",
                "开关控制菜单项显隐；内置动作仅允许开关，自定义动作可编辑或删除。",
                true,
            ),
            SettingsSubpage::Duplicates => ("重复文件", "重复项扫描规则与清理策略。", false),
            SettingsSubpage::QuickLook => ("快速预览", "空格键快速预览的行为配置。", false),
        };
        self.ui.set_settings_page_title(title.into());
        self.ui.set_settings_page_desc(desc.into());
        self.ui.set_settings_show_add(show_add);

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
                label: a.label.clone().into(),
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
                    "一级独立菜单项".into()
                } else {
                    g.clone().into()
                };
                SettingsActionGroup {
                    name,
                    expanded: true,
                    items: ModelRc::new(VecModel::from(groups_map.remove(&g).unwrap_or_default())),
                }
            })
            .collect();
        let groups_model = VecModel::from(groups);
        // Flickable 滚动范围：按各分组展开状态累加卡片高度（44 头 + 42/行）+ 12 间距。
        let mut content_h = 0.0f32;
        let n = groups_model.row_count();
        for i in 0..n {
            if let Some(g) = groups_model.row_data(i) {
                content_h += if g.expanded {
                    44.0 + g.items.row_count() as f32 * 42.0
                } else {
                    44.0
                } + 12.0;
            }
        }
        self.ui.set_settings_content_height(content_h);
        self.ui.set_settings_action_groups(ModelRc::new(groups_model));
    }

    /// 打开视图模式下拉菜单（Pulse 式）。
    fn open_view_picker(&mut self) {
        let current = self.config.view_mode.clone();
        let entry = |vid: &str, label: &str, active: bool| MenuItem {
            id: format!("view:{vid}").into(),
            label: if active {
                format!("● {label}").into()
            } else {
                label.to_string().into()
            },
            icon_text: SharedString::default(),
            icon_color: Color::default(),
            kind: "normal".into(),
            enabled: true,
        };
        let items = vec![
            entry("icons-xl", "超大图标", current == "icons-xl"),
            entry("icons-large", "大图标", current == "icons-large"),
            entry("icons-medium", "中等图标", current == "icons-medium"),
            entry("icons-small", "小图标", current == "icons-small"),
            entry("list", "列表", current == "list"),
            entry("details", "详细信息", current == "details"),
        ];
        let height = items.len() as f32 * 28.0 + 8.0;
        self.ui.set_menu_items(ModelRc::new(VecModel::from(items)));
        self.ui.set_menu_items_height(height);
        // 弹在工具栏视图按钮下方（按钮在窗口右侧区域，取搜索框左侧固定区起点）。
        // y = 工具栏底部（topbar 44px 之下），菜单顶贴住按钮。
        let x = self.ui.get_view_width() - 220.0;
        self.ui.invoke_open_menu(x, 44.0);
    }

    /// 面包屑路径编辑提交（点击地址栏输入路径后回车）。
    fn commit_path_edit(&mut self, path: &str) {
        let trimmed = path.trim().trim_matches('"');
        if trimmed.is_empty() {
            return;
        }
        let p = PathBuf::from(trimmed);
        if !p.exists() {
            self.set_status_job(format!("路径不存在：{trimmed}"));
            return;
        }
        self.navigate_active(Some(p));
    }

    /// 侧栏各行的顶 y（内容坐标）。行高与 sidebar.slint 保持一致：
    /// header 30 / drive 52 / 其余 32，行间距 2，顶部内边距 8。
    fn sidebar_row_tops(&self) -> Vec<f32> {
        let model = self.ui.get_sidebar_items();
        let n = model.row_count();
        let mut tops = Vec::with_capacity(n);
        let mut y = 8.0_f32;
        for i in 0..n {
            tops.push(y);
            let h = model
                .row_data(i)
                .map(|it| match it.kind.as_str() {
                    "header" => 30.0,
                    "drive" => 52.0,
                    _ => 32.0,
                })
                .unwrap_or(32.0);
            y += h + 2.0;
        }
        tops
    }

    /// 侧栏分区拖拽排序（phase：0 按下 / 1 拖动 / 2 松开）。
    fn sidebar_header_drag(&mut self, index: usize, phase: i32, y_in_row: f32) {
        let tops = self.sidebar_row_tops();
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
            let key = match it.label.as_str() {
                "快速访问" => "quick",
                "最近使用" => "recent",
                "此电脑" => "computer",
                "网络位置" => "network",
                _ => "",
            };
            if !key.is_empty() {
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
                // 落点：非拖拽分区中，中点在指针上方的数量。
                let drop_slot = headers
                    .iter()
                    .enumerate()
                    .filter(|(s, _)| *s != drag_slot)
                    .filter(|(_, (start, _))| {
                        let next_start = headers
                            .iter()
                            .map(|(i, _)| *i)
                            .find(|h| *h > *start)
                            .unwrap_or(n);
                        let mid = tops[*start] + (tops[next_start.min(n - 1)] - tops[*start]) / 2.0;
                        mid < pointer_y
                    })
                    .count();
                // 指示线 y：目标分区顶部（或末尾）。
                let remaining: Vec<usize> = headers
                    .iter()
                    .enumerate()
                    .filter(|(s, _)| *s != drag_slot)
                    .map(|(_, (i, _))| *i)
                    .collect();
                let indicator_y = remaining
                    .get(drop_slot)
                    .map(|&row| tops[row])
                    .unwrap_or_else(|| tops[n - 1] + 32.0 + 2.0);
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
                let drop_slot = headers
                    .iter()
                    .enumerate()
                    .filter(|(s, _)| *s != drag_slot)
                    .filter(|(_, (start, _))| {
                        let next_start = headers
                            .iter()
                            .map(|(i, _)| *i)
                            .find(|h| *h > *start)
                            .unwrap_or(n);
                        let mid = tops[*start] + (tops[next_start.min(n - 1)] - tops[*start]) / 2.0;
                        mid < pointer_y
                    })
                    .count();
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

    /// 侧边栏项激活。
    fn sidebar_activated(&mut self, id: &str) {
        if id == "quick:recycle" {
            // 回收站站内浏览：虚拟目录 recycle://。
            self.navigate_active(Some(fs::drives::recycle_virtual_path()));
            return;
        }
        if id == "net:none" {
            return;
        }
        if let Some(rest) = id.strip_prefix("quick:") {
            if let Some((_, _, Some(dir))) =
                quick_access_dirs().into_iter().find(|(_, key, _)| *key == rest)
            {
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

    /// 列头点击：切换排序键/方向。
    fn header_clicked(&mut self, key: &str) {
        let new_key = match key {
            "name" => SortKey::Name,
            "size" => SortKey::Size,
            "modified" => SortKey::Modified,
            "type" => SortKey::Type,
            "path" => SortKey::Path,
            _ => return,
        };
        let tab = self.state.active_mut();
        if tab.sort.key == new_key {
            tab.sort.ascending = !tab.sort.ascending;
        } else {
            tab.sort.key = new_key;
            tab.sort.ascending = true;
        }
        let sort = tab.sort;
        sort_entries(&mut tab.entries, sort);
        tab.rebuild_display();
        self.visible_cache = None;
        self.sync_file_list();
        self.sync_status();
    }

    /// 提交重命名。
    fn commit_rename(&mut self, path: &Path, new_name: &str) {
        self.ui.set_rename_path(SharedString::default());
        let new_name = new_name.trim();
        if new_name.is_empty() {
            return;
        }
        if new_name == "." || new_name == ".." || new_name.contains(['\\', '/', ':', '*', '?', '"', '<', '>', '|']) {
            self.set_status_job("名称包含非法字符");
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
            self.set_status_job("同名文件已存在");
            return;
        }
        match std::fs::rename(path, &new_path) {
            Ok(()) => {
                // 更新选择/暂存/剪切集合中的旧路径。
                let tab = self.state.active_mut();
                if tab.selection.remove(path) {
                    tab.selection.insert(new_path.clone());
                }
                self.state.cut_paths.remove(path);
                self.reload_active();
            }
            Err(e) => self.set_status_job(format!("重命名失败：{e}")),
        }
    }

    /// 暂存盘内容复制/移动到当前目录。
    fn staging_transfer(&mut self, is_move: bool) {
        let entries = self.state.staging_present();
        if entries.is_empty() {
            self.set_status_job("暂存盘为空（或文件已不存在）");
            self.sync_staging();
            return;
        }
        let Some(dest) = self.state.active().path.clone() else {
            self.set_status_job("请先进入目标文件夹");
            return;
        };
        let sources: Vec<PathBuf> = entries.iter().map(|e| e.path.clone()).collect();
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

    /// 开始重命名选中项（仅单项）。
    fn start_rename(&self) {
        let paths = self.state.active().selected_paths();
        if paths.len() == 1 {
            self.ui
                .set_rename_path(paths[0].to_string_lossy().into_owned().into());
        }
    }

    /// 打开选中项。
    fn open_selection(&mut self) {
        let paths = self.state.active().selected_paths();
        if let Some(path) = paths.first() {
            let path = path.clone();
            self.open_path(&path);
        }
    }

    /// 删除选中项。
    fn delete_selection(&mut self, to_trash: bool) {
        let paths = self.state.active().selected_paths();
        if !paths.is_empty() {
            self.submit_job(JobKind::Delete { paths, to_trash });
        }
    }

    /// 全选当前显示项。
    fn select_all(&mut self) {
        let selected: HashSet<PathBuf> = {
            let tab = self.state.active();
            tab.display
                .iter()
                .filter_map(|&i| tab.entries.get(i).map(|e| e.path.clone()))
                .collect()
        };
        self.state.active_mut().selection = selected;
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
        let Some(target) = self.menu_target.clone() else {
            return;
        };
        self.close_menu();
        // 视图模式切换（Phase 5）。
        if let Some(mode) = id.strip_prefix("view:") {
            self.set_view_mode(mode);
            return;
        }
        // actions.toml 驱动的动作（Phase 4 / Sprint 2）。
        if let Some(action_id) = id.strip_prefix("act:") {
            match action_id {
                "builtin.open" => {
                    if let MenuTarget::Files(paths) = &target {
                        if let Some(path) = paths.first() {
                            self.open_path(path);
                        }
                    }
                    return;
                }
                "builtin.open_with" => {
                    if let MenuTarget::Files(paths) = &target {
                        if let Some(path) = paths.first() {
                            let _ = std::process::Command::new("rundll32.exe")
                                .args(["shell32.dll,OpenAs_RunDLL", &path.to_string_lossy()])
                                .spawn();
                        }
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
                "builtin.rename" => {
                    if let MenuTarget::Files(paths) = &target {
                        if let Some(path) = paths.first() {
                            self.ui.set_rename_path(path.to_string_lossy().into_owned().into());
                        }
                    }
                    return;
                }
                "builtin.delete" => {
                    if let MenuTarget::Files(paths) = &target {
                        self.submit_job(JobKind::Delete {
                            paths: paths.clone(),
                            to_trash: true,
                        });
                    }
                    return;
                }
                "builtin.stage" => {
                    if let MenuTarget::Files(paths) = &target {
                        let added = self.state.stage_paths(paths);
                        self.sync_staging();
                        self.set_status_job(format!("已加入暂存盘 {added} 项"));
                    }
                    return;
                }
                "builtin.terminal" => {
                    let dir = match &target {
                        MenuTarget::Files(paths) => paths
                            .first()
                            .and_then(|p| if p.is_dir() { Some(p.clone()) } else { p.parent().map(|d| d.to_path_buf()) }),
                        MenuTarget::Background => self.state.active().path.clone(),
                    };
                    if let Some(dir) = dir {
                        if let Err(e) = ops::open_terminal(&dir) {
                            self.set_status_job(e);
                        }
                    }
                    return;
                }
                "builtin.properties" => {
                    // 占位：后续可调用 Windows 属性对话框。
                    return;
                }
                _ => {
                    let entry = self.actions.iter().find(|a| a.id == action_id).cloned();
                    if let Some(entry) = entry {
                        let targets = match &target {
                            MenuTarget::Files(p) => p.clone(),
                            _ => Vec::new(),
                        };
                        match crate::ops::actions::run_action(&entry, &targets) {
                            Ok(()) => {
                                self.set_status_job(format!("已执行：{}", entry.label));
                                self.reload_active();
                            }
                            Err(e) => self.set_status_job(format!("执行失败：{e}")),
                        }
                    }
                    return;
                }
            }
        }
        if id == ACT_MANAGE_ACTIONS {
            self.open_settings_tab(crate::state::SettingsSubpage::ContextMenu);
            return;
        }
        // 回收站视图动作。
        match id {
            "recycle-restore" => {
                if let MenuTarget::Files(paths) = &target {
                    let mut restored = 0;
                    for p in paths {
                        restored += fs::drives::restore_recycle_entry(p);
                    }
                    if restored > 0 {
                        self.set_status_job(format!("已还原 {restored} 项到原位置"));
                    } else {
                        self.set_status_job("还原失败".to_string());
                    }
                    self.reload_active();
                    self.sidebar_fingerprint = 0;
                }
            }
            "recycle-purge" => {
                if let MenuTarget::Files(paths) = &target {
                    let n = fs::drives::purge_recycle_entries(paths);
                    self.set_status_job(if n > 0 {
                        format!("已彻底删除 {n} 项")
                    } else {
                        "删除失败".to_string()
                    });
                    self.reload_active();
                    self.sidebar_fingerprint = 0;
                }
            }
            "recycle-empty" => {
                if fs::drives::empty_recycle_bin() {
                    self.set_status_job("回收站已清空".to_string());
                } else {
                    self.set_status_job("清空回收站失败".to_string());
                }
                self.reload_active();
                self.sidebar_fingerprint = 0;
            }
            _ => {}
        }
        if id.starts_with("recycle-") {
            return;
        }
        match id {
            // 旧硬编码 ID 仍保留，防止外部旧调用；新菜单统一走 act:builtin.*。
            ACT_OPEN => {
                if let MenuTarget::Files(paths) = &target {
                    if let Some(path) = paths.first() {
                        self.open_path(path);
                    }
                }
            }
            ACT_COPY => self.copy_selection(false),
            ACT_CUT => self.copy_selection(true),
            ACT_PASTE => self.paste_clipboard(),
            ACT_RENAME => {
                if let MenuTarget::Files(paths) = &target {
                    if let Some(path) = paths.first() {
                        self.ui
                            .set_rename_path(path.to_string_lossy().into_owned().into());
                    }
                }
            }
            ACT_DELETE => {
                if let MenuTarget::Files(paths) = &target {
                    self.submit_job(JobKind::Delete {
                        paths: paths.clone(),
                        to_trash: true,
                    });
                }
            }
            ACT_STAGE => {
                if let MenuTarget::Files(paths) = &target {
                    let added = self.state.stage_paths(paths);
                    self.sync_staging();
                    self.set_status_job(format!("已加入暂存盘 {added} 项"));
                }
            }
            ACT_TERMINAL => {
                let dir = match &target {
                    MenuTarget::Files(paths) => paths
                        .first()
                        .and_then(|p| if p.is_dir() { Some(p.clone()) } else { p.parent().map(|d| d.to_path_buf()) }),
                    MenuTarget::Background => self.state.active().path.clone(),
                };
                if let Some(dir) = dir {
                    if let Err(e) = ops::open_terminal(&dir) {
                        self.set_status_job(e);
                    }
                }
            }
            ACT_NEW_FOLDER => self.create_folder(),
            ACT_REFRESH => self.reload_active(),
            ACT_SELECT_ALL => {
                let tab = self.state.active_mut();
                let all: Vec<PathBuf> = tab
                    .display
                    .iter()
                    .filter_map(|&i| tab.entries.get(i).map(|e| e.path.clone()))
                    .collect();
                tab.selection = all.into_iter().collect();
                self.refresh_visible_rows();
                self.sync_status();
            }
            ACT_SENDTO_DESKTOP | ACT_SENDTO_STARTMENU => {
                if let MenuTarget::Files(paths) = &target {
                    let dir = if id == ACT_SENDTO_DESKTOP {
                        shortcut::desktop_dir()
                    } else {
                        shortcut::start_menu_programs_dir()
                    };
                    match dir {
                        Some(dir) => match shortcut::create_shortcut_in(&dir, &paths[0]) {
                            Ok(lnk) => self.set_status_job(format!(
                                "已创建快捷方式：{}",
                                lnk.file_name().unwrap_or_default().to_string_lossy()
                            )),
                            Err(e) => self.set_status_job(e),
                        },
                        None => self.set_status_job("找不到目标目录"),
                    }
                }
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
            self.set_status_job(format!("打开失败：{e}"));
        }
    }

    /// 激活行（双击 / Enter）：普通路径按类型打开；
    /// 回收站视图下还原到原位置。
    fn activate_row(&mut self, index: usize) {
        let Some(entry) = self.state.active().display_entry(index).cloned() else {
            return;
        };
        if self.state.active().path.as_deref() == Some(fs::drives::recycle_virtual_path().as_path()) {
            let n = fs::drives::restore_recycle_entry(&entry.path);
            if n > 0 {
                self.set_status_job(format!("已还原 {} 项到原位置", n));
                self.reload_active();
                self.sidebar_fingerprint = 0;
            } else {
                self.set_status_job("还原失败".to_string());
            }
            return;
        }
        self.open_path(&entry.path);
    }

    /// 复制/剪切当前选择到系统剪贴板。
    fn copy_selection(&mut self, is_move: bool) {
        let paths = self.state.active().selected_paths();
        if paths.is_empty() {
            return;
        }
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
                    format!("已剪切 {} 项", self.state.clipboard_paths.len())
                } else {
                    format!("已复制 {} 项", self.state.clipboard_paths.len())
                });
            }
            Err(e) => self.set_status_job(format!("写入剪贴板失败：{e}")),
        }
    }

    /// 粘贴剪贴板内容到当前目录。
    fn paste_clipboard(&mut self) {
        let files = clipboard::read_files();
        if files.paths.is_empty() {
            // 读不到时回落内部缓存。
            if self.state.clipboard_paths.is_empty() {
                return;
            }
        }
        let paths = if files.paths.is_empty() {
            self.state.clipboard_paths.clone()
        } else {
            files.paths
        };
        let is_move = files.operation == "move" || self.state.clipboard_operation == "move";
        let Some(dest) = self.state.active().path.clone() else {
            self.set_status_job("请先进入目标文件夹");
            return;
        };
        self.state.clipboard_paths = paths.clone();
        self.state.clipboard_operation = if is_move { "move".into() } else { "copy".into() };
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
        let Some(dir) = self.state.active().path.clone() else {
            self.set_status_job("请先进入一个文件夹");
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
                self.reload_active();
                self.ui
                    .set_rename_path(path.to_string_lossy().into_owned().into());
            }
            Err(e) => self.set_status_job(format!("创建文件夹失败：{e}")),
        }
    }

    fn submit_job(&mut self, kind: JobKind) {
        if !self.jobs.submit(kind) {
            self.set_status_job("有任务正在进行，请稍候");
        }
    }

    fn set_status_job(&self, message: impl Into<SharedString>) {
        self.ui.set_status_job(message.into());
    }

    /// 任务完成后的处理：刷新受影响目录。
    fn on_job_finished(&mut self, summary: JobSummary) {
        self.set_status_job(summary.message.clone());
        // 刷新路径受影响的标签（源父目录 / 目标目录及其子路径）。
        let affected = &summary.affected_dirs;
        for index in 0..self.state.tabs.len() {
            let needs = self.state.tabs[index]
                .path
                .as_ref()
                .is_some_and(|p| affected.iter().any(|a| a == p || p.starts_with(a)));
            if needs {
                self.reload_tab(index);
            }
        }
        // 移动操作会消耗剪贴板。
        if summary.message.starts_with("已移动") {
            self.state.clipboard_paths.clear();
            self.state.cut_paths.clear();
            self.visible_cache = None;
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
        self.state.active_mut().navigate(path);
        self.sync_after_navigate();
    }

    fn sync_after_navigate(&mut self) {
        // 导航后退出地址栏编辑态。
        self.ui.set_path_editing(false);
        self.sync_tabs();
        self.sync_nav();
        self.sync_breadcrumbs();
        self.sync_sidebar_active();
        self.sync_status();
        self.ui.set_rename_path(SharedString::default());
        self.ui.set_filter_text(SharedString::default());
        self.visible_cache = None;
        self.sync_file_list();
        self.ui.invoke_scroll_list_to_top();
        self.reload_active();
    }

    fn nav_back(&mut self) {
        let tab = self.state.active();
        if !tab.can_go_back() {
            return;
        }
        let index = tab.history_index - 1;
        self.state.active_mut().jump_to(index);
        self.sync_after_navigate();
    }

    fn nav_forward(&mut self) {
        let tab = self.state.active();
        if !tab.can_go_forward() {
            return;
        }
        let index = tab.history_index + 1;
        self.state.active_mut().jump_to(index);
        self.sync_after_navigate();
    }

    fn nav_up(&mut self) {
        let Some(parent) = self.state.active().parent_path() else {
            return;
        };
        self.navigate_active(parent);
    }
}

// ---------------------------------------------------------------------------
// 选择与行交互
// ---------------------------------------------------------------------------

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

impl App {
    fn row_pointer_event(&mut self, index: usize, event: &PointerEvent, x: f32, y: f32) {
        // 任务 2.3：拖出手势（按下 → 位移超阈值 → OLE 拖放循环）。
        match event.kind {
            PointerEventKind::Up | PointerEventKind::Cancel => {
                self.drag_pending = None;
                self.ui.invoke_set_drag_armed(false);
            }
            PointerEventKind::Move => {
                // 仅当位移越过阈值才取走待定手势并启动拖出；否则保留，
                // 否则首次微小移动就会清空 drag_pending 导致永远拖不出（见 #2 缺陷）。
                if let Some(paths) = drag_gesture_paths(&self.drag_pending, x, y) {
                    self.drag_pending = None;
                    self.ui.invoke_set_drag_armed(false);
                    self.begin_drag_out(paths);
                }
            }
            _ => {}
        }
        if event.kind != PointerEventKind::Down {
            return;
        }
        // 地址栏编辑中：第一次点击列表仅取消编辑（与资源管理器一致）。
        if self.ui.get_path_editing() {
            self.ui.set_path_editing(false);
            return;
        }
        let Some(path) = self.state.active().display_entry(index).map(|e| e.path.clone()) else {
            return;
        };
        match event.button {
            PointerEventButton::Right => {
                // 未选中则先选中它，再弹菜单。
                if !self.state.active().selection.contains(&path) {
                    let tab = self.state.active_mut();
                    tab.selection.clear();
                    tab.selection.insert(path.clone());
                    tab.anchor = Some(path.clone());
                }
                let target = MenuTarget::Files(self.state.active().selected_paths());
                self.menu_target = Some(target.clone());
                self.visible_cache = None;
                self.refresh_visible_rows();
                self.sync_status();
                self.open_menu(target, x, y);
            }
            PointerEventButton::Left => {
                // 双击检测在 Rust 侧做（500ms 内同一路径二次按下）：
                // 不依赖 Slint 的 double-clicked，行模型重建也不会打断判定。
                let now = std::time::Instant::now();
                if self
                    .last_click
                    .as_ref()
                    .is_some_and(|(p, t)| *p == path && t.elapsed().as_millis() < 500)
                {
                    self.last_click = None;
                    self.activate_row(index);
                    return;
                }
                self.last_click = Some((path.clone(), now));
                let ctrl = key_down(VK_CONTROL);
                let shift = key_down(VK_SHIFT);
                let tab = self.state.active_mut();
                if ctrl {
                    if tab.selection.contains(&path) {
                        tab.selection.remove(&path);
                    } else {
                        tab.selection.insert(path.clone());
                    }
                    tab.anchor = Some(path);
                } else if shift {
                    if let Some(anchor) = tab.anchor.clone() {
                        let anchor_pos = tab
                            .display
                            .iter()
                            .position(|&i| tab.entries.get(i).is_some_and(|e| e.path == anchor));
                        if let Some(anchor_pos) = anchor_pos {
                            let (lo, hi) = if anchor_pos <= index {
                                (anchor_pos, index)
                            } else {
                                (index, anchor_pos)
                            };
                            tab.selection.clear();
                            for di in lo..=hi {
                                if let Some(e) = tab.display_entry(di) {
                                    tab.selection.insert(e.path.clone());
                                }
                            }
                        }
                    } else {
                        tab.selection.clear();
                        tab.selection.insert(path.clone());
                        tab.anchor = Some(path);
                    }
                } else {
                    tab.selection.clear();
                    tab.selection.insert(path.clone());
                    tab.anchor = Some(path);
                }
                self.visible_cache = None;
                self.refresh_visible_rows();
                self.sync_status();
                // 登记拖出待定手势：拖动的是选中集（行在选中集中时）或该行。
                if self.state.active().path.is_some() && !self.ui.get_rename_path().is_empty() {
                    // 重命名输入态不触发拖出。
                } else if let Some(row_path) = self.state.active().display_entry(index).map(|e| e.path.clone()) {
                    let tab = self.state.active();
                    let paths = if tab.selection.contains(&row_path) && tab.selection.len() > 1 {
                        tab.selected_paths()
                    } else if tab.selection.contains(&row_path) {
                        vec![row_path]
                    } else {
                        vec![row_path]
                    };
                    self.drag_pending = Some((x, y, paths));
                    // 上膛：禁用 flick 交互滚动，使行拖动仅用于发起拖出。
                    self.ui.invoke_set_drag_armed(true);
                }
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

        // 原生焦点框高亮（仅 Windows）：DrawFocusRect 对同一矩形二次绘制即为自擦除。
        #[cfg(windows)]
        let hwnd: isize = {
            use raw_window_handle::RawWindowHandle;
            if let Ok(h) = raw_window_handle::HasWindowHandle::window_handle(&self.ui.window().window_handle()) {
                if let RawWindowHandle::Win32(win) = h.as_raw() {
                    win.hwnd.get() as _
                } else {
                    0
                }
            } else {
                0
            }
        };

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
        let last_idx: std::rc::Rc<std::cell::Cell<i32>> = std::rc::Rc::new(std::cell::Cell::new(-1));

        let outcome = crate::sys::dnd::begin_drag_out(&paths, {
            #[cfg(windows)]
            let last_highlight_rect = last_highlight_rect.clone();
            #[cfg(windows)]
            let last_idx = last_idx.clone();
            move || {
                #[cfg(windows)]
                if let Some(ui) = ui_weak.upgrade() {
                    #[cfg(windows)]
                    if let Some((lx, ly)) = cursor_window_logical(&ui, scale) {
                        let idx = ui.invoke_row_hit_test(lx, ly);

                        if idx != last_idx.get() {
                            last_idx.set(idx);

                            // 1. 擦除上一个高亮框
                            if let Some(old_rect) = last_highlight_rect.borrow_mut().take() {
                                if hwnd != 0 {
                                    unsafe { draw_focus(hwnd, &old_rect) };
                                }
                            }

                            // 2. 在当前悬停项绘制原生高亮焦点框
                            if idx >= 0 && hwnd != 0 {
                                let rect_arr = ui.invoke_get_item_rect(idx);
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

        self.ui.set_file_drop_hover_index(-1);
        self.ui.window().request_redraw();
        if let crate::sys::dnd::DragOutOutcome::Dropped(_) = outcome {
            self.reload_active();
        }
    }

    /// 任务 2.2：处理外部或内部拖入。
    fn process_external_drop(&mut self) {
        let paths = std::mem::take(&mut self.drop_buffer);
        if paths.is_empty() || self.ui.get_settings_open() {
            return;
        }
        let Some(base) = self.state.active().path.clone() else {
            return;
        };

        // 光标（屏幕物理坐标）→ 窗口逻辑坐标 → 行命中
        let mut target = base.clone();
        #[cfg(windows)]
        {
            let scale = self.ui.window().scale_factor() as f32;
            if let Some((lx, ly)) = cursor_window_logical(&self.ui, scale) {
                let idx = self.ui.invoke_row_hit_test(lx, ly);
                if idx >= 0 {
                    if let Some(e) = self.state.active().display_entry(idx as usize) {
                        if e.is_dir {
                            target = e.path.clone();
                        }
                    }
                }
            }
        }

        // 1. 如果源文件本来就在目标目录下，说明是同级空白处误放，直接忽略（防止自我移动）
        if paths.iter().all(|p| p.parent() == Some(&target)) {
            return;
        }

        // 2. 严禁将文件夹移动到自身或自身的子文件夹中
        if paths.iter().any(|p| p == &target || target.starts_with(p)) {
            self.set_status_job("无法将文件夹移动到自身或其子文件夹中");
            return;
        }

        let ctrl = key_down(VK_CONTROL);
        let shift = key_down(VK_SHIFT);
        let is_move = if ctrl {
            false
        } else if shift {
            true
        } else {
            same_volume(&paths[0], &target)
        };
        let kind = if is_move {
            JobKind::Move { sources: paths, dest_dir: target }
        } else {
            JobKind::Copy { sources: paths, dest_dir: target }
        };
        self.submit_job(kind);
    }

    /// 任务 3.1：Space 触发的 QuickLook 预览开关。
    fn toggle_preview(&mut self) {
        if self.preview.window().is_visible() {
            self.close_preview();
            return;
        }
        let Some(path) = self.state.active().selected_paths().into_iter().next() else {
            self.set_status_job("请先选中一项以预览");
            return;
        };
        self.load_preview(&path);
    }

    fn close_preview(&mut self) {
        let _ = self.preview.window().hide();
        focus_window(&self.ui);
    }

    /// 显示预览窗口：居中到主窗口并以 show() 显示（hide 后再 show 可重新显示）。
    fn show_preview_window(&mut self) {
        let pos = self.ui.window().position();
        let size = self.ui.window().size();
        let pw = 720i32;
        let ph = 540i32;
        let cx = pos.x + ((size.width as i32) - pw) / 2;
        let cy = pos.y + ((size.height as i32) - ph) / 2;
        self.preview
            .window()
            .set_position(slint::PhysicalPosition { x: cx, y: cy });
        let _ = self.preview.window().show();
        self.preview_shown = true;
    }

    /// 按类型加载预览内容（图片/视频/文本/元数据）。
    fn load_preview(&mut self, path: &Path) {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        self.preview.set_title_text(name.into());
        self.preview.set_sub_title(path.to_string_lossy().into_owned().into());

        let ext = path
            .extension()
            .map(|e| e.to_string_lossy().to_ascii_lowercase())
            .unwrap_or_default();

        let is_image = matches!(
            ext.as_str(),
            "jpg" | "jpeg" | "png" | "gif" | "webp" | "bmp" | "tif" | "tiff" | "ico"
        );
        let is_video = matches!(
            ext.as_str(),
            "mp4" | "mkv" | "avi" | "mov" | "webm" | "wmv" | "m4v" | "mpg" | "mpeg"
        );
        let is_text = matches!(
            ext.as_str(),
            "txt" | "md" | "rs" | "py" | "js" | "ts" | "json" | "toml" | "yaml" | "yml"
                | "xml" | "html" | "htm" | "css" | "c" | "cpp" | "h" | "hpp" | "java" | "go"
                | "log" | "ini" | "bat" | "ps1" | "sh"
        );

        if is_image || is_video {
            // 先显示元数据兜底，图片异步加载完成后切换为图片视图。
            self.set_preview_meta(path, is_video);
            #[cfg(windows)]
            self.start_preview_image(path.to_path_buf());
            self.show_preview_window();
        } else if is_text {
            match std::fs::read(&path) {
                Ok(bytes) => {
                    let text = String::from_utf8_lossy(&bytes[..bytes.len().min(200_000)]).into_owned();
                    self.preview.set_text_content(text.into());
                    self.preview.set_preview_kind(1);
                    self.show_preview_window();
                }
                Err(e) => {
                    self.preview.set_meta_text(format!("无法读取：{e}").into());
                    self.preview.set_preview_kind(2);
                    self.show_preview_window();
                }
            }
        } else {
            self.set_preview_meta(path, false);
            self.show_preview_window();
        }
    }

    /// 构造元数据预览文本（文件信息卡片）。
    fn set_preview_meta(&mut self, path: &Path, is_video: bool) {
        let meta = std::fs::symlink_metadata(path);
        let is_dir = meta.as_ref().map(|m| m.is_dir()).unwrap_or(false);
        let size = meta.as_ref().map(|m| m.len()).unwrap_or(0);
        let modified = meta.as_ref().ok().and_then(|m| m.modified().ok());
        let kind = if is_dir {
            "文件夹".to_string()
        } else if is_video {
            "视频文件".to_string()
        } else {
            path.extension()
                .map(|e| format!("{} 文件", e.to_string_lossy().to_uppercase()))
                .unwrap_or_else(|| "文件".to_string())
        };
        let mut lines = vec![
            format!("类型：{kind}"),
            format!("路径：{}", path.to_string_lossy()),
        ];
        if !is_dir {
            lines.push(format!("大小：{}", crate::fs::format_size_bytes(size)));
        }
        if let Some(t) = modified {
            lines.push(format!("修改时间：{}", crate::fs::format_time(Some(t))));
        }
        self.preview.set_meta_text(lines.join("\n").into());
        self.preview.set_preview_kind(2);
    }

    /// 任务 3.1：异步加载大尺寸缩略图（图片/视频首帧）并切换到图片视图。
    /// 仅在 Windows 可用（依赖 Shell 缩略图管线），其余平台仅显示元数据兜底。
    #[cfg(windows)]
    fn start_preview_image(&mut self, path: PathBuf) {
        let preview_weak = slint::ComponentHandle::as_weak(&self.preview);
        std::thread::spawn(move || {
            unsafe {
                use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
                let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
            }
            match crate::ops::thumbs::get_thumbnail(&path, 640) {
                Ok((w, h, rgba)) => {
                    // 仅把可发送的 RGBA 字节送回 UI 线程，Image 在事件循环内构造
                    // （slint::Image 含裸指针，不可跨线程）。
                    let path2 = path.clone();
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(preview) = preview_weak.upgrade() {
                            if let Some(img) = to_slint_image(w, h, &rgba) {
                                preview.set_preview_image(img);
                                preview.set_preview_kind(0);
                                preview.set_sub_title(
                                    format!(
                                        "{} · {}",
                                        path2.to_string_lossy(),
                                        crate::fs::format_size_bytes(
                                            std::fs::metadata(&path2).map(|m| m.len()).unwrap_or(0)
                                        )
                                    )
                                    .into(),
                                );
                            }
                        }
                    });
                }
                Err(_) => {}
            }
        });
    }

    fn background_pointer_event(&mut self, event: &PointerEvent, x: f32, y: f32) {
        if event.kind != PointerEventKind::Down {
            return;
        }
        // 地址栏编辑中：点击空白处仅取消编辑。
        if self.ui.get_path_editing() {
            self.ui.set_path_editing(false);
            return;
        }
        match event.button {
            PointerEventButton::Left => {
                let tab = self.state.active_mut();
                tab.selection.clear();
                tab.anchor = None;
                self.visible_cache = None;
                self.refresh_visible_rows();
                self.sync_status();
            }
            PointerEventButton::Right => {
                let target = MenuTarget::Background;
                self.menu_target = Some(target.clone());
                self.open_menu(target, x, y);
            }
            _ => {}
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
                let mut app = app.borrow_mut();
                while let Ok(result) = app.list_rx.try_recv() {
                    app.apply_list_result(result);
                }
                while let Ok(result) = app.deep_rx.try_recv() {
                    app.apply_deep_result(result);
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
                let running_text = status_text(&status);
                if app.ui.get_status_job().is_empty() || !running_text.is_empty() {
                    app.ui.set_status_job(running_text.clone().into());
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
    {
        let timer_app = Rc::downgrade(app);
        app.borrow()
            .refresh_timer
            .start(TimerMode::Repeated, Duration::from_secs(15), move || {
                if let Some(app) = timer_app.upgrade() {
                    app.borrow_mut().sync_sidebar();
                }
            });
    }
}

/// 窗口原生键盘事件：应用级快捷键（非模态，仅常规加速键）。
fn setup_keyboard(app: &Rc<RefCell<App>>) {
    use slint::winit_030::{EventResult, WinitWindowAccessor};
    use slint::winit_030::winit::event::{ElementState, KeyEvent, WindowEvent};
    use slint::winit_030::winit::keyboard::Key;

    let key_app = Rc::downgrade(app);
    app.borrow()
        .ui
        .window()
        .on_winit_window_event(move |_window, event| {
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
        // 焦点在过滤框 / 重命名输入框时，按键归输入框所有。
        if app.ui.get_filter_has_focus() || !app.ui.get_rename_path().is_empty() {
            return EventResult::Propagate;
        }
        // 菜单打开时只处理 Escape。
        if app.ui.get_menu_open() {
            if matches!(logical_key, Key::Named(slint::winit_030::winit::keyboard::NamedKey::Escape)) {
                app.close_menu();
                return EventResult::Propagate;
            }
            return EventResult::Propagate;
        }

        let ctrl = key_down(VK_CONTROL);
        let shift = key_down(VK_SHIFT);
        match logical_key {
            Key::Named(named) => {
                use slint::winit_030::winit::keyboard::NamedKey;
                match named {
                    NamedKey::Space => {
                        // 任务 3.1：QuickLook 预览开关。
                        app.toggle_preview();
                    }
                    NamedKey::F2 => {
                        app.start_rename();
                    }
                    NamedKey::F5 => {
                        app.reload_active();
                    }
                    NamedKey::Enter => {
                        app.open_selection();
                    }
                    NamedKey::Delete => {
                        if shift {
                            app.delete_selection(false);
                        } else {
                            app.delete_selection(true);
                        }
                    }
                    NamedKey::Escape => {
                        app.ui.set_filter_text(SharedString::default());
                        {
                            let tab = app.state.active_mut();
                            tab.filter.clear();
                        }
                        app.start_deep_search();
                        app.state.active_mut().rebuild_display();
                        app.sync_file_list();
                        app.sync_status();
                    }
                    _ => return EventResult::Propagate,
                }
                let _ = ctrl;
            }
            Key::Character(ch) => {
                let ch = ch.to_lowercase();
                if ctrl {
                    match ch.as_str() {
                        "c" => app.copy_selection(false),
                        "x" => app.copy_selection(true),
                        "v" => app.paste_clipboard(),
                        "a" => app.select_all(),
                        _ => return EventResult::Propagate,
                    }
                } else {
                    return EventResult::Propagate;
                }
            }
            _ => return EventResult::Propagate,
        }
        EventResult::Propagate
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
            let current = app.borrow().state.active().path.clone();
            app.borrow_mut().state.new_tab(current);
            let mut app = app.borrow_mut();
            app.sync_active_tab();
            app.reload_active();
        });
    }
    {
        let app = Rc::downgrade(app);
        upgrade().on_tab_activated(move |id| {
            let Some(app) = app.upgrade() else { return };
            let mut app = app.borrow_mut();
            if let Some(index) = app.state.tab_by_id(id as u32) {
                app.state.active_tab = index;
                app.sync_active_tab();
                // 标签内容已在状态中，直接展示；若正在加载则保持 loading。
                if app.state.active().loading {
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
        upgrade().on_sidebar_item_activated(move |id| {
            let Some(app) = app.upgrade() else { return };
            app.borrow_mut().sidebar_activated(&id);
        });
    }

    // ---- 文件列表 ----
    {
        let app = Rc::downgrade(app);
        upgrade().on_file_row_pointer_event(move |index, event, x, y| {
            let Some(app) = app.upgrade() else { return };
            app.borrow_mut()
                .row_pointer_event(index as usize, &event, x, y);
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
        upgrade().on_file_scroll_changed(move |offset| {
            let Some(app) = app.upgrade() else { return };
            let mut app = app.borrow_mut();
            app.state.active_mut().scroll_y = offset;
            app.refresh_visible_rows();
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
        upgrade().on_file_rename_committed(move |path, new_name| {
            let Some(app) = app.upgrade() else { return };
            app.borrow_mut()
                .commit_rename(&PathBuf::from(path.as_str()), new_name.as_str());
        });
    }
    {
        let app = Rc::downgrade(app);
        upgrade().on_file_rename_cancelled(move || {
            let Some(app) = app.upgrade() else { return };
            app.borrow().ui.set_rename_path(SharedString::default());
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

    // ---- 右键菜单 ----
    {
        let app = Rc::downgrade(app);
        upgrade().on_menu_item_triggered(move |id| {
            let Some(app) = app.upgrade() else { return };
            app.borrow_mut().run_menu_action(&id);
        });
    }
    {
        let app = Rc::downgrade(app);
        upgrade().on_menu_submenu_hovered(move |index, _entered| {
            let Some(app) = app.upgrade() else { return };
            let app = app.borrow_mut();
            if index < 0 {
                app.ui.set_submenu_items(ModelRc::new(VecModel::from(Vec::<MenuItem>::new())));
                app.ui.set_submenu_row_y(0.0);
                return;
            }
            // 二级菜单顶部与悬停项对齐:累计该项之前的行高(含菜单 padding-top)。
            {
                let model = app.ui.get_menu_items();
                let mut row_y = 4.0f32;
                for i in 0..(index as usize).min(model.row_count()) {
                    if let Some(it) = model.row_data(i) {
                        row_y += if it.kind == "separator" { 9.0 } else { 28.0 };
                    }
                }
                app.ui.set_submenu_row_y(row_y);
            }
            // 仅当悬停的是带子菜单的项时展开（按 id 分发内容）。
            let hover_id = app
                .ui
                .get_menu_items()
                .row_data(index as usize)
                .filter(|item| item.kind == "submenu")
                .map(|item| item.id.to_string());
            if let Some(hover_id) = hover_id {
                let (items, height) = app.build_submenu_for(&hover_id);
                app.ui.set_submenu_items(ModelRc::new(VecModel::from(items)));
                app.ui.set_submenu_items_height(height);
            } else {
                app.ui.set_submenu_items(ModelRc::new(VecModel::from(Vec::<MenuItem>::new())));
            }
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
            app.reload_active();
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
            app.borrow_mut().open_settings_tab(crate::state::SettingsSubpage::ContextMenu);
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
                "duplicates" => crate::state::SettingsSubpage::Duplicates,
                "quicklook" => crate::state::SettingsSubpage::QuickLook,
                _ => crate::state::SettingsSubpage::General,
            };
            app.state.active_mut().settings_subpage = subpage;
            app.sync_settings();
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
            // 分组展开状态保存在 UI 模型里（重进设置页会重置），
            // 同时重算 Flickable 滚动范围。
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
            let mut content_h = 0.0f32;
            for i in 0..model.row_count() {
                if let Some(g) = model.row_data(i) {
                    content_h += if g.expanded {
                        44.0 + g.items.row_count() as f32 * 42.0
                    } else {
                        44.0
                    } + 12.0;
                }
            }
            app.ui.set_settings_content_height(content_h);
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
            app.ui.set_settings_editor_title("新建动作".into());
            app.ui.set_settings_editor_group_options(
                ModelRc::new(VecModel::from(group_options(&app.actions))),
            );
            app.ui.set_settings_editor_open(true);
            app.ui.set_settings_editor_id(format!("custom.{stamp}").into());
            app.ui.set_settings_editor_label("".into());
            app.ui.set_settings_editor_group("".into());
            app.ui.set_settings_editor_target("any".into());
            app.ui.set_settings_editor_extensions("".into());
            app.ui.set_settings_editor_program("".into());
            // 参数模板默认给 {target_path}，符合任务书约定。
            app.ui.set_settings_editor_args("{target_path}".into());
        });
    }
    {
        let app = Rc::downgrade(app);
        upgrade().on_settings_action_edit(move |id| {
            let Some(app) = app.upgrade() else { return };
            let app = app.borrow();
            let id = id.to_string();
            if let Some(a) = app.actions.iter().find(|a| a.id == id) {
                app.ui.set_settings_editor_title("编辑动作".into());
                // 层级归属候选：已有二级分组 + 内置「一级菜单」。
                app.ui.set_settings_editor_group_options(
                    ModelRc::new(VecModel::from(group_options(&app.actions))),
                );
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

    // ---- 关闭：保存配置并销毁会话（守护常驻） ----
    {
        let app = Rc::downgrade(app);
        upgrade().window().on_close_requested(move || {
            logln!("close requested");
            let Some(app) = app.upgrade() else {
                return slint::CloseRequestResponse::HideWindow;
            };
            let mut app = app.borrow_mut();
            // 注意：size() 是物理像素，配置里的窗口尺寸按逻辑像素保存，
            // 否则在高 DPI 下每次会话窗口都会按缩放系数膨胀，
            // 导致布局超出屏幕（状态栏、暂存托盘被挤到可视区外）。
            let window = app.ui.window();
            let scale = window.scale_factor() as f64;
            let size = window.size();
            app.config.window_width = (size.width as f64 / scale).round() as u32;
            app.config.window_height = (size.height as f64 / scale).round() as u32;
            app.config.save();
            drop(app);
            // 不能在关闭回调内同步销毁窗口（正在关闭中）；
            // 延迟到事件循环下一拍再整体 drop。
            schedule_destroy();
            slint::CloseRequestResponse::HideWindow
        });
    }
}

/// 延迟销毁会话：投递到事件循环下一拍执行（避免在回调栈内 drop 窗口）。
/// 用 `invoke_from_event_loop` 而非 Timer：最后一个窗口隐藏后
/// winit 后端会挂起事件循环，Timer 不再触发，而用户事件能唤醒它。
fn schedule_destroy() {
    let _ = slint::invoke_from_event_loop(destroy_session);
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

// 活跃 UI 会话槽：None 表示 UI 已销毁、仅剩守护。
// 存放于事件循环线程的 thread_local —— 管道线程发来的唤醒请求经
// `invoke_from_event_loop`（空捕获、可跨线程）在本线程执行时访问。
thread_local! {
    static SESSION: RefCell<Option<Rc<RefCell<App>>>> = const { RefCell::new(None) };
}

pub fn run() {
    logln!("starting, args: {:?}", std::env::args().collect::<Vec<_>>());

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

/// 任务 3.1：预览窗口的 Esc/Space 关闭 + 焦点归还主窗口。
fn setup_preview_keys(app: &Rc<RefCell<App>>) {
    use slint::winit_030::{
        EventResult, WinitWindowAccessor,
    };
    use slint::winit_030::winit::event::{ElementState, WindowEvent};
    use slint::winit_030::winit::keyboard::Key;

    let weak = Rc::downgrade(app);
    app.borrow()
        .preview
        .window()
        .on_winit_window_event(move |_window, event| {
            let WindowEvent::KeyboardInput {
                event:
                    slint::winit_030::winit::event::KeyEvent {
                        logical_key,
                        state: ElementState::Pressed,
                        ..
                    },
                ..
            } = event
            else {
                return EventResult::Propagate;
            };
            let Some(app) = weak.upgrade() else {
                return EventResult::Propagate;
            };
            let close = match logical_key {
                Key::Named(slint::winit_030::winit::keyboard::NamedKey::Escape) => true,
                Key::Named(slint::winit_030::winit::keyboard::NamedKey::Space) => true,
                _ => false,
            };
            if close {
                app.borrow_mut().close_preview();
            }
            EventResult::Propagate
        });
}

/// 创建一个 UI 会话（窗口 + 状态 + 回调 + 定时器）。
/// 回调/定时器均以 Weak 持有 App，销毁时整图释放。
fn create_session(initial_path: Option<PathBuf>, jobs: JobManager) {
    let config = Config::load();

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
    if config.window_width >= 480 && config.window_height >= 360 {
        // 钳制到工作区：避免窗口高于屏幕时，底部的暂存托盘与状态栏
        // 被裁掉（WSLg 缩放环境下尤其明显）。
        let (mut w, mut h) = (config.window_width as f32, config.window_height as f32);
        if let Some((aw, ah)) = work_area_logical(ui.window().scale_factor()) {
            w = w.min(aw).max(480.0);
            h = h.min(ah - 60.0).max(360.0); // 留出标题栏
        }
        ui.window().set_size(WindowSize::Logical(LogicalSize::new(w, h)));
    }
    ui.set_staging_expanded(config.staging_expanded);
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
    let (thumb_tx, thumb_rx) = mpsc::channel::<(PathBuf, u32)>();
    #[cfg(windows)]
    spawn_thumb_worker(thumb_rx);
    #[cfg(not(windows))]
    drop(thumb_rx);

    let app = Rc::new(RefCell::new(App::new(
        ui,
        config,
        jobs,
        list_tx,
        list_rx,
        deep_tx,
        deep_rx,
        thumb_tx,
    )));
    {
        let raw = crate::ops::actions::load_or_create();
        app.borrow_mut().actions = crate::ops::actions::heal_full(&raw);
    }

    // 先入槽再接线：会话槽持有唯一强引用，回调/定时器均为 Weak；
    // 漏掉此行会导致 App 在本函数返回时即被释放（列表收不到数据）。
    SESSION.with(|slot| *slot.borrow_mut() = Some(app.clone()));
    wire_callbacks(&app);
    setup_timers(&app);
    setup_keyboard(&app);
    setup_preview_keys(&app);

    // 初始数据。
    {
        let mut app = app.borrow_mut();
        app.state.recent = app.config.recent.clone();
        if let Some(path) = initial_path {
            app.state.new_tab(Some(path));
        }
        app.sync_tabs();
        app.sync_nav();
        app.sync_breadcrumbs();
        app.sync_status();
        app.sync_sidebar();
        app.sync_staging();
        app.sync_file_list();
        app.reload_active();

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
    app.borrow().ui.show().unwrap();
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
    // 显式隐藏后整体 drop：回调均为 Weak，无强引用环，
    // Slint 组件树与 winit 窗口（GPU 纹理、表面）随之释放。
    let ui = app.borrow().ui.as_weak();
    if let Some(w) = ui.upgrade() {
        let _ = w.hide();
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
