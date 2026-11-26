//! src/state/mod.rs
//! 应用状态：标签页、导航历史、暂存托盘、选择集。

use std::collections::HashSet;
use std::path::{Path, PathBuf};

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
    /// 本窗格使用的视图模式（details / list / icons-*）。
    ///
    /// 「独立视图和排序」打开后每栏各记各的（同一路径的两栏也可以不同）；
    /// 关掉时全部分栏统一取 `config.view_mode`。
    pub view_mode: String,
    pub filter: String,
    /// 类型筛选（表头「类型」右侧的小箭头下拉）：空 = 不过滤。
    /// 元素是类型键 —— 目录统一为 `"dir"`，文件是小写扩展名（无扩展名为 `"noext"`）。
    /// 与搜索框的 `filter` 是两套独立的收窄条件，同时生效（AND）。
    pub type_filter: Vec<String>,
    /// 与 `history` 平行：每个页面各自的类型筛选。
    /// 与 `history_filters` 同理 —— 筛选也要能前进/后退。
    pub history_types: Vec<Vec<String>>,
    /// 进入类型筛选前的滚动位置（清空筛选时回到这里，而不是弹回顶部）。
    pub pre_type_scroll_y: f32,
    /// 选择集（按路径，跨排序/过滤稳定）。
    pub selection: HashSet<PathBuf>,
    /// 范围选择锚点（路径）。
    pub anchor: Option<PathBuf>,
    /// 滚动偏移（逻辑像素），切换标签时恢复。
    pub scroll_y: f32,
    /// 上一次列举是「读失败」而不是「空目录」。切回这个标签时会自动重试一次，
    /// 见 App::retry_failed_panes。
    pub list_failed: bool,
    pub loading: bool,
    /// 导航代号：用于丢弃过期的异步列表结果。
    pub generation: u64,
    /// 与 entries 平行的拼音预计算缓存（Phase 5 拼音过滤）。
    /// 长度与 entries 不一致时惰性重建。
    pub pinyin_cache: Vec<crate::search::PinyinFields>,
    /// 拼音缓存的请求号：每次让缓存失效、以及每次发起后台计算都 +1。
    /// 后台结果回来时对不上就丢弃（条目已经换了，缓存会错位）。
    pub pinyin_seq: u64,
    /// 正在后台计算的那批拼音对应的请求号（`None` = 没有在算）。
    /// 用请求号而不是布尔：只有"同一世代的结算"才算数，过期结果不能把
    /// 新一轮的"在算"标记清掉，否则会重复派活。
    pub pinyin_pending: Option<u64>,
    /// 深度搜索（子树）附加结果：仅 filter 非空时存在。
    /// 显示下标空间 = 本地条目 [0, entries.len()) ⊕ 深度结果 [len, len+deep)。
    pub deep_entries: Vec<FileEntry>,
    /// 与 deep_entries 平行的拼音缓存（后台算好传回，供过滤与高亮）。
    pub deep_pinyin: Vec<crate::search::PinyinFields>,
    /// 深度搜索代号：每次 filter 编辑/导航递增，过期结果丢弃。
    pub deep_generation: u64,
    /// 深度搜索进行中（状态栏提示）。
    pub deep_searching: bool,
    /// 结果是被上限掐停的（不是真的扫完）：状态栏要说清楚还有多少没收。
    pub deep_capped: bool,
    /// 深搜已经扫过多少个条目（按时间节流回传，只用于状态栏进度）。
    pub deep_scanned: usize,
    /// 流式结果只追加、还没重排：等用户停手（或扫描结束）再整体排一次。
    ///
    /// 和「按大小排序」那套是同一个思路（`App::size_sort_pending`）：每批结果
    /// 都整体重排，用户正在看的那一行会被后插进来的高分条目挤走，屏幕上就是
    /// 列表一直在跳。搜索期间只往末尾接，排序留到不打扰人的时候做。
    pub deep_sort_pending: bool,
    /// 全局索引搜索的未截断命中总数；`Some(n)` 且 `display.len() < n` 时，
    /// 状态栏提示「结果已截断」。
    pub search_total: Option<usize>,
}

/// 条目在类型筛选里的键：目录统一 `dir`，文件是小写扩展名，无扩展名为 `noext`。
pub fn type_key(e: &FileEntry) -> &str {
    if e.is_dir {
        "dir"
    } else if e.ext.is_empty() {
        "noext"
    } else {
        e.ext.as_str()
    }
}

/// 改名后的条目：保留体积/时间/隐藏属性与目录体积统计状态，只重算名字、
/// 扩展名、图标风格与类型徽标（`FileEntry::from_path` 干的就是这件事）。
/// `original_path` 也照搬：那是回收站条目的还原来源，改名不该把它清掉。
fn rebuild_renamed(old: &FileEntry, new: &Path) -> FileEntry {
    let mut rebuilt =
        FileEntry::from_path(new, old.is_dir, old.size, old.modified, old.is_hidden);
    rebuilt.dir_size_loaded = old.dir_size_loaded;
    rebuilt.dir_size_partial = old.dir_size_partial;
    rebuilt.dir_size_deferred = old.dir_size_deferred;
    rebuilt.original_path = old.original_path.clone();
    rebuilt
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
            view_mode: "details".to_string(),
            filter: String::new(),
            type_filter: Vec::new(),
            history_types: vec![Vec::new()],
            pre_type_scroll_y: 0.0,
            selection: HashSet::new(),
            anchor: None,
            scroll_y: 0.0,
            loading: false,
            list_failed: false,
            generation: 0,
            pinyin_cache: Vec::new(),
            pinyin_seq: 0,
            pinyin_pending: None,
            deep_entries: Vec::new(),
            deep_pinyin: Vec::new(),
            deep_generation: 0,
            deep_searching: false,
            deep_capped: false,
            deep_scanned: 0,
            deep_sort_pending: false,
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
        self.deep_capped = false;
        self.deep_scanned = 0;
        self.deep_sort_pending = false;
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
        self.history_types.truncate(n);
        self.history_scroll.truncate(n);
        self.history.push(path.clone());
        self.history_filters.push(String::new());
        self.history_types.push(Vec::new());
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
        self.type_filter.clear();
        self.pre_filter_scroll_y = 0.0;
        self.pre_type_scroll_y = 0.0;
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
        // 类型筛选同样按页面恢复：后退能回到「筛之前」的全量列表。
        self.type_filter = self.history_types.get(index).cloned().unwrap_or_default();
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
        // 词没变就别碰历史：后退回搜索页时 App 会重放一次同一个词，
        // 这次重放若走下面的「就地更新」会把前进记录一起砍掉（前进按钮直接失效）。
        if text == self.filter {
            return;
        }
        let was_empty = self.filter.is_empty();
        let now_empty = text.is_empty();
        if let Some(s) = self.history_scroll.get_mut(self.history_index) {
            *s = self.scroll_y;
        }
        if was_empty && !now_empty {
            // 记住搜索前的滚动位置，清空搜索框时回到这里。
            self.pre_filter_scroll_y = self.scroll_y;
        }
        let scroll = if !was_empty && now_empty {
            self.pre_filter_scroll_y
        } else if was_empty && !now_empty {
            0.0
        } else {
            self.scroll_y
        };
        self.filter = text.to_string();
        let types = self.type_filter.clone();
        self.book_page_change(text.to_string(), types, scroll);
    }

    /// 改搜索词 / 改类型筛选之后的历史记账。
    ///
    /// 规则（既不能每次勾选都堆一页，也不能丢了「后退回未筛选」这一步）：
    ///  · 新状态正好等于**上一页** → 这次改动等于把当前这页取消掉，直接丢掉它
    ///    （典型：取消筛选后回到「普通目录、没筛选」）；
    ///  · 当前页已经是派生页（搜索页 / 筛选页）→ 就地改它，同一段搜索里反复
    ///    改词改筛选都不堆历史；
    ///  · 当前页是普通目录页 → 压入一页派生页，这样才退得回未筛选的状态。
    fn book_page_change(&mut self, filter: String, types: Vec<String>, scroll: f32) {
        if self.history_index > 0 {
            let i = self.history_index;
            let same_as_prev = self.history[i] == self.history[i - 1]
                && self
                    .history_filters
                    .get(i - 1)
                    .is_some_and(|f| *f == filter)
                && self.history_types.get(i - 1).is_some_and(|t| *t == types);
            if same_as_prev {
                self.history.truncate(i);
                self.history_filters.truncate(i);
                self.history_types.truncate(i);
                self.history_scroll.truncate(i);
                self.history_index = i - 1;
                self.scroll_y = scroll;
                if let Some(s) = self.history_scroll.get_mut(self.history_index) {
                    *s = scroll;
                }
                return;
            }
        }
        let derived = self
            .history_filters
            .get(self.history_index)
            .is_some_and(|f| !f.is_empty())
            || self
                .history_types
                .get(self.history_index)
                .is_some_and(|t| !t.is_empty());
        if derived {
            // 就地改这一页：顺手砍掉前进记录 —— 用户已经从那几页走开了，
            // 留着的旧状态只会在按前进时把人送回去。
            let n = self.history_index + 1;
            self.history.truncate(n);
            self.history_filters.truncate(n);
            self.history_types.truncate(n);
            self.history_scroll.truncate(n);
            if let Some(f) = self.history_filters.get_mut(self.history_index) {
                *f = filter;
            }
            if let Some(t) = self.history_types.get_mut(self.history_index) {
                *t = types;
            }
            self.scroll_y = scroll;
            if let Some(s) = self.history_scroll.get_mut(self.history_index) {
                *s = scroll;
            }
        } else {
            self.push_search_page(filter, scroll, types);
        }
    }

    fn push_search_page(&mut self, filter: String, scroll: f32, types: Vec<String>) {
        let n = self.history_index + 1;
        self.history.truncate(n);
        self.history_filters.truncate(n);
        self.history_types.truncate(n);
        self.history_scroll.truncate(n);
        self.history.push(self.path.clone());
        self.history_filters.push(filter);
        self.history_types.push(types);
        self.history_scroll.push(scroll);
        self.history_index = self.history.len() - 1;
        self.scroll_y = scroll;
    }

    /// 类型筛选变化：历史规则同搜索词（见 `book_page_change`）——
    /// 从普通目录开始筛压一页、筛选途中反复勾选只改本页、取消筛选回到目录页。
    pub fn set_type_filter(&mut self, keys: Vec<String>) {
        if keys == self.type_filter {
            return;
        }
        let was_empty = self.type_filter.is_empty();
        let now_empty = keys.is_empty();
        if let Some(s) = self.history_scroll.get_mut(self.history_index) {
            *s = self.scroll_y;
        }
        if was_empty && !now_empty {
            // 记住筛选前的滚动位置，清空筛选时回到这里。
            self.pre_type_scroll_y = self.scroll_y;
        }
        let scroll = if !was_empty && now_empty {
            self.pre_type_scroll_y
        } else if was_empty && !now_empty {
            0.0
        } else {
            self.scroll_y
        };
        self.type_filter = keys.clone();
        let filter = self.filter.clone();
        self.book_page_change(filter, keys, scroll);
    }

    /// 改名成功后就地更新条目：路径换成新名字，重算扩展名与类型徽标。
    ///
    /// 搜索态不能靠「重新列举目录」来同步：此电脑的全局搜索没有目录可列，
    /// 子目录里搜到的命中也不在当前目录的列表情单里。这里按路径把本目录条目与
    /// 子目录命中一起改掉，并**只重算这一条**的拼音字段 —— 整体作废拼音缓存会
    /// 让 `rebuild_display` 退回「全部条目」的兜底分支，搜索页会闪一下整目录。
    ///
    /// 返回是否命中；没命中说明这条结果不在本面板，调用方应退回列举刷新。
    pub fn rename_entry(&mut self, old: &Path, new: &Path) -> bool {
        let mut hit = false;
        let pinyin_ok = self.pinyin_cache.len() == self.entries.len();
        for i in 0..self.entries.len() {
            if self.entries[i].path != old {
                continue;
            }
            let rebuilt = rebuild_renamed(&self.entries[i], new);
            let name = rebuilt.name.clone();
            self.entries[i] = rebuilt;
            if pinyin_ok {
                self.pinyin_cache[i] = crate::search::pinyin_fields(&name);
            }
            hit = true;
        }
        let deep_pinyin_ok = self.deep_pinyin.len() == self.deep_entries.len();
        for j in 0..self.deep_entries.len() {
            if self.deep_entries[j].path != old {
                continue;
            }
            let rebuilt = rebuild_renamed(&self.deep_entries[j], new);
            let name = rebuilt.name.clone();
            self.deep_entries[j] = rebuilt;
            if deep_pinyin_ok {
                self.deep_pinyin[j] = crate::search::pinyin_fields(&name);
            }
            hit = true;
        }
        if hit {
            // 让在途的拼音任务作废：那一批是按旧名字算的，落地会把刚改好的
            // 这一条又覆盖回旧拼音。缓存本身已经逐项齐了，不需要重算。
            self.pinyin_seq += 1;
            self.pinyin_pending = None;
            self.rebuild_display();
        }
        hit
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

    /// 让拼音缓存失效（条目被替换 / 被重新排序时都必须调用）。
    ///
    /// 缓存与 `entries` **逐项平行**，`entries` 一变（哪怕只是原地重新排序、
    /// 长度没变）就会错位，所以不能只靠"长度不同才重建"来判断。
    pub fn invalidate_pinyin(&mut self) {
        self.pinyin_cache.clear();
        self.pinyin_seq += 1;
        self.pinyin_pending = None;
    }

    /// 按当前排序键重排条目，并把拼音缓存一起搬到新位置。
    ///
    /// 不能直接 `sort_entries` + 清缓存：那样每次点列头（以及目录体积算完后
    /// 按大小重排）都要重算一遍拼音，中文名的搜索结果会有几百毫秒「先不过滤」。
    pub fn resort(&mut self) {
        let sort = self.sort;
        if self.pinyin_cache.len() != self.entries.len() {
            crate::fs::sort_entries(&mut self.entries, sort);
            return;
        }
        // 缓存与条目逐项平行：配对后整体排序再拆开，位置关系不会错。
        let fields = std::mem::take(&mut self.pinyin_cache);
        let mut pairs: Vec<(FileEntry, crate::search::PinyinFields)> =
            std::mem::take(&mut self.entries).into_iter().zip(fields).collect();
        pairs.sort_by(|(a, _), (b, _)| crate::fs::entry_cmp(a, b, sort));
        let (entries, fields): (Vec<_>, Vec<_>) = pairs.into_iter().unzip();
        self.entries = entries;
        self.pinyin_cache = fields;
        // 顺序变了：还在算的那批结果按旧顺序生成，回来时必须丢弃（否则会用
        // 错位的拼音去匹配）。缓存本身是对的，所以不清它、也不重算。
        self.pinyin_seq += 1;
        self.pinyin_pending = None;
    }

    /// 重建过滤后的可见下标表（拼音/字面匹配 + 打分排序）。
    /// 下标空间 = 本地条目 [0, entries.len()) ⊕ 深度结果（深度条目已由
    /// 后台按同一查询过滤，直接并入统一打分排序）。
    pub fn rebuild_display(&mut self) {
        // 剥离盘符限定词（如 c:），避免冒号误杀文件名里的无关字符
        let (_, clean_query) = crate::search::parse_drive_prefix(&self.filter);
        let query = clean_query.trim().to_lowercase();
        if query.is_empty() {
            self.display = (0..self.entries.len())
                .filter(|&i| self.matches_type_filter(&self.entries[i]))
                .collect();
            return;
        }
        // 拼音缓存由后台线程算（大目录里同步算要十几到几十毫秒，会在输入
        // 第一个字符时明显卡一下）。还没算好就先用纯文本匹配兜底 ——
        // `matches_query` 对空的拼音字段会退化成只匹配文件名；算好后
        // App 那边会再调一次本函数，拼音命中与高亮随之补上。
        let no_pinyin = crate::search::PinyinFields::default();
        let mut matcher = nucleo_matcher::Matcher::new(nucleo_matcher::Config::DEFAULT);
        let mut bufs = (Vec::new(), Vec::new(), Vec::new());
        let mut scored: Vec<(usize, u16)> = Vec::new();
        for (i, e) in self.entries.iter().enumerate() {
            let fields = self.pinyin_cache.get(i).unwrap_or(&no_pinyin);
            if self.matches_type_filter(e)
                && crate::search::matches_query(&e.name, fields, &query, &mut matcher, &mut bufs)
            {
                // 加分（文件夹 +40、短名优先）跟着分数一起排：同分时文件夹在前，
                // 这也是资源管理器 / pulse 的次序。
                let s = crate::search::score_ranked(
                    &e.name,
                    fields,
                    &query,
                    &mut matcher,
                    &mut bufs,
                    e.is_dir,
                )
                .unwrap_or(0);
                scored.push((i, s));
            }
        }
        let local_len = self.entries.len();
        for (j, e) in self.deep_entries.iter().enumerate() {
            // 类型筛选对搜索结果一视同仁：子目录里搜到的也要过筛选，
            // 否则「筛了只剩 PDF」时深层命中照样往列表里塞。
            if !self.matches_type_filter(e) {
                continue;
            }
            let s = self
                .deep_pinyin
                .get(j)
                .and_then(|f| {
                    crate::search::score_ranked(&e.name, f, &query, &mut matcher, &mut bufs, e.is_dir)
                })
                .unwrap_or(0);
            scored.push((local_len + j, s));
        }
        // 拼音缓存还没算好时，纯文本匹配为空并不代表真的没有结果（中文名要靠
        // 拼音命中）。此时绝不能把列表清空 —— 那会让界面闪一下「此文件夹为空」。
        // 先退回「全部条目」给用户看，缓存到手后 App 会再调一次本函数把结果收窄。
        if self.pinyin_cache.len() != self.entries.len() && scored.is_empty() {
            // 类型筛选照样生效，否则「筛了只剩 PDF」时兜底会把全部条目又倒回来。
            // 深度结果由后台按查询词自己过滤，类型键不在那边判定，原样并入。
            self.display = (0..self.entries.len())
                .filter(|&i| self.matches_type_filter(&self.entries[i]))
                .chain(
                    (0..self.deep_entries.len())
                        .map(|j| local_len + j)
                        .filter(|&i| self.matches_type_filter(&self.deep_entries[i - local_len])),
                )
                .collect();
            return;
        }
        scored.sort_by(|a, b| b.1.cmp(&a.1));
        self.display = scored.into_iter().map(|(i, _)| i).collect();
    }

    /// 搜索结果流式到达时用：**不动已有顺序**，只把新出现的命中接到末尾。
    ///
    /// 深搜结果在后台已经按同一查询过滤过，这里只需重新判定本地条目（拼音缓存
    /// 可能刚算好，命中集合会比上一批大）。真正按得分重排留给
    /// `rebuild_display`，由 App 在用户停手或扫描结束时调用。
    pub fn extend_display_streaming(&mut self) {
        let (_, clean_query) = crate::search::parse_drive_prefix(&self.filter);
        let query = clean_query.trim().to_lowercase();
        if query.is_empty() {
            self.rebuild_display();
            return;
        }
        let local_len = self.entries.len();
        let total = local_len + self.deep_entries.len();
        // 拼音缓存没算好时只做「纯文本」判定会漏掉拼音命中，进而把已经显示的
        // 条目踢掉（闪一下空列表再回来）—— 那种情况保留原样，等缓存到了再说。
        let pinyin_ready = self.pinyin_cache.len() == local_len;
        let no_pinyin = crate::search::PinyinFields::default();
        let mut matcher = nucleo_matcher::Matcher::new(nucleo_matcher::Config::DEFAULT);
        let mut bufs = (Vec::new(), Vec::new(), Vec::new());
        let mut keep: Vec<usize> = Vec::with_capacity(self.display.len());
        let mut present = vec![false; total];
        for &idx in &self.display {
            let hit = if idx >= local_len {
                // 深度结果本来就是这次查询的命中，但类型筛选照样要过。
                self.deep_entries
                    .get(idx - local_len)
                    .is_some_and(|e| self.matches_type_filter(e))
            } else if !pinyin_ready {
                true
            } else {
                // 条目表刚被换掉（列举结果更新）而显示表还没重建时，旧下标可能
                // 越界 —— 取不到就当它已作废，绝不让这里 panic。
                match self.entries.get(idx) {
                    Some(e) => {
                        self.matches_type_filter(e)
                            && crate::search::matches_query(
                                &e.name,
                                self.pinyin_cache.get(idx).unwrap_or(&no_pinyin),
                                &query,
                                &mut matcher,
                                &mut bufs,
                            )
                    }
                    None => false,
                }
            };
            if hit && idx < total && !present[idx] {
                present[idx] = true;
                keep.push(idx);
            }
        }
        // 本地条目里新命中的（上一批之后拼音缓存/类型筛选变化带来的）。
        if pinyin_ready {
            for i in 0..local_len {
                if present[i] {
                    continue;
                }
                let e = &self.entries[i];
                if self.matches_type_filter(e)
                    && crate::search::matches_query(
                        &e.name,
                        self.pinyin_cache.get(i).unwrap_or(&no_pinyin),
                        &query,
                        &mut matcher,
                        &mut bufs,
                    )
                {
                    present[i] = true;
                    keep.push(i);
                }
            }
        }
        // 末尾接上这一批新到的深度结果（保持到达顺序），类型筛选同样生效。
        for idx in local_len..total {
            if !present[idx]
                && self
                    .deep_entries
                    .get(idx - local_len)
                    .is_some_and(|e| self.matches_type_filter(e))
            {
                keep.push(idx);
            }
        }
        self.display = keep;
    }

    /// 条目是否通过类型筛选（空筛选集 = 全通过）。
    pub fn matches_type_filter(&self, e: &FileEntry) -> bool {
        self.type_filter.is_empty() || self.type_filter.iter().any(|k| k == type_key(e))
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

    /// 当前显示的**全部**条目路径，顺序与显示一致（含深度搜索结果）。
    ///
    /// 全选 / 反选 / Shift 范围的锚点都必须用它：`entries` 里只有本地条目，
    /// 搜索时深层结果在 `deep_entries` 里，直接查 `entries` 会漏掉它们
    ///（真机表现：搜索后 `Ctrl+A` 只选了本地那几条，按住 Shift 点第二个
    /// 文件毫无反应）。
    pub fn displayed_paths(&self) -> Vec<PathBuf> {
        (0..self.display.len())
            .filter_map(|di| self.display_entry(di).map(|e| e.path.clone()))
            .collect()
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
            // 不能用 `file_name()` 直接兼底：UNC 共享根（`\\server\share`）
            // 的最后一段属于 Prefix，`file_name()` 返回 None，会退化成整条路径。
            Some(p) => crate::fs::dir::leaf_name(p),
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
                pane.history_types = vec![Vec::new()];
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

    /// 全选 / 反选 / Shift 范围用的「显示路径」必须带上深度搜索结果。
    ///
    /// 真机两条：搜索后 `Ctrl+A` 只选了本地那几条；按住 Shift 点第二个文件
    /// 毫无反应（锚点只在 `entries` 里找，深层结果找不到）。
    #[test]
    fn displayed_paths_include_deep_entries() {
        let mut p = pane();
        p.entries = vec![
            crate::fs::dir::test_entry("本地命中.mp4", false, 0),
            crate::fs::dir::test_entry("别的.mp4", false, 0),
        ];
        p.deep_entries = vec![crate::fs::dir::test_entry("深层命中.mp4", false, 0)];
        p.filter = "命中".into();
        p.rebuild_display();

        let paths: Vec<String> = p
            .displayed_paths()
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            paths,
            vec!["本地命中.mp4".to_string(), "深层命中.mp4".to_string()],
            "显示序列里的深层结果不能漏"
        );
    }

    /// 流式搜索：新到的结果**接在末尾**，不打断已有顺序；排序等用户停手。
    ///
    /// 真机反馈是「列表一直在跳」——每批都整体按分数重排，正在看的那一行会被
    /// 后面插进来的高分条目挤走。这里先追加，再由 App 的空闲防抖排一次。
    #[test]
    fn streaming_results_append_without_reordering() {
        let mut p = pane();
        p.filter = "ziyan".into();
        p.entries.clear();
        p.deep_entries = vec![crate::fs::dir::test_entry("子様.mp4", false, 0)];
        p.deep_pinyin = vec![crate::search::pinyin_fields("子様.mp4")];
        p.rebuild_display();
        assert_eq!(p.display, vec![0]);

        // 高得多的命中后到：追加，不许插队。
        p.deep_entries
            .push(crate::fs::dir::test_entry("紫颜-小仙紫.mp4", false, 0));
        p.deep_pinyin
            .push(crate::search::pinyin_fields("紫颜-小仙紫.mp4"));
        p.extend_display_streaming();
        assert_eq!(p.display, vec![0, 1], "已有顺序不能被打乱");

        // 停手后整体重排：高分回到最前。
        p.rebuild_display();
        assert_eq!(p.display, vec![1, 0]);
    }

    /// 类型筛选键：目录统一 `dir`，文件是小写扩展名，无扩展名 `noext`。
    #[test]
    fn type_key_separates_dirs_files_and_extensionless() {
        let dir = crate::fs::dir::test_entry("子目录", true, 0);
        assert_eq!(type_key(&dir), "dir");
        let mut pdf = crate::fs::dir::test_entry("a.PDF", false, 0);
        pdf.ext = "pdf".into();
        assert_eq!(type_key(&pdf), "pdf");
        let plain = crate::fs::dir::test_entry("LICENSE", false, 0);
        assert_eq!(type_key(&plain), "noext");
    }

    /// 类型筛选按扩展名/目录收窄，并且与搜索词是「且」的关系。
    #[test]
    fn type_filter_narrows_display_and_combines_with_search() {
        let mut p = pane();
        let mut pdf = crate::fs::dir::test_entry("报告.pdf", false, 0);
        pdf.ext = "pdf".into();
        let mut txt = crate::fs::dir::test_entry("readme.txt", false, 0);
        txt.ext = "txt".into();
        let folder = crate::fs::dir::test_entry("子目录", true, 0);
        p.entries = vec![folder, pdf, txt];
        // 拼音缓存长度对齐（默认值即可）：否则 rebuild_display 会走
        // 「拼音没算好先显示全部」的兜底分支，测不到真正的匹配路径。
        p.pinyin_cache = vec![Default::default(); 3];

        p.type_filter = vec!["pdf".into()];
        p.rebuild_display();
        assert_eq!(p.display, vec![1]);

        // 与搜索词同时生效：类型对上但名字对不上，仍是空。
        p.filter = "readme".into();
        p.rebuild_display();
        assert!(p.display.is_empty());
        p.filter.clear();

        // 只筛目录。
        p.type_filter = vec!["dir".into()];
        p.rebuild_display();
        assert_eq!(p.display, vec![0]);

        // 多选：pdf + txt。
        p.type_filter = vec!["pdf".into(), "txt".into()];
        p.rebuild_display();
        assert_eq!(p.display, vec![1, 2]);

        // 空筛选集 = 不过滤。
        p.type_filter.clear();
        p.rebuild_display();
        assert_eq!(p.display, vec![0, 1, 2]);
    }

    /// 类型筛选必须同样作用在「子目录里搜到的结果」上：以前深层命中直接放行，
    /// 筛了只剩 PDF 时深层里的 txt/mp4 照样往列表里塞，筛选看着只对当前目录有效。
    #[test]
    fn type_filter_applies_to_deep_search_hits() {
        let mut p = pane();
        let mut pdf = crate::fs::dir::test_entry("深层报告.pdf", false, 0);
        pdf.ext = "pdf".into();
        p.entries = vec![pdf];
        p.pinyin_cache = vec![Default::default(); 1];

        let mut deep_txt = crate::fs::dir::test_entry("深层.txt", false, 0);
        deep_txt.ext = "txt".into();
        let mut deep_pdf = crate::fs::dir::test_entry("深层.pdf", false, 0);
        deep_pdf.ext = "pdf".into();
        p.deep_entries = vec![deep_txt, deep_pdf];
        p.deep_pinyin = vec![Default::default(); 2];
        p.filter = "深层".into();

        let sorted = |d: &Vec<usize>| {
            let mut v = d.clone();
            v.sort();
            v
        };
        // 不筛：本目录 1 条 + 深层 2 条，共 3 条（得分顺序不在此断言）。
        p.rebuild_display();
        assert_eq!(sorted(&p.display), vec![0, 1, 2]);

        // 只留 pdf：本目录的 pdf + 深层的 pdf，深层 txt 必须被剔掉。
        p.type_filter = vec!["pdf".into()];
        p.rebuild_display();
        assert_eq!(sorted(&p.display), vec![0, 2]);

        // 流式追加路径同样要过筛选：已显示的深层 txt 要被剔除。
        p.display = vec![0, 1, 2];
        p.extend_display_streaming();
        assert_eq!(sorted(&p.display), vec![0, 2]);
    }

    /// 类型筛选要能前进/后退：开始筛压一页、中途改只改本页、清空筛回到目录页。
    #[test]
    fn type_filter_participates_in_history() {
        let mut p = pane();
        p.entries = vec![crate::fs::dir::test_entry("a.pdf", false, 0)];
        p.set_type_filter(vec!["pdf".into()]);
        assert_eq!(p.history.len(), 2, "开始筛选应压入一页");
        assert_eq!(p.history_types[1], vec!["pdf".to_string()]);
        assert!(p.can_go_back());

        // 中途改筛选：不新增历史页，只更新本页。
        p.set_type_filter(vec!["pdf".into(), "txt".into()]);
        assert_eq!(p.history.len(), 2);
        assert_eq!(p.history_types[1].len(), 2);

        // 后退：回到筛之前的全量页，筛选被清掉。
        p.jump_to(0);
        assert!(p.type_filter.is_empty());
        // 前进：筛选恢复。
        p.jump_to(1);
        assert_eq!(p.type_filter, vec!["pdf".to_string(), "txt".to_string()]);

        // 清空筛选 = 再压一页。
        p.set_type_filter(Vec::new());
        assert!(p.type_filter.is_empty());
        assert!(p.history_types[p.history_index].is_empty());
    }

    /// 拼音缓存没算好时用纯文本兜底，算好后拼音命中随之补上。
    #[test]
    fn display_falls_back_to_text_then_upgrades_to_pinyin() {
        let mut p = pane();
        p.entries = vec![
            crate::fs::dir::test_entry("测试文件.txt", false, 0),
            crate::fs::dir::test_entry("readme.txt", false, 0),
        ];
        p.filter = "ceshi".into();
        // 缓存还没算好：纯文本匹配不到中文名的拼音，此时不能清空列表
        // （会让界面闪「此文件夹为空」），而是先显示全部条目。
        p.rebuild_display();
        assert_eq!(p.display, vec![0, 1]);
        // 后台算好写回后，同一条查询就能命中。
        p.invalidate_pinyin();
        let seq = p.pinyin_seq;
        p.pinyin_cache = p
            .entries
            .iter()
            .map(|e| crate::search::pinyin_fields(&e.name))
            .collect();
        p.rebuild_display();
        assert_eq!(p.display, vec![0]);
        assert_eq!(p.pinyin_seq, seq, "填缓存本身不该再让缓存失效");
        // 原地重排必须让缓存失效（否则缓存与 entries 错位）。
        p.invalidate_pinyin();
        assert_eq!(p.pinyin_seq, seq + 1);
        assert!(p.pinyin_cache.is_empty());
        assert!(p.pinyin_pending.is_none());
    }

    /// 重排条目时拼音缓存必须跟着走，不能只是"排完再清缓存"。
    #[test]
    fn resort_keeps_pinyin_aligned() {
        let mut p = pane();
        p.entries = vec![
            crate::fs::dir::test_entry("b.txt", false, 0),
            crate::fs::dir::test_entry("测试.txt", false, 0),
        ];
        p.pinyin_cache = p
            .entries
            .iter()
            .map(|e| crate::search::pinyin_fields(&e.name))
            .collect();
        p.sort = SortState {
            key: crate::fs::SortKey::Name,
            ascending: true,
        };
        let seq = p.pinyin_seq;
        p.resort();
        // 名称升序：b.txt 排在中文名之后（自然序按字符比较）。
        let idx = p
            .entries
            .iter()
            .position(|e| e.name == "b.txt")
            .expect("b.txt 还在");
        assert_eq!(p.pinyin_cache.len(), p.entries.len());
        assert_eq!(p.pinyin_cache[idx].full.as_ref(), "b.txt");
        // 缓存本身留着（不用重算），但请求号要前进：万一有后台结果正按旧顺序
        // 往回送，必须让它对不上号而被丢弃，否则会用错位的拼音去匹配。
        assert_eq!(p.pinyin_seq, seq + 1, "重排要让在途结果作废");
        assert!(p.pinyin_pending.is_none());
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
        // 清空搜索框 = 回到搜索前的目录页，不再多留一页「空搜索页」。
        p.set_filter_from_search("");
        assert_eq!(p.history.len(), 1);
        assert!(p.filter.is_empty());
        assert_eq!(p.scroll_y, 240.0);
    }

    /// 反复改搜索词 + 反复改筛选，历史里只应留「目录页 + 最终那一页」。
    /// 例：搜 x → 筛 mp4 → 取消 → 筛 avi → 改词 xp。
    #[test]
    fn repeated_search_and_filter_changes_leave_one_page() {
        let mut p = pane();
        p.set_filter_from_search("x");
        p.set_type_filter(vec!["mp4".into()]);
        p.set_type_filter(Vec::new());
        p.set_type_filter(vec!["avi".into()]);
        p.set_filter_from_search("xp");
        assert_eq!(p.history.len(), 2, "整段搜索只算一页");
        assert_eq!(p.history_index, 1);
        assert_eq!(p.filter, "xp");
        assert_eq!(p.history_filters[1], "xp");
        assert_eq!(p.history_types[1], vec!["avi".to_string()]);
        p.jump_to(0);
        assert!(p.filter.is_empty());
        assert!(p.type_filter.is_empty());
        p.jump_to(1);
        assert_eq!(p.filter, "xp");
        assert_eq!(p.type_filter, vec!["avi".to_string()]);
    }

    /// 改名要就地更新条目与拼音缓存：搜索态不能靠重新列举（此电脑全局搜索
    /// 没有目录可列，子目录里搜到的命中也不在列表情单里）。
    #[test]
    fn rename_updates_entry_in_place() {
        let mut p = pane();
        let mut a = crate::fs::dir::test_entry("a.txt", false, 0);
        a.ext = "txt".into();
        p.entries = vec![a];
        p.pinyin_cache = vec![crate::search::pinyin_fields("a.txt")];
        let old = p.entries[0].path.clone();
        let new = old.with_file_name("报告.pdf");
        assert!(p.rename_entry(&old, &new));
        assert_eq!(p.entries[0].name, "报告.pdf");
        assert_eq!(p.entries[0].ext, "pdf");
        assert!(p.entries[0].path.ends_with("报告.pdf"));
        // 拼音缓存不能整体作废（那会让搜索页闪一下整目录），只重算这一条。
        assert_eq!(p.pinyin_cache.len(), 1);
        let want = crate::search::pinyin_fields("报告.pdf");
        assert_eq!(p.pinyin_cache[0].full.as_ref(), want.full.as_ref());
        assert_eq!(p.pinyin_cache[0].full.as_ref(), "baogao.pdf");
        assert_eq!(p.display, vec![0]);
    }

    /// 子目录里的搜索命中改名后同样要更新（名字/拼音都换），且仍留在结果里 ——
    /// 深层命中是后台按查询词筛过的一批结果，不逐条重匹配；用户刚改完名，
    /// 直接把那一行抽走反而更莫名其妙。
    #[test]
    fn rename_updates_deep_hit_and_respects_query() {
        let mut p = pane();
        p.entries.clear();
        p.pinyin_cache.clear();
        let mut d = crate::fs::dir::test_entry("deep.txt", false, 0);
        d.ext = "txt".into();
        let old = d.path.clone();
        p.deep_entries = vec![d];
        p.deep_pinyin = vec![crate::search::pinyin_fields("deep.txt")];
        p.filter = "deep".into();
        p.rebuild_display();
        assert_eq!(p.display, vec![0]);

        let new = old.with_file_name("zzz.txt");
        assert!(p.rename_entry(&old, &new));
        assert_eq!(p.deep_entries[0].name, "zzz.txt");
        assert_eq!(p.deep_pinyin[0].full.as_ref(), "zzz.txt");
        assert_eq!(p.display, vec![0]);
    }

    /// 后退回搜索页时 App 会重放同一次搜索：不许把前进记录砍掉。
    #[test]
    fn replayed_search_keeps_forward_pages() {
        let mut p = pane();
        p.set_filter_from_search("x");
        p.navigate(Some(PathBuf::from("C:\\other")));
        p.jump_to(1);
        assert!(p.can_go_forward());
        p.set_filter_from_search("x");
        assert!(p.can_go_forward(), "重放同一个搜索词不该清掉前进记录");
        assert_eq!(p.history.len(), 3);
    }

    /// 后退之后又改筛选/改词：前进记录要作废（否则前进会回到改了之前的旧状态）。
    #[test]
    fn editing_after_back_drops_forward_pages() {
        let mut p = pane();
        p.set_type_filter(vec!["mp4".into()]);
        p.navigate(Some(PathBuf::from("C:\\other")));
        p.jump_to(0);
        assert!(p.can_go_forward());
        p.set_type_filter(vec!["avi".into()]);
        assert_eq!(p.history.len(), 2);
        assert!(!p.can_go_forward(), "改筛选后前进记录必须作废");
    }

    /// 普通目录：筛文件夹 → 取消 → 筛 mp4，历史只应留「未筛选、筛选mp4」。
    #[test]
    fn plain_dir_filter_collapses_when_cancelled() {
        let mut p = pane();
        p.set_type_filter(vec!["文件夹".into()]);
        assert_eq!(p.history.len(), 2);
        p.set_type_filter(Vec::new());
        assert_eq!(p.history.len(), 1, "取消筛选不该留下重复的目录页");
        p.set_type_filter(vec!["mp4".into()]);
        assert_eq!(p.history.len(), 2);
        assert_eq!(p.history_types[1], vec!["mp4".to_string()]);
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
