//! 配置持久化：主题、最近目录、窗口尺寸、暂存盘展开态。
//! 存储于 `%APPDATA%\Rove\config.json`。

use std::path::PathBuf;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ThemeChoice {
    Light,
    Dark,
}

#[derive(Clone, Debug)]
pub struct Config {
    pub theme: ThemeChoice,
    pub recent: Vec<PathBuf>,
    pub window_width: u32,
    pub window_height: u32,
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
    /// 全局排序字段：name | size | modified | type | path。
    pub sort_key: String,
    /// 全局排序方向：true 递增 / false 递减。
    pub sort_ascending: bool,
    /// 是否显示隐藏文件/文件夹/驱动器（默认 false，即过滤掉）。
    pub show_hidden: bool,
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
}

/// 侧边栏「网络位置」里的一条用户自定义项。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NetworkLocation {
    /// 侧边栏显示的名字。
    pub label: String,
    /// `\\host\share` 形式的地址。
    pub path: String,
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
/// 用 `!` 精确匹配，避免连带藏掉「复制到文件夹」这类变体。
/// 中英两份都列：系统语言不同时靠另一份生效。
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
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

impl Default for Config {
    fn default() -> Self {
        Self {
            theme: ThemeChoice::Light,
            recent: Vec::new(),
            window_width: 1150,
            window_height: 690,
            sidebar_width: 236.0,
            staging_expanded: true,
            recent_expanded: false, // 默认收起
            quick_expanded: true,   // 默认展开
            computer_expanded: true,// 默认展开
            network_expanded: true, // 默认展开
            explorer_net_expanded: true,
            sidebar_order: default_sidebar_order(),
            view_mode: "details".to_string(),
            sort_key: "name".to_string(),
            sort_ascending: true,
            show_hidden: false,
            shortcuts: std::collections::HashMap::new(),
            search_excluded_drives: Vec::new(),
            search_excluded_paths: Vec::new(),
            context_menu_hide: default_context_menu_hide(),
            search_idle_rescan_minutes: 60,
            search_max_results: 1000,
            is_first_run: true,
            network_locations: Vec::new(),
            sidebar_width_repaired: false,
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
        if let Some(theme) = json.get("theme").and_then(|v| v.as_str()) {
            config.theme = match theme {
                "dark" => ThemeChoice::Dark,
                _ => ThemeChoice::Light,
            };
        }
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
        if let Some(sk) = json.get("sort_key").and_then(|v| v.as_str()) {
            if matches!(sk, "name" | "size" | "modified" | "type" | "path") {
                config.sort_key = sk.to_string();
            }
        }
        if let Some(sa) = json.get("sort_ascending").and_then(|v| v.as_bool()) {
            config.sort_ascending = sa;
        }
        if let Some(sh) = json.get("show_hidden").and_then(|v| v.as_bool()) {
            config.show_hidden = sh;
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
            "theme": match self.theme {
                ThemeChoice::Light => "light",
                ThemeChoice::Dark => "dark",
            },
            "recent": self.recent.iter().map(|p| p.to_string_lossy().into_owned()).collect::<Vec<_>>(),
            "window_width": self.window_width,
            "window_height": self.window_height,
            "sidebar_width": self.sidebar_width,
            "staging_expanded": self.staging_expanded,
            "recent_expanded": self.recent_expanded,
            "quick_expanded": self.quick_expanded,
            "computer_expanded": self.computer_expanded,
            "network_expanded": self.network_expanded,
            "explorer_net_expanded": self.explorer_net_expanded,
            "sidebar_order": self.sidebar_order,
            "view_mode": self.view_mode,
            "sort_key": self.sort_key,
            "sort_ascending": self.sort_ascending,
            "show_hidden": self.show_hidden,
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
    fn label_defaults_to_tail_segment() {
        assert_eq!(network_label_from_path("\\\\192.168.1.5\\Video"), "Video");
        assert_eq!(network_label_from_path("\\\\nas\\a\\b\\"), "b");
        assert_eq!(network_label_from_path("\\\\192.168.1.5"), "192.168.1.5");
        assert_eq!(network_label_from_path("\\\\nas"), "nas");
    }
}
