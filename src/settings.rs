use std::fs;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// 应用设置，持久化于 `%APPDATA%\Rove\settings.toml`
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settings {
    /// 关闭所有标签页后销毁 Slint 窗口句柄，后台仅保留索引与管道监听
    #[serde(default = "default_true")]
    pub destroy_ui_on_last_tab_close: bool,

    /// 是否接管资源管理器（双击文件夹 / 盘符 / 此电脑）
    #[serde(default)]
    pub take_over_explorer: bool,

    /// 是否显示系统托盘
    #[serde(default = "default_true")]
    pub show_tray: bool,

    /// 是否启用双栏对比
    #[serde(default)]
    pub split_view: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            destroy_ui_on_last_tab_close: true,
            take_over_explorer: false,
            show_tray: true,
            split_view: false,
        }
    }
}

fn default_true() -> bool {
    true
}

/// Rove 配置目录，不存在则创建
pub fn config_dir() -> PathBuf {
    let base = dirs::config_dir().unwrap_or_else(std::env::temp_dir);
    let dir = base.join("Rove");
    let _ = fs::create_dir_all(&dir);
    dir
}

pub fn settings_path() -> PathBuf {
    config_dir().join("settings.toml")
}

pub fn load() -> Settings {
    match fs::read_to_string(settings_path()) {
        Ok(text) => toml::from_str(&text).unwrap_or_default(),
        Err(_) => Settings::default(),
    }
}

pub fn save(settings: &Settings) -> std::io::Result<()> {
    let text = toml::to_string_pretty(settings)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    fs::write(settings_path(), text)
}
