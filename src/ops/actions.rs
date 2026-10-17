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

/// 压缩软件探测结果。
#[derive(Clone, Debug)]
#[allow(dead_code)]
pub struct DetectedTool {
    pub id: &'static str,
    pub label: &'static str,
    pub exe: PathBuf,
    /// 该工具默认生成的压缩包扩展名。
    pub archive_ext: &'static str,
}

/// 一条右键动作（兼容用户手写 schema：program/working_dir/target/extensions）。
#[derive(Clone, Debug)]
pub struct ActionEntry {
    pub id: String,
    pub label: String,
    /// 归属压缩工具 id（自愈时用于重新探测；可为空 = 自定义）。
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
    /// 二级菜单分组名（空 = 一级独立菜单项）。
    pub group: String,
    /// 是否系统内置（true = 只许开关，不许改命令/删除）。
    pub builtin: bool,
}

/// 内置右键动作元数据（与 Rust 内建操作一一对应，由 actions.toml 开关控制显隐）。
pub fn builtin_actions() -> Vec<ActionEntry> {
    vec![
        ActionEntry {
            id: "builtin.open".into(),
            label: "打开".into(),
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
            id: "builtin.open_with".into(),
            label: "打开方式".into(),
            tool: "".into(),
            program: "".into(),
            args: "".into(),
            working_dir: "".into(),
            target: "any".into(),
            extensions: vec![],
            enabled: true,
            group: "打开方式".into(),
            builtin: true,
        },
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
            label: "移动".into(),
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
            id: "builtin.rename".into(),
            label: "重命名".into(),
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
            id: "builtin.sendto".into(),
            label: "发送到".into(),
            tool: "".into(),
            program: "".into(),
            args: "".into(),
            working_dir: "".into(),
            target: "any".into(),
            extensions: vec![],
            enabled: true,
            group: "发送到".into(),
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
            id: "builtin.delete".into(),
            label: "删除".into(),
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
            id: "builtin.properties".into(),
            label: "属性".into(),
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

/// 判断选中路径是否为受支持的压缩包（保留：供未来压缩包检测增强）。
#[allow(dead_code)]
pub fn is_archive_file(path: &Path) -> bool {
    let ext = path
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    matches!(
        ext.as_str(),
        "zip" | "rar" | "7z" | "tar" | "gz" | "bz2" | "xz" | "cab" | "iso" | "jar"
    )
}

// ---------------------------------------------------------------------------
// 探测
// ---------------------------------------------------------------------------

/// 探测全部支持的压缩软件（按优先级：WinRAR > 7-Zip > Bandizip）。
pub fn detect_all() -> Vec<DetectedTool> {
    let mut out = Vec::new();
    if let Some(exe) = detect_winrar() {
        out.push(DetectedTool { id: "winrar", label: "WinRAR", exe, archive_ext: "rar" });
    }
    if let Some(exe) = detect_7zip() {
        out.push(DetectedTool { id: "7zip", label: "7-Zip", exe, archive_ext: "7z" });
    }
    if let Some(exe) = detect_bandizip() {
        out.push(DetectedTool { id: "bandizip", label: "Bandizip", exe, archive_ext: "zip" });
    }
    out
}

/// 按工具 id 探测单个软件（自愈时使用）。
pub fn detect_by_id(id: &str) -> Option<PathBuf> {
    match id {
        "winrar" => detect_winrar(),
        "7zip" => detect_7zip(),
        "bandizip" => detect_bandizip(),
        _ => None,
    }
}

fn detect_winrar() -> Option<PathBuf> {
    #[cfg(windows)]
    if let Some(p) = win::reg_app_paths("WinRAR.exe") {
        return Some(p);
    }
    program_files_candidates("WinRAR\\WinRAR.exe")
        .into_iter()
        .find(|p| p.is_file())
}

fn detect_7zip() -> Option<PathBuf> {
    #[cfg(windows)]
    if let Some(dir) = win::reg_value_hklm("SOFTWARE\\7-Zip", "Path") {
        let p = PathBuf::from(&dir).join("7z.exe");
        if p.is_file() {
            return Some(p);
        }
    }
    program_files_candidates("7-Zip\\7z.exe")
        .into_iter()
        .find(|p| p.is_file())
}

fn detect_bandizip() -> Option<PathBuf> {
    #[cfg(windows)]
    if let Some(dir) = win::reg_value_hklm("SOFTWARE\\Bandizip", "Path") {
        let p = PathBuf::from(&dir).join("bz.exe");
        if p.is_file() {
            return Some(p);
        }
    }
    program_files_candidates("Bandizip\\bz.exe")
        .into_iter()
        .find(|p| p.is_file())
}

fn program_files_candidates(rel: &str) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for var in ["ProgramFiles", "ProgramFiles(x86)"] {
        if let Some(base) = std::env::var_os(var) {
            out.push(PathBuf::from(base).join(rel));
        }
    }
    out
}

// ---------------------------------------------------------------------------
// 配置生成与自愈
// ---------------------------------------------------------------------------

/// 首次运行的默认配置：内置动作 + 探测到的压缩软件动作。
fn generate_default(detected: &[DetectedTool]) -> String {
    let mut s = String::from(
        "# Rove 右键自定义动作配置\n\
         #\n\
         # 本文件由 Rove 在每次启动时自动维护：\n\
         #   - 压缩软件路径变化 → 自动更新 program；\n\
         #   - 软件被卸载       → 对应条目标记 enabled = false；\n\
         #   - 手动添加的注释与排版会被保留。\n\
         #\n\
         # 字段：kind = \"action\"；target = any | file | dir；\n\
         # group = 二级菜单分组名（空 = 一级独立项）；\n\
         # builtin = true 表示系统内置（仅允许开关）；\n\
         # extensions = 适用扩展名（仅 target = file）；\n\
         # 参数模板占位符：\n\
         #   {target_path}  第一个选中路径\n\
         #   {target_paths} 全部选中路径（引号包裹、空格分隔）\n\
         #   {target_dir}   选中项所在目录\n\
         #   {name}         第一个选中项的文件名\n\
         #   {stem}         第一个选中项的文件名（不含扩展名）\n\n",
    );
    // 内置动作（仅允许开关，不显示命令）。
    for a in builtin_actions() {
        s.push_str(&action_to_toml(&a));
    }
    // 压缩软件动作。
    for tool in detected {
        let (label, compress_args, extract_args) = match tool.id {
            "winrar" => (
                "WinRAR",
                "a -r -ep1 \"{target_dir}\\{stem}.rar\" \"{target_path}\"",
                "x -r \"{target_path}\" \"{target_dir}\\{stem}\"",
            ),
            "7zip" => (
                "7-Zip",
                "a -r \"{target_dir}\\{stem}.7z\" \"{target_path}\"",
                "x -r \"{target_path}\" -o{target_dir}\\{stem} -y",
            ),
            _ => (
                "Bandizip",
                "c -t:zip \"{target_dir}\\{stem}.zip\" \"{target_path}\"",
                "bx -y -o:{target_dir}\\{stem} \"{target_path}\"",
            ),
        };
        let exe = tool.exe.to_string_lossy().into_owned();
        let compress = ActionEntry {
            id: format!("archive.{id}.compress", id = tool.id),
            label: format!("压缩为 {label}"),
            tool: tool.id.into(),
            program: exe.clone(),
            args: compress_args.into(),
            working_dir: "{target_dir}".into(),
            target: "any".into(),
            extensions: vec![],
            enabled: true,
            group: "压缩与解压".into(),
            builtin: true,
        };
        let extract = ActionEntry {
            id: format!("archive.{id}.extract", id = tool.id),
            label: "解压到同名文件夹".into(),
            tool: tool.id.into(),
            program: exe,
            args: extract_args.into(),
            working_dir: "{target_dir}".into(),
            target: "file".into(),
            extensions: vec![
                "zip".into(), "7z".into(), "rar".into(), "tar".into(), "gz".into(),
                "tgz".into(), "bz2".into(), "xz".into(), "zst".into(), "cab".into(),
                "iso".into(),
            ],
            enabled: true,
            group: "压缩与解压".into(),
            builtin: true,
        };
        s.push_str(&action_to_toml(&compress));
        s.push_str(&action_to_toml(&extract));
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
        let detected = detect_all();
        let text = generate_default(&detected);
        if std::fs::write(&path, &text).is_err() {
            logln!("actions.toml write failed");
            return Vec::new();
        }
        logln!("actions.toml created with {} tools", detected.len());
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

    let Some(items) = doc.get_mut("action").and_then(|i| i.as_array_of_tables_mut()) else {
        return Vec::new();
    };

    // 一次性修复：早期版本把「无外部程序」的内置动作误判为命令缺失，
    // 并写回 enabled=false，导致整批内置菜单项被禁用。特征：所有内置项
    // （program 为空）同时处于禁用态——正常用户几乎不会这么做。
    let is_builtin_row = |t: &toml_edit::Table| {
        t.get("builtin").and_then(|v| v.as_bool()).unwrap_or(false)
            && str_field(t, "program")
                .or_else(|| str_field(t, "command"))
                .unwrap_or_default()
                .is_empty()
    };
    let (mut total, mut disabled) = (0usize, 0usize);
    for t in items.iter() {
        if !is_builtin_row(t) {
            continue;
        }
        total += 1;
        if !t.get("enabled").and_then(|v| v.as_bool()).unwrap_or(true) {
            disabled += 1;
        }
    }
    if total >= 4 && disabled == total {
        logln!("heal: 内置菜单项被历史版本批量禁用，恢复启用（{} 项）", total);
        for t in items.iter_mut() {
            if is_builtin_row(t) {
                t["enabled"] = toml_edit::value(true);
            }
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

        // 自愈：压缩类动作（tool 非空）按 tool 重新探测。
        if !tool.is_empty() {
            // program 仍有效 → 尊重用户自定义安装位置，不改动。
            let program_valid = !program.is_empty() && Path::new(&program).is_file();
            if !program_valid {
                match detect_by_id(&tool) {
                    Some(exe) => {
                        let current = exe.to_string_lossy().into_owned();
                        if current != program {
                            logln!("heal {}: {} -> {}", id, program, current);
                            set_str_field(table, "program", &current);
                            set_str_field(table, "command", &current);
                        }
                    }
                    None => {
                        if enabled {
                            logln!("heal {}: tool missing, disable", id);
                            enabled = false;
                            table["enabled"] = toml_edit::value(false);
                        }
                    }
                }
            }
        } else {
            // 内置动作由 Rust 内部实现（无外部程序），不做程序存在性校验，
            // 否则会被误判为「命令缺失」而整批禁用。
            let builtin = table.get("builtin").and_then(|v| v.as_bool()).unwrap_or(false);
            if !builtin && !program.is_empty() {
                // 自定义动作：仅当填写的是绝对路径且已失效时才禁用；
                // 裸程序名（如 notepad.exe）运行时按 PATH 解析，不予禁用。
                let is_path = program.contains('\\') || program.contains('/');
                if is_path && !Path::new(&program).is_file() && enabled {
                    logln!("heal {}: command missing, disable", id);
                    enabled = false;
                    table["enabled"] = toml_edit::value(false);
                }
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

    // 写回（toml_edit 保留注释与排版）。
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

    // 2) 压缩软件动作：按当前探测结果自愈。
    let detected = detect_all();
    for tool in detected {
        let ids = [
            format!("archive.{id}.compress", id = tool.id),
            format!("archive.{id}.extract", id = tool.id),
        ];
        for (i, id) in ids.iter().enumerate() {
            if let Some(user) = by_id.remove(id) {
                let mut e = user;
                // program 失效时更新为当前探测路径；enabled 尊重用户开关。
                let exe = tool.exe.to_string_lossy().into_owned();
                if e.program.is_empty() || !Path::new(&e.program).is_file() {
                    e.program = exe.clone();
                }
                // 旧版配置无 group 字段：归一化到「压缩与解压」，
                // 否则设置页里会与一级独立项混在一组。
                if e.group.is_empty() {
                    e.group = "压缩与解压".into();
                }
                out.push(e);
            } else {
                // 新增探测到的压缩动作。
                let (label, compress_args, extract_args) = match tool.id {
                    "winrar" => (
                        "WinRAR",
                        "a -r -ep1 \"{target_dir}\\{stem}.rar\" \"{target_path}\"",
                        "x -r \"{target_path}\" \"{target_dir}\\{stem}\"",
                    ),
                    "7zip" => (
                        "7-Zip",
                        "a -r \"{target_dir}\\{stem}.7z\" \"{target_path}\"",
                        "x -r \"{target_path}\" -o{target_dir}\\{stem} -y",
                    ),
                    _ => (
                        "Bandizip",
                        "c -t:zip \"{target_dir}\\{stem}.zip\" \"{target_path}\"",
                        "bx -y -o:{target_dir}\\{stem} \"{target_path}\"",
                    ),
                };
                out.push(ActionEntry {
                    id: id.clone(),
                    label: if i == 0 {
                        format!("压缩为 {label}")
                    } else {
                        "解压到同名文件夹".into()
                    },
                    tool: tool.id.into(),
                    program: tool.exe.to_string_lossy().into_owned(),
                    args: if i == 0 {
                        compress_args.into()
                    } else {
                        extract_args.into()
                    },
                    working_dir: "{target_dir}".into(),
                    target: if i == 0 { "any".into() } else { "file".into() },
                    extensions: if i == 0 {
                        vec![]
                    } else {
                        vec![
                            "zip".into(), "7z".into(), "rar".into(), "tar".into(), "gz".into(),
                            "tgz".into(), "bz2".into(), "xz".into(), "zst".into(), "cab".into(),
                            "iso".into(),
                        ]
                    },
                    enabled: true,
                    group: "压缩与解压".into(),
                    builtin: true,
                });
            }
        }
    }

    // 3) 剩余条目：用户自定义动作（非 builtin、非压缩），保留原顺序。
    for e in entries {
        if !e.builtin && e.tool.is_empty() && by_id.contains_key(&e.id) {
            out.push(e.clone());
        }
    }
    out
}

/// 写字符串字段（兼容 program/command 双字段名）。
fn set_str_field(table: &mut toml_edit::Table, key: &str, value: &str) {
    if table.get(key).is_some() {
        table[key] = toml_edit::value(value);
    }
}

fn str_field(table: &toml_edit::Table, key: &str) -> Option<String> {
    table.get(key).and_then(|v| v.as_str()).map(str::to_string)
}

// ---------------------------------------------------------------------------
// 执行
// ---------------------------------------------------------------------------

/// 为压缩动作生成压缩包路径（{archive} 占位符，保留：供 {archive} 模板用户）。
#[allow(dead_code)]
pub fn make_archive_path(tool: &str, targets: &[PathBuf]) -> Option<PathBuf> {
    let ext = match tool {
        "winrar" => "rar",
        "7zip" => "7z",
        "bandizip" => "zip",
        _ => "zip",
    };
    let first = targets.first()?;
    let dir = first.parent()?;
    let base = if targets.len() == 1 {
        first
            .file_stem()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "Archive".to_string())
    } else {
        dir.file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "Archive".to_string())
    };
    Some(crate::ops::jobs::unique_dest(dir, &format!("{base}.{ext}"), false))
}

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
    fn archive_detection() {
        assert!(is_archive_file(Path::new("a.RAR")));
        assert!(is_archive_file(Path::new("b.7z")));
        assert!(!is_archive_file(Path::new("c.txt")));
    }

    #[test]
    fn generate_default_contains_actions() {
        let tools = vec![DetectedTool {
            id: "winrar",
            label: "WinRAR",
            exe: PathBuf::from(r"C:\Program Files\WinRAR\WinRAR.exe"),
            archive_ext: "rar",
        }];
        let text = generate_default(&tools);
        let doc = text.parse::<toml_edit::DocumentMut>().unwrap();
        let arr = doc.get("action").unwrap().as_array_of_tables().unwrap();
        // 内置动作 + 每个探测到的压缩工具各 2 条（压缩 / 解压）。
        assert_eq!(arr.len(), builtin_actions().len() + 2);
        assert!(text.contains("# Rove 右键自定义动作配置"));
        // 关键条目齐备。
        assert!(text.contains("id = \"builtin.open\""));
        assert!(text.contains("id = \"archive.winrar.compress\""));
        assert!(text.contains("id = \"archive.winrar.extract\""));
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

// ---------------------------------------------------------------------------
// Windows 注册表探测
// ---------------------------------------------------------------------------

#[cfg(windows)]
pub(crate) mod win {
    use std::path::PathBuf;
    use windows_sys::Win32::System::Registry::{
        RegCloseKey, RegOpenKeyExW, RegQueryValueExW, HKEY, HKEY_CURRENT_USER,
        HKEY_LOCAL_MACHINE, KEY_READ, REG_SZ,
    };

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain([0]).collect()
    }

    fn read_string(root: HKEY, subkey: &str, value: &str) -> Option<String> {
        unsafe {
            let mut key: HKEY = std::ptr::null_mut();
            if RegOpenKeyExW(root, wide(subkey).as_ptr(), 0, KEY_READ, &mut key) != 0 {
                return None;
            }
            let name = if value.is_empty() { vec![0u16] } else { wide(value) };
            let mut size = 0u32;
            let mut kind = 0u32;
            let status = RegQueryValueExW(
                key,
                name.as_ptr(),
                std::ptr::null(),
                &mut kind,
                std::ptr::null_mut(),
                &mut size,
            );
            if status != 0 || kind != REG_SZ || size == 0 {
                RegCloseKey(key);
                return None;
            }
            let mut buf = vec![0u8; size as usize];
            let status = RegQueryValueExW(
                key,
                name.as_ptr(),
                std::ptr::null(),
                std::ptr::null_mut(),
                buf.as_mut_ptr().cast(),
                &mut size,
            );
            RegCloseKey(key);
            if status != 0 {
                return None;
            }
            // 去掉结尾 NUL 后按 UTF-16 解码。
            let u16_len = size as usize / 2;
            let units: Vec<u16> = buf
                .chunks_exact(2)
                .take(u16_len)
                .map(|c| u16::from_le_bytes([c[0], c[1]]))
                .collect();
            let s = String::from_utf16_lossy(&units);
            Some(s.trim_end_matches('\0').to_string())
        }
    }

    /// App Paths 注册：`HKCU/HKLM\...\App Paths\<name>` 默认值。
    pub fn reg_app_paths(exe_name: &str) -> Option<PathBuf> {
        let sub = format!("SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\App Paths\\{exe_name}");
        read_string(HKEY_CURRENT_USER, &sub, "")
            .or_else(|| read_string(HKEY_LOCAL_MACHINE, &sub, ""))
            .map(|s| s.trim_matches('"').to_string())
            .map(PathBuf::from)
            .filter(|p| p.is_file())
    }

    /// HKLM 指定键的字符串值。
    pub fn reg_value_hklm(subkey: &str, value: &str) -> Option<String> {
        read_string(HKEY_LOCAL_MACHINE, subkey, value)
    }
}
