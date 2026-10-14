//! 应用控制器：标签模型、导航、侧边栏、搜索、菜单与设置页。

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

use nucleo_matcher::Matcher;
use slint::{ComponentHandle, Model, ModelRc, VecModel};

use crate::actions::{self, Action, Filter, NodeKind, TargetKind};
use crate::archive_sync;
use crate::browse;
#[cfg(windows)]
use crate::drives;
use crate::matcher::{self, MatchEntry, MatcherScratch};
use crate::ops;
use crate::platform;
use crate::preview;
use crate::settings::{self, Settings};
use crate::shortcut;
use crate::staging::Staging;
use crate::thumbnails;

use crate::index::SearchHit;
#[cfg(windows)]
use crate::index::{self, IndexService};

slint::include_modules!();

// 预览 / 对比窗口由独立 .slint 生成
include!(concat!(env!("OUT_DIR"), "/preview.rs"));
include!(concat!(env!("OUT_DIR"), "/diff.rs"));

// ---- 菜单动作 ID ----
const ACTION_OPEN: i32 = 1;
const ACTION_OPEN_WITH: i32 = 2;
const ACTION_COPY: i32 = 3;
const ACTION_CUT: i32 = 4;
const ACTION_PASTE: i32 = 5;
const ACTION_RENAME: i32 = 6;
const ACTION_DELETE: i32 = 7;
const ACTION_TERMINAL: i32 = 8;
const ACTION_SHORTCUT: i32 = 9;
const SUBMENU_SEND_TO: i32 = 10;
const ACTION_SEND_DESKTOP: i32 = 20;
const ACTION_SEND_START: i32 = 21;
const ACTION_STAGE: i32 = 22;
const ACTION_DIFF: i32 = 23;
const ACTION_OPEN_LOCATION: i32 = 24;
/// 自定义动作（actions.toml）的菜单 ID 基数
const CUSTOM_BASE: i32 = 1000;

/// 设置标签固定 ID
const SETTINGS_TAB_ID: i32 = -1;

/// 搜索作用域
const SCOPE_DIR_NAME: i32 = 0;
const SCOPE_DIR_CONTENT: i32 = 1;
const SCOPE_GLOBAL: i32 = 2;

/// 应用级命令
#[derive(Debug, Clone)]
pub enum Command {
    Open(PathBuf),
    ShowWindow,
    ShowSettings,
    Quit,
}

thread_local! {
    static APP: RefCell<App> = RefCell::new(App::new());
}

fn with_app<R>(f: impl FnOnce(&mut App) -> R) -> R {
    APP.with(|app| f(&mut app.borrow_mut()))
}

pub fn handle(command: Command) {
    with_app(|app| app.on_command(command));
}

pub fn post(command: Command) {
    std::thread::spawn(move || {
        for _ in 0..40 {
            let command = command.clone();
            if slint::invoke_from_event_loop(move || handle(command)).is_ok() {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    });
}

/// 标签类型
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TabKind {
    Explorer,
    Settings,
}

struct Tab {
    id: i32,
    kind: TabKind,
    path: PathBuf,
    /// 分栏右栏路径
    path2: PathBuf,
    history: Vec<PathBuf>,
    history_index: usize,
    selected: usize,
    selected2: usize,
    entries: Vec<browse::Entry>,
    entries2: Vec<browse::Entry>,
}

impl Tab {
    fn new(id: i32, path: PathBuf) -> Self {
        Self {
            id,
            kind: TabKind::Explorer,
            history: vec![path.clone()],
            history_index: 0,
            path,
            path2: default_path(),
            selected: usize::MAX,
            selected2: usize::MAX,
            entries: Vec::new(),
            entries2: Vec::new(),
        }
    }

    fn settings(id: i32) -> Self {
        Self {
            id,
            kind: TabKind::Settings,
            path: default_path(),
            path2: default_path(),
            history: Vec::new(),
            history_index: 0,
            selected: usize::MAX,
            selected2: usize::MAX,
            entries: Vec::new(),
            entries2: Vec::new(),
        }
    }

    fn is_settings(&self) -> bool {
        self.kind == TabKind::Settings
    }
}

pub struct App {
    window: Option<MainWindow>,
    tabs: Vec<Tab>,
    active: usize,
    next_tab_id: i32,
    settings_page: i32,

    settings: Settings,
    clipboard: Vec<PathBuf>,
    clipboard_cut: bool,

    action_store: actions::Store,
    menu_custom: Vec<Action>,
    menu_submenu: HashMap<String, Vec<i32>>,

    preview_window: Option<PreviewWindow>,
    diff_window: Option<DiffWindow>,

    #[cfg(windows)]
    index: Option<IndexService>,
    search_active: bool,
    search_hits: Vec<SearchHit>,
    search_query: String,
    search_scope: i32,
    sort_mode: i32,

    staging: Staging,
    staging_visible: bool,
    network_expanded: bool,

    matcher: RefCell<Matcher>,
    matcher_scratch: RefCell<MatcherScratch>,

    thumb_cache: Rc<RefCell<HashMap<PathBuf, slint::Image>>>,
    entries_model: Rc<VecModel<FileEntry>>,
    entries2_model: Rc<VecModel<FileEntry>>,
    thumb_timer: Option<slint::Timer>,
}

impl App {
    fn new() -> Self {
        let first = Tab::new(1, default_path());
        Self {
            window: None,
            tabs: vec![first],
            active: 0,
            next_tab_id: 2,
            settings_page: 0,
            settings: settings::load(),
            clipboard: Vec::new(),
            clipboard_cut: false,
            action_store: archive_sync::store(),
            menu_custom: Vec::new(),
            menu_submenu: HashMap::new(),
            preview_window: None,
            diff_window: None,
            #[cfg(windows)]
            index: Some(IndexService::start()),
            search_active: false,
            search_hits: Vec::new(),
            search_query: String::new(),
            search_scope: SCOPE_DIR_NAME,
            sort_mode: 0,
            staging: Staging::new(),
            staging_visible: true,
            network_expanded: false,
            matcher: RefCell::new(matcher::new_matcher()),
            matcher_scratch: RefCell::new(MatcherScratch::default()),
            thumb_cache: Rc::new(RefCell::new(HashMap::new())),
            entries_model: Rc::new(VecModel::default()),
            entries2_model: Rc::new(VecModel::default()),
            thumb_timer: None,
        }
    }

    // ---------------------------------------------------------------- 命令

    fn on_command(&mut self, command: Command) {
        match command {
            Command::Open(path) => {
                if path.is_dir() {
                    self.navigate_to(&path);
                } else if path.exists() {
                    if let Some(parent) = browse::parent_dir(&path) {
                        self.navigate_to(&parent);
                    }
                    if let Err(e) = platform::shell_open(&path) {
                        self.set_status(&format!("Open failed: {e}"));
                    }
                } else {
                    self.set_status(&format!("Path not found: {}", path.display()));
                }
                self.ensure_window();
            }
            Command::ShowWindow => {
                if self.tabs.is_empty() {
                    self.tabs.push(Tab::new(self.next_tab_id, default_path()));
                    self.next_tab_id += 1;
                    self.active = 0;
                    self.reload();
                }
                self.ensure_window();
            }
            Command::ShowSettings => self.open_settings_tab(),
            Command::Quit => {
                if let Some(window) = self.window.as_ref() {
                    let _ = window.window().hide();
                }
                self.window = None;
                let _ = slint::quit_event_loop();
            }
        }
    }

    // ---------------------------------------------------------------- 窗口

    fn ensure_window(&mut self) {
        if self.window.is_none() {
            self.create_window();
        }
        if let Some(window) = self.window.as_ref() {
            let _ = window.show();
        }
    }

    fn create_window(&mut self) {
        let window = match MainWindow::new() {
            Ok(window) => window,
            Err(e) => {
                eprintln!("[rove] Slint window creation failed: {e}");
                return;
            }
        };

        // 导航
        window.on_navigate(|path| with_app(|app| app.navigate_to(Path::new(path.as_str()))));
        window.on_go_up(|| with_app(|app| app.go_up()));
        window.on_go_back(|| with_app(|app| app.go_back()));
        window.on_go_forward(|| with_app(|app| app.go_forward()));
        window.on_refresh(|| with_app(|app| app.reload()));

        // 标签
        window.on_new_tab(|| with_app(|app| app.new_tab()));
        window.on_close_tab(|index| with_app(|app| app.close_tab(index)));
        window.on_activate_tab(|index| with_app(|app| app.activate_tab(index)));
        window.on_open_settings_tab(|| with_app(|app| app.open_settings_tab()));
        window.on_settings_select_page(|page| with_app(|app| app.select_settings_page(page)));

        // 列表
        window.on_select_entry(|index, pane| with_app(|app| app.select_entry(index, pane)));
        window.on_open_entry(|index, pane| with_app(|app| app.open_entry(index, pane)));
        window.on_select_entry2(|index| with_app(|app| app.select_entry(index, true)));
        window.on_open_entry2(|index| with_app(|app| app.open_entry(index, true)));
        window.on_right_clicked(|index, x, y| with_app(|app| app.right_click(index, x, y)));
        window.on_rename_commit(|text| with_app(|app| app.commit_rename(text.as_str())));
        window.on_rename_cancel(|| with_app(|app| app.cancel_rename()));
        window.on_toggle_split(|| with_app(|app| app.toggle_split()));
        window.on_sort_by(|mode| with_app(|app| app.sort_by(mode)));

        // 工具条
        window.on_new_folder(|| with_app(|app| app.new_folder()));
        window.on_cut_selected(|| with_app(|app| app.clipboard_cut()));
        window.on_copy_selected(|| with_app(|app| app.clipboard_copy()));
        window.on_rename_selected(|| with_app(|app| app.begin_rename()));
        window.on_delete_selected(|| with_app(|app| app.delete_selected()));
        window.on_paste_here(|| with_app(|app| app.paste()));

        // 侧边栏 / 暂存盘
        window.on_side_activate(|id| with_app(|app| app.side_activate(&id)));
        window.on_toggle_network(|| with_app(|app| app.toggle_network()));
        window.on_stage_toggle(|| with_app(|app| app.stage_toggle()));
        window.on_stage_add(|| with_app(|app| app.stage_add()));
        window.on_stage_copy_here(|| with_app(|app| app.stage_copy_here()));
        window.on_stage_move_here(|| with_app(|app| app.stage_move_here()));
        window.on_stage_clear(|| with_app(|app| app.stage_clear()));
        window.on_stage_remove(|index| with_app(|app| app.stage_remove(index)));

        // 搜索
        window.on_search_changed(|text| with_app(|app| app.search_changed(text.as_str())));
        window.on_search_submit(|scope| with_app(|app| app.search_submit(scope)));
        window.on_search_focus(|| with_app(|app| app.search_focus()));
        window.on_close_search(|| with_app(|app| app.close_search()));

        // 菜单 / 预览 / 对比
        window.on_menu_action(|id| with_app(|app| app.menu_action(id)));
        window.on_menu_hover(|id| with_app(|app| app.menu_hover(id)));
        window.on_preview_selected(|| with_app(|app| app.preview_selected()));
        window.on_diff_selected(|| with_app(|app| app.diff_selected()));

        // 设置
        window.on_save_general(|| with_app(|app| app.save_general()));
        window.on_takeover_changed(|| with_app(|app| app.save_general()));
        window.on_select_action(|index| with_app(|app| app.settings_select_action(index)));
        window.on_toggle_action(|index, enabled| {
            with_app(|app| app.settings_toggle_action(index, enabled))
        });
        window.on_apply_edit(|| with_app(|app| app.settings_apply_edit()));
        window.on_add_action(|| with_app(|app| app.settings_add_action()));
        window.on_delete_action(|| with_app(|app| app.settings_delete_action()));

        window.set_tabs(ModelRc::new(VecModel::default()));
        window.set_entries(ModelRc::from(Rc::clone(&self.entries_model)));
        window.set_entries2(ModelRc::from(Rc::clone(&self.entries2_model)));

        self.start_thumbnail_timer();
        self.window = Some(window);
        self.reload();
        self.sync_ui();
    }

    fn destroy_window(&mut self) {
        if let Some(window) = self.window.as_ref() {
            let _ = window.window().hide();
        }
        self.window = None;
        self.thumb_timer = None;
        thumbnails::global().reset();
    }

    // ---------------------------------------------------------------- 标签

    fn tab(&self) -> &Tab {
        &self.tabs[self.active.min(self.tabs.len().saturating_sub(1))]
    }

    fn tab_mut(&mut self) -> &mut Tab {
        let index = self.active.min(self.tabs.len().saturating_sub(1));
        &mut self.tabs[index]
    }

    fn current_dir(&self) -> PathBuf {
        self.tab().path.clone()
    }

    fn new_tab(&mut self) {
        let path = self.current_dir();
        self.tabs.push(Tab::new(self.next_tab_id, path));
        self.next_tab_id += 1;
        self.active = self.tabs.len() - 1;
        self.reload();
    }

    fn close_tab(&mut self, index: i32) {
        let index = index as usize;
        if index >= self.tabs.len() {
            return;
        }
        self.tabs.remove(index);

        if self.tabs.is_empty() {
            if self.settings.destroy_ui_on_last_tab_close {
                self.destroy_window();
                return;
            }
            self.tabs.push(Tab::new(self.next_tab_id, default_path()));
            self.next_tab_id += 1;
        }
        if self.active >= self.tabs.len() {
            self.active = self.tabs.len() - 1;
        }
        self.reload();
    }

    fn activate_tab(&mut self, index: i32) {
        let index = index as usize;
        if index < self.tabs.len() {
            self.active = index;
            self.reload();
        }
    }

    /// 打开 / 切换到设置标签（全局唯一）
    fn open_settings_tab(&mut self) {
        if let Some(index) = self.tabs.iter().position(|tab| tab.is_settings()) {
            self.active = index;
            self.reload();
            return;
        }
        self.tabs.push(Tab::settings(SETTINGS_TAB_ID));
        self.active = self.tabs.len() - 1;
        self.ensure_window();
        self.reload();
    }

    fn select_settings_page(&mut self, page: i32) {
        self.settings_page = page;
        self.sync_ui();
    }

    // ---------------------------------------------------------------- 导航

    fn navigate_to(&mut self, path: &Path) {
        self.close_search();
        let path = path.to_path_buf();
        {
            let tab = self.tab_mut();
            tab.path = path;
            tab.selected = usize::MAX;
            tab.selected2 = usize::MAX;
            tab.history.truncate(tab.history_index + 1);
            tab.history.push(tab.path.clone());
            tab.history_index = tab.history.len() - 1;
        }
        self.reload();
    }

    fn go_up(&mut self) {
        let current = self.current_dir();
        if let Some(parent) = browse::parent_dir(&current) {
            self.navigate_to(&parent);
        }
    }

    fn go_back(&mut self) {
        let (target, ok) = {
            let tab = self.tab_mut();
            if tab.history_index > 0 {
                tab.history_index -= 1;
                (tab.history[tab.history_index].clone(), true)
            } else {
                (PathBuf::new(), false)
            }
        };
        if ok {
            self.close_search();
            self.tab_mut().path = target;
            self.tab_mut().selected = usize::MAX;
            self.reload();
        }
    }

    fn go_forward(&mut self) {
        let (target, ok) = {
            let tab = self.tab_mut();
            if tab.history_index + 1 < tab.history.len() {
                tab.history_index += 1;
                (tab.history[tab.history_index].clone(), true)
            } else {
                (PathBuf::new(), false)
            }
        };
        if ok {
            self.close_search();
            self.tab_mut().path = target;
            self.tab_mut().selected = usize::MAX;
            self.reload();
        }
    }

    fn toggle_split(&mut self) {
        self.settings.split_view = !self.settings.split_view;
        if let Some(window) = self.window.as_ref() {
            window.set_split_view(self.settings.split_view);
        }
        self.reload();
    }

    fn toggle_network(&mut self) {
        self.network_expanded = !self.network_expanded;
        self.sync_ui();
    }

    fn side_activate(&mut self, id: &str) {
        #[cfg(windows)]
        {
            let quick = drives::quick_access();
            match id {
                "desktop" => {
                    if let Some(path) = quick.desktop {
                        self.navigate_to(&path);
                    }
                }
                "downloads" => {
                    if let Some(path) = quick.downloads {
                        self.navigate_to(&path);
                    }
                }
                "recycle" => {
                    let _ = platform::shell_open(Path::new(&quick.recycle_bin));
                }
                "network" => {
                    let _ = platform::shell_open(Path::new(
                        "::{208D2C60-3AEA-1069-A2D7-08002B30309D}",
                    ));
                }
                other => {
                    let path = PathBuf::from(format!("{other}\\"));
                    self.navigate_to(&path);
                }
            }
        }
    }

    // ---------------------------------------------------------------- 列表

    fn reload(&mut self) {
        self.close_search();
        let (path, path2) = {
            let tab = self.tab();
            (tab.path.clone(), tab.path2.clone())
        };

        match browse::list(&path) {
            Ok(mut entries) => {
                sort_entries(&mut entries, self.sort_mode);
                self.tab_mut().entries = entries;
            }
            Err(e) => {
                self.tab_mut().entries.clear();
                self.set_status(&format!("Cannot read {}: {e}", path.display()));
            }
        }
        if self.settings.split_view {
            match browse::list(&path2) {
                Ok(mut entries) => {
                    sort_entries(&mut entries, self.sort_mode);
                    self.tab_mut().entries2 = entries;
                }
                Err(_) => self.tab_mut().entries2.clear(),
            }
        } else {
            self.tab_mut().entries2.clear();
        }

        self.sync_ui();
        self.request_thumbnails();
    }

    fn sort_by(&mut self, mode: i32) {
        self.sort_mode = mode;
        let (path, path2) = {
            let tab = self.tab();
            (tab.path.clone(), tab.path2.clone())
        };
        if let Ok(mut entries) = browse::list(&path) {
            sort_entries(&mut entries, mode);
            self.tab_mut().entries = entries;
        }
        if let Ok(mut entries) = browse::list(&path2) {
            sort_entries(&mut entries, mode);
            self.tab_mut().entries2 = entries;
        }
        self.sync_ui();
    }

    fn select_entry(&mut self, index: i32, pane2: bool) {
        let editing = self.rename_index();
        if editing != usize::MAX && index as usize != editing {
            self.cancel_rename();
        }
        let entries_len = if pane2 {
            self.tab().entries2.len()
        } else {
            self.tab().entries.len()
        };
        let selected = if (index as usize) < entries_len {
            index as usize
        } else {
            usize::MAX
        };
        // 状态隔离：仅更新 selected_index，绝不重建模型
        let tab = self.tab_mut();
        if pane2 {
            tab.selected2 = selected;
        } else {
            tab.selected = selected;
        }
        if let Some(window) = self.window.as_ref() {
            if pane2 {
                window.set_selected_index2(selected as i32);
            } else {
                window.set_selected_index(selected as i32);
            }
        }
    }

    fn open_entry(&mut self, index: i32, pane2: bool) {
        if self.search_active && !pane2 {
            if let Some(hit) = self.search_hits.get(index as usize) {
                let path = PathBuf::from(&hit.path);
                if hit.is_dir {
                    self.navigate_to(&path);
                } else if let Err(e) = platform::shell_open(&path) {
                    self.set_status(&format!("Open failed: {e}"));
                }
            }
            return;
        }
        let path = {
            let tab = self.tab();
            let entries = if pane2 { &tab.entries2 } else { &tab.entries };
            entries.get(index as usize).map(|entry| entry.path.clone())
        };
        let Some(path) = path else { return };
        if path.is_dir() {
            if pane2 {
                self.tab_mut().path2 = path;
                self.tab_mut().selected2 = usize::MAX;
                self.reload();
            } else {
                self.navigate_to(&path);
            }
        } else if let Err(e) = platform::shell_open(&path) {
            self.set_status(&format!("Open failed: {e}"));
        }
    }

    // ---------------------------------------------------------------- 工具条

    fn new_folder(&mut self) {
        let dir = self.current_dir();
        let mut candidate = dir.join("新建文件夹");
        let mut index = 2;
        while candidate.exists() {
            candidate = dir.join(format!("新建文件夹 ({index})"));
            index += 1;
        }
        match std::fs::create_dir(&candidate) {
            Ok(()) => {
                self.set_status("已创建文件夹");
                self.reload();
                // 选中新文件夹
                if let Some(position) = self
                    .tab()
                    .entries
                    .iter()
                    .position(|entry| entry.path == candidate)
                {
                    self.tab_mut().selected = position;
                    self.sync_ui();
                }
            }
            Err(e) => self.set_status(&format!("创建失败：{e}")),
        }
    }

    fn clipboard_copy(&mut self) {
        if let Some(path) = self.selected_path() {
            self.clipboard = vec![path];
            self.clipboard_cut = false;
            self.set_status("已复制");
        }
    }

    fn clipboard_cut(&mut self) {
        if let Some(path) = self.selected_path() {
            self.clipboard = vec![path];
            self.clipboard_cut = true;
            self.set_status("已剪切");
        }
    }

    fn begin_rename(&mut self) {
        if self.search_active {
            return;
        }
        let selected = self.tab().selected;
        if selected >= self.tab().entries.len() {
            return;
        }
        let name = self.tab().entries[selected].name.clone();
        if let Some(window) = self.window.as_ref() {
            window.set_rename_index(selected as i32);
            window.set_rename_text(name.into());
        }
    }

    fn delete_selected(&mut self) {
        let Some(path) = self.selected_path() else { return };
        match ops::recycle(&[path]) {
            Ok(()) => {
                self.tab_mut().selected = usize::MAX;
                self.set_status("已移到回收站");
                self.reload();
            }
            Err(e) => self.set_status(&format!("删除失败：{e}")),
        }
    }

    fn paste(&mut self) {
        if self.clipboard.is_empty() {
            return;
        }
        let dest = self.current_dir();
        let sources = self.clipboard.clone();
        let cut = self.clipboard_cut;
        let result = if cut {
            ops::move_to(&sources, &dest)
        } else {
            ops::copy(&sources, &dest)
        };
        match result {
            Ok(count) => {
                if cut {
                    self.clipboard.clear();
                }
                self.set_status(&format!("已粘贴 {count} 项"));
                self.reload();
            }
            Err(e) => self.set_status(&format!("粘贴失败：{e}")),
        }
    }

    // ---------------------------------------------------------------- 搜索

    fn search_focus(&mut self) {
        self.ensure_window();
        if let Some(window) = self.window.as_ref() {
            window.set_search_popup_open(true);
        }
        self.sync_suggestions();
    }

    fn search_changed(&mut self, text: &str) {
        self.search_query = text.to_string();
        if text.trim().is_empty() {
            self.close_search();
            return;
        }
        self.search_active = true;
        self.search_scope = SCOPE_DIR_NAME;
        if let Some(window) = self.window.as_ref() {
            window.set_search_active(true);
            window.set_search_popup_open(true);
            window.set_show_path_column(false);
        }
        self.run_current_dir_search();
        self.sync_suggestions();
    }

    fn search_submit(&mut self, scope: i32) {
        if self.search_query.trim().is_empty() {
            return;
        }
        self.search_scope = scope;
        self.search_active = true;
        if let Some(window) = self.window.as_ref() {
            window.set_search_active(true);
            window.set_search_popup_open(false);
            window.set_show_path_column(scope == SCOPE_GLOBAL || scope == SCOPE_DIR_CONTENT);
        }
        match scope {
            SCOPE_DIR_CONTENT => self.run_content_search(),
            SCOPE_GLOBAL => {
                self.set_status(&format!("搜索 \"{}\" · 全盘索引中…", self.search_query));
                #[cfg(windows)]
                if let Some(index) = &self.index {
                    index.search(&self.search_query);
                }
            }
            _ => self.run_current_dir_search(),
        }
    }

    fn close_search(&mut self) {
        if !self.search_active {
            return;
        }
        self.search_active = false;
        self.search_hits.clear();
        self.search_query.clear();
        if let Some(window) = self.window.as_ref() {
            window.set_search_active(false);
            window.set_search_popup_open(false);
            window.set_show_path_column(false);
            window.set_search_text(String::new().into());
            window.set_search_count(String::new().into());
        }
        self.sync_ui();
    }

    /// 当前目录按名称搜索（同步，带高亮）
    fn run_current_dir_search(&mut self) {
        let query = self.search_query.clone();
        let entries = self.tab().entries.clone();
        let matched: Vec<browse::Entry> = entries
            .into_iter()
            .filter(|entry| {
                let candidate = MatchEntry::new(&entry.name);
                matcher::matches(
                    &candidate,
                    &query,
                    &mut self.matcher.borrow_mut(),
                    &mut self.matcher_scratch.borrow_mut(),
                )
            })
            .collect();
        self.search_hits = matched
            .iter()
            .map(|entry| SearchHit {
                name: entry.name.clone(),
                path: entry.path.display().to_string(),
                is_dir: entry.is_dir,
                size: entry.size,
            })
            .collect();
        self.search_active = true;
        self.request_hit_thumbnails();
        self.sync_ui();
        self.set_status(&format!(
            "搜索 \"{}\" · {} 项",
            self.search_query,
            self.search_hits.len()
        ));
    }

    /// 当前目录按文件内容搜索（仅文本类文件，限 1MB）
    fn run_content_search(&mut self) {
        let query = self.search_query.clone();
        if query.trim().is_empty() {
            return;
        }
        let entries = self.tab().entries.clone();
        let mut hits = Vec::new();
        for entry in entries {
            if entry.is_dir || entry.size > 1024 * 1024 {
                continue;
            }
            let Ok(bytes) = std::fs::read(&entry.path) else {
                continue;
            };
            let text = String::from_utf8_lossy(&bytes);
            if text.to_lowercase().contains(&query.to_lowercase()) {
                hits.push(SearchHit {
                    name: entry.name.clone(),
                    path: entry.path.display().to_string(),
                    is_dir: false,
                    size: entry.size,
                });
            }
        }
        self.search_hits = hits;
        self.search_active = true;
        self.request_hit_thumbnails();
        self.sync_ui();
        self.set_status(&format!(
            "搜索 \"{}\" 内容 · {} 项",
            self.search_query,
            self.search_hits.len()
        ));
    }

    fn sync_suggestions(&mut self) {
        let query = self.search_query.trim().to_string();
        if query.is_empty() {
            if let Some(window) = self.window.as_ref() {
                window.set_search_suggestions(ModelRc::new(VecModel::from(Vec::<
                    SearchSuggestion,
                >::new())));
            }
            return;
        }
        let dir_name = self
            .current_dir()
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| self.current_dir().display().to_string());

        let suggestions = vec![
            SearchSuggestion {
                id: SCOPE_DIR_NAME,
                label: format!("在 {dir_name} 中搜索名称 \"{query}\"").into(),
                hint: "Enter".into(),
            },
            SearchSuggestion {
                id: SCOPE_DIR_CONTENT,
                label: format!("在 {dir_name} 中搜索文件内容 \"{query}\"").into(),
                hint: "Alt+Enter".into(),
            },
            SearchSuggestion {
                id: SCOPE_GLOBAL,
                label: format!("在所有已索引位置中搜索 \"{query}\"").into(),
                hint: "Shift+Enter".into(),
            },
        ];
        if let Some(window) = self.window.as_ref() {
            window.set_search_suggestions(ModelRc::new(VecModel::from(suggestions)));
        }
    }

    /// 定时器回调：取回全盘索引结果
    fn apply_search_results(&mut self, hits: Vec<SearchHit>) {
        self.search_hits = hits;
        self.request_hit_thumbnails();
        self.sync_ui();
        self.set_status(&format!(
            "搜索 \"{}\" · {} 项",
            self.search_query,
            self.search_hits.len()
        ));
    }

    // ---------------------------------------------------------------- 选中项

    fn selected_path(&self) -> Option<PathBuf> {
        if self.search_active {
            return self
                .search_hits
                .get(self.tab().selected)
                .map(|hit| PathBuf::from(&hit.path));
        }
        self.tab()
            .entries
            .get(self.tab().selected)
            .map(|entry| entry.path.clone())
    }

    fn selected_is_dir(&self) -> bool {
        if self.search_active {
            return self
                .search_hits
                .get(self.tab().selected)
                .map(|hit| hit.is_dir)
                .unwrap_or(false);
        }
        self.tab()
            .entries
            .get(self.tab().selected)
            .map(|entry| entry.is_dir)
            .unwrap_or(false)
    }

    // ---------------------------------------------------------------- 右键菜单

    fn rename_index(&self) -> usize {
        self.window
            .as_ref()
            .map(|window| window.get_rename_index())
            .filter(|index| *index >= 0)
            .map(|index| index as usize)
            .unwrap_or(usize::MAX)
    }

    fn right_click(&mut self, index: i32, x: f32, y: f32) {
        if index >= 0 && !self.search_active {
            let entries_len = self.tab().entries.len();
            if (index as usize) < entries_len {
                self.tab_mut().selected = index as usize;
                if let Some(window) = self.window.as_ref() {
                    window.set_selected_index(index);
                }
            }
        }
        self.sync_ui();

        let (items, sub_items) = self.build_menu();
        let Some(window) = self.window.as_ref() else {
            return;
        };
        window.set_menu_items(ModelRc::new(VecModel::from(items)));
        window.set_menu_sub_items(ModelRc::new(VecModel::from(sub_items)));
        window.set_menu_sub_parent(-1);
        window.set_menu_x(x);
        window.set_menu_y(y);
        window.set_menu_show_trigger(window.get_menu_show_trigger() + 1);
    }

    fn close_context_menu(&mut self) {
        if let Some(window) = self.window.as_ref() {
            window.set_menu_sub_parent(-1);
            window.set_menu_hide_trigger(window.get_menu_hide_trigger() + 1);
        }
    }

    fn menu_hover(&mut self, id: i32) {
        let Some(window) = self.window.as_ref() else {
            return;
        };
        if id >= CUSTOM_BASE {
            if let Some(action) = self.menu_custom.get((id - CUSTOM_BASE) as usize).cloned() {
                if action.is_submenu() {
                    let children = self
                        .menu_submenu
                        .get(&action.id)
                        .cloned()
                        .unwrap_or_default();
                    let nodes: Vec<MenuNode> = children
                        .iter()
                        .filter_map(|&menu_id| {
                            let index = (menu_id - CUSTOM_BASE) as usize;
                            self.menu_custom.get(index).map(|child| (menu_id, child.clone()))
                        })
                        .map(|(menu_id, child)| MenuNode {
                            id: menu_id,
                            label: child.label.clone().into(),
                            kind: "item".into(),
                            enabled: true,
                        })
                        .collect();
                    window.set_menu_sub_items(ModelRc::new(VecModel::from(nodes)));
                    window.set_menu_sub_parent(id);
                    return;
                }
            }
            window.set_menu_sub_parent(-1);
            return;
        }
        window.set_menu_sub_parent(if id == SUBMENU_SEND_TO { id } else { -1 });
    }

    fn build_menu(&mut self) -> (Vec<MenuNode>, Vec<MenuNode>) {
        let separator = || MenuNode {
            id: 0,
            label: String::new().into(),
            kind: "separator".into(),
            enabled: false,
        };
        let item = |id: i32, label: &str| MenuNode {
            id,
            label: label.into(),
            kind: "item".into(),
            enabled: true,
        };

        let sub_items = vec![
            item(ACTION_SEND_DESKTOP, "桌面快捷方式"),
            item(ACTION_SEND_START, "开始菜单快捷方式"),
        ];

        let selection = self.selected_path();
        let is_dir = self.selected_is_dir();
        let mut items = Vec::new();

        if selection.is_some() {
            items.push(item(ACTION_OPEN, "打开"));
            if is_dir {
                items.push(item(ACTION_TERMINAL, "在此打开终端"));
            } else {
                items.push(item(ACTION_OPEN_WITH, "打开方式"));
            }
            items.push(separator());
            items.push(item(ACTION_COPY, "复制"));
            items.push(item(ACTION_CUT, "剪切"));
            let mut paste = item(ACTION_PASTE, "粘贴");
            paste.enabled = !self.clipboard.is_empty();
            items.push(paste);
            items.push(item(ACTION_RENAME, "重命名"));
            items.push(item(ACTION_DELETE, "删除"));
            items.push(separator());
            items.push(item(ACTION_SHORTCUT, "创建快捷方式"));
            items.push(item(ACTION_STAGE, "加入暂存盘"));
            items.push(item(ACTION_DIFF, "文本对比…"));
            items.push(item(ACTION_OPEN_LOCATION, "打开文件所在位置"));
            items.push(MenuNode {
                id: SUBMENU_SEND_TO,
                label: "发送到".into(),
                kind: "submenu".into(),
                enabled: true,
            });
        } else {
            let mut paste = item(ACTION_PASTE, "粘贴");
            paste.enabled = !self.clipboard.is_empty();
            items.push(paste);
            items.push(separator());
            items.push(item(ACTION_TERMINAL, "在此打开终端"));
        }

        // ---- 自定义动作（actions.toml） ----
        self.menu_custom.clear();
        self.menu_submenu.clear();
        if let Some(target) = selection.as_deref() {
            let custom = self.collect_custom_menu(target);
            if !custom.is_empty() {
                items.push(separator());
                items.extend(custom);
            }
        }

        (items, sub_items)
    }

    fn collect_custom_menu(&mut self, target: &Path) -> Vec<MenuNode> {
        let top_ids: Vec<String> = {
            let store = &self.action_store;
            store
                .actions()
                .iter()
                .filter(|action| action.parent.is_none() && action.enabled)
                .filter(|action| {
                    !action.is_submenu() || Self::has_visible_children(store, &action.id, target)
                })
                .map(|action| action.id.clone())
                .collect()
        };

        let mut nodes = Vec::new();
        for id in top_ids {
            let Some(action) = self.action_store.find(&id).cloned() else {
                continue;
            };
            if action.is_submenu() {
                let child_ids: Vec<String> = self
                    .action_store
                    .actions()
                    .iter()
                    .filter(|child| {
                        child.parent.as_deref() == Some(id.as_str())
                            && child.enabled
                            && child
                                .command()
                                .is_some_and(|(.., filter)| filter.matches(target))
                    })
                    .map(|child| child.id.clone())
                    .collect();
                let mut children = Vec::new();
                for child_id in child_ids {
                    if let Some(child) = self.action_store.find(&child_id).cloned() {
                        children.push(self.register_custom(child));
                    }
                }
                let menu_id = self.register_custom(action.clone());
                self.menu_submenu.insert(id.clone(), children);
                nodes.push(MenuNode {
                    id: menu_id,
                    label: action.label.clone().into(),
                    kind: "submenu".into(),
                    enabled: true,
                });
            } else {
                let Some((.., filter)) = action.command() else {
                    continue;
                };
                if !filter.matches(target) {
                    continue;
                }
                let menu_id = self.register_custom(action.clone());
                nodes.push(MenuNode {
                    id: menu_id,
                    label: action.label.clone().into(),
                    kind: "item".into(),
                    enabled: true,
                });
            }
        }
        nodes
    }

    fn has_visible_children(store: &actions::Store, submenu_id: &str, target: &Path) -> bool {
        store.actions().iter().any(|child| {
            child.parent.as_deref() == Some(submenu_id)
                && child.enabled
                && child
                    .command()
                    .is_some_and(|(.., filter)| filter.matches(target))
        })
    }

    fn register_custom(&mut self, action: Action) -> i32 {
        self.menu_custom.push(action);
        CUSTOM_BASE + (self.menu_custom.len() - 1) as i32
    }

    fn menu_action(&mut self, id: i32) {
        self.close_context_menu();

        // 自定义动作
        if id >= CUSTOM_BASE {
            let Some(action) = self.menu_custom.get((id - CUSTOM_BASE) as usize).cloned() else {
                return;
            };
            let Some(target) = self.selected_path() else { return };
            match actions::run(&action, &target) {
                Ok(()) => self.set_status(&format!("已执行：{}", action.label)),
                Err(e) => self.set_status(&format!("执行失败（{}）：{e}", action.label)),
            }
            return;
        }

        let selection = self.selected_path();
        match id {
            ACTION_OPEN => {
                if let Some(path) = selection {
                    if path.is_dir() {
                        self.navigate_to(&path);
                    } else if let Err(e) = platform::shell_open(&path) {
                        self.set_status(&format!("Open failed: {e}"));
                    }
                }
            }
            ACTION_OPEN_WITH => {
                if let Some(path) = selection {
                    if let Err(e) = platform::shell_open_as(&path) {
                        self.set_status(&format!("Open with failed: {e}"));
                    }
                }
            }
            ACTION_OPEN_LOCATION => {
                if let Some(path) = selection {
                    if let Some(parent) = browse::parent_dir(&path) {
                        self.navigate_to(&parent);
                    }
                }
            }
            ACTION_COPY => self.clipboard_copy(),
            ACTION_CUT => self.clipboard_cut(),
            ACTION_PASTE => self.paste(),
            ACTION_RENAME => self.begin_rename(),
            ACTION_DELETE => self.delete_selected(),
            ACTION_TERMINAL => {
                let dir = if self.selected_is_dir() {
                    self.selected_path().unwrap_or_else(|| self.current_dir())
                } else {
                    self.current_dir()
                };
                if let Err(e) = platform::open_terminal(&dir) {
                    self.set_status(&format!("Terminal failed: {e}"));
                }
            }
            ACTION_SHORTCUT => {
                if let Some(path) = selection {
                    let name = shortcut::link_name_for(&path);
                    let dest = path.parent().unwrap_or(Path::new(".")).join(name);
                    match shortcut::create(&path, &dest, "") {
                        Ok(()) => {
                            self.set_status(&format!("Created {}", dest.display()));
                            self.reload();
                        }
                        Err(e) => self.set_status(&format!("Shortcut failed: {e}")),
                    }
                }
            }
            ACTION_STAGE => self.stage_add(),
            ACTION_DIFF => self.diff_selected(),
            ACTION_SEND_DESKTOP => {
                if let Some(path) = selection {
                    let Some(desktop) = dirs::desktop_dir() else {
                        self.set_status("Desktop folder not found");
                        return;
                    };
                    let dest = desktop.join(shortcut::link_name_for(&path));
                    match shortcut::create(&path, &dest, "") {
                        Ok(()) => self.set_status(&format!("Sent to {}", dest.display())),
                        Err(e) => self.set_status(&format!("Send failed: {e}")),
                    }
                }
            }
            ACTION_SEND_START => {
                if let Some(path) = selection {
                    let Some(config) = dirs::config_dir() else {
                        self.set_status("Start menu folder not found");
                        return;
                    };
                    let programs = config
                        .join("Microsoft")
                        .join("Windows")
                        .join("Start Menu")
                        .join("Programs");
                    let dest = programs.join(shortcut::link_name_for(&path));
                    match shortcut::create(&path, &dest, "") {
                        Ok(()) => self.set_status(&format!("Sent to {}", dest.display())),
                        Err(e) => self.set_status(&format!("Send failed: {e}")),
                    }
                }
            }
            _ => {}
        }
    }

    fn commit_rename(&mut self, text: &str) {
        let index = self.rename_index();
        if index == usize::MAX {
            return;
        }
        if let Some(window) = self.window.as_ref() {
            window.set_rename_index(-1);
        }
        let Some(entry) = self.tab().entries.get(index) else {
            return;
        };
        let path = entry.path.clone();
        match ops::rename(&path, text) {
            Ok(_) => {
                self.tab_mut().selected = usize::MAX;
                self.set_status(&format!("Renamed to {text}"));
                self.reload();
            }
            Err(e) => {
                self.set_status(&format!("Rename failed: {e}"));
                self.reload();
            }
        }
    }

    fn cancel_rename(&mut self) {
        if let Some(window) = self.window.as_ref() {
            window.set_rename_index(-1);
        }
    }

    // ---------------------------------------------------------------- 暂存盘

    fn stage_toggle(&mut self) {
        self.staging_visible = !self.staging_visible;
        if let Some(window) = self.window.as_ref() {
            window.set_staging_visible(self.staging_visible);
        }
        self.sync_staging();
    }

    fn stage_add(&mut self) {
        let Some(path) = self.selected_path() else { return };
        self.staging.add(&path);
        self.staging_visible = true;
        if let Some(window) = self.window.as_ref() {
            window.set_staging_visible(true);
        }
        self.sync_staging();
        self.set_status(&format!("已加入暂存盘（{} 项）", self.staging.len()));
    }

    fn stage_remove(&mut self, index: i32) {
        self.staging.remove(index as usize);
        self.sync_staging();
    }

    fn stage_clear(&mut self) {
        self.staging.clear();
        self.sync_staging();
    }

    fn stage_copy_here(&mut self) {
        let dest = self.current_dir();
        let report = self.staging.copy_to(&dest);
        self.set_status(&format!("暂存盘复制：{}", report.summary()));
        self.reload();
    }

    fn stage_move_here(&mut self) {
        let dest = self.current_dir();
        let report = self.staging.move_to(&dest);
        self.set_status(&format!("暂存盘移动：{}", report.summary()));
        self.sync_staging();
        self.reload();
    }

    fn sync_staging(&mut self) {
        let Some(window) = self.window.as_ref() else {
            return;
        };
        let items: Vec<StagingItem> = self
            .staging
            .items()
            .iter()
            .map(|path| StagingItem {
                name: path
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| path.display().to_string())
                    .into(),
                path: path.display().to_string().into(),
            })
            .collect();
        window.set_staging_items(ModelRc::new(VecModel::from(items)));
    }

    // ---------------------------------------------------------------- 预览

    fn preview_selected(&mut self) {
        let Some(path) = self.selected_path() else { return };
        if path.is_dir() {
            self.set_status("文件夹暂不支持预览");
            return;
        }
        let Ok(content) = preview::load(&path) else {
            self.set_status("预览失败");
            return;
        };
        if self.preview_window.is_none() {
            self.create_preview_window();
        }
        let Some(window) = self.preview_window.as_ref() else {
            return;
        };
        window.set_file_name(content.title.clone().into());
        window.set_file_info(content.info.clone().into());
        window.set_content_kind(content.kind.as_str().into());
        window.set_message(content.message.clone().unwrap_or_default().into());
        window.set_text_content(content.text.clone().unwrap_or_default().into());
        window.set_preview_image(content.image.clone().unwrap_or_default());

        let rows: Vec<PreviewRow> = content
            .rows
            .iter()
            .map(|row| PreviewRow {
                c0: row.cells[0].clone().into(),
                c1: row.cells[1].clone().into(),
                c2: row.cells[2].clone().into(),
                c3: row.cells[3].clone().into(),
                c4: row.cells[4].clone().into(),
                c5: row.cells[5].clone().into(),
                c6: row.cells[6].clone().into(),
                c7: row.cells[7].clone().into(),
                count: row.count as i32,
            })
            .collect();
        window.set_table_rows(ModelRc::new(VecModel::from(rows)));
        let _ = window.show();
    }

    fn create_preview_window(&mut self) {
        let window = match PreviewWindow::new() {
            Ok(window) => window,
            Err(e) => {
                eprintln!("[rove] preview window creation failed: {e}");
                return;
            }
        };
        window.on_open_external(|| {
            with_app(|app| {
                if let Some(path) = app.selected_path() {
                    let _ = platform::shell_open(&path);
                }
            });
        });
        window.on_close_window(|| with_app(|app| app.close_preview_window()));
        self.preview_window = Some(window);
    }

    fn close_preview_window(&mut self) {
        if let Some(window) = self.preview_window.as_ref() {
            let _ = window.window().hide();
        }
    }

    // ---------------------------------------------------------------- 对比

    fn diff_selected(&mut self) {
        let left = self
            .selected_path()
            .map(|path| path.display().to_string())
            .unwrap_or_default();
        let right = self
            .tab()
            .entries2
            .get(self.tab().selected2)
            .map(|entry| entry.path.display().to_string())
            .unwrap_or_default();

        if self.diff_window.is_none() {
            self.create_diff_window();
        }
        let Some(window) = self.diff_window.as_ref() else {
            return;
        };
        window.set_left_path(left.into());
        window.set_right_path(right.into());
        window.set_rows(ModelRc::new(VecModel::from(Vec::<DiffRow>::new())));
        window.set_summary(String::new().into());
        window.set_status_text(String::new().into());
        let _ = window.show();
    }

    fn create_diff_window(&mut self) {
        let window = match DiffWindow::new() {
            Ok(window) => window,
            Err(e) => {
                eprintln!("[rove] diff window creation failed: {e}");
                return;
            }
        };
        window.on_compare(|| with_app(|app| app.run_diff()));
        window.on_close_window(|| with_app(|app| app.close_diff_window()));
        self.diff_window = Some(window);
    }

    fn run_diff(&mut self) {
        let Some(window) = self.diff_window.as_ref() else {
            return;
        };
        let left = window.get_left_path().to_string();
        let right = window.get_right_path().to_string();

        let read = |path: &str| -> std::io::Result<String> {
            let bytes = std::fs::read(path)?;
            Ok(String::from_utf8_lossy(&bytes).to_string())
        };
        let Ok(old_text) = read(&left) else {
            window.set_status_text(format!("无法读取：{left}").into());
            return;
        };
        let Ok(new_text) = read(&right) else {
            window.set_status_text(format!("无法读取：{right}").into());
            return;
        };

        let result = crate::diff::compare(&old_text, &new_text);
        let rows: Vec<DiffRow> = result
            .rows
            .iter()
            .map(|row| DiffRow {
                kind: row.kind.into(),
                old_line: row.old_line as i32,
                new_line: row.new_line as i32,
                text: row.text.clone().into(),
            })
            .collect();
        window.set_rows(ModelRc::new(VecModel::from(rows)));
        window.set_summary(result.summary().into());
        window.set_status_text(String::new().into());
    }

    fn close_diff_window(&mut self) {
        if let Some(window) = self.diff_window.as_ref() {
            let _ = window.window().hide();
        }
    }

    // ---------------------------------------------------------------- 设置

    fn save_general(&mut self) {
        let Some(window) = self.window.as_ref() else {
            return;
        };
        self.settings.destroy_ui_on_last_tab_close = window.get_opt_destroy_ui();
        self.settings.show_tray = window.get_opt_tray();
        match settings::save(&self.settings) {
            Ok(()) => {
                window.set_status_text("设置已保存".into());
                self.set_status("设置已保存");
            }
            Err(e) => window.set_status_text(format!("保存失败：{e}").into()),
        }
    }

    fn action_rows(&self) -> Vec<ActionRow> {
        let mut rows = Vec::new();
        for action in self.action_store.actions() {
            if action.parent.is_some() {
                continue;
            }
            rows.push(action_row(action, 0));
            if action.is_submenu() {
                for child in self
                    .action_store
                    .actions()
                    .iter()
                    .filter(|child| child.parent.as_deref() == Some(action.id.as_str()))
                {
                    rows.push(action_row(child, 1));
                }
            }
        }
        rows
    }

    fn sync_settings_actions(&mut self) {
        let Some(window) = self.window.as_ref() else {
            return;
        };
        let selected = window.get_selected_action();
        window.set_actions(ModelRc::new(VecModel::from(self.action_rows())));
        window.set_selected_action(selected);
        window.set_edit_enabled(false);
        window.set_archiver_summary(archive_sync::summary().into());
    }

    fn settings_select_action(&mut self, index: i32) {
        let rows = self.action_rows();
        let Some(row) = rows.get(index as usize) else {
            return;
        };
        let Some(action) = self.action_store.find(&row.id).cloned() else {
            return;
        };
        let Some(window) = self.window.as_ref() else {
            return;
        };
        window.set_selected_action(index);
        window.set_edit_enabled(!action.is_submenu());
        if let Some((program, args, working_dir, filter)) = action.command() {
            window.set_edit_label(action.label.clone().into());
            window.set_edit_program(program.to_string().into());
            window.set_edit_args(args.to_string().into());
            window.set_edit_working_dir(working_dir.to_string().into());
            window.set_edit_target_index(match filter.target {
                TargetKind::Any => 0,
                TargetKind::File => 1,
                TargetKind::Dir => 2,
            });
            window.set_edit_extensions(filter.extensions.join(", ").into());
        } else {
            window.set_edit_label(action.label.clone().into());
            window.set_edit_program(String::new().into());
            window.set_edit_args(String::new().into());
            window.set_edit_working_dir(String::new().into());
            window.set_edit_extensions(String::new().into());
        }
    }

    fn settings_toggle_action(&mut self, index: i32, enabled: bool) {
        let rows = self.action_rows();
        let Some(row) = rows.get(index as usize) else {
            return;
        };
        let id = row.id.clone();
        self.action_store.set_enabled(&id, enabled);
        self.save_actions();
        self.sync_settings_actions();
    }

    fn settings_apply_edit(&mut self) {
        let Some(window) = self.window.as_ref() else {
            return;
        };
        let index = window.get_selected_action();
        let rows = self.action_rows();
        let Some(row) = rows.get(index as usize) else {
            return;
        };
        let id = row.id.clone();

        let label = window.get_edit_label().to_string();
        let program = window.get_edit_program().to_string();
        let args = window.get_edit_args().to_string();
        let working_dir = window.get_edit_working_dir().to_string();
        let target = match window.get_edit_target_index() {
            1 => TargetKind::File,
            2 => TargetKind::Dir,
            _ => TargetKind::Any,
        };
        let extensions: Vec<String> = window
            .get_edit_extensions()
            .split(',')
            .map(|part| part.trim().to_string())
            .filter(|part| !part.is_empty())
            .collect();

        self.action_store.update_command(
            &id,
            &label,
            &program,
            &args,
            &working_dir,
            target,
            &extensions,
        );
        self.save_actions();
        self.sync_settings_actions();
        if let Some(window) = self.window.as_ref() {
            window.set_status_text("已保存修改".into());
        }
    }

    fn settings_add_action(&mut self) {
        let next = self
            .action_store
            .actions()
            .iter()
            .filter(|action| action.id.starts_with("custom."))
            .count()
            + 1;
        let action = Action {
            id: format!("custom.{next}"),
            label: "新动作".to_string(),
            enabled: true,
            parent: None,
            tool: None,
            auto_disabled: false,
            kind: NodeKind::Command {
                program: String::new(),
                args: String::new(),
                working_dir: "{target_dir}".to_string(),
                filter: Filter {
                    target: TargetKind::Any,
                    extensions: Vec::new(),
                },
            },
        };
        let id = action.id.clone();
        self.action_store.add_command(action);
        self.save_actions();
        self.sync_settings_actions();

        let rows = self.action_rows();
        if let Some(position) = rows.iter().position(|row| row.id == id) {
            self.settings_select_action(position as i32);
        }
    }

    fn settings_delete_action(&mut self) {
        let Some(window) = self.window.as_ref() else {
            return;
        };
        let index = window.get_selected_action();
        let rows = self.action_rows();
        let Some(row) = rows.get(index as usize) else {
            return;
        };
        let id = row.id.clone();
        self.action_store.remove(&id);
        self.save_actions();
        self.sync_settings_actions();
        if let Some(window) = self.window.as_ref() {
            window.set_selected_action(-1);
            window.set_edit_enabled(false);
            window.set_status_text("已删除".into());
        }
    }

    fn save_actions(&mut self) {
        if let Err(e) = self.action_store.save() {
            if let Some(window) = self.window.as_ref() {
                window.set_status_text(format!("保存失败：{e}").into());
            }
        }
    }

    // ---------------------------------------------------------------- 缩略图

    fn start_thumbnail_timer(&mut self) {
        let cache = Rc::clone(&self.thumb_cache);
        let entries_model = Rc::clone(&self.entries_model);
        let entries2_model = Rc::clone(&self.entries2_model);
        let timer = slint::Timer::default();
        timer.start(slint::TimerMode::Repeated, Duration::from_millis(150), move || {
            for (path, image) in thumbnails::global().poll() {
                apply_thumbnail_result(&cache, &entries_model, &entries2_model, &path, &image);
            }
            #[cfg(windows)]
            if let Some(hits) = index::poll_results() {
                with_app(|app| app.apply_search_results(hits));
            }
        });
        self.thumb_timer = Some(timer);
    }

    fn request_thumbnails(&mut self) {
        let mut ready: Vec<(PathBuf, slint::Image)> = Vec::new();
        for entries in [&self.tab().entries, &self.tab().entries2] {
            for entry in entries {
                if let Some(image) = thumbnails::global().request(&entry.path) {
                    ready.push((entry.path.clone(), image));
                }
            }
        }
        for (path, image) in ready {
            self.apply_thumbnail(&path, image);
        }
    }

    fn apply_thumbnail(&mut self, path: &Path, image: slint::Image) {
        let entries_model = Rc::clone(&self.entries_model);
        let entries2_model = Rc::clone(&self.entries2_model);
        apply_thumbnail_result(
            &self.thumb_cache,
            &entries_model,
            &entries2_model,
            path,
            &image,
        );
    }

    // ---------------------------------------------------------------- 同步 UI

    fn sync_ui(&mut self) {
        if self.window.is_none() {
            return;
        }
        // 先完成需要 &mut self 的部分
        self.sync_staging();
        self.sync_settings_actions();

        let Some(window) = self.window.as_ref() else {
            return;
        };

        // 标签
        let tabs: Vec<TabItem> = self
            .tabs
            .iter()
            .map(|tab| TabItem {
                id: tab.id,
                kind: if tab.is_settings() {
                    "settings".into()
                } else {
                    "explorer".into()
                },
                title: if tab.is_settings() {
                    "⚙ 设置".into()
                } else {
                    tab_title(&tab.path).into()
                },
                path: tab.path.display().to_string().into(),
            })
            .collect();
        window.set_tabs(ModelRc::new(VecModel::from(tabs)));
        window.set_active_tab(self.active as i32);
        let is_settings = self.tab().is_settings();
        window.set_active_tab_kind(if is_settings { "settings" } else { "explorer" }.into());
        window.set_settings_page(self.settings_page);

        // 侧边栏（仅 Windows）
        #[cfg(windows)]
        let quick = drives::quick_access();
        #[cfg(windows)]
        let quick_items: Vec<SideItem> = vec![
            SideItem {
                id: "desktop".into(),
                label: "桌面".into(),
                detail: "".into(),
                tint: slint::Color::from_rgb_u8(0x1a, 0x73, 0xe8),
            },
            SideItem {
                id: "downloads".into(),
                label: "下载".into(),
                detail: "".into(),
                tint: slint::Color::from_rgb_u8(0x18, 0x7a, 0x4d),
            },
            SideItem {
                id: "recycle".into(),
                label: "回收站".into(),
                detail: quick
                    .recycle_count
                    .map(|count| format!("{count} 项"))
                    .unwrap_or_default()
                    .into(),
                tint: slint::Color::from_rgb_u8(0x9a, 0xa0, 0xa6),
            },
        ];
        #[cfg(not(windows))]
        let quick_items: Vec<SideItem> = Vec::new();
        window.set_quick_items(ModelRc::new(VecModel::from(quick_items)));
        #[cfg(windows)]
        let drive_items: Vec<DriveItem> = drives::fixed_drives()
            .iter()
            .map(|drive| DriveItem {
                letter: drive.letter.clone().into(),
                label: drive.label.clone().into(),
                used_percent: drive.used_percent(),
                capacity: drive.capacity_label().into(),
            })
            .collect();
        #[cfg(windows)]
        window.set_drives(ModelRc::new(VecModel::from(drive_items)));
        #[cfg(not(windows))]
        window.set_drives(ModelRc::new(VecModel::from(Vec::<DriveItem>::new())));
        window.set_network_expanded(self.network_expanded);
        window.set_staging_visible(self.staging_visible);

        // 面包屑与导航按钮
        let current = self.current_dir();
        let breadcrumbs = build_breadcrumbs(&current);
        window.set_breadcrumbs(ModelRc::new(VecModel::from(breadcrumbs)));
        window.set_current_path(current.display().to_string().into());
        window.set_current_path2(self.tab().path2.display().to_string().into());
        let (can_back, can_forward) = {
            let tab = self.tab();
            (tab.history_index > 0, tab.history_index + 1 < tab.history.len())
        };
        window.set_can_go_back(can_back);
        window.set_can_go_forward(can_forward);
        window.set_split_view(self.settings.split_view);
        window.set_sort_mode(self.sort_mode);

        // 列表（搜索模式优先）
        if self.search_active {
            window.set_entries(ModelRc::new(VecModel::from(self.hits_to_model())));
            window.set_entries2(ModelRc::new(VecModel::from(Vec::<FileEntry>::new())));
            window.set_selected_index(self.tab().selected as i32);
            window.set_show_path_column(true);
            window.set_search_count(format!("{} 个结果", self.search_hits.len()).into());
        } else {
            self.entries_model
                .set_vec(entries_to_model(&self.tab().entries, &self.thumb_cache));
            self.entries2_model
                .set_vec(entries_to_model(&self.tab().entries2, &self.thumb_cache));
            window.set_selected_index(clamp_index(self.tab().selected, self.tab().entries.len()));
            window.set_selected_index2(clamp_index(self.tab().selected2, self.tab().entries2.len()));
            window.set_show_path_column(false);
        }

        // 设置页数据
        window.set_opt_destroy_ui(self.settings.destroy_ui_on_last_tab_close);
        window.set_opt_tray(self.settings.show_tray);
        #[cfg(windows)]
        window.set_index_status(index_status_text(&self.index).into());
        #[cfg(not(windows))]
        window.set_index_status("索引仅支持 Windows".into());

        if !self.search_active {
            let count = self.tab().entries.len();
            window.set_status_text(format!("{count} 个项目").into());
        }
    }

    fn hits_to_model(&self) -> Vec<FileEntry> {
        let query = self.search_query.clone();
        let cache = self.thumb_cache.borrow();
        self.search_hits
            .iter()
            .map(|hit| {
                let parent = Path::new(&hit.path)
                    .parent()
                    .map(|p| p.display().to_string())
                    .unwrap_or_default();
                let (badge, badge_color) = badge_for(&hit.name, hit.is_dir);
                // 搜索结果与普通目录项共用同一套缩略图缓存
                let cached = cache.get(Path::new(&hit.path));
                FileEntry {
                    name: hit.name.clone().into(),
                    path: hit.path.clone().into(),
                    kind: parent.into(),
                    size: browse::format_size(hit.size).into(),
                    modified: "-".to_string().into(),
                    is_dir: hit.is_dir,
                    has_image: cached.is_some(),
                    image: cached.cloned().unwrap_or_default(),
                    badge: badge.into(),
                    badge_color,
                    spans: ModelRc::new(VecModel::from(spans_for(
                        &hit.name,
                        &query,
                        &self.matcher,
                        &self.matcher_scratch,
                    ))),
                }
            })
            .collect()
    }

    /// 搜索结果为命中项申请缩略图（走同一套后台管线）
    fn request_hit_thumbnails(&mut self) {
        let mut ready: Vec<(PathBuf, slint::Image)> = Vec::new();
        for hit in &self.search_hits {
            let path = PathBuf::from(&hit.path);
            if let Some(image) = thumbnails::global().request(&path) {
                ready.push((path, image));
            }
        }
        for (path, image) in ready {
            self.apply_thumbnail(&path, image);
        }
    }

    fn set_status(&mut self, text: &str) {
        if let Some(window) = self.window.as_ref() {
            window.set_status_text(text.into());
        }
    }
}

// ================================================================ 辅助函数

fn clamp_index(index: usize, len: usize) -> i32 {
    if index < len {
        index as i32
    } else {
        -1
    }
}

fn default_path() -> PathBuf {
    dirs::home_dir().unwrap_or_else(|| PathBuf::from("C:\\"))
}

fn tab_title(path: &Path) -> String {
    if path.parent().is_none() {
        return path.display().to_string();
    }
    path.file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| path.display().to_string())
}

/// 面包屑：此电脑 / 盘符 / 各级目录。
/// `Path::components()` 会把根目录的 `\` 单独产出一个 RootDir 组件，
/// 直接合并进盘符那一级，避免出现「D: > \ >」的诡异切分。
fn build_breadcrumbs(path: &Path) -> Vec<Breadcrumb> {
    let mut crumbs = Vec::new();
    crumbs.push(Breadcrumb {
        name: "此电脑".into(),
        path: "".into(),
    });

    let mut accumulated = String::new();
    for component in path.components() {
        use std::path::Component;
        match component {
            Component::Prefix(prefix) => {
                // 盘符 "C:"
                accumulated = prefix.as_os_str().to_string_lossy().to_string();
                crumbs.push(Breadcrumb {
                    name: accumulated.clone().into(),
                    path: format!("{accumulated}\\").into(),
                });
            }
            Component::RootDir => {
                // 根目录斜杠：合并进上一级（盘符），不单独成层
                if let Some(last) = crumbs.last_mut() {
                    last.path = format!("{}\\", accumulated).into();
                }
            }
            Component::Normal(part) => {
                let part = part.to_string_lossy().to_string();
                if !accumulated.is_empty() {
                    accumulated = format!("{accumulated}\\{part}");
                } else {
                    accumulated = part.clone();
                }
                crumbs.push(Breadcrumb {
                    name: part.into(),
                    path: accumulated.clone().into(),
                });
            }
            _ => {}
        }
    }
    crumbs
}

/// 排序
fn sort_entries(entries: &mut [browse::Entry], mode: i32) {
    match mode {
        1 => entries.sort_by(|a, b| {
            b.is_dir
                .cmp(&a.is_dir)
                .then_with(|| b.modified.cmp(&a.modified))
        }),
        2 => entries.sort_by(|a, b| {
            b.is_dir
                .cmp(&a.is_dir)
                .then_with(|| b.size.cmp(&a.size))
        }),
        3 => entries.sort_by(|a, b| {
            b.is_dir
                .cmp(&a.is_dir)
                .then_with(|| a.kind().cmp(&b.kind()))
        }),
        _ => entries.sort_by(|a, b| {
            b.is_dir
                .cmp(&a.is_dir)
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        }),
    }
}

/// 名称 -> (徽章文本, 徽章颜色)
fn badge_for(name: &str, is_dir: bool) -> (&'static str, slint::Color) {
    if is_dir {
        return ("DIR", slint::Color::from_rgb_u8(0x5f, 0x63, 0x68));
    }
    let extension = Path::new(name)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let color = |r, g, b| slint::Color::from_rgb_u8(r, g, b);
    match extension.as_str() {
        "png" | "jpg" | "jpeg" | "gif" | "bmp" | "webp" | "svg" | "ico" => ("IMG", color(0x1a, 0x73, 0xe8)),
        "txt" | "md" | "log" => ("TXT", color(0x5f, 0x63, 0x68)),
        "pdf" | "doc" | "docx" | "xls" | "xlsx" | "ppt" | "pptx" => ("DOC", color(0xd9, 0x30, 0x25)),
        "zip" | "7z" | "rar" | "tar" | "gz" | "bz2" | "xz" => ("ZIP", color(0x84, 0x2b, 0xd0)),
        "csv" | "tsv" => ("CSV", color(0x18, 0x7a, 0x4d)),
        "mp3" | "wav" | "flac" | "m4a" => ("AUD", color(0xe3, 0x74, 0x00)),
        "mp4" | "mkv" | "avi" | "mov" => ("VID", color(0xc5, 0x22, 0x8a)),
        "rs" | "py" | "js" | "ts" | "c" | "cpp" | "h" | "json" | "toml" | "html" | "css" => {
            ("CODE", color(0x0b, 0x57, 0xd0))
        }
        "exe" | "lnk" | "bat" | "cmd" | "ps1" => ("EXE", color(0x3c, 0x40, 0x43)),
        "" => ("FILE", color(0x9a, 0xa0, 0xa6)),
        _ => ("FILE", color(0x9a, 0xa0, 0xa6)),
    }
}

/// 计算某个名称的命中片段（转换为 Slint 的 TextSpan）
fn spans_for(
    name: &str,
    query: &str,
    matcher: &RefCell<Matcher>,
    scratch: &RefCell<MatcherScratch>,
) -> Vec<TextSpan> {
    if query.trim().is_empty() {
        return vec![TextSpan {
            text: name.to_string().into(),
            is_match: false,
        }];
    }
    let entry = MatchEntry::new(name);
    let indices = matcher::highlight_indices(
        &entry,
        query,
        &mut matcher.borrow_mut(),
        &mut scratch.borrow_mut(),
    );
    matcher::build_highlight_spans(name, &indices)
        .into_iter()
        .map(|span| TextSpan {
            text: span.text.into(),
            is_match: span.is_match,
        })
        .collect()
}

/// 目录条目 -> 列表模型
fn entries_to_model(
    entries: &[browse::Entry],
    cache: &Rc<RefCell<HashMap<PathBuf, slint::Image>>>,
) -> Vec<FileEntry> {
    let cache = cache.borrow();
    entries
        .iter()
        .map(|entry| {
            let cached = cache.get(&entry.path);
            let (badge, badge_color) = badge_for(&entry.name, entry.is_dir);
            FileEntry {
                name: entry.name.clone().into(),
                path: entry.path.display().to_string().into(),
                kind: entry.kind().into(),
                size: if entry.is_dir {
                    String::new()
                } else {
                    browse::format_size(entry.size)
                }
                .into(),
                modified: entry
                    .modified
                    .map(browse::format_time)
                    .unwrap_or_else(|| "-".to_string())
                    .into(),
                is_dir: entry.is_dir,
                has_image: cached.is_some(),
                image: cached.cloned().unwrap_or_default(),
                badge: badge.into(),
                badge_color,
                spans: ModelRc::new(VecModel::from(vec![TextSpan {
                    text: entry.name.clone().into(),
                    is_match: false,
                }])),
            }
        })
        .collect()
}

/// 动作列表行
fn action_row(action: &Action, depth: i32) -> ActionRow {
    let detail = match action.command() {
        Some((program, ..)) if !program.is_empty() => program.to_string(),
        Some(_) => "(未配置程序)".to_string(),
        None => "二级子菜单".to_string(),
    };
    ActionRow {
        id: action.id.clone().into(),
        label: action.label.clone().into(),
        kind: if action.is_submenu() {
            "submenu".into()
        } else {
            "action".into()
        },
        enabled: action.enabled,
        depth,
        detail: detail.into(),
        has_command: !action.is_submenu(),
    }
}

/// 索引状态文案
#[cfg(windows)]
fn index_status_text(index: &Option<IndexService>) -> String {
    let Some(index) = index else {
        return "索引未启动".to_string();
    };
    let Some(stats) = index.poll_stats() else {
        return "索引启动中…".to_string();
    };
    if stats.scanning {
        return "正在扫描索引…".to_string();
    }
    format!("已索引 {} 个条目 · USN 实时监听中", stats.nodes)
}

/// 写入缩略图缓存并刷新两个面板中的对应行
fn apply_thumbnail_result(
    cache: &Rc<RefCell<HashMap<PathBuf, slint::Image>>>,
    entries_model: &Rc<VecModel<FileEntry>>,
    entries2_model: &Rc<VecModel<FileEntry>>,
    path: &Path,
    image: &slint::Image,
) {
    cache.borrow_mut().insert(path.to_path_buf(), image.clone());
    let target = path.display().to_string();
    for model in [entries_model, entries2_model] {
        let rows = model.row_count();
        for row in 0..rows {
            let Some(entry) = model.row_data(row) else {
                continue;
            };
            if entry.path != target {
                continue;
            }
            let mut updated = entry;
            updated.has_image = true;
            updated.image = image.clone();
            model.set_row_data(row, updated);
            break;
        }
    }
}
