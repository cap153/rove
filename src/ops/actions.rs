//! `actions.toml`：自定义右键动作与压缩软件集成（Phase 4）。
//!
//! 启动时探测已安装的压缩软件（WinRAR / 7-Zip / Bandizip），生成或校验
//! `%APPDATA%/Rove/actions.toml`：
//! - 首次运行：探测 → 生成默认配置（带说明注释）。
//! - 每次启动：用 `toml_edit` 无损校验（不破坏用户注释与排版）：
//!   - 压缩类动作路径变化 → 自动更新 `command`；
//!   - 软件卸载（探测失败）→ 标记 `enabled = false`；
//!   - 自定义动作的 command 不存在 → 同样标记禁用。
//!
//! 参数模板占位符（执行时替换）：
//! - `{target_path}`  第一个选中路径
//! - `{target_paths}` 全部选中路径（引号包裹、空格分隔）
//! - `{target_dir}`   目标目录（解压 = 压缩包所在目录）
//! - `{name}`         第一个选中项的文件名
//! - `{archive}`      自动生成的压缩包完整路径（仅 compress 动作）

use std::path::{Path, PathBuf};

use crate::logln;

/// 一条右键动作（兼容用户手写 schema：program/working_dir/target/extensions）。
#[derive(Clone, Debug)]
pub struct ActionEntry {
    pub id: String,
    pub label: String,
    /// 归属工具 id（历史字段：旧版自动生成的压缩动作曾用它重新探测；现已不再生成）。
    pub tool: String,
    /// 可执行文件（兼容旧字段名 command）。
    pub program: String,
    pub args: String,
    /// 工作目录模板（支持占位符；空 = 选中项所在目录）。
    pub working_dir: String,
    /// 适用对象：any | file | dir。
    pub target: String,
    /// 适用扩展名（仅 target = file 时生效；空 = 全部）。
    pub extensions: Vec<String>,
    pub enabled: bool,
    /// 二级菜单分组名（空 = 一级菜单）。
    pub group: String,
    /// 是否系统内置（true = 只许开关，不许改命令/删除）。
    pub builtin: bool,
}

/// 已废弃的内置动作 id。
///
/// 这些项全部由系统菜单提供，隐藏它们改用设置页的关键词过滤，
/// 不再需要单独开关；启动时从 actions.toml 里清掉历史残留。
pub const RETIRED_BUILTINS: &[&str] = &[
    "builtin.open",
    "builtin.open_with",
    "builtin.sendto",
    "builtin.rename",
    "builtin.delete",
    "builtin.properties",
];

/// 内置右键动作元数据（与 Rust 内建操作一一对应，由 actions.toml 开关控制显隐）。
///
/// 只列 Rove 自己追加的项：标准命令（打开 / 打开方式 / 发送到 / 重命名 /
/// 删除 / 属性）由 Shell 菜单提供，要藏就用关键词，不在设置页留开关。
pub fn builtin_actions() -> Vec<ActionEntry> {
    vec![
        ActionEntry {
            id: "builtin.copy".into(),
            label: "复制".into(),
            tool: "".into(),
            program: "".into(),
            args: "".into(),
            working_dir: "".into(),
            target: "any".into(),
            extensions: vec![],
            enabled: true,
            group: "".into(),
            builtin: true,
        },
        ActionEntry {
            id: "builtin.move".into(),
            label: "剪切".into(),
            tool: "".into(),
            program: "".into(),
            args: "".into(),
            working_dir: "".into(),
            target: "any".into(),
            extensions: vec![],
            enabled: true,
            group: "".into(),
            builtin: true,
        },
        ActionEntry {
            id: "builtin.stage".into(),
            label: "添加到暂存盘".into(),
            tool: "".into(),
            program: "".into(),
            args: "".into(),
            working_dir: "".into(),
            target: "any".into(),
            extensions: vec![],
            enabled: true,
            group: "".into(),
            builtin: true,
        },
        ActionEntry {
            id: "builtin.terminal".into(),
            label: "在此处打开终端".into(),
            tool: "".into(),
            program: "".into(),
            args: "".into(),
            working_dir: "".into(),
            target: "dir".into(),
            extensions: vec![],
            enabled: true,
            group: "".into(),
            builtin: true,
        },
        ActionEntry {
            id: "builtin.pin_quick".into(),
            label: "固定到快速访问".into(),
            tool: "".into(),
            program: "".into(),
            args: "".into(),
            working_dir: "".into(),
            target: "dir".into(),
            extensions: vec![],
            enabled: true,
            group: "".into(),
            builtin: true,
        },
        // 「在资源管理器中显示」：作为一级菜单项，可在设置页「本地右键菜单」
        // 的「一级菜单」分组里开关。菜单构建处按 enabled 决定是否 push，
        // 并由 CMD_REVEAL_EXPLORER 分发（不渲染成 act: 项，避免重复）。
        ActionEntry {
            id: "builtin.reveal_explorer".into(),
            label: "在资源管理器中显示".into(),
            tool: "".into(),
            program: "".into(),
            args: "".into(),
            working_dir: "".into(),
            target: "any".into(),
            extensions: vec![],
            // 默认关闭：首次启动生成 actions.toml 时不把这项塞进一级菜单。
            enabled: false,
            group: "".into(),
            builtin: true,
        },
    ]
}

impl ActionEntry {
    /// 该动作是否适用于当前选中集。
    pub fn matches_selection(&self, paths: &[PathBuf]) -> bool {
        let Some(first) = paths.first() else {
            return false;
        };
        match self.target.as_str() {
            "file" => {
                let is_file = first.is_file();
                if self.extensions.is_empty() {
                    is_file
                } else {
                    let ext = first
                        .extension()
                        .map(|e| e.to_string_lossy().to_lowercase())
                        .unwrap_or_default();
                    is_file && self.extensions.iter().any(|e| e.eq_ignore_ascii_case(&ext))
                }
            }
            "dir" => first.is_dir(),
            _ => true,
        }
    }
}

/// 配置文件路径。
pub fn config_path() -> Option<PathBuf> {
    let appdata = std::env::var_os("APPDATA").map(PathBuf::from)?;
    Some(appdata.join("Rove").join("actions.toml"))
}

// ---------------------------------------------------------------------------
// 配置生成与自愈
// ---------------------------------------------------------------------------

/// 首次运行的默认配置：内置动作（只含开关）+ 字段说明。
fn generate_default() -> String {
    let mut s = String::from(
        "# Rove 右键自定义动作配置\n\
         #\n\
         # 本文件由 Rove 在每次启动时自动维护：\n\
         #   - 新增/重命名内置动作 → 自动补全对应开关；\n\
         #   - 命令行程序不存在   → 对应条目标记 enabled = false；\n\
         #   - 手动添加的注释与排版会被保留。\n\
         #\n\
         # 字段：target = any | file | dir；\n\
         # group = 二级菜单分组名（空 = 一级菜单）；\n\
         # builtin = true 表示系统内置（仅允许开关）；\n\
         # extensions = 适用扩展名（仅 target = file）；\n\
         # 参数模板占位符：\n\
         #   {target_path}  第一个选中路径\n\
         #   {target_paths} 全部选中路径（引号包裹、空格分隔）\n\
         #   {target_dir}   选中项所在目录\n\
         #   {name}         第一个选中项的文件名\n\
         #   {stem}         第一个选中项的文件名（不含扩展名）\n\n",
    );
    for a in builtin_actions() {
        s.push_str(&action_to_toml(&a));
    }
    s
}

/// 把一条动作序列化为 TOML 表。
fn action_to_toml(a: &ActionEntry) -> String {
    let mut lines = vec![
        "[[action]]".into(),
        format!("id = \"{}\"", a.id),
        format!("label = \"{}\"", a.label),
        format!("enabled = {}", a.enabled),
        format!("builtin = {}", a.builtin),
    ];
    if !a.group.is_empty() {
        lines.push(format!("group = \"{}\"", a.group));
    }
    if !a.tool.is_empty() {
        lines.push(format!("tool = \"{}\"", a.tool));
    }
    if !a.program.is_empty() {
        lines.push(format!("program = '{}'", a.program.replace('\\', "\\\\").replace('\'', "\\'")));
    }
    if !a.args.is_empty() {
        lines.push(format!("args = '{}'", a.args.replace('\\', "\\\\").replace('\'', "\\'")));
    }
    if !a.working_dir.is_empty() {
        lines.push(format!("working_dir = \"{}\"", a.working_dir));
    }
    lines.push(format!("target = \"{}\"", a.target));
    if !a.extensions.is_empty() {
        let exts: Vec<String> = a.extensions.iter().map(|e| format!("\"{}\"", e)).collect();
        lines.push(format!("extensions = [{}]", exts.join(", ")));
    }
    lines.push(String::new());
    lines.join("\n")
}
/// 加载（必要时生成）并自愈配置，返回全部动作（含禁用）。
pub fn load_or_create() -> Vec<ActionEntry> {
    let Some(path) = config_path() else {
        return Vec::new();
    };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if !path.exists() {
        let text = generate_default();
        if std::fs::write(&path, &text).is_err() {
            logln!("actions.toml write failed");
            return Vec::new();
        }
        logln!("actions.toml created");
    }
    heal_and_load(&path)
}

/// 用 toml_edit 无损校验并加载。
fn heal_and_load(path: &Path) -> Vec<ActionEntry> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let Ok(mut doc) = text.parse::<toml_edit::DocumentMut>() else {
        logln!("actions.toml parse failed");
        return Vec::new();
    };

    // 一次性迁移：「在资源管理器中显示」改为首次启动默认关闭。
    // 老配置里这一项由旧默认值写成 enabled=true，仅改 builtin_actions 的默认值
    // 不会覆盖用户配置（heal_full 会继承 enabled）。用文件里的标记保证只回落一次，
    // 之后用户在设置页手动打开不会再被强制关掉。
    // 必须在取得 `items` 可变借用之前读标记，避免 doc 被同时不可变借用。
    let reveal_default_fixed = doc
        .get("rove_reveal_default_fixed")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let Some(items) = doc.get_mut("action").and_then(|i| i.as_array_of_tables_mut()) else {
        return Vec::new();
    };

    if !reveal_default_fixed {
        for table in items.iter_mut() {
            if str_field(table, "id").as_deref() == Some("builtin.reveal_explorer") {
                table["enabled"] = toml_edit::value(false);
                logln!("actions.toml: migrate builtin.reveal_explorer -> disabled by default");
            }
        }
    }

    // 清掉已废弃的内置动作（打开 / 打开方式 / 发送到 / 重命名 / 删除 / 属性）：
    // 它们现在由系统菜单提供，隐藏改用关键词，不再需要开关。
    for index in (0..items.len()).rev() {
        let retired = items
            .get(index)
            .map(|t| str_field(t, "id").unwrap_or_default())
            .map(|id| RETIRED_BUILTINS.contains(&id.as_str()))
            .unwrap_or(false);
        if retired {
            items.remove(index);
        }
    }

    let mut entries = Vec::new();
    for table in items.iter_mut() {
        let id = str_field(table, "id").unwrap_or_default();
        let tool = str_field(table, "tool").unwrap_or_default();
        // program 优先，兼容 command 字段名。
        let program = str_field(table, "program")
            .or_else(|| str_field(table, "command"))
            .unwrap_or_default();
        let mut enabled = table
            .get("enabled")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);

        // 内置动作由 Rust 内部实现（无外部程序），不做程序存在性校验，
        // 否则会被误判为「命令缺失」而整批禁用。
        // 历史配置里遗留的压缩类条目（tool 非空）不再进菜单，由 `heal_full` 丢弃。
        let builtin = table.get("builtin").and_then(|v| v.as_bool()).unwrap_or(false);
        if !builtin && tool.is_empty() && !program.is_empty() {
            // 自定义动作：仅当填写的是绝对路径且已失效时才禁用；
            // 裸程序名（如 notepad.exe）运行时按 PATH 解析，不予禁用。
            let is_path = program.contains('\\') || program.contains('/');
            if is_path && !Path::new(&program).is_file() && enabled {
                logln!("heal {}: command missing, disable", id);
                enabled = false;
                table["enabled"] = toml_edit::value(false);
            }
        }

        entries.push(ActionEntry {
            id,
            label: str_field(table, "label").unwrap_or_default(),
            tool,
            program: str_field(table, "program")
                .or_else(|| str_field(table, "command"))
                .unwrap_or_default(),
            args: str_field(table, "args").unwrap_or_default(),
            working_dir: str_field(table, "working_dir").unwrap_or_default(),
            target: str_field(table, "target").unwrap_or_else(|| "any".to_string()),
            extensions: table
                .get("extensions")
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default(),
            enabled,
            group: str_field(table, "group").unwrap_or_default(),
            builtin: table
                .get("builtin")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
        });
    }

    // 写回（toml_edit 保留注释与排版）。迁移标记写在顶层，只写一次；
    // 之后设置页保存时也保留它（save() 只替换 action 数组，不重建整个文档）。
    if !reveal_default_fixed {
        doc["rove_reveal_default_fixed"] = toml_edit::value(true);
    }
    let _ = std::fs::write(path, doc.to_string());
    entries
}

/// 无损写回全部动作配置（设置页开关 / 新建 / 编辑 / 删除后保存）。
/// 保留原有注释：先重读原始 DocumentMut，替换 action 数组。
pub fn save(entries: &[ActionEntry]) -> Result<(), String> {
    let Some(path) = config_path() else {
        return Err("无法定位配置目录".into());
    };
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    let mut doc = text
        .parse::<toml_edit::DocumentMut>()
        .unwrap_or_else(|_| toml_edit::DocumentMut::new());

    let arr = doc["action"].or_insert(toml_edit::Item::ArrayOfTables(Default::default()));
    let arr = arr
        .as_array_of_tables_mut()
        .ok_or("actions 不是数组")?;
    arr.clear();
    for e in entries {
        let mut t = toml_edit::Table::new();
        t["id"] = toml_edit::value(&e.id);
        t["label"] = toml_edit::value(&e.label);
        t["enabled"] = toml_edit::value(e.enabled);
        t["builtin"] = toml_edit::value(e.builtin);
        if !e.group.is_empty() {
            t["group"] = toml_edit::value(&e.group);
        }
        if !e.tool.is_empty() {
            t["tool"] = toml_edit::value(&e.tool);
        }
        if !e.program.is_empty() {
            t["program"] = toml_edit::value(&e.program);
        }
        if !e.args.is_empty() {
            t["args"] = toml_edit::value(&e.args);
        }
        if !e.working_dir.is_empty() {
            t["working_dir"] = toml_edit::value(&e.working_dir);
        }
        t["target"] = toml_edit::value(&e.target);
        if !e.extensions.is_empty() {
            let mut a = toml_edit::Array::new();
            for ext in &e.extensions {
                a.push(ext.as_str());
            }
            t["extensions"] = toml_edit::value(a);
        }
        arr.push(t);
    }

    std::fs::write(&path, doc.to_string())
        .map_err(|e| format!("actions.toml 写入失败: {e}"))?;
    Ok(())
}

/// 用内置动作元数据对现有配置做一次「合并」：保留用户的开关状态，
/// 新增/重命名内置动作时自动补全，缺失的压缩动作按当前探测结果生成。
pub fn heal_full(entries: &[ActionEntry]) -> Vec<ActionEntry> {
    let mut out: Vec<ActionEntry> = Vec::new();
    let mut by_id: std::collections::HashMap<String, ActionEntry> = entries
        .iter()
        .map(|e| (e.id.clone(), e.clone()))
        .collect();

    // 1) 内置动作：以代码为准，继承用户 enabled。
    for builtin in builtin_actions() {
        let mut e = builtin;
        if let Some(user) = by_id.remove(&e.id) {
            e.enabled = user.enabled;
            // 若是自定义把 builtin 设成 true 的条目，同样保留。
        }
        out.push(e);
    }

    // 2) 剩余条目：用户自定义动作，保留原顺序。
    for e in entries {
        if !e.builtin && e.tool.is_empty() && by_id.contains_key(&e.id) {
            out.push(e.clone());
        }
    }
    out
}

fn str_field(table: &toml_edit::Table, key: &str) -> Option<String> {
    table.get(key).and_then(|v| v.as_str()).map(str::to_string)
}

// ---------------------------------------------------------------------------
// 执行
// ---------------------------------------------------------------------------

/// 执行一条动作（参数模板替换 + 启动进程）。
pub fn run_action(entry: &ActionEntry, targets: &[PathBuf]) -> Result<(), String> {
    if targets.is_empty() {
        return Err("没有选中项".into());
    }
    let target_dir = targets[0]
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."));
    let args = build_substituted(&entry.args, targets);
    let working_dir = if entry.working_dir.is_empty() {
        target_dir.clone()
    } else {
        PathBuf::from(build_substituted(&entry.working_dir, targets))
    };

    let argv = split_args(&args);
    if entry.program.is_empty() {
        return Err("未配置可执行文件".into());
    }
    if !Path::new(&entry.program).is_file() {
        return Err(format!("程序不存在：{}", entry.program));
    }
    // 控制台程序（7z/bz）不闪黑框。
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        std::process::Command::new(&entry.program)
            .args(&argv)
            .creation_flags(CREATE_NO_WINDOW)
            .current_dir(&working_dir)
            .spawn()
            .map(|_| ())
            .map_err(|e| format!("启动 {} 失败: {e}", entry.program))
    }
    #[cfg(not(windows))]
    {
        std::process::Command::new(&entry.program)
            .args(&argv)
            .current_dir(&working_dir)
            .spawn()
            .map(|_| ())
            .map_err(|e| format!("启动 {} 失败: {e}", entry.program))
    }
}

/// 参数/工作目录模板替换（纯函数，便于单测）。
pub fn build_substituted(template: &str, targets: &[PathBuf]) -> String {
    let target_dir = targets[0]
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."));
    let name = targets[0]
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let stem = targets[0]
        .file_stem()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    // 约定：模板自带引号（生成器与用户模板均如此），占位符替换为裸路径；
    // {target_paths} 输出带引号（多项路径必须逐个引号包裹）。
    let all = targets
        .iter()
        .map(|p| format!("\"{}\"", p.display()))
        .collect::<Vec<_>>()
        .join(" ");
    template
        .replace("{target_paths}", &all)
        .replace("{target_path}", &targets[0].to_string_lossy().into_owned())
        .replace("{target_dir}", &target_dir.to_string_lossy().into_owned())
        .replace("{name}", &name)
        .replace("{stem}", &stem)
}

/// 引号感知的参数切分（剥离成对引号，Command 会重新转义）。
fn split_args(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_quote = false;
    for ch in s.chars() {
        match ch {
            '"' => in_quote = !in_quote,
            ' ' | '\t' if !in_quote => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            _ => cur.push(ch),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_args_respects_quotes() {
        let argv = split_args(r#"a -ep1 "C:\my dir\a.rar" "D:\x y.txt""#);
        assert_eq!(argv, vec!["a", "-ep1", r"C:\my dir\a.rar", r"D:\x y.txt"]);
    }

    #[test]
    fn generate_default_contains_builtins() {
        let text = generate_default();
        let doc = text.parse::<toml_edit::DocumentMut>().unwrap();
        let arr = doc.get("action").unwrap().as_array_of_tables().unwrap();
        assert_eq!(arr.len(), builtin_actions().len());
        assert!(text.contains("# Rove 右键自定义动作配置"));
        assert!(text.contains("id = \"builtin.copy\""));
        assert!(text.contains("id = \"builtin.terminal\""));
        // 系统菜单提供的项不再生成开关。
        for retired in RETIRED_BUILTINS {
            assert!(!text.contains(&format!("id = \"{}\"", retired)));
        }
    }

    #[test]
    fn template_substitution_winrar() {
        let targets = vec![PathBuf::from("C:/Users/x/p4test.txt")];
        let out = build_substituted(
            "a -r -ep1 \"{target_dir}\\{stem}.rar\" \"{target_path}\"",
            &targets,
        );
        assert_eq!(out, "a -r -ep1 \"C:/Users/x\\p4test.rar\" \"C:/Users/x/p4test.txt\"");
    }

    #[test]
    fn template_substitution_extract() {
        let targets = vec![PathBuf::from("D:/Video/a.zip")];
        let out = build_substituted(
            "x -r \"{target_path}\" \"{target_dir}\\{stem}\"",
            &targets,
        );
        assert_eq!(out, "x -r \"D:/Video/a.zip\" \"D:/Video\\a\"");
    }

    #[test]
    fn template_multiple_targets() {
        let targets = vec![PathBuf::from("C:/a.txt"), PathBuf::from("C:/b.txt")];
        let out = build_substituted("{target_paths}", &targets);
        assert_eq!(out, "\"C:/a.txt\" \"C:/b.txt\"");
    }
}
