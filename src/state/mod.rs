//! src/state/mod.rs
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
    Shortcuts,
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

/// 批量关闭标签的范围（标签栏右键菜单）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TabCloseScope {
    /// 只留下被右键的标签。
    Others,
    /// 关掉锚点左边的所有标签。
    Left,
    /// 关掉锚点右边的所有标签。
    Right,
}

impl Default for TabKind {
    fn default() -> Self {
        TabKind::Explorer
    }
}

/// 单个分屏面板的完整状态：path / history / entries 等都挂在面板上。
pub struct PaneState {
    /// None 表示"此电脑"根视图。
    pub path: Option<PathBuf>,
    /// 导航历史（含当前位置）。
    pub history: Vec<Option<PathBuf>>,
    /// 与 `history` 平行：每个页面的搜索词。搜索也作为一个独立页面，
    /// 因此后退可以回到搜索前的目录、前进可以回到搜索结果。
    pub history_filters: Vec<String>,
    /// 与 `history` 平行：每个页面各自的滚动位置。
    pub history_scroll: Vec<f32>,
    /// 进入搜索前的滚动位置；清空搜索框时恢复到这里。
    pub pre_filter_scroll_y: f32,
    /// 记录每个目录的滚动位置。只要标签不关，不管用什么方式回到该目录，都能精准恢复。
    /// 用 `Option<PathBuf>` 作为 Key，是为了兼容“此电脑（None）”视图的滚动记忆。
    pub path_scroll_memo: std::collections::HashMap<Option<PathBuf>, f32>,
    pub history_index: usize,
    /// 是否正在恢复滚动位置（导航时，加载完成前不要被 clamp 逻辑清零）。
    /// 关键：打开子文件夹再后退时，jump_to 恢复了 scroll_y，但异步列表结果尚未来，
    /// 期间 refresh_visible_rows 的 clamp 会用“空 display”把 scroll_y 夹到 0 → 跳顶。
    /// 置此标记后 clamp 跳过本 pane，直到 apply_list_result 数据就位再真正恢复。
    pub pending_scroll_restore: bool,
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
    /// 全局索引搜索的未截断命中总数；`Some(n)` 且 `display.len() < n` 时，
    /// 状态栏提示「结果已截断」。
    pub search_total: Option<usize>,
}

impl PaneState {
    pub fn new(path: Option<PathBuf>, sort: SortState) -> Self {
        Self {
            path: path.clone(),
            history: vec![path],
            history_filters: vec![String::new()],
            history_scroll: vec![0.0],
            pre_filter_scroll_y: 0.0,
            path_scroll_memo: std::collections::HashMap::new(), // 按路径记忆滚动位置
            history_index: 0,
            pending_scroll_restore: false,
            entries: Vec::new(),
            display: Vec::new(),
            sort, // 核心修改：使用传入的排序状态
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
            search_total: None,
        }
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
        // 离开当前页面时，按路径与按页面各保存一次滚动位置。
        self.path_scroll_memo.insert(self.path.clone(), self.scroll_y);
        if let Some(s) = self.history_scroll.get_mut(self.history_index) {
            *s = self.scroll_y;
        }

        let n = self.history_index + 1;
        self.history.truncate(n);
        self.history_filters.truncate(n);
        self.history_scroll.truncate(n);
        self.history.push(path.clone());
        self.history_filters.push(String::new());
        let restored = self.path_scroll_memo.get(&path).copied().unwrap_or(0.0);
        self.history_scroll.push(restored);
        self.history_index = self.history.len() - 1;
        self.path = path.clone();
        self.selection.clear();
        self.anchor = None;

        // 检查是否有记忆，有则恢复，无则顶部
        self.scroll_y = restored;
        self.pending_scroll_restore = true; // 只要导航了，就等数据回来再推 UI
        self.generation += 1;
        self.loading = true;
        self.filter.clear();
        self.pre_filter_scroll_y = 0.0;
        self.search_total = None;
        self.clear_deep();
    }

    /// 后退/前进（不重新入栈）。同一路径内的「搜索页 ↔ 目录页」也走这里，
    /// 此时只恢复搜索词和滚动，不重新列举目录（否则搜索结果会被冲掉）。
    pub fn jump_to(&mut self, index: usize) {
        if index >= self.history.len() {
            return;
        }
        self.path_scroll_memo.insert(self.path.clone(), self.scroll_y);
        if let Some(s) = self.history_scroll.get_mut(self.history_index) {
            *s = self.scroll_y;
        }

        let same_path = self.path == self.history[index];
        self.history_index = index;
        self.path = self.history[index].clone();
        // 搜索页面本身带着搜索词，前进/后退时一并恢复（不再无条件清空）。
        self.filter = self
            .history_filters
            .get(index)
            .cloned()
            .unwrap_or_default();
        self.selection.clear();
        self.anchor = None;

        self.scroll_y = self
            .history_scroll
            .get(index)
            .copied()
            .unwrap_or_else(|| self.path_scroll_memo.get(&self.path).copied().unwrap_or(0.0));
        // 同一路径不会触发异步列举，别让 pending 标记永远挡住滚动回调。
        self.pending_scroll_restore = !same_path;
        if !same_path {
            self.generation += 1;
            self.loading = true;
            self.search_total = None;
            self.clear_deep();
        }
    }

    /// 搜索词变化：把「开始搜索」和「清空搜索」各记成一个可前进/后退的页面。
    /// 同一页里继续改词只更新本页的搜索词，不新增历史项。
    pub fn set_filter_from_search(&mut self, text: &str) {
        let was_empty = self.filter.is_empty();
        let now_empty = text.is_empty();
        if let Some(s) = self.history_scroll.get_mut(self.history_index) {
            *s = self.scroll_y;
        }
        if was_empty && !now_empty {
            // 记住搜索前的滚动位置，清空搜索框时回到这里。
            self.pre_filter_scroll_y = self.scroll_y;
            self.push_search_page(text.to_string(), 0.0);
        } else if !was_empty && now_empty {
            self.push_search_page(String::new(), self.pre_filter_scroll_y);
        } else if !now_empty {
            if let Some(f) = self.history_filters.get_mut(self.history_index) {
                *f = text.to_string();
            }
        }
        self.filter = text.to_string();
    }

    fn push_search_page(&mut self, filter: String, scroll: f32) {
        let n = self.history_index + 1;
        self.history.truncate(n);
        self.history_filters.truncate(n);
        self.history_scroll.truncate(n);
        self.history.push(self.path.clone());
        self.history_filters.push(filter);
        self.history_scroll.push(scroll);
        self.history_index = self.history.len() - 1;
        self.scroll_y = scroll;
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
        // 剥离盘符限定词（如 c:），避免冒号误杀文件名里的无关字符
        let (_, clean_query) = crate::search::parse_drive_prefix(&self.filter);
        let query = clean_query.trim().to_lowercase();
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
}

/// 一个标签页的完整状态：包含若干分屏面板（PaneState）、整体类型与分屏配置。
pub struct TabState {
    pub id: u32,
    pub panes: Vec<PaneState>,
    /// 当前聚焦的面板下标（0 或 1）。
    pub active_pane: usize,
    /// 是否处于分屏状态（仅 Explorer 标签；设置页不分屏）。
    pub is_split: bool,
    /// 左右分屏比例，0~1 之间（左栏占比）。
    pub split_ratio: f32,
    /// 标签页类型：普通文件管理器 / 设置页。
    pub kind: TabKind,
    /// 设置页当前子页面（仅 kind = Settings 时有效）。
    pub settings_subpage: SettingsSubpage,
}

impl TabState {
    pub fn new(id: u32, path: Option<PathBuf>, sort: SortState) -> Self {
        Self {
            id,
            panes: vec![PaneState::new(path.clone(), sort)],
            active_pane: 0,
            is_split: false,
            split_ratio: 0.5,
            kind: TabKind::Explorer,
            settings_subpage: SettingsSubpage::default(),
        }
    }

    /// 新建一个设置页标签。
    pub fn new_settings(id: u32, subpage: SettingsSubpage) -> Self {
        let mut tab = Self::new(id, None, SortState::default());
        tab.kind = TabKind::Settings;
        tab.settings_subpage = subpage;
        tab
    }

    /// 当前聚焦面板。
    pub fn active(&self) -> &PaneState {
        &self.panes[self.active_pane]
    }

    /// 当前聚焦面板（可变）。
    pub fn active_mut(&mut self) -> &mut PaneState {
        &mut self.panes[self.active_pane]
    }

    /// 标签标题：当前目录名 / "此电脑" / 设置页。
    /// Explorer 标签取当前聚焦面板的目录名；设置页取子页面名。
    pub fn title(&self) -> String {
        if self.kind == TabKind::Settings {
            return match self.settings_subpage {
                SettingsSubpage::General => "通用设置".to_string(),
                SettingsSubpage::SearchAndIndex => "搜索与索引".to_string(),
                SettingsSubpage::ContextMenu => "本地右键菜单".to_string(),
                SettingsSubpage::Shortcuts => "快捷键".to_string(),
            };
        }
        match &self.panes[self.active_pane].path {
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
    /// 记录当前剪贴板中的内容是否从回收站剪切出来。
    pub clipboard_from_recycle: bool,
    pub recent: Vec<PathBuf>,
}

impl AppState {
    pub fn new(initial: Option<PathBuf>, sort: SortState) -> Self {
        let first = TabState::new(1, initial, sort);
        Self {
            tabs: vec![first],
            active_tab: 0,
            next_tab_id: 2,
            staging: Vec::new(),
            cut_paths: HashSet::new(),
            clipboard_paths: Vec::new(),
            clipboard_operation: "copy".into(),
            clipboard_from_recycle: false,
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

    pub fn new_tab(&mut self, path: Option<PathBuf>, sort: SortState) -> u32 {
        self.new_tab_ex(path, sort, true)
    }

    /// 新建标签，`inherit` 控制 path 为 None 时是否继承当前目录。
    ///
    /// 「+」按钮 / Ctrl+T 这类用户快捷键要继承当前目录（Explorer 习惯）；
    /// 但资源管理器送来的「此电脑」请求（`--this-pc`）必须真的进此电脑视图，
    /// 继承会把当前目录塞进去，右键此电脑就变成"再打开一次当前文件夹"。
    pub fn new_tab_ex(&mut self, path: Option<PathBuf>, sort: SortState, inherit: bool) -> u32 {
        let id = self.next_tab_id;
        self.next_tab_id += 1;
        let mut tab = TabState::new(id, path, sort);
        // 新标签马上要发起列举：必须像 navigate 那样置「正在加载」+待恢复滚动。
        // 否则 UI 认为该标签「已经加载完且内容为空」，在列举结果回来之前
        // 就把「此文件夹为空」画出来；结果回来得慢时这个空态会一直留着。
        // 每个 pane 都置一遍（分屏时两栏各自有状态）。
        for pane in tab.panes.iter_mut() {
            pane.loading = true;
            pane.pending_scroll_restore = true;
        }
        // 新标签默认继承当前目录（Explorer/Pulse 习惯）：写入当前聚焦面板。
        if inherit && tab.active().path.is_none() {
            if let Some(current) = self.active().active().path.clone() {
                let pane = tab.active_mut();
                pane.path = Some(current.clone());
                pane.history = vec![Some(current)];
                pane.history_filters = vec![String::new()];
                pane.history_scroll = vec![0.0];
            }
        }
        self.tabs.push(tab);
        self.active_tab = self.tabs.len() - 1;
        id
    }

    /// 打开或聚焦设置页标签（确保全局唯一设置标签）。
    /// - `force_subpage = false`（顶部齿轮点击）：已存在时仅切换聚焦，保持当前子页面不变；未打开时以 `subpage` 创建。
    /// - `force_subpage = true`（如右键菜单显式点击管理动作）：已存在时切换聚焦并强制切换到目标 `subpage`。
    pub fn open_settings(&mut self, subpage: SettingsSubpage, force_subpage: bool) -> u32 {
        // 全局唯一判定：只要是设置页标签即可复用
        if let Some(idx) = self.tabs.iter().position(|t| t.kind == TabKind::Settings) {
            self.active_tab = idx;
            if force_subpage {
                self.tabs[idx].settings_subpage = subpage;
            }
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

    /// 批量关闭标签的范围（右键菜单「关闭其他 / 左侧 / 右侧」）。
    ///
    /// 三种范围都以被右键的那个标签为锚点，**锚点本身永远保留** ——
    /// 这样批量关闭后至少有 1 个标签，不会出现「全关掉」的歧义。
    pub fn close_tabs_scope(&mut self, anchor_id: u32, scope: TabCloseScope) -> usize {
        let Some(anchor) = self.tab_by_id(anchor_id) else {
            return 0;
        };
        let before = self.tabs.len();
        let mut index = 0usize;
        self.tabs.retain(|_| {
            let keep = match scope {
                TabCloseScope::Others => index == anchor,
                TabCloseScope::Left => index >= anchor,
                TabCloseScope::Right => index <= anchor,
            };
            index += 1;
            keep
        });
        let closed = before - self.tabs.len();
        if closed == 0 {
            return 0;
        }
        // 当前聚焦的标签若被关掉，落到锚点上（与资源管理器一致：
        // 批量关闭后显示你右键的那一页，而不是随便找一个）。
        self.active_tab = self.tab_by_id(anchor_id).unwrap_or(0);
        closed
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

#[cfg(test)]
mod tests {
    use super::*;

    fn pane() -> PaneState {
        PaneState::new(Some(PathBuf::from("C:\\work")), SortState::default())
    }

    /// 搜索页 ↔ 目录页要能前进/后退，且清空搜索时回到搜索前的滚动位置。
    #[test]
    fn search_pages_are_navigable() {
        let mut p = pane();
        p.scroll_y = 240.0;
        p.set_filter_from_search("abc");
        assert_eq!(p.history.len(), 2);
        assert_eq!(p.filter, "abc");
        assert_eq!(p.history_filters[1], "abc");
        assert_eq!(p.scroll_y, 0.0);
        // 同一页里继续改词不新增历史项。
        p.set_filter_from_search("abcd");
        assert_eq!(p.history.len(), 2);
        assert_eq!(p.history_filters[1], "abcd");
        p.scroll_y = 80.0;
        p.set_filter_from_search("");
        assert_eq!(p.history.len(), 3);
        assert_eq!(p.scroll_y, 240.0);
        p.jump_to(1);
        assert_eq!(p.filter, "abcd");
        assert_eq!(p.scroll_y, 80.0);
        p.jump_to(2);
        assert!(p.filter.is_empty());
        assert_eq!(p.scroll_y, 240.0);
    }

    /// 导航到新路径仍是普通页面（搜索词为空），并丢弃前进记录。
    #[test]
    fn navigate_clears_search_and_forward_history() {
        let mut p = pane();
        p.set_filter_from_search("abc");
        p.navigate(Some(PathBuf::from("C:\\other")));
        assert_eq!(p.history.len(), 3);
        assert!(p.filter.is_empty());
        assert_eq!(p.history_filters.last().unwrap(), "");
        assert_eq!(p.history_index, 2);
    }

#[cfg(test)]
mod new_tab_tests {
    use super::*;

    fn state_with_one_tab(path: Option<PathBuf>) -> AppState {
        AppState::new(path, SortState::default())
    }

    #[test]
    fn new_tab_inherits_current_dir_by_default() {
        let mut st = state_with_one_tab(Some(PathBuf::from(r"D:\\重")));
        st.new_tab(None, SortState::default());
        assert_eq!(st.active().active().path, Some(PathBuf::from(r"D:\\重")));
    }

    #[test]
    fn this_pc_tab_does_not_inherit_current_dir() {
        // 资源管理器右键「此电脑」必须真的进此电脑视图（path=None），
        // 否则会变成"再打开一次当前文件夹"。
        let mut st = state_with_one_tab(Some(PathBuf::from(r"D:\\重")));
        st.new_tab_ex(None, SortState::default(), false);
        assert_eq!(st.active().active().path, None);
    }

    /// 建 5 个标签，返回各自 id。
    fn five_tabs() -> (AppState, Vec<u32>) {
        let mut st = state_with_one_tab(Some(PathBuf::from(r"D:\\a")));
        let ids = vec![st.tabs[0].id];
        let mut ids = ids;
        for name in ["b", "c", "d", "e"] {
            ids.push(st.new_tab(Some(PathBuf::from(format!("D:\\{name}"))), SortState::default()));
        }
        (st, ids)
    }

    #[test]
    fn close_others_keeps_only_the_anchor() {
        let (mut st, ids) = five_tabs();
        assert_eq!(st.close_tabs_scope(ids[2], TabCloseScope::Others), 4);
        assert_eq!(st.tabs.len(), 1);
        assert_eq!(st.active().id, ids[2]);
    }

    #[test]
    fn close_left_drops_tabs_before_the_anchor() {
        let (mut st, ids) = five_tabs();
        assert_eq!(st.close_tabs_scope(ids[2], TabCloseScope::Left), 2);
        // 锚点及右侧原样保留、顺序不变。
        let left: Vec<u32> = st.tabs.iter().map(|t| t.id).collect();
        assert_eq!(left, vec![ids[2], ids[3], ids[4]]);
        assert_eq!(st.active().id, ids[2]);
    }

    #[test]
    fn close_right_drops_tabs_after_the_anchor() {
        let (mut st, ids) = five_tabs();
        assert_eq!(st.close_tabs_scope(ids[1], TabCloseScope::Right), 3);
        let left: Vec<u32> = st.tabs.iter().map(|t| t.id).collect();
        assert_eq!(left, vec![ids[0], ids[1]]);
        assert_eq!(st.active().id, ids[1]);
    }

    #[test]
    fn close_scope_is_a_noop_when_nothing_to_close() {
        let (mut st, ids) = five_tabs();
        // 第一个标签左边没有东西、最后一个右边没有东西。
        assert_eq!(st.close_tabs_scope(ids[0], TabCloseScope::Left), 0);
        assert_eq!(st.close_tabs_scope(ids[4], TabCloseScope::Right), 0);
        assert_eq!(st.tabs.len(), 5);
    }

    #[test]
    fn close_scope_keeps_anchor_active_when_active_was_closed() {
        let (mut st, ids) = five_tabs();
        // 聚焦最后一个，然后关掉它右边的（没有）；改为聚焦第 0 个，
        // 关闭右侧 —— 锚点必须变成当前标签，而不是随便留一个。
        st.active_tab = 0;
        st.close_tabs_scope(ids[3], TabCloseScope::Right);
        assert_eq!(st.active().id, ids[3]);
    }

    #[test]
    fn close_scope_ignores_unknown_anchor() {
        let (mut st, _ids) = five_tabs();
        assert_eq!(st.close_tabs_scope(9999, TabCloseScope::Others), 0);
        assert_eq!(st.tabs.len(), 5);
    }

    #[test]
    fn new_tab_starts_in_loading_state() {
        // 不置 loading 的话 UI 会先画「此文件夹为空」，等列举结果回来才替换。
        let mut st = state_with_one_tab(Some(PathBuf::from(r"D:\\重")));
        st.new_tab(Some(PathBuf::from(r"C:\\Windows")), SortState::default());
        assert!(st.active().active().loading);
        assert!(st.active().active().pending_scroll_restore);
    }
}
}
