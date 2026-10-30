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
    pub staging_expanded: bool,
    /// 「最近使用」是否展开（默认 false 收起）。
    pub recent_expanded: bool,
    /// 「快速访问」是否展开（默认 true 展开）。
    pub quick_expanded: bool,
    /// 「此电脑」是否展开（默认 true 展开）。
    pub computer_expanded: bool,
    /// 「网络位置」是否展开（默认 true 展开）。
    pub network_expanded: bool,
    /// 侧栏分区顺序（分区 key：quick / recent / computer / network）。
    pub sidebar_order: Vec<String>,
    /// 视图模式：details | list | icons-small | icons-medium | icons-large | icons-xl。
    pub view_mode: String,
    /// 是否显示隐藏文件/文件夹/驱动器（默认 false，即过滤掉）。
    pub show_hidden: bool,
    /// 用户自定义快捷键映射: ActionId -> [Chord1, Chord2]（空字符串代表已取消绑定）。
    pub shortcuts: std::collections::HashMap<String, Vec<String>>,
}

/// 侧栏分区的默认顺序。
pub fn default_sidebar_order() -> Vec<String> {
    ["quick", "recent", "computer", "network"]
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
            staging_expanded: true,
            recent_expanded: false, // 默认收起
            quick_expanded: true,   // 默认展开
            computer_expanded: true,// 默认展开
            network_expanded: true, // 默认展开
            sidebar_order: default_sidebar_order(),
            view_mode: "details".to_string(),
            show_hidden: false,
            shortcuts: std::collections::HashMap::new(),
        }
    }
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
        if let Some(order) = json.get("sidebar_order").and_then(|v| v.as_array()) {
            let keys: Vec<String> = order
                .iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect();
            // 与默认 key 集合完全一致才采用，防止脏数据。
            let mut sorted_keys = keys.clone();
            sorted_keys.sort();
            let mut sorted_default = default_sidebar_order();
            sorted_default.sort();
            if sorted_keys == sorted_default {
                config.sidebar_order = keys;
            }
        }
        if let Some(vm) = json.get("view_mode").and_then(|v| v.as_str()) {
            if matches!(
                vm,
                "details" | "list" | "icons-small" | "icons-medium" | "icons-large" | "icons-xl"
            ) {
                config.view_mode = vm.to_string();
            }
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
            "staging_expanded": self.staging_expanded,
            "recent_expanded": self.recent_expanded,
            "quick_expanded": self.quick_expanded,
            "computer_expanded": self.computer_expanded,
            "network_expanded": self.network_expanded,
            "sidebar_order": self.sidebar_order,
            "view_mode": self.view_mode,
            "show_hidden": self.show_hidden,
            "shortcuts": self.shortcuts,
        });
        let text = serde_json::to_string_pretty(&json).unwrap_or_default();
        let _ = std::fs::write(&path, text);
    }
}
