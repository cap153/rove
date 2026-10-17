//! 应用状态：标签页、导航历史、暂存托盘、选择集。

use std::collections::HashSet;
use std::path::PathBuf;

use crate::fs::{FileEntry, SortState};

/// 设置页子页面。
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum SettingsSubpage {
    General,
    SearchAndIndex,
    ContextMenu,
    Duplicates,
    QuickLook,
}

impl Default for SettingsSubpage {
    fn default() -> Self {
        SettingsSubpage::General
    }
}

/// 标签页类型：普通文件管理器 / 设置页。
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum TabKind {
    Explorer,
    Settings,
}

impl Default for TabKind {
    fn default() -> Self {
        TabKind::Explorer
    }
}

/// 一个标签页的完整状态。
pub struct TabState {
    pub id: u32,
    /// None 表示"此电脑"根视图。
    pub path: Option<PathBuf>,
    /// 导航历史（含当前位置）。
    pub history: Vec<Option<PathBuf>>,
    pub history_index: usize,
    /// 排序后的完整条目表。
    pub entries: Vec<FileEntry>,
    /// 过滤后可见条目在 entries 中的下标。
    pub display: Vec<usize>,
    pub sort: SortState,
    pub filter: String,
    /// 选择集（按路径，跨排序/过滤稳定）。
    pub selection: HashSet<PathBuf>,
    /// 范围选择锚点（路径）。
    pub anchor: Option<PathBuf>,
    /// 滚动偏移（逻辑像素），切换标签时恢复。
    pub scroll_y: f32,
    pub loading: bool,
    /// 导航代号：用于丢弃过期的异步列表结果。
    pub generation: u64,
    /// 与 entries 平行的拼音预计算缓存（Phase 5 拼音过滤）。
    /// 长度与 entries 不一致时惰性重建。
    pub pinyin_cache: Vec<crate::search::PinyinFields>,
    /// 深度搜索（子树）附加结果：仅 filter 非空时存在。
    /// 显示下标空间 = 本地条目 [0, entries.len()) ⊕ 深度结果 [len, len+deep)。
    pub deep_entries: Vec<FileEntry>,
    /// 与 deep_entries 平行的拼音缓存（后台算好传回，供过滤与高亮）。
    pub deep_pinyin: Vec<crate::search::PinyinFields>,
    /// 深度搜索代号：每次 filter 编辑/导航递增，过期结果丢弃。
    pub deep_generation: u64,
    /// 深度搜索进行中（状态栏提示）。
    pub deep_searching: bool,
    /// 标签页类型：普通文件管理器 / 设置页。
    pub kind: TabKind,
    /// 设置页当前子页面（仅 kind = Settings 时有效）。
    pub settings_subpage: SettingsSubpage,
}

impl TabState {
    pub fn new(id: u32, path: Option<PathBuf>) -> Self {
        Self {
            id,
            path: path.clone(),
            history: vec![path],
            history_index: 0,
            entries: Vec::new(),
            display: Vec::new(),
            sort: SortState::default(),
            filter: String::new(),
            selection: HashSet::new(),
            anchor: None,
            scroll_y: 0.0,
            loading: false,
            generation: 0,
            pinyin_cache: Vec::new(),
            deep_entries: Vec::new(),
            deep_pinyin: Vec::new(),
            deep_generation: 0,
            deep_searching: false,
            kind: TabKind::Explorer,
            settings_subpage: SettingsSubpage::default(),
        }
    }

    /// 新建一个设置页标签。
    pub fn new_settings(id: u32, subpage: SettingsSubpage) -> Self {
        let mut tab = Self::new(id, None);
        tab.kind = TabKind::Settings;
        tab.settings_subpage = subpage;
        tab
    }

    pub fn can_go_back(&self) -> bool {
        self.history_index > 0
    }

    pub fn can_go_forward(&self) -> bool {
        self.history_index + 1 < self.history.len()
    }

    /// 清空深度搜索结果(每次搜索词变化/导航时递增代号,丢弃在途结果)。
    /// 注意:不清 filter——start_deep_search 依赖先设置好的 filter。
    pub fn clear_deep(&mut self) {
        self.deep_entries.clear();
        self.deep_pinyin.clear();
        self.deep_searching = false;
        self.deep_generation += 1;
    }

    /// 导航到新路径（压入历史，丢弃前进记录）。
    pub fn navigate(&mut self, path: Option<PathBuf>) {
        self.history.truncate(self.history_index + 1);
        self.history.push(path.clone());
        self.history_index = self.history.len() - 1;
        self.path = path;
        self.selection.clear();
        self.anchor = None;
        self.scroll_y = 0.0;
        self.generation += 1;
        self.loading = true;
        self.filter.clear();
        self.clear_deep();
    }

    /// 后退/前进（不重新入栈）。
    pub fn jump_to(&mut self, index: usize) {
        if index >= self.history.len() {
            return;
        }
        self.history_index = index;
        self.path = self.history[index].clone();
        self.selection.clear();
        self.anchor = None;
        self.scroll_y = 0.0;
        self.generation += 1;
        self.loading = true;
        self.filter.clear();
        self.clear_deep();
    }

    /// 上一级目录（盘符根之上为"此电脑"）。
    pub fn parent_path(&self) -> Option<Option<PathBuf>> {
        match &self.path {
            None => None,
            Some(p) => {
                let parent = p.parent().filter(|pp| !pp.as_os_str().is_empty());
                match parent {
                    Some(pp) => Some(Some(pp.to_path_buf())),
                    None => Some(None), // 盘符根 → 此电脑
                }
            }
        }
    }

    /// 重建过滤后的可见下标表（拼音模糊匹配 + 打分排序）。
    /// 下标空间 = 本地条目 [0, entries.len()) ⊕ 深度结果（深度条目已由
    /// 后台按同一查询过滤，直接并入统一打分排序）。
    pub fn rebuild_display(&mut self) {
        let query = self.filter.trim().to_lowercase();
        if query.is_empty() {
            self.display = (0..self.entries.len()).collect();
            return;
        }
        self.ensure_pinyin_cache();
        let mut matcher = nucleo_matcher::Matcher::new(nucleo_matcher::Config::DEFAULT);
        let mut bufs = (Vec::new(), Vec::new(), Vec::new());
        let mut scored: Vec<(usize, u16)> = Vec::new();
        for (i, e) in self.entries.iter().enumerate() {
            if crate::search::matches_query(
                &e.name,
                &self.pinyin_cache[i],
                &query,
                &mut matcher,
                &mut bufs,
            ) {
                let s = crate::search::score(
                    &e.name,
                    &self.pinyin_cache[i],
                    &query,
                    &mut matcher,
                    &mut bufs,
                )
                .unwrap_or(0);
                scored.push((i, s));
            }
        }
        let local_len = self.entries.len();
        for (j, e) in self.deep_entries.iter().enumerate() {
            let s = self
                .deep_pinyin
                .get(j)
                .and_then(|f| crate::search::score(&e.name, f, &query, &mut matcher, &mut bufs))
                .unwrap_or(0);
            scored.push((local_len + j, s));
        }
        scored.sort_by(|a, b| b.1.cmp(&a.1));
        self.display = scored.into_iter().map(|(i, _)| i).collect();
    }

    /// 惰性构建拼音缓存（entries 变化后首次过滤时执行）。
    fn ensure_pinyin_cache(&mut self) {
        if self.pinyin_cache.len() != self.entries.len() {
            self.pinyin_cache = self
                .entries
                .iter()
                .map(|e| crate::search::pinyin_fields(&e.name))
                .collect();
        }
    }

    /// 显示下标 → 条目。不依赖拼音缓存（普通导航时缓存可能未构建）。
    pub fn display_entry(&self, display_index: usize) -> Option<&FileEntry> {
        let &idx = self.display.get(display_index)?;
        let local = self.entries.len();
        if idx < local {
            self.entries.get(idx)
        } else {
            self.deep_entries.get(idx - local)
        }
    }

    /// 显示下标 → (条目, 拼音字段)，仅高亮渲染使用。
    /// 本地缓存未构建（filter 为空的普通浏览）时返回 None → UI 回落原文；
    /// 深度结果始终携带后台预计算的字段。
    pub fn display_parts(
        &self,
        display_index: usize,
    ) -> Option<(&FileEntry, &crate::search::PinyinFields)> {
        let &idx = self.display.get(display_index)?;
        let local = self.entries.len();
        if idx < local {
            Some((self.entries.get(idx)?, self.pinyin_cache.get(idx)?))
        } else {
            let j = idx - local;
            Some((self.deep_entries.get(j)?, self.deep_pinyin.get(j)?))
        }
    }

    pub fn selected_paths(&self) -> Vec<PathBuf> {
        // 按显示顺序输出，保证菜单/操作顺序稳定。
        self.display
            .iter()
            .enumerate()
            .filter_map(|(di, _)| self.display_entry(di))
            .filter(|e| self.selection.contains(&e.path))
            .map(|e| e.path.clone())
            .collect()
    }

    pub fn selection_count(&self) -> usize {
        self.selection.len()
    }

    /// 统计（文件夹数、文件数、选中文件总大小）。
    pub fn stats(&self) -> (usize, usize, u64) {
        let mut dirs = 0usize;
        let mut files = 0usize;
        for di in 0..self.display.len() {
            if let Some(e) = self.display_entry(di) {
                if e.is_dir {
                    dirs += 1;
                } else {
                    files += 1;
                }
            }
        }
        let selected_size: u64 = (0..self.display.len())
            .filter_map(|di| self.display_entry(di))
            .filter(|e| self.selection.contains(&e.path) && !e.is_dir)
            .map(|e| e.size)
            .sum();
        (dirs, files, selected_size)
    }

    /// 标签标题：当前目录名 / "此电脑" / 设置页。
    pub fn title(&self) -> String {
        if self.kind == TabKind::Settings {
            return match self.settings_subpage {
                SettingsSubpage::General => "通用设置".to_string(),
                SettingsSubpage::SearchAndIndex => "搜索与索引".to_string(),
                SettingsSubpage::ContextMenu => "本地右键菜单".to_string(),
                SettingsSubpage::Duplicates => "重复文件".to_string(),
                SettingsSubpage::QuickLook => "快速预览".to_string(),
            };
        }
        match &self.path {
            None => "此电脑".to_string(),
            Some(p) if *p == crate::fs::drives::recycle_virtual_path() => "回收站".to_string(),
            Some(p) => p
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| p.to_string_lossy().into_owned()),
        }
    }
}

/// 暂存托盘条目。
#[derive(Clone, Debug)]
pub struct StagingEntry {
    pub path: PathBuf,
    pub is_dir: bool,
}

/// 应用级状态（跨标签共享）。
pub struct AppState {
    pub tabs: Vec<TabState>,
    pub active_tab: usize,
    pub next_tab_id: u32,
    pub staging: Vec<StagingEntry>,
    /// 剪切态路径（用于半透明显示）。
    pub cut_paths: HashSet<PathBuf>,
    /// 剪贴板中的文件（缓存副本，用于"粘贴"可用性判断）。
    pub clipboard_paths: Vec<PathBuf>,
    pub clipboard_operation: String,
    pub recent: Vec<PathBuf>,
}

impl AppState {
    pub fn new(initial: Option<PathBuf>) -> Self {
        let first = TabState::new(1, initial);
        Self {
            tabs: vec![first],
            active_tab: 0,
            next_tab_id: 2,
            staging: Vec::new(),
            cut_paths: HashSet::new(),
            clipboard_paths: Vec::new(),
            clipboard_operation: "copy".into(),
            recent: Vec::new(),
        }
    }

    pub fn active(&self) -> &TabState {
        &self.tabs[self.active_tab]
    }

    pub fn active_mut(&mut self) -> &mut TabState {
        &mut self.tabs[self.active_tab]
    }

    pub fn tab_by_id(&self, id: u32) -> Option<usize> {
        self.tabs.iter().position(|t| t.id == id)
    }

    pub fn new_tab(&mut self, path: Option<PathBuf>) -> u32 {
        let id = self.next_tab_id;
        self.next_tab_id += 1;
        let mut tab = TabState::new(id, path);
        // 新标签默认继承当前目录（Explorer/Pulse 习惯）。
        if tab.path.is_none() {
            if let Some(current) = self.active().path.clone() {
                tab.path = Some(current.clone());
                tab.history = vec![Some(current)];
            }
        }
        self.tabs.push(tab);
        self.active_tab = self.tabs.len() - 1;
        id
    }

    /// 打开设置页标签；若已存在同子页面则切换过去，否则新建。
    pub fn open_settings(&mut self, subpage: SettingsSubpage) -> u32 {
        if let Some(idx) = self.tabs.iter().position(|t| {
            t.kind == TabKind::Settings && t.settings_subpage == subpage
        }) {
            self.active_tab = idx;
            return self.tabs[idx].id;
        }
        let id = self.next_tab_id;
        self.next_tab_id += 1;
        let tab = TabState::new_settings(id, subpage);
        self.tabs.push(tab);
        self.active_tab = self.tabs.len() - 1;
        id
    }

    pub fn close_tab(&mut self, id: u32) -> bool {
        if self.tabs.len() <= 1 {
            return false;
        }
        let Some(index) = self.tab_by_id(id) else {
            return false;
        };
        self.tabs.remove(index);
        if self.active_tab >= self.tabs.len() {
            self.active_tab = self.tabs.len() - 1;
        } else if index < self.active_tab {
            self.active_tab -= 1;
        }
        true
    }

    /// 记录最近访问目录。
    pub fn push_recent(&mut self, path: &PathBuf) {
        // 虚拟位置（回收站）不进最近列表。
        if *path == crate::fs::drives::recycle_virtual_path() {
            return;
        }
        self.recent.retain(|p| p != path);
        self.recent.insert(0, path.clone());
        self.recent.truncate(12);
    }

    /// 添加到暂存盘（去重）。
    pub fn stage_paths(&mut self, paths: &[PathBuf]) -> usize {
        let mut added = 0;
        for path in paths {
            if self.staging.iter().any(|s| &s.path == path) {
                continue;
            }
            let is_dir = std::fs::metadata(path).map(|m| m.is_dir()).unwrap_or(false);
            self.staging.push(StagingEntry {
                path: path.clone(),
                is_dir,
            });
            added += 1;
        }
        added
    }

    /// 暂存盘中仍在磁盘上存在的条目（不存在则视为已失效）。
    pub fn staging_present(&self) -> Vec<StagingEntry> {
        self.staging
            .iter()
            .filter(|s| s.path.exists())
            .cloned()
            .collect()
    }
}
