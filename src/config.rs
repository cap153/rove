//! src/config.rs
//! 配置持久化：主题、最近目录、窗口尺寸、暂存盘展开态。
//! 存储于 `%APPDATA%\Rove\config.json`。

use std::path::PathBuf;

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum ThemeChoice {
    /// 跟随系统（Windows「个性化 → 颜色」里的应用深浅）。
    #[default]
    System,
    Light,
    Dark,
}

/// 列宽默认值（逻辑像素）。顺序：名称 / 修改时间 / 类型 / 大小 / 路径。
pub const DEFAULT_COLUMN_WIDTHS: [f32; 5] = [320.0, 150.0, 116.0, 96.0, 240.0];
/// 单列最小 / 最大宽度，防止拖到不可用或撑爆。
pub const MIN_COLUMN_W: f32 = 60.0;
pub const MAX_COLUMN_W: f32 = 2000.0;

/// 上次会话的一个标签页快照（含分屏）。
#[derive(Clone, Debug)]
pub struct TabSnapshot {
    /// 各分屏的路径字符串；None 表示「此电脑」。
    pub paths: Vec<Option<String>>,
    pub active_pane: usize,
    pub is_settings: bool,
}

/// 批量重命名窗口上次用过的输入与选项。
///
/// 同时开多个窗口时，记住**最后一个关闭的**那个：用户关窗的动作就是
/// "这一套设置我下次还要用" 的信号。
#[derive(Clone, Debug, PartialEq)]
pub struct BatchRenamePrefs {
    pub search: String,
    pub replace: String,
    pub use_regex: bool,
    pub match_all: bool,
    pub case_sensitive: bool,
    /// 搜索串同时按全拼匹配（`baogao` → 「报告」）。
    pub pinyin: bool,
    /// 0 = 文件名 + 扩展名，1 = 文件名，2 = 扩展名。
    pub apply_to: i32,
}

impl Default for BatchRenamePrefs {
    fn default() -> Self {
        Self {
            search: String::new(),
            replace: String::new(),
            use_regex: false,
            match_all: false,
            case_sensitive: false,
            pinyin: true,
            apply_to: 0,
        }
    }
}

/// 解析 `config.json` 里的 `batch_rename` 段。缺字段一律保留默认值
/// （「拼音」默认勾选，老配置文件里没有这个键时不能被读成关）。
fn apply_batch_rename_json(p: &mut BatchRenamePrefs, br: &serde_json::Value) {
    if let Some(v) = br.get("search").and_then(|v| v.as_str()) {
        p.search = v.to_string();
    }
    if let Some(v) = br.get("replace").and_then(|v| v.as_str()) {
        p.replace = v.to_string();
    }
    if let Some(v) = br.get("use_regex").and_then(|v| v.as_bool()) {
        p.use_regex = v;
    }
    if let Some(v) = br.get("match_all").and_then(|v| v.as_bool()) {
        p.match_all = v;
    }
    if let Some(v) = br.get("case_sensitive").and_then(|v| v.as_bool()) {
        p.case_sensitive = v;
    }
    if let Some(v) = br.get("pinyin").and_then(|v| v.as_bool()) {
        p.pinyin = v;
    }
    if let Some(v) = br.get("apply_to").and_then(|v| v.as_i64()) {
        p.apply_to = v.clamp(0, 2) as i32;
    }
}

/// 从配置 JSON 解析主题选择。
///
/// 键名从早期的 `theme` 变成 `theme-mode`：老配置里的 `theme` 只可能是
/// "light"（那时界面还不能选深色），一律当「跟随系统」才符合新默认；
/// 手动改过 "dark" 的仍然认，免得把人手写的设置抹掉。
fn theme_from_json(json: &serde_json::Value) -> ThemeChoice {
    match json.get("theme-mode").and_then(|v| v.as_str()) {
        Some("light") => ThemeChoice::Light,
        Some("dark") => ThemeChoice::Dark,
        Some(_) => ThemeChoice::System,
        None => match json.get("theme").and_then(|v| v.as_str()) {
            Some("dark") => ThemeChoice::Dark,
            _ => ThemeChoice::System,
        },
    }
}

#[derive(Clone, Debug)]
pub struct Config {
    pub theme: ThemeChoice,
    pub recent: Vec<PathBuf>,
    pub window_width: u32,
    pub window_height: u32,
    /// 上次窗口左上角位置（逻辑像素）。-1 = 尚未记录（首次启动）。
    pub window_x: i32,
    pub window_y: i32,
    /// 上次退出时是否处于最大化状态（仅 Windows 有效）。
    pub window_maximized: bool,
    /// 侧边栏当前宽度
    pub sidebar_width: f32,
    pub staging_expanded: bool,
    /// 「最近使用」是否展开（默认 false 收起）。
    pub recent_expanded: bool,
    /// 「快速访问」是否展开（默认 true 展开）。
    pub quick_expanded: bool,
    /// 「此电脑」是否展开（默认 true 展开）。
    pub computer_expanded: bool,
    /// 「网络位置」是否展开（默认 true 展开）。
    pub network_expanded: bool,
    /// 资源管理器里已有的网络位置（自动检测）是否展开（默认 true）。
    pub explorer_net_expanded: bool,
    /// 侧栏分区顺序（分区 key：quick / recent / computer / network / explorer-net）。
    pub sidebar_order: Vec<String>,
    /// 视图模式：details | list | icons-small | icons-medium | icons-large | icons-xl。
    pub view_mode: String,
    /// 显示语言：system（跟随系统）| zh-CN | en。
    pub language: String,
    /// 启动时打开：last_tabs（上次的标签页，含分屏）| computer（电脑选项）。
    pub startup_tab: String,
    /// 批量重命名窗口上次的输入与选项。
    pub batch_rename: BatchRenamePrefs,
    /// 「上次的标签页」快照，关闭时写入。
    pub last_tabs: Vec<TabSnapshot>,
    pub last_active_tab: usize,
    /// 全局排序字段：name | size | modified | type | path。
    pub sort_key: String,
    /// 全局排序方向：true 递增 / false 递减。
    pub sort_ascending: bool,
    /// 「此电脑」视图独立的排序字段：drive | name | size | modified | type | path。
    /// 该视图没有「修改时间」列，主键是「盘符」；独立存放以免被全局排序带走。
    pub this_pc_sort_key: String,
    pub this_pc_sort_ascending: bool,
    /// 「回收站」视图独立的排序字段（默认修改时间递减）。
    pub recycle_sort_key: String,
    pub recycle_sort_ascending: bool,
    /// 详细信息列宽一律自适应（不按目录持久化）：用户手动拖宽只在本次会话生效，
    /// 切目录后自动重新按内容 + 视口估算，避免「某目录被记住过宽 → 一直横向滚动」。
    /// 是否显示隐藏文件/文件夹/驱动器（默认 false，即过滤掉）。
    pub show_hidden: bool,
    /// 「+」新建标签后是否自动进入地址栏编辑态（默认 false，不自动进）。
    pub auto_edit_path_on_new_tab: bool,
    /// 接管资源管理器：开启后文件夹 / 磁盘 / 此电脑的默认打开程序变成 Rove
    /// （只写 HKCU\Software\Classes，见 src/sys/integration.rs）。
    /// 首次启动默认关闭，用户改过之后持久化记住。
    pub explorer_takeover: bool,
    /// 用户自定义快捷键映射: ActionId -> [Chord1, Chord2]（空字符串代表已取消绑定）。
    pub shortcuts: std::collections::HashMap<String, Vec<String>>,
    /// 排除建立索引的驱动器盘符（大写，如 ["D", "E"]，为空表示全部索引）。
    pub search_excluded_drives: Vec<String>,
    /// 用户自定义排除的路径/通配符列表（如 `**/*.log`, `C:/Cache`）。
    pub search_excluded_paths: Vec<String>,
    /// 要隐藏的系统右键菜单项关键词（包含即隐藏；`!` 开头表示精确匹配）。
    pub context_menu_hide: Vec<String>,
    /// 空闲重新索引间隔（分钟，0 表示关闭）。
    pub search_idle_rescan_minutes: u32,
    /// 搜索最大展示结果数。
    pub search_max_results: usize,
    /// 是否首次启动（用于触发 SSD 检测与初始自动化配置）。
    pub is_first_run: bool,
    /// 用户添加的网络位置（别名 + 地址）。
    pub network_locations: Vec<NetworkLocation>,
    /// 是否已完成侧边栏宽度的历史数据修复（旧版 bug 会把被动挤压后的宽度落盘）。
    pub sidebar_width_repaired: bool,
    /// 「快速访问」条目：**数组顺序就是显示顺序**，拖动排序、取消固定、固定新文件夹
    /// 都直接改这个数组（与 `network_locations` 同一套持久化方式）。
    pub quick_items: Vec<QuickItem>,
    /// 是否已把新增的默认隐藏菜单项补进旧版 `context_menu_hide`。
    /// 只补一次，之后用户在设置页删除这些项不会被启动逻辑强行加回来。
    pub context_menu_hide_seeded: bool,
    /// 默认隐藏项补种的代次（0 = 老配置）。
    ///
    /// 每次给 `default_context_menu_hide()` 加新关键词就把代次加一，老配置
    /// 启动时按代次补齐缺少的那几条 —— 用单个整数而不是一串 bool，加一条
    /// 就多一个字段的写法撑不了多久。
    pub context_menu_hide_gen: u32,
}

/// 当前默认隐藏项的补种代次（与 `Config::load` 里的补齐逻辑对应）。
pub const CONTEXT_MENU_HIDE_GEN: u32 = 8;

/// 侧边栏「网络位置」里的一条用户自定义项。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NetworkLocation {
    /// 侧边栏显示的名字。
    pub label: String,
    /// `\\host\share` 形式的地址。
    pub path: String,
}

/// 侧边栏「快速访问」里的一条。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct QuickItem {
    /// 稳定标识：系统目录 "desktop"…"videos"、"recycle"；用户固定项 "pin:<路径>"。
    pub id: String,
    /// 侧边栏显示名。
    pub label: String,
    /// 空 = 系统项（含回收站，运行时由 `quick_access_dirs()` 解析路径）；
    /// 非空 = 用户固定文件夹的绝对路径。
    pub path: String,
}

impl QuickItem {
    /// 特殊项：可拖动调位置，但不允许取消固定。
    pub fn is_special(&self) -> bool {
        self.id == "recycle"
    }
}

/// 「快速访问」的默认条目与顺序（老配置没有 `quick_items` 字段时用这套）。
pub fn default_quick_items() -> Vec<QuickItem> {
    [
        ("desktop", "桌面"),
        ("downloads", "下载"),
        ("pictures", "图片"),
        ("documents", "文档"),
        ("music", "音乐"),
        ("videos", "视频"),
        ("recycle", "回收站"),
    ]
    .iter()
    .map(|(id, label)| QuickItem {
        id: (*id).to_string(),
        label: (*label).to_string(),
        path: String::new(),
    })
    .collect()
}

/// 侧栏分区的默认顺序。
pub fn default_sidebar_order() -> Vec<String> {
    [
        "network",
        "explorer-net",
        "computer",
        "quick",
        "recent",
    ]
        .iter()
        .map(|s| s.to_string())
        .collect()
}

/// 默认隐藏的系统菜单项。
///
/// 前四条（复制/剪切/粘贴/刷新）是系统菜单里的标准命令：Rove 自己有同名的项，
/// 系统的这几项拿进来用不了（剪贴板内容读不出、刷新不清缓存），必须默认藏掉。
/// 「固定到快速访问」同理：系统那项固定到资源管理器，Rove 自己的动作固定到
/// rove 侧边栏，两者语义不同，不能让用户误点。
/// 用 `!` 精确匹配，避免连带藏掉「复制到文件夹」这类变体。
/// 中英两份都列：系统语言不同时靠另一份生效。
///
/// 最后两条「撤消 / Undo」刻意**不带** `!`：Shell 那一项的文字跟着上一次
/// 操作变（「撤消复制」「撤消移动」「撤消删除」/ "Undo Copy"…），精确匹配
/// 一条都藏不掉。Rove 自己有撤消栈（Ctrl+Z），系统那项走的是 Explorer 的
/// 历史，对不上，点下去只会让人以为撤消失灵。
pub fn default_context_menu_hide() -> Vec<String> {
    [
        "!Copy",
        "!Cut",
        "!Paste",
        "!Refresh",
        "!复制(C)",
        "!剪切(T)",
        "!粘贴(P)",
        "!刷新(E)",
        "!Pin to Quick access",
        "!固定到快速访问",
        "!Folder",
        "!文件夹(F)",
        // 系统菜单那两项「删除(D)」/ "Delete" 走 Explorer 的删除，Rove 记不了账
        // （撤消栈里没有它），Rove 自己的删除项排在「剪切」下面，所以精确藏掉。
        "!删除(D)",
        "!Delete",
        // 属性 / 重命名 / 创建快捷方式 / 桌面快捷方式：Rove 自己已有同名项
        // （桌面快捷方式在主菜单里，系统的那个藏在「发送到」子菜单里），
        // 系统那几项留着就是同一个功能出两份，全部精确藏掉。
        "!属性(R)",
        "!重命名(M)",
        "!创建快捷方式(S)",
        "!桌面快捷方式",
        "!Properties",
        "!Rename",
        "!Create shortcut",
        "!Desktop (create shortcut)",
        // 「打开方式」有两种：带省略号的弹系统选择器（Rove 自己已经有同名项，
        // 精确藏掉），不带省略号的会展开「程序」子菜单，那个保留不动。
        // 省略号在精确匹配里是有意义的：`normalize_label` 会把结尾的点吃掉，
        // 只靠文字分不出这两条（见 sys::menu::keyword_hit）。
        "!打开方式(H)...",
        "!Open with...",
        // 兜底：万一某版系统给的带点项没有热键尾巴（`打开方式...`），
        // 「不带点 = 展开子菜单」那条仍然匹配不上，不会被误伤。
        "!打开方式...",
        "Undo",
        "撤消",
        // 重做也按包含匹配：Shell 那项的文字同样会跟着操作变
        // （「恢复复制」「恢复移动」/ "Redo Copy"…），跟撤消那两条一致。
        "Redo",
        "恢复",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

impl Default for Config {
    fn default() -> Self {
        Self {
            theme: ThemeChoice::System,
            recent: Vec::new(),
            window_width: 1150,
            window_height: 690,
            window_x: -1,
            window_y: -1,
            window_maximized: false,
            sidebar_width: 236.0,
            staging_expanded: true,
            recent_expanded: false, // 默认收起
            quick_expanded: true,   // 默认展开
            computer_expanded: true,// 默认展开
            network_expanded: true, // 默认展开
            explorer_net_expanded: true,
            sidebar_order: default_sidebar_order(),
            view_mode: "details".to_string(),
            language: "system".to_string(),
            batch_rename: BatchRenamePrefs::default(),
            startup_tab: "last_tabs".to_string(),
            last_tabs: Vec::new(),
            last_active_tab: 0,
            sort_key: "name".to_string(),
            sort_ascending: true,
            this_pc_sort_key: "drive".to_string(),
            this_pc_sort_ascending: true,
            recycle_sort_key: "modified".to_string(),
            recycle_sort_ascending: false,

            show_hidden: false,
            auto_edit_path_on_new_tab: false,
            explorer_takeover: false,
            shortcuts: std::collections::HashMap::new(),
            search_excluded_drives: Vec::new(),
            search_excluded_paths: Vec::new(),
            context_menu_hide: default_context_menu_hide(),
            search_idle_rescan_minutes: 60,
            search_max_results: 1000,
            is_first_run: true,
            network_locations: Vec::new(),
            sidebar_width_repaired: false,
            quick_items: default_quick_items(),
            context_menu_hide_seeded: true,
            context_menu_hide_gen: CONTEXT_MENU_HIDE_GEN,
        }
    }
}

/// 把用户输入的服务器地址整理成 `\\host` 或 `\\host\share` 形式。
/// 允许用 `/` 分隔，也允许不写开头的 `\\`（直接写 `192.168.1.5/Video` 也行）。
/// 只写主机（`\\192.168.1.5` / `\\nas`）也是合法的：资源管理器能直接列出该主机的共享。
pub fn normalize_network_path(input: &str) -> Option<String> {
    let mut text = input.trim().replace('/', "\\");
    while text.ends_with('\\') {
        text.pop();
    }
    if text.is_empty() {
        return None;
    }
    if !text.starts_with("\\\\") {
        text.insert_str(0, if text.starts_with('\\') { "\\" } else { "\\\\" });
    }
    let host = text[2..].split('\\').next().unwrap_or("").trim();
    if host.is_empty() {
        return None;
    }
    Some(text)
}

/// 别名留空时取地址尾段：`\\192.168.1.5\Video` → `Video`，
/// `\\192.168.1.5` / `\\nas` → `192.168.1.5` / `nas`。
pub fn network_label_from_path(path: &str) -> String {
    let trimmed = path.trim_end_matches('\\');
    trimmed
        .rsplit('\\')
        .find(|segment| !segment.is_empty())
        .unwrap_or(trimmed)
        .to_string()
}

impl Config {
    pub fn config_path() -> Option<PathBuf> {
        let appdata = std::env::var_os("APPDATA").map(PathBuf::from)?;
        Some(appdata.join("Rove").join("config.json"))
    }

    pub fn load() -> Self {
        let Some(path) = Self::config_path() else {
            return Self::default();
        };
        let Ok(text) = std::fs::read_to_string(&path) else {
            return Self::default();
        };
        let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) else {
            return Self::default();
        };
        let mut config = Self::default();
        config.theme = theme_from_json(&json);
        if let Some(recent) = json.get("recent").and_then(|v| v.as_array()) {
            config.recent = recent
                .iter()
                .filter_map(|v| v.as_str().map(PathBuf::from))
                .collect();
        }
        if let Some(w) = json.get("window_width").and_then(|v| v.as_u64()) {
            if (480..=10000).contains(&w) {
                config.window_width = w as u32;
            }
        }
        if let Some(h) = json.get("window_height").and_then(|v| v.as_u64()) {
            if (360..=10000).contains(&h) {
                config.window_height = h as u32;
            }
        }
        if let Some(x) = json.get("window_x").and_then(|v| v.as_i64()) {
            if (-10000..=10000).contains(&x) {
                config.window_x = x as i32;
            }
        }
        if let Some(y) = json.get("window_y").and_then(|v| v.as_i64()) {
            if (-10000..=10000).contains(&y) {
                config.window_y = y as i32;
            }
        }
        if let Some(sw) = json.get("sidebar_width").and_then(|v| v.as_f64()) {
            if (70.0..=1200.0).contains(&sw) {
                config.sidebar_width = sw as f32;
            }
        }
        if let Some(expanded) = json.get("staging_expanded").and_then(|v| v.as_bool()) {
            config.staging_expanded = expanded;
        }
        if let Some(expanded) = json.get("recent_expanded").and_then(|v| v.as_bool()) {
            config.recent_expanded = expanded;
        }
        if let Some(expanded) = json.get("quick_expanded").and_then(|v| v.as_bool()) {
            config.quick_expanded = expanded;
        }
        if let Some(expanded) = json.get("computer_expanded").and_then(|v| v.as_bool()) {
            config.computer_expanded = expanded;
        }
        if let Some(expanded) = json.get("network_expanded").and_then(|v| v.as_bool()) {
            config.network_expanded = expanded;
        }
        if let Some(expanded) = json.get("explorer_net_expanded").and_then(|v| v.as_bool()) {
            config.explorer_net_expanded = expanded;
        }
        if let Some(order) = json.get("sidebar_order").and_then(|v| v.as_array()) {
            let keys: Vec<String> = order
                .iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect();
            // 已知 key 按用户顺序保留，缺失的（如升级新增的）补到末尾，
            // 未知 key 丢弃。这样升级后不会把用户的顺序重置成默认值；
            // 用户从未调整过时，加载的就是存盘的默认顺序，不受此合并影响。
            let known = default_sidebar_order();
            let mut merged: Vec<String> = Vec::new();
            for key in keys {
                if known.contains(&key) && !merged.contains(&key) {
                    merged.push(key);
                }
            }
            for key in known {
                if !merged.contains(&key) {
                    merged.push(key);
                }
            }
            config.sidebar_order = merged;
        }
        if let Some(vm) = json.get("view_mode").and_then(|v| v.as_str()) {
            if matches!(
                vm,
                "details" | "list" | "icons-small" | "icons-medium" | "icons-large" | "icons-xl"
            ) {
                config.view_mode = vm.to_string();
            }
        }
        if let Some(l) = json.get("language").and_then(|v| v.as_str()) {
            if matches!(l, "system" | "zh-CN" | "en") {
                config.language = l.to_string();
            }
        }
        if let Some(br) = json.get("batch_rename") {
            apply_batch_rename_json(&mut config.batch_rename, br);
        }
        if let Some(s) = json.get("startup_tab").and_then(|v| v.as_str()) {
            if matches!(s, "last_tabs" | "computer") {
                config.startup_tab = s.to_string();
            }
        }
        if let Some(arr) = json.get("last_tabs").and_then(|v| v.as_array()) {
            let mut snaps = Vec::new();
            for item in arr {
                let paths = item
                    .get("paths")
                    .and_then(|v| v.as_array())
                    .map(|a| {
                        a.iter()
                            .map(|v| v.as_str().map(|s| s.to_string()))
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                snaps.push(TabSnapshot {
                    paths,
                    active_pane: item
                        .get("active_pane")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0) as usize,
                    is_settings: item
                        .get("is_settings")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false),
                });
            }
            config.last_tabs = snaps;
        }
        if let Some(a) = json.get("last_active_tab").and_then(|v| v.as_u64()) {
            config.last_active_tab = a as usize;
        }
        if let Some(m) = json.get("window_maximized").and_then(|v| v.as_bool()) {
            config.window_maximized = m;
        }
        if let Some(sk) = json.get("sort_key").and_then(|v| v.as_str()) {
            if matches!(sk, "name" | "size" | "modified" | "type" | "path") {
                config.sort_key = sk.to_string();
            }
        }
        if let Some(sa) = json.get("sort_ascending").and_then(|v| v.as_bool()) {
            config.sort_ascending = sa;
        }
        if let Some(sk) = json.get("this_pc_sort_key").and_then(|v| v.as_str()) {
            if matches!(sk, "drive" | "name" | "size" | "modified" | "type" | "path") {
                config.this_pc_sort_key = sk.to_string();
            }
        }
        if let Some(sa) = json.get("this_pc_sort_ascending").and_then(|v| v.as_bool()) {
            config.this_pc_sort_ascending = sa;
        }
        if let Some(sk) = json.get("recycle_sort_key").and_then(|v| v.as_str()) {
            if matches!(sk, "drive" | "name" | "size" | "modified" | "type" | "path") {
                config.recycle_sort_key = sk.to_string();
            }
        }
        if let Some(sa) = json.get("recycle_sort_ascending").and_then(|v| v.as_bool()) {
            config.recycle_sort_ascending = sa;
        }
        // 旧版 config.json 里的 column_layouts 一律忽略：列宽改为统一自适应，
        // 不再按目录还原用户拖过的宽度（避免某些目录被记住超宽、一直横向滚动）。
        if let Some(sh) = json.get("show_hidden").and_then(|v| v.as_bool()) {
            config.show_hidden = sh;
        }
        if let Some(v) = json.get("auto_edit_path_on_new_tab").and_then(|v| v.as_bool()) {
            config.auto_edit_path_on_new_tab = v;
        }
        if let Some(v) = json.get("explorer_takeover").and_then(|v| v.as_bool()) {
            config.explorer_takeover = v;
        }
        if let Some(map) = json.get("shortcuts").and_then(|v| v.as_object()) {
            for (k, v) in map {
                // 兼容旧版单个字符串与新版列表两种格式
                if let Some(arr) = v.as_array() {
                    let chords = arr
                        .iter()
                        .filter_map(|c| c.as_str().map(str::to_string))
                        .collect();
                    config.shortcuts.insert(k.clone(), chords);
                } else if let Some(s) = v.as_str() {
                    config.shortcuts.insert(k.clone(), vec![s.to_string()]);
                }
            }
        }
        if let Some(arr) = json.get("search_excluded_drives").and_then(|v| v.as_array()) {
            config.search_excluded_drives = arr
                .iter()
                .filter_map(|v| v.as_str().map(|s| s.to_ascii_uppercase()))
                .collect();
        }
        if let Some(arr) = json.get("search_excluded_paths").and_then(|v| v.as_array()) {
            config.search_excluded_paths = arr
                .iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect();
        }
        if let Some(arr) = json.get("context_menu_hide").and_then(|v| v.as_array()) {
            config.context_menu_hide = arr
                .iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect();
        }
        if let Some(m) = json.get("search_idle_rescan_minutes").and_then(|v| v.as_u64()) {
            config.search_idle_rescan_minutes = m as u32;
        }
        if let Some(r) = json.get("search_max_results").and_then(|v| v.as_u64()) {
            config.search_max_results = (r as usize).clamp(50, 10000);
        }
        if let Some(b) = json.get("is_first_run").and_then(|v| v.as_bool()) {
            config.is_first_run = b;
        }
        if let Some(arr) = json.get("network_locations").and_then(|v| v.as_array()) {
            config.network_locations = arr
                .iter()
                .filter_map(|v| {
                    let label = v.get("label")?.as_str()?.trim().to_string();
                    let path = v.get("path")?.as_str()?.trim().to_string();
                    if path.is_empty() {
                        return None;
                    }
                    Some(NetworkLocation {
                        label: if label.is_empty() {
                            network_label_from_path(&path)
                        } else {
                            label
                        },
                        path,
                    })
                })
                .collect();
        }
        if let Some(arr) = json.get("quick_items").and_then(|v| v.as_array()) {
            let mut unique: Vec<QuickItem> = Vec::new();
            for v in arr {
                let Some(id) = v.get("id").and_then(|s| s.as_str()).map(str::trim) else {
                    continue;
                };
                if id.is_empty() {
                    continue;
                }
                let path = v
                    .get("path")
                    .and_then(|s| s.as_str())
                    .unwrap_or("")
                    .trim()
                    .to_string();
                let label = v
                    .get("label")
                    .and_then(|s| s.as_str())
                    .unwrap_or("")
                    .trim()
                    .to_string();
                let label = if label.is_empty() {
                    // 用户固定项取路径末段，系统项退化为 id。
                    path.rsplit(|c| c == '\\' || c == '/')
                        .find(|s| !s.is_empty())
                        .map(str::to_string)
                        .unwrap_or_else(|| id.to_string())
                } else {
                    label
                };
                let item = QuickItem {
                    id: id.to_string(),
                    label,
                    path,
                };
                if !unique.iter().any(|q| q.id == item.id) {
                    unique.push(item);
                }
            }
            // 特殊项（回收站）必须始终存在：可拖动排序，但不允许取消固定。
            if !unique.iter().any(QuickItem::is_special) {
                if let Some(recycle) = default_quick_items().into_iter().find(QuickItem::is_special) {
                    unique.push(recycle);
                }
            }
            if !unique.is_empty() {
                config.quick_items = unique;
            }
        }
        config.sidebar_width_repaired = json
            .get("sidebar_width_repaired")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        // 一次性修复：旧版在窗口最小化/变窄时会把被挤压的宽度写进配置，
        // 导致用户“自定义宽度被永久吃掉”。宽于 120px 才算用户可接受的正常值。
        if !config.sidebar_width_repaired && config.sidebar_width < 120.0 {
            config.sidebar_width = Self::default().sidebar_width;
            let mut fixed = config.clone();
            fixed.sidebar_width_repaired = true;
            fixed.save();
        }
        // 一次性升级：旧版 config 里的 `context_menu_hide` 会整体覆盖默认值，
        // 新增的两条默认隐藏项对老安装不生效。只补一次，之后用户可自由增删。
        config.context_menu_hide_seeded = json
            .get("context_menu_hide_seeded")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        if !config.context_menu_hide_seeded {
            for kw in ["!Pin to Quick access", "!固定到快速访问"] {
                if !config
                    .context_menu_hide
                    .iter()
                    .any(|k| k.eq_ignore_ascii_case(kw))
                {
                    config.context_menu_hide.push(kw.to_string());
                }
            }
            config.context_menu_hide_seeded = true;
            config.save();
        }
        // 逐代补齐默认隐藏项：老配置里的数组已经存在，跑不到上面的补种分支。
        // 每代只补自己那几条，缺哪条补哪条，用户删掉过的不会被反复塞回来
        // （代次已经推到最新，下次不再补）。
        config.context_menu_hide_gen = json
            .get("context_menu_hide_gen")
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as u32;
        let seeds: [(u32, &[&str]); 6] = [
            (3, &["Undo", "撤消"]),
            (4, &["Redo", "恢复"]),
            (5, &["!删除(D)", "!Delete"]),
            (
                6,
                &[
                    "!属性(R)",
                    "!重命名(M)",
                    "!创建快捷方式(S)",
                    "!桌面快捷方式",
                    "!Properties",
                    "!Rename",
                    "!Create shortcut",
                    "!Desktop (create shortcut)",
                ],
            ),
            (7, &["!打开方式(H)...", "!Open with..."]),
            (8, &["!打开方式..."]),
        ];
        if config.context_menu_hide_gen < CONTEXT_MENU_HIDE_GEN {
            for (gen, keywords) in seeds {
                if config.context_menu_hide_gen >= gen {
                    continue;
                }
                for kw in keywords {
                    if !config
                        .context_menu_hide
                        .iter()
                        .any(|k| k.eq_ignore_ascii_case(kw))
                    {
                        config.context_menu_hide.push(kw.to_string());
                    }
                }
            }
            config.context_menu_hide_gen = CONTEXT_MENU_HIDE_GEN;
            config.save();
        }
        config
    }

    pub fn save(&self) {
        let Some(path) = Self::config_path() else {
            return;
        };
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let json = serde_json::json!({
            "theme-mode": match self.theme {
                ThemeChoice::System => "system",
                ThemeChoice::Light => "light",
                ThemeChoice::Dark => "dark",
            },
            "recent": self.recent.iter().map(|p| p.to_string_lossy().into_owned()).collect::<Vec<_>>(),
            "window_width": self.window_width,
            "window_height": self.window_height,
            "window_x": self.window_x,
            "window_y": self.window_y,
            "sidebar_width": self.sidebar_width,
            "staging_expanded": self.staging_expanded,
            "recent_expanded": self.recent_expanded,
            "quick_expanded": self.quick_expanded,
            "computer_expanded": self.computer_expanded,
            "network_expanded": self.network_expanded,
            "explorer_net_expanded": self.explorer_net_expanded,
            "sidebar_order": self.sidebar_order,
            "view_mode": self.view_mode,
            "language": self.language,
            "batch_rename": {
                "search": self.batch_rename.search,
                "replace": self.batch_rename.replace,
                "use_regex": self.batch_rename.use_regex,
                "match_all": self.batch_rename.match_all,
                "case_sensitive": self.batch_rename.case_sensitive,
                "pinyin": self.batch_rename.pinyin,
                "apply_to": self.batch_rename.apply_to,
            },
            "startup_tab": self.startup_tab,
            "last_tabs": self
                .last_tabs
                .iter()
                .map(|t| serde_json::json!({
                    "paths": t.paths.iter().map(|p| serde_json::json!(p)).collect::<Vec<_>>(),
                    "active_pane": t.active_pane,
                    "is_settings": t.is_settings,
                }))
                .collect::<Vec<_>>(),
            "last_active_tab": self.last_active_tab,
            "window_maximized": self.window_maximized,
            "sort_key": self.sort_key,
            "sort_ascending": self.sort_ascending,
            "this_pc_sort_key": self.this_pc_sort_key,
            "this_pc_sort_ascending": self.this_pc_sort_ascending,
            "recycle_sort_key": self.recycle_sort_key,
            "recycle_sort_ascending": self.recycle_sort_ascending,
            "show_hidden": self.show_hidden,
            "auto_edit_path_on_new_tab": self.auto_edit_path_on_new_tab,
            "explorer_takeover": self.explorer_takeover,
            "shortcuts": self.shortcuts,
            "search_excluded_drives": self.search_excluded_drives,
            "search_excluded_paths": self.search_excluded_paths,
            "context_menu_hide": self.context_menu_hide,
            "search_idle_rescan_minutes": self.search_idle_rescan_minutes,
            "search_max_results": self.search_max_results,
            "is_first_run": self.is_first_run,
            "network_locations": self
                .network_locations
                .iter()
                .map(|n| serde_json::json!({ "label": n.label, "path": n.path }))
                .collect::<Vec<_>>(),
            "sidebar_width_repaired": self.sidebar_width_repaired,
            "quick_items": self
                .quick_items
                .iter()
                .map(|q| serde_json::json!({ "id": q.id, "label": q.label, "path": q.path }))
                .collect::<Vec<_>>(),
            "context_menu_hide_seeded": self.context_menu_hide_seeded,
            "context_menu_hide_gen": self.context_menu_hide_gen,
        });
        let text = serde_json::to_string_pretty(&json).unwrap_or_default();
        let _ = std::fs::write(&path, text);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_adds_unc_prefix_and_normalizes_slashes() {
        assert_eq!(
            normalize_network_path("192.168.1.5/Video").as_deref(),
            Some("\\\\192.168.1.5\\Video")
        );
        assert_eq!(
            normalize_network_path("\\\\nas\\media\\").as_deref(),
            Some("\\\\nas\\media")
        );
        assert_eq!(normalize_network_path("   ").as_deref(), None);
        // 只有主机名（没有共享名）也合法：资源管理器能直接列出该主机的共享。
        assert_eq!(normalize_network_path("\\\\nas").as_deref(), Some("\\\\nas"));
        assert_eq!(normalize_network_path("\\\\").as_deref(), None);
    }

    #[test]
    fn theme_defaults_to_follow_system_and_reads_both_keys() {
        // 没有键（首次启动）→ 跟随系统。
        assert_eq!(theme_from_json(&serde_json::json!({})), ThemeChoice::System);
        // 老配置里的 "light"（那时界面只能亮）→ 跟随系统，别把人锁在亮色。
        assert_eq!(
            theme_from_json(&serde_json::json!({ "theme": "light" })),
            ThemeChoice::System
        );
        // 老配置里手改的 "dark" 仍然认。
        assert_eq!(
            theme_from_json(&serde_json::json!({ "theme": "dark" })),
            ThemeChoice::Dark
        );
        // 新键优先。
        assert_eq!(
            theme_from_json(&serde_json::json!({ "theme-mode": "light", "theme": "dark" })),
            ThemeChoice::Light
        );
        assert_eq!(
            theme_from_json(&serde_json::json!({ "theme-mode": "system" })),
            ThemeChoice::System
        );
    }

    #[test]
    fn batch_rename_pinyin_defaults_on_and_sticks_when_off() {
        let mut p = BatchRenamePrefs::default();
        assert!(p.pinyin, "拼音默认勾选");

        // 老配置文件没有这个键：保持默认勾选，不能被读成关。
        let old = serde_json::json!({ "search": "a", "replace": "b" });
        apply_batch_rename_json(&mut p, &old);
        assert!(p.pinyin);
        assert_eq!(p.search, "a");

        // 用户取消勾选后必须真的关掉。
        let off = serde_json::json!({ "pinyin": false });
        apply_batch_rename_json(&mut p, &off);
        assert!(!p.pinyin);

        // 再打开也能回来。
        let on = serde_json::json!({ "pinyin": true });
        apply_batch_rename_json(&mut p, &on);
        assert!(p.pinyin);
    }

    #[test]
    fn label_defaults_to_tail_segment() {
        assert_eq!(network_label_from_path("\\\\192.168.1.5\\Video"), "Video");
        assert_eq!(network_label_from_path("\\\\nas\\a\\b\\"), "b");
        assert_eq!(network_label_from_path("\\\\192.168.1.5"), "192.168.1.5");
        assert_eq!(network_label_from_path("\\\\nas"), "nas");
    }
}
