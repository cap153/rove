//! 声明式快捷键系统（Sigma 风格架构）。
//! 负责键位定义、多槽位差量合并、Winit 按键规范化。

use std::collections::HashMap;

/// 单个快捷键动作元数据
#[derive(Clone, Debug)]
pub struct ShortcutMeta {
    pub id: &'static str,
    pub name: &'static str,
    pub group: &'static str,
    pub default_chord: &'static str,
    /// 是否只读锁定（如 Escape 专属系统退出，禁止重绑）
    pub is_readonly: bool,
}

/// 现有核心快捷键定义表（Escape 锁定）
pub const DEFAULT_SHORTCUTS: &[ShortcutMeta] = &[
    // 文件操作
    ShortcutMeta { id: "open", name: "打开选中项", group: "文件操作", default_chord: "Enter", is_readonly: false },
    ShortcutMeta { id: "rename", name: "重命名", group: "文件操作", default_chord: "F2", is_readonly: false },
    ShortcutMeta { id: "delete", name: "删除至回收站", group: "文件操作", default_chord: "Delete", is_readonly: false },
    ShortcutMeta { id: "delete_perm", name: "永久删除", group: "文件操作", default_chord: "Shift+Delete", is_readonly: false },
    // 导航与查看
    ShortcutMeta { id: "nav_back", name: "后退", group: "导航与查看", default_chord: "Alt+Left", is_readonly: false },
    ShortcutMeta { id: "nav_forward", name: "前进", group: "导航与查看", default_chord: "Alt+Right", is_readonly: false },
    ShortcutMeta { id: "nav_up", name: "上一级", group: "导航与查看", default_chord: "Alt+Up", is_readonly: false },
    ShortcutMeta { id: "refresh", name: "刷新当前目录", group: "导航与查看", default_chord: "F5", is_readonly: false },
    ShortcutMeta { id: "clear_filter", name: "清空筛选 / 取消", group: "导航与查看", default_chord: "Escape", is_readonly: true },
    ShortcutMeta { id: "focus_search", name: "聚焦搜索框", group: "导航与查看", default_chord: "Ctrl+F", is_readonly: false },
    // 编辑与选择
    ShortcutMeta { id: "copy", name: "复制", group: "编辑与选择", default_chord: "Ctrl+C", is_readonly: false },
    ShortcutMeta { id: "cut", name: "剪切", group: "编辑与选择", default_chord: "Ctrl+X", is_readonly: false },
    ShortcutMeta { id: "paste", name: "粘贴", group: "编辑与选择", default_chord: "Ctrl+V", is_readonly: false },
    ShortcutMeta { id: "select_all", name: "全选", group: "编辑与选择", default_chord: "Ctrl+A", is_readonly: false },
    // 标签页
    ShortcutMeta { id: "close_tab", name: "关闭当前标签", group: "标签页", default_chord: "Ctrl+W", is_readonly: false },
];

/// 快捷键运行期映射表（差量合并后，支持多槽位备用键）
#[derive(Clone, Debug, Default)]
pub struct Keymap {
    /// Action ID -> 绑定的按键组合列表（空字符串代表已取消绑定的空槽位）
    pub action_to_chords: HashMap<String, Vec<String>>,
    /// 组合键 -> Action ID
    chord_to_action: HashMap<String, String>,
}

impl Keymap {
    /// 差量合并：`overrides` 结构为 ActionId -> Vec<Chord>
    pub fn new(overrides: &HashMap<String, Vec<String>>) -> Self {
        let mut action_to_chords = HashMap::new();
        let mut chord_to_action = HashMap::new();

        for meta in DEFAULT_SHORTCUTS {
            let chords = if meta.is_readonly {
                vec![meta.default_chord.to_string()]
            } else if let Some(custom) = overrides.get(meta.id) {
                custom.clone()
            } else {
                vec![meta.default_chord.to_string()]
            };

            for chord in &chords {
                let trimmed = chord.trim();
                if !trimmed.is_empty() && trimmed != "—" {
                    chord_to_action.insert(trimmed.to_string(), meta.id.to_string());
                }
            }
            action_to_chords.insert(meta.id.to_string(), chords);
        }

        Self {
            action_to_chords,
            chord_to_action,
        }
    }

    /// 根据按下的组合键反查触发的动作 ID
    pub fn resolve(&self, chord: &str) -> Option<&str> {
        self.chord_to_action.get(chord).map(|s| s.as_str())
    }
}

/// 将 Winit 键盘事件与修饰键状态解析为规范化字符串（如 "Ctrl+Shift+N"）
pub fn format_chord(
    key: &slint::winit_030::winit::keyboard::Key,
    ctrl: bool,
    shift: bool,
    alt: bool,
) -> Option<String> {
    use slint::winit_030::winit::keyboard::{Key, NamedKey};

    let key_str = match key {
        Key::Named(named) => match named {
            NamedKey::Space => "Space",
            NamedKey::Enter => "Enter",
            NamedKey::Escape => "Escape",
            NamedKey::Delete => "Delete",
            NamedKey::Backspace => "Backspace",
            NamedKey::ArrowLeft => "Left",
            NamedKey::ArrowRight => "Right",
            NamedKey::ArrowUp => "Up",
            NamedKey::ArrowDown => "Down",
            NamedKey::F1 => "F1",
            NamedKey::F2 => "F2",
            NamedKey::F3 => "F3",
            NamedKey::F4 => "F4",
            NamedKey::F5 => "F5",
            NamedKey::F6 => "F6",
            NamedKey::F7 => "F7",
            NamedKey::F8 => "F8",
            NamedKey::F9 => "F9",
            NamedKey::F10 => "F10",
            NamedKey::F11 => "F11",
            NamedKey::F12 => "F12",
            _ => return None,
        },
        Key::Character(ch) => {
            let s = ch.to_uppercase();
            if s.is_empty() || matches!(s.as_str(), "\u{1b}" | "\t" | "\r" | "\n") {
                return None;
            }
            return Some(build_chord_string(s.as_str(), ctrl, shift, alt));
        }
        _ => return None,
    };

    Some(build_chord_string(key_str, ctrl, shift, alt))
}

fn build_chord_string(key: &str, ctrl: bool, shift: bool, alt: bool) -> String {
    let mut parts = Vec::new();
    if ctrl {
        parts.push("Ctrl");
    }
    if alt {
        parts.push("Alt");
    }
    if shift {
        parts.push("Shift");
    }
    parts.push(key);
    parts.join("+")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 方向键必须能格式化：Alt+方向 是后退/前进/上一级的默认键位，
    /// 这里一旦漏掉 `NamedKey::Arrow*`，设置页就录不进、运行期也解析不出。
    #[test]
    fn arrow_keys_are_formattable() {
        use slint::winit_030::winit::keyboard::{Key, NamedKey};
        let chord = |key, ctrl, shift, alt| format_chord(&Key::Named(key), ctrl, shift, alt);
        assert_eq!(
            chord(NamedKey::ArrowLeft, false, false, true).as_deref(),
            Some("Alt+Left")
        );
        assert_eq!(
            chord(NamedKey::ArrowRight, false, false, true).as_deref(),
            Some("Alt+Right")
        );
        assert_eq!(
            chord(NamedKey::ArrowUp, false, false, true).as_deref(),
            Some("Alt+Up")
        );
        assert_eq!(
            chord(NamedKey::ArrowDown, false, false, true).as_deref(),
            Some("Alt+Down")
        );
        // 修饰键顺序固定为 Ctrl → Alt → Shift，改键后回显才不会变来变去。
        assert_eq!(
            chord(NamedKey::ArrowLeft, true, true, true).as_deref(),
            Some("Ctrl+Alt+Shift+Left")
        );
    }

    /// 三个导航动作必须真的绑在 Alt+方向 上（默认键位表改错时立刻炸出来）。
    #[test]
    fn navigation_actions_default_to_alt_arrows() {
        let map = Keymap::new(&HashMap::new());
        assert_eq!(map.resolve("Alt+Left"), Some("nav_back"));
        assert_eq!(map.resolve("Alt+Right"), Some("nav_forward"));
        assert_eq!(map.resolve("Alt+Up"), Some("nav_up"));
    }

    /// 默认键位不能撞车：每个默认组合键必须解析到它自己那个动作。
    /// 以前只有十来条、肉眼能扫，加 Ctrl+W 这种常用键时很容易和别处重复。
    #[test]
    fn default_chords_are_unique() {
        let map = Keymap::new(&HashMap::new());
        for meta in DEFAULT_SHORTCUTS {
            assert_eq!(
                map.resolve(meta.default_chord),
                Some(meta.id),
                "默认键位 {} 已经被别的动作占用",
                meta.default_chord
            );
        }
    }
}
