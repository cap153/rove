//! 压缩软件探测结果 -> `actions.toml` 的启动同步。
//!
//! * 首次启动：写入探测到的 7-Zip / WinRAR / Bandizip 动作；
//! * 每次启动：比对开始菜单与注册表，处理路径迁移、卸载与重新安装；
//! * 主动作（压缩 / 解压）默认 WinRAR，其余工具收纳进"其他压缩软件"二级子菜单；
//! * 所有写回均保留用户注释与排版（`toml_edit`）。

use std::path::PathBuf;
use std::sync::OnceLock;

use crate::actions;
use crate::actions::{ManagedEntry, Store, TargetKind, ARCHIVE_EXTENSIONS};
use crate::archivers::{self, Tool};

/// "其他压缩软件"二级子菜单 id
pub const OTHERS_SUBMENU_ID: &str = "archive.others";

/// 启动时同步后的存储实例（供 UI 线程取用）
static STORE: OnceLock<Store> = OnceLock::new();

/// 启动引导：加载 actions.toml -> 探测同步 -> 写回（保留排版与注释）。
/// 在进入事件循环前调用，避免阻塞 UI 线程。
pub fn bootstrap() {
    let mut store = actions::Store::load();
    sync(&mut store);
    if let Err(e) = store.save() {
        eprintln!("[rove] actions.toml 同步失败: {e}");
    }
    let _ = STORE.set(store);
}

/// 取用启动时同步过的存储（未初始化则直接加载）
pub fn store() -> Store {
    STORE
        .get()
        .cloned()
        .unwrap_or_else(actions::Store::load)
}

/// 每个工具的命令行模板
struct ToolSpec {
    archive_ext: &'static str,
    compress_args: &'static str,
    extract_args: &'static str,
}

fn spec(tool: Tool) -> ToolSpec {
    match tool {
        Tool::WinRAR => ToolSpec {
            archive_ext: "rar",
            compress_args: "a -r -ep1 \"{target_dir}\\{stem}.rar\" \"{target_path}\"",
            extract_args: "x -r \"{target_path}\" \"{target_dir}\\{stem}\\\"",
        },
        Tool::SevenZip => ToolSpec {
            archive_ext: "7z",
            compress_args: "a -t7z \"{target_dir}\\{stem}.7z\" \"{target_path}\"",
            extract_args: "x -o\"{target_dir}\\{stem}\" -y \"{target_path}\"",
        },
        Tool::Bandizip => ToolSpec {
            archive_ext: "zip",
            compress_args: "c -y \"{target_dir}\\{stem}.zip\" \"{target_path}\"",
            extract_args: "x -y \"{target_path}\" \"{target_dir}\\{stem}\"",
        },
    }
}

/// 启动时同步：探测 -> 生成 / 更新 / 禁用托管条目
pub fn sync(store: &mut Store) {
    let detected = archivers::detect();
    let primary = archivers::primary_tool(&detected);
    let has_others = detected.iter().any(|entry| Some(entry.tool) != primary);

    // 二级子菜单：仅在存在非主动作工具时启用
    if has_others {
        store.add_submenu(OTHERS_SUBMENU_ID, "其他压缩软件");
        store.set_auto_disabled(OTHERS_SUBMENU_ID, false);
    } else if store.find(OTHERS_SUBMENU_ID).is_some() {
        store.set_auto_disabled(OTHERS_SUBMENU_ID, true);
    }

    let mut entries = Vec::new();
    for entry in &detected {
        let is_primary = Some(entry.tool) == primary;
        let spec = spec(entry.tool);
        let parent = (!is_primary).then(|| OTHERS_SUBMENU_ID.to_string());
        let program = entry.program.display().to_string();

        entries.push(ManagedEntry {
            id: format!("archive.{}.compress", entry.tool.id()),
            label: if is_primary {
                format!("压缩为 {}", spec.archive_ext.to_uppercase())
            } else {
                format!("{} 压缩", entry.tool.display())
            },
            tool: entry.tool.id().to_string(),
            parent: parent.clone(),
            program: program.clone(),
            args: spec.compress_args.to_string(),
            working_dir: "{target_dir}".to_string(),
            target: TargetKind::Any,
            extensions: Vec::new(),
            detected: true,
            program_changed: program_needs_update(store, &format!("archive.{}.compress", entry.tool.id()), &entry.program),
            role_changed: role_changed(store, &format!("archive.{}.compress", entry.tool.id()), is_primary, &parent),
        });

        entries.push(ManagedEntry {
            id: format!("archive.{}.extract", entry.tool.id()),
            label: if is_primary {
                "解压到同名文件夹".to_string()
            } else {
                format!("{} 解压", entry.tool.display())
            },
            tool: entry.tool.id().to_string(),
            parent: parent.clone(),
            program: program.clone(),
            args: spec.extract_args.to_string(),
            working_dir: "{target_dir}".to_string(),
            target: TargetKind::File,
            extensions: ARCHIVE_EXTENSIONS
                .iter()
                .map(|ext| ext.to_string())
                .collect(),
            detected: true,
            program_changed: program_needs_update(store, &format!("archive.{}.extract", entry.tool.id()), &entry.program),
            role_changed: role_changed(store, &format!("archive.{}.extract", entry.tool.id()), is_primary, &parent),
        });
    }
    store.sync_managed(&entries);

    // 已卸载工具的遗留条目：优雅标记禁用
    let missing: Vec<String> = store
        .actions()
        .iter()
        .filter(|action| {
            action
                .tool
                .as_deref()
                .is_some_and(|tool| !detected.iter().any(|entry| entry.tool.id() == tool))
        })
        .map(|action| action.id.clone())
        .collect();
    for id in missing {
        store.set_auto_disabled(&id, true);
    }
}

/// 存储中的程序路径是否已失效（路径迁移）
fn program_needs_update(store: &Store, id: &str, detected: &std::path::Path) -> bool {
    match store.find(id).and_then(|action| action.command()) {
        Some((program, ..)) => {
            let stored = PathBuf::from(program);
            !stored.exists() && stored != detected
        }
        None => false,
    }
}

/// 角色是否发生变化（主动作 <-> 其他软件，通过 parent 归属判断）
fn role_changed(store: &Store, id: &str, _is_primary: bool, parent: &Option<String>) -> bool {
    match store.find(id) {
        Some(action) => action.parent != *parent,
        None => false,
    }
}

/// 供 UI 展示的探测摘要（设置界面使用）
pub fn summary() -> String {
    let detected = archivers::detect();
    if detected.is_empty() {
        return "未探测到压缩软件".to_string();
    }
    detected
        .iter()
        .map(|entry| entry.tool.display())
        .collect::<Vec<_>>()
        .join(" / ")
}
