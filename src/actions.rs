//! 自定义动作引擎（`actions.toml`）。
//!
//! * 数据层用 `toml_edit` 读写，**保留用户注释与排版**；
//! * 节点分两类：单一命令（`kind = "action"`）与二级子菜单（`kind = "submenu"`，
//!   子项通过 `parent` 指向子菜单 id）；
//! * 过滤条件：`target = file | dir | any` + `extensions` 后缀白名单；
//! * 占位符：`{target_path}` `{target_dir}` `{name}` `{stem}` `{ext}`；
//! * 压缩软件相关条目由 `crate::archivers` 托管，启动时自动同步。

use std::io;
use std::path::{Path, PathBuf};
#[cfg(windows)]
use std::os::windows::process::CommandExt;

use toml_edit::{ArrayOfTables, DocumentMut, Item, Table, Value};

/// 配置文件名字（位于 `%APPDATA%\Rove\`）
pub const FILE_NAME: &str = "actions.toml";

/// 归档类后缀白名单（解压动作默认生效范围）
pub const ARCHIVE_EXTENSIONS: &[&str] = &[
    "zip", "7z", "rar", "tar", "gz", "tgz", "bz2", "xz", "zst", "cab", "iso", "img", "arj", "lzh",
    "001",
];

// ---------------------------------------------------------------- 数据模型

/// 动作生效的目标类型
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetKind {
    File,
    Dir,
    Any,
}

impl TargetKind {
    pub fn as_str(self) -> &'static str {
        match self {
            TargetKind::File => "file",
            TargetKind::Dir => "dir",
            TargetKind::Any => "any",
        }
    }

    pub fn from_str(value: &str) -> Self {
        match value.trim().to_ascii_lowercase().as_str() {
            "file" => TargetKind::File,
            "dir" | "folder" | "directory" => TargetKind::Dir,
            _ => TargetKind::Any,
        }
    }
}

/// 生效条件
#[derive(Debug, Clone)]
pub struct Filter {
    pub target: TargetKind,
    pub extensions: Vec<String>,
}

impl Filter {
    /// 目标是否满足该动作的生效条件
    pub fn matches(&self, target: &Path) -> bool {
        let kind_ok = match self.target {
            TargetKind::File => target.is_file(),
            TargetKind::Dir => target.is_dir(),
            TargetKind::Any => true,
        };
        if !kind_ok {
            return false;
        }
        if self.extensions.is_empty() {
            return true;
        }
        match target.extension().and_then(|ext| ext.to_str()) {
            Some(ext) => self
                .extensions
                .iter()
                .any(|allowed| allowed.eq_ignore_ascii_case(ext)),
            None => false,
        }
    }
}

/// 节点类型
#[derive(Debug, Clone)]
pub enum NodeKind {
    Command {
        program: String,
        args: String,
        working_dir: String,
        filter: Filter,
    },
    Submenu,
}

/// 一个动作节点
#[derive(Debug, Clone)]
pub struct Action {
    pub id: String,
    pub label: String,
    pub enabled: bool,
    /// 二级子菜单父节点 id（顶级动作为 None）
    pub parent: Option<String>,
    /// 压缩软件探测器托管的工具标识（winrar / 7zip / bandizip）
    pub tool: Option<String>,
    /// 因工具缺失被自动禁用；工具恢复后自动解除
    pub auto_disabled: bool,
    pub kind: NodeKind,
}

impl Action {
    pub fn is_submenu(&self) -> bool {
        matches!(self.kind, NodeKind::Submenu)
    }

    pub fn command(&self) -> Option<(&str, &str, &str, &Filter)> {
        match &self.kind {
            NodeKind::Command {
                program,
                args,
                working_dir,
                filter,
            } => Some((program, args, working_dir, filter)),
            NodeKind::Submenu => None,
        }
    }
}

// ---------------------------------------------------------------- 占位符

/// 占位符取值
#[derive(Debug, Clone, Default)]
pub struct Placeholders {
    pub target_path: String,
    pub target_dir: String,
    pub name: String,
    pub stem: String,
    pub ext: String,
}

pub fn placeholders(target: &Path) -> Placeholders {
    let name = target
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    let stem = target
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    let ext = target
        .extension()
        .map(|e| e.to_string_lossy().to_string())
        .unwrap_or_default();
    let target_dir = target
        .parent()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();
    Placeholders {
        target_path: target.to_string_lossy().to_string(),
        target_dir,
        name,
        stem,
        ext,
    }
}

/// 展开 `{target_path}` / `{target_dir}` / `{name}` / `{stem}` / `{ext}`
pub fn expand(template: &str, vars: &Placeholders) -> String {
    template
        .replace("{target_path}", &vars.target_path)
        .replace("{target_dir}", &vars.target_dir)
        .replace("{name}", &vars.name)
        .replace("{stem}", &vars.stem)
        .replace("{ext}", &vars.ext)
}

// ---------------------------------------------------------------- 执行

/// 简易参数拆分（非 Windows 平台使用）：按空格切分并去除引号
#[cfg(not(windows))]
fn split_args(args: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    for ch in args.chars() {
        match quote {
            Some(q) if ch == q => quote = None,
            Some(_) => current.push(ch),
            None if ch == '"' || ch == '\'' => quote = Some(ch),
            None if ch.is_whitespace() => {
                if !current.is_empty() {
                    out.push(std::mem::take(&mut current));
                }
            }
            None => current.push(ch),
        }
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

/// 执行动作（异步拉起外部程序，不阻塞 UI）
pub fn run(action: &Action, target: &Path) -> io::Result<()> {
    let Some((program, args, working_dir, _)) = action.command() else {
        return Err(io::Error::other("not a command action"));
    };
    let vars = placeholders(target);
    let program = expand(program, &vars).trim().to_string();
    if program.is_empty() {
        return Err(io::Error::other("empty program path"));
    }
    let args = expand(args, &vars);

    let mut command = std::process::Command::new(&program);
    if !args.trim().is_empty() {
        #[cfg(windows)]
        {
            // 以原始参数字符串传递，保留模板中的引号语义
            command.raw_arg(format!(" {args}"));
        }
        #[cfg(not(windows))]
        {
            for arg in split_args(&args) {
                command.arg(arg);
            }
        }
    }
    if !working_dir.trim().is_empty() {
        command.current_dir(expand(working_dir, &vars));
    }
    command.spawn().map(|_| ())
}

// ---------------------------------------------------------------- 存储

/// `actions.toml` 存储：文档保留排版，视图供 UI 使用
#[derive(Clone)]
pub struct Store {
    path: PathBuf,
    doc: DocumentMut,
    /// 加载时的原始文本，用于判断是否真的需要写盘
    original: String,
    actions: Vec<Action>,
}

impl Store {
    /// 从配置目录加载；文件不存在时返回空存储
    pub fn load() -> Self {
        let path = crate::settings::config_dir().join(FILE_NAME);
        let text = std::fs::read_to_string(&path).unwrap_or_default();
        let doc = text.parse::<DocumentMut>().unwrap_or_default();
        let actions = parse_actions(&doc);
        Self {
            path,
            doc,
            original: text,
            actions,
        }
    }

    pub fn actions(&self) -> &[Action] {
        &self.actions
    }

    pub fn find(&self, id: &str) -> Option<&Action> {
        self.actions.iter().find(|action| action.id == id)
    }

    /// 写回磁盘（保留注释与排版）；内容无变化时跳过
    pub fn save(&mut self) -> io::Result<()> {
        let text = self.doc.to_string();
        if text == self.original {
            return Ok(());
        }
        std::fs::write(&self.path, &text)?;
        self.original = text;
        Ok(())
    }

    /// 切换启用开关（用户手动操作会清除 auto_disabled 标记）
    pub fn set_enabled(&mut self, id: &str, enabled: bool) {
        let Some(index) = self.index_of(id) else {
            return;
        };
        if let Some(table) = self.table_mut(index) {
            table_set_bool(table, "enabled", enabled);
            if enabled {
                table_remove(table, "auto_disabled");
            }
        }
        self.actions[index].enabled = enabled;
        if enabled {
            self.actions[index].auto_disabled = false;
        }
    }

    /// 更新一个命令动作的全部可编辑属性
    pub fn update_command(
        &mut self,
        id: &str,
        label: &str,
        program: &str,
        args: &str,
        working_dir: &str,
        target: TargetKind,
        extensions: &[String],
    ) {
        let Some(index) = self.index_of(id) else {
            return;
        };
        let Some(table) = self.table_mut(index) else {
            return;
        };
        table_set_str(table, "label", label);
        table_set_str(table, "program", program);
        table_set_str(table, "args", args);
        table_set_str(table, "working_dir", working_dir);
        table_set_str(table, "target", target.as_str());
        table_set_extensions(table, extensions);

        self.actions[index].label = label.to_string();
        if let NodeKind::Command {
            program: p,
            args: a,
            working_dir: w,
            filter,
        } = &mut self.actions[index].kind
        {
            *p = program.to_string();
            *a = args.to_string();
            *w = working_dir.to_string();
            filter.target = target;
            filter.extensions = extensions.to_vec();
        }
    }

    /// 新增一个命令动作
    pub fn add_command(&mut self, action: Action) {
        let mut table = Table::new();
        table_set_str(&mut table, "id", &action.id);
        table_set_str(&mut table, "label", &action.label);
        table_set_bool(&mut table, "enabled", action.enabled);
        table_set_str(&mut table, "kind", "action");
        if let Some(parent) = &action.parent {
            table_set_str(&mut table, "parent", parent);
        }
        if let NodeKind::Command {
            program,
            args,
            working_dir,
            filter,
        } = &action.kind
        {
            table_set_str(&mut table, "program", program);
            table_set_str(&mut table, "args", args);
            table_set_str(&mut table, "working_dir", working_dir);
            table_set_str(&mut table, "target", filter.target.as_str());
            table_set_extensions(&mut table, &filter.extensions);
        }
        ensure_array(&mut self.doc).push(table);
        self.actions.push(action);
    }

    /// 新增一个二级子菜单节点
    pub fn add_submenu(&mut self, id: &str, label: &str) {
        if self.index_of(id).is_some() {
            return;
        }
        let mut table = Table::new();
        table_set_str(&mut table, "id", id);
        table_set_str(&mut table, "label", label);
        table_set_bool(&mut table, "enabled", true);
        table_set_str(&mut table, "kind", "submenu");
        ensure_array(&mut self.doc).push(table);
        self.actions.push(Action {
            id: id.to_string(),
            label: label.to_string(),
            enabled: true,
            parent: None,
            tool: None,
            auto_disabled: false,
            kind: NodeKind::Submenu,
        });
    }

    /// 托管条目的自动禁用 / 自动解除禁用
    pub fn set_auto_disabled(&mut self, id: &str, disabled: bool) {
        let Some(index) = self.index_of(id) else {
            return;
        };
        if disabled {
            if self.actions[index].enabled {
                if let Some(table) = self.table_mut(index) {
                    table_set_bool(table, "enabled", false);
                    table_set_bool(table, "auto_disabled", true);
                }
                self.actions[index].enabled = false;
            }
            self.actions[index].auto_disabled = true;
        } else if self.actions[index].auto_disabled {
            // 工具恢复：解除禁用
            if let Some(table) = self.table_mut(index) {
                table_set_bool(table, "enabled", true);
                table_remove(table, "auto_disabled");
            }
            self.actions[index].enabled = true;
            self.actions[index].auto_disabled = false;
        }
    }

    /// 删除一个动作（子菜单会连带删除其子项）
    pub fn remove(&mut self, id: &str) {
        let children: Vec<String> = self
            .actions
            .iter()
            .filter(|action| action.parent.as_deref() == Some(id))
            .map(|action| action.id.clone())
            .collect();
        for child in children {
            self.remove(&child);
        }
        let Some(index) = self.index_of(id) else {
            return;
        };
        if let Some(array) = self.doc.get_mut("action").and_then(Item::as_array_of_tables_mut) {
            if index < array.len() {
                array.remove(index);
            }
        }
        self.actions.remove(index);
    }

    fn index_of(&self, id: &str) -> Option<usize> {
        self.actions.iter().position(|action| action.id == id)
    }

    fn table_mut(&mut self, index: usize) -> Option<&mut Table> {
        self.doc
            .get_mut("action")
            .and_then(Item::as_array_of_tables_mut)
            .and_then(|array| array.get_mut(index))
    }

    /// 由探测器托管：插入 / 更新 / 禁用，全部保留原排版
    pub fn sync_managed(&mut self, entries: &[ManagedEntry]) {
        for entry in entries {
            let existing = self.index_of(&entry.id);
            match existing {
                Some(index) => {
                    // 路径迁移：原程序已失效则更新为新路径
                    if entry.program_changed {
                        if let Some(table) = self.table_mut(index) {
                            table_set_str(table, "program", &entry.program);
                        }
                        if let NodeKind::Command { program: p, .. } = &mut self.actions[index].kind {
                            *p = entry.program.clone();
                        }
                    }
                    // 角色变化：主动作 <-> 其他压缩软件
                    if entry.role_changed {
                        if let Some(table) = self.table_mut(index) {
                            table_set_str(table, "label", &entry.label);
                            match &entry.parent {
                                Some(parent) => table_set_str(table, "parent", parent),
                                None => table_remove(table, "parent"),
                            }
                        }
                        self.actions[index].label = entry.label.clone();
                        self.actions[index].parent = entry.parent.clone();
                    }
                    if entry.detected {
                        // 工具恢复：解除自动禁用
                        if self.actions[index].auto_disabled {
                            self.set_enabled(&entry.id, true);
                        }
                    } else if self.actions[index].enabled {
                        // 工具缺失：优雅标记禁用
                        if let Some(table) = self.table_mut(index) {
                            table_set_bool(table, "enabled", false);
                            table_set_bool(table, "auto_disabled", true);
                        }
                        self.actions[index].enabled = false;
                        self.actions[index].auto_disabled = true;
                    }
                }
                None => {
                    let mut table = Table::new();
                    table_set_str(&mut table, "id", &entry.id);
                    table_set_str(&mut table, "label", &entry.label);
                    table_set_bool(&mut table, "enabled", entry.detected);
                    table_set_str(&mut table, "kind", "action");
                    table_set_str(&mut table, "tool", &entry.tool);
                    if let Some(parent) = &entry.parent {
                        table_set_str(&mut table, "parent", parent);
                    }
                    table_set_str(&mut table, "program", &entry.program);
                    table_set_str(&mut table, "args", &entry.args);
                    table_set_str(&mut table, "working_dir", &entry.working_dir);
                    table_set_str(&mut table, "target", entry.target.as_str());
                    table_set_extensions(&mut table, &entry.extensions);
                    if !entry.detected {
                        table_set_bool(&mut table, "auto_disabled", true);
                    }
                    ensure_array(&mut self.doc).push(table);

                    self.actions.push(Action {
                        id: entry.id.clone(),
                        label: entry.label.clone(),
                        enabled: entry.detected,
                        parent: entry.parent.clone(),
                        tool: Some(entry.tool.clone()),
                        auto_disabled: !entry.detected,
                        kind: NodeKind::Command {
                            program: entry.program.clone(),
                            args: entry.args.clone(),
                            working_dir: entry.working_dir.clone(),
                            filter: Filter {
                                target: entry.target,
                                extensions: entry.extensions.clone(),
                            },
                        },
                    });
                }
            }
        }
    }
}

/// 探测器托管条目的期望状态
pub struct ManagedEntry {
    pub id: String,
    pub label: String,
    pub tool: String,
    pub parent: Option<String>,
    pub program: String,
    pub args: String,
    pub working_dir: String,
    pub target: TargetKind,
    pub extensions: Vec<String>,
    /// 工具当前是否探测到
    pub detected: bool,
    /// 存储中的程序路径失效（路径迁移），需要更新
    pub program_changed: bool,
    /// 角色发生变化（主动作 <-> 其他软件），需要更新 parent/label
    pub role_changed: bool,
}

// ---------------------------------------------------------------- 文档解析

fn parse_actions(doc: &DocumentMut) -> Vec<Action> {
    let Some(array) = doc.get("action").and_then(Item::as_array_of_tables) else {
        return Vec::new();
    };
    array.iter().map(parse_action).collect()
}

fn parse_action(table: &Table) -> Action {
    let id = table_get_str(table, "id").unwrap_or_default();
    let label = table_get_str(table, "label").unwrap_or_else(|| id.clone());
    let enabled = table_get_bool(table, "enabled").unwrap_or(true);
    let parent = table_get_str(table, "parent").filter(|value| !value.is_empty());
    let tool = table_get_str(table, "tool").filter(|value| !value.is_empty());
    let auto_disabled = table_get_bool(table, "auto_disabled").unwrap_or(false);

    let kind = if table_get_str(table, "kind").as_deref() == Some("submenu") {
        NodeKind::Submenu
    } else {
        NodeKind::Command {
            program: table_get_str(table, "program").unwrap_or_default(),
            args: table_get_str(table, "args").unwrap_or_default(),
            working_dir: table_get_str(table, "working_dir").unwrap_or_default(),
            filter: Filter {
                target: TargetKind::from_str(&table_get_str(table, "target").unwrap_or_default()),
                extensions: table_get_extensions(table),
            },
        }
    };

    Action {
        id,
        label,
        enabled,
        parent,
        tool,
        auto_disabled,
        kind,
    }
}

fn ensure_array(doc: &mut DocumentMut) -> &mut ArrayOfTables {
    if doc
        .get("action")
        .and_then(Item::as_array_of_tables)
        .is_none()
    {
        doc.insert("action", Item::ArrayOfTables(ArrayOfTables::new()));
    }
    doc.get_mut("action")
        .and_then(Item::as_array_of_tables_mut)
        .expect("action array")
}

// ---------------------------------------------------------------- 字段读写

fn table_get_str(table: &Table, key: &str) -> Option<String> {
    table
        .get(key)
        .and_then(Item::as_str)
        .map(|value| value.to_string())
}

fn table_get_bool(table: &Table, key: &str) -> Option<bool> {
    table.get(key).and_then(Item::as_bool)
}

fn table_get_extensions(table: &Table) -> Vec<String> {
    table
        .get("extensions")
        .and_then(Item::as_array)
        .map(|array| {
            array
                .iter()
                .filter_map(|value| value.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default()
}

fn table_set_str(table: &mut Table, key: &str, value: &str) {
    table.insert(key, Item::Value(Value::from(value.to_string())));
}

fn table_set_bool(table: &mut Table, key: &str, value: bool) {
    table.insert(key, Item::Value(Value::from(value)));
}

fn table_set_extensions(table: &mut Table, extensions: &[String]) {
    if extensions.is_empty() {
        table.remove("extensions");
        return;
    }
    let mut array = toml_edit::Array::new();
    for extension in extensions {
        array.push(extension.clone());
    }
    table.insert("extensions", Item::Value(Value::Array(array)));
}

fn table_remove(table: &mut Table, key: &str) {
    table.remove(key);
}
