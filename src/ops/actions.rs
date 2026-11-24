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

/// 「添加到压缩文件」的动作 id（压缩软件关联项）。
pub const ARCHIVE_ADD_ID: &str = "builtin.archive_add";
/// 「解压文件」的动作 id（弹工具自己的「解压路径和选项」对话框）。
pub const ARCHIVE_EXTRACT_FILES_ID: &str = "builtin.archive_extract_files";
/// 「解压到当前文件夹」的动作 id。
pub const ARCHIVE_EXTRACT_HERE_ID: &str = "builtin.archive_extract_here";
/// 「解压到"{name}"\」的动作 id。
pub const ARCHIVE_EXTRACT_ID: &str = "builtin.archive_extract";
/// 设置页里这几条单独成一组用的组名（菜单里它们仍是一级项）。
pub const ARCHIVE_GROUP: &str = "一级菜单(压缩)";

/// 压缩关联项的动作 id，按菜单里的固定顺序排列。
pub const ARCHIVE_IDS: [&str; 4] = [
    ARCHIVE_ADD_ID,
    ARCHIVE_EXTRACT_FILES_ID,
    ARCHIVE_EXTRACT_HERE_ID,
    ARCHIVE_EXTRACT_ID,
];

/// 压缩关联项布局的代数：改了条数/默认参数就 +1，老配置会按新模板补齐。
pub const ARCHIVE_GEN: i64 = 7;

fn ext_lower(p: &Path) -> String {
    p.extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default()
}

/// 这个路径是不是压缩包（含分卷的各卷）。
pub fn is_archive_file(p: &Path) -> bool {
    let ext = ext_lower(p);
    if ARCHIVE_EXTS.iter().any(|x| *x == ext) {
        return true;
    }
    // 分卷：`x.7z.001` / `x.zip.001` / `x.rar.001` / 旧式 `x.r00`、`x.z01`。
    let three: Vec<char> = ext.chars().collect();
    if three.len() == 3 && three[1..].iter().all(|c| c.is_ascii_digit()) {
        return match three[0] {
            'r' | 'z' => true,
            c if c.is_ascii_digit() => true,
            _ => false,
        };
    }
    false
}

/// 是不是“后续卷”（`part2.rar`、`x.7z.002`、`x.r00`、`x.z01`）。
///
/// 只有第一卷能交给压缩软件：把整组卷一起递过去，WinRAR 会卡在
/// 「找不到下一卷」的对话框上（实测），7-Zip 直接失败。
fn is_later_volume(p: &Path) -> bool {
    let ext = ext_lower(p);
    let three: Vec<char> = ext.chars().collect();
    if three.len() == 3 && three[1..].iter().all(|c| c.is_ascii_digit()) {
        if three[0].is_ascii_digit() {
            return ext != "001";
        }
        // r00/z01 起头的旧式分卷：永远不是“第一卷”。
        return true;
    }
    // `name.part12.rar` / `name.part02.rar`
    let name = p
        .file_name()
        .map(|n| n.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    if let Some(idx) = name.rfind(".part") {
        let tail = &name[idx + 5..];
        let digits: String = tail.chars().take_while(|c| c.is_ascii_digit()).collect();
        if !digits.is_empty() {
            return digits.trim_start_matches('0') != "1";
        }
    }
    false
}

/// 后续卷对应的「第一卷」候选（按优先级，纯字符串推导，不碰磁盘）。
fn first_volume_candidates(p: &Path) -> Option<Vec<PathBuf>> {
    if !is_later_volume(p) {
        return None;
    }
    // 全程用 lower 切片：大小写转换可能改变字节长度，拿 lower 的下标去切原始
    // 文件名会越界 panic。Windows 文件名不分大小写，候选用小写照样能找到。
    let name = p.file_name()?.to_string_lossy().to_lowercase();
    let dir = p.parent()?.to_path_buf();
    // `name.part12.rar` → `name.part1.rar` / `name.part01.rar`
    if let Some(idx) = name.rfind(".part") {
        let tail = &name[idx + 5..];
        let digits: String = tail.chars().take_while(|c| c.is_ascii_digit()).collect();
        if !digits.is_empty() {
            let rest = &name[idx + 5 + digits.len()..];
            let stem = &name[..idx];
            let mut padded = String::from("1");
            while padded.len() < digits.len() {
                padded.insert(0, '0');
            }
            let mut out = vec![dir.join(format!("{stem}.part{padded}{rest}"))];
            let plain = dir.join(format!("{stem}.part1{rest}"));
            if plain != out[0] {
                out.push(plain);
            }
            return Some(out);
        }
    }
    let ext = ext_lower(p);
    let three: Vec<char> = ext.chars().collect();
    // `x.7z.002` → `x.7z.001`
    if three.len() == 3 && three.iter().all(|c| c.is_ascii_digit()) {
        return Some(vec![p.with_extension("001")]);
    }
    // 旧式 `x.r00` → `x.rar`；`x.z01` → `x.zip`
    if three.len() == 3 && three[0].is_ascii_alphabetic() {
        let base = p.with_extension("");
        return Some(vec![
            base.with_extension("rar"),
            base.with_extension("zip"),
            base.with_extension("7z"),
        ]);
    }
    None
}

/// 把选中的某一卷解析成「该交给工具的那一个」：后续卷会自己去找第一卷。
fn resolve_first_volume(p: &Path) -> Option<PathBuf> {
    if !is_later_volume(p) {
        return Some(p.to_path_buf());
    }
    first_volume_candidates(p)?
        .into_iter()
        .find(|c| c.is_file())
}

/// 选中项里真正该交给压缩软件的那些：每组分卷只留第一卷（选中 part2
/// 也会自己去找 part1）。找不到第一卷的那些组不交出去 —— 缺第一卷本来
/// 也解不开，交出去只会弹错误框。
pub fn extractable_archives(paths: &[PathBuf]) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    for p in paths.iter().filter(|p| is_archive_file(p)) {
        if let Some(first) = resolve_first_volume(p) {
            if !out.contains(&first) {
                out.push(first);
            }
        }
    }
    out
}

/// 找不到第一卷、被跳过的分卷数量（状态栏提示用，免得用户以为点了没反应）。
pub fn skipped_volumes(paths: &[PathBuf]) -> usize {
    paths
        .iter()
        .filter(|p| is_archive_file(p) && is_later_volume(p) && resolve_first_volume(p).is_none())
        .count()
}

/// 是不是压缩关联项。
pub fn is_archive_action(id: &str) -> bool {
    ARCHIVE_IDS.contains(&id)
}

/// 当成压缩包的扩展名（「添加到压缩文件」要避开它们，解压类只认它们）。
pub const ARCHIVE_EXTS: &[&str] = &[
    "rar", "zip", "zipx", "7z", "tar", "gz", "tgz", "bz2", "tbz", "tbz2", "xz", "txz", "lzma",
    "zst", "iso", "cab", "lzh", "arj", "z", "jar", "wim", "esd", "rev",
];

/// 生成压缩软件关联的动作（按 [`ARCHIVE_IDS`] 的顺序）。
///
/// `tool = None`（没装）时程序与参数留空、`enabled = false` —— 设置页里仍然
/// 有这几行，用户装了软件之后点「更新」就能接上。
pub fn archive_actions(tool: Option<&crate::sys::archive::Tool>) -> Vec<ActionEntry> {
    use crate::sys::archive::ToolKind;

    let (add_program, extract_program, templates) = match tool {
        Some(t) => {
            let add_program = t.add_program().to_string_lossy().into_owned();
            let extract_program = t.extract_program().to_string_lossy().into_owned();
            // 参数模板约定：模板自带引号，占位符替换成裸路径。
            // 「解压到当前文件夹」这类要显式给目标目录时，WinRAR 的目录参数
            // 必须以 `\` 收尾，否则它报「诊断信息」并且什么都不解。
            // `{dest}` 只出现在「解压文件」里，运行前换成用户挑的目录。
            let templates: [String; 4] = match t.kind {
                // 三款都没有「命令行弹对话框」的开关（WinRAR 试了空档案名 / `?` /
                // `--` / `-ieml`，7zG 试了 `a` 不给档案名 / `-o?`，Bandizip 试了
                // `x` 不给 `-o:`，全都不弹，只报错或直接按默认干）：
                // 所以「添加到压缩文件」直接用默认配置建 `名字.<ext>`，
                // 「解压文件」由 Rove 自己问一个目标目录（模板里的 `{dest}`）。
                ToolKind::WinRar => [
                    // -ep1：不把选中项的路径一起存进包里，否则包里会多出
                    // 一层层目录（用户说的「像洋葱」）。包里只留文件名。
                    r#"a -ep1 "{target_dir}\{archive_name}.rar" {target_paths}"#.to_string(),
                    // 解压类每次只处理一个包（多选由调用方逐个跑）：三款工具都
                    // 不接受「一次给多个包」，多出来的参数会被当成包内文件名。
                    r#"x {target_paths} "{dest}\""#.to_string(),
                    r#"x {target_paths} "{target_dir}\""#.to_string(),
                    r#"x -ad {target_paths} "{target_dir}\""#.to_string(),
                ],
                // 7zG：`a` 直接建包，`x` 靠 `-o` 指定落点。
                ToolKind::SevenZip => [
                    r#"a "{target_dir}\{archive_name}.7z" {target_paths}"#.to_string(),
                    r#"x {target_paths} -o"{dest}""#.to_string(),
                    r#"x {target_paths} -o"{target_dir}""#.to_string(),
                    r#"x {target_paths} -o"{target_dir}\{stem}""#.to_string(),
                ],
                // Bandizip：开关必须写在压缩包**前面**，否则 `-o:` 被忽略、
                // 直接解到当前目录（实测）。
                ToolKind::Bandizip => [
                    r#"c "{target_dir}\{archive_name}.zip" {target_paths}"#.to_string(),
                    r#"x -o:"{dest}" {target_paths}"#.to_string(),
                    r#"x -o:"{target_dir}" {target_paths}"#.to_string(),
                    r#"x -o:"{target_dir}\{stem}" {target_paths}"#.to_string(),
                ],
            };
            (add_program, extract_program, templates)
        }
        None => (
            String::new(),
            String::new(),
            std::array::from_fn(|_| String::new()),
        ),
    };

    let make = |id: &str, label: &str, program: &str, args: &str| ActionEntry {
        id: id.to_string(),
        label: label.to_string(),
        tool: tool.map(|t| t.kind.id().to_string()).unwrap_or_default(),
        program: program.to_string(),
        args: args.to_string(),
        working_dir: "{target_dir}".into(),
        target: "file".into(),
        extensions: vec![],
        enabled: tool.is_some(),
        group: "".into(),
        builtin: true,
    };

    let [add_args, files_args, here_args, extract_args] = templates;
    vec![
        // 压缩：文件和文件夹都收，压缩包本身不收。包名走 `{archive_name}`
        // （单选 = 那一项的主名；多选 = 当前文件夹名，盘根用盘符）。
        make(ARCHIVE_ADD_ID, "添加到压缩文件", &add_program, &add_args),
        make(
            ARCHIVE_EXTRACT_FILES_ID,
            "解压文件",
            &extract_program,
            &files_args,
        ),
        make(
            ARCHIVE_EXTRACT_HERE_ID,
            "解压到当前文件夹",
            &extract_program,
            &here_args,
        ),
        make(
            ARCHIVE_EXTRACT_ID,
            r#"解压到"{name}\""#,
            &extract_program,
            &extract_args,
        ),
    ]
}

/// 已废弃的内置动作 id。
///
/// 这些项全部由系统菜单提供，隐藏它们改用设置页的关键词过滤，
/// 不再需要单独开关；启动时从 actions.toml 里清掉历史残留。
pub const RETIRED_BUILTINS: &[&str] = &[
    "builtin.open",
    "builtin.sendto",
    "builtin.delete",
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
        // 「删除」紧跟在「剪切」下面：它与复制/剪切同一类（作用于选中项），
        // 而且必须走 Rove 自己的任务队列 —— 系统菜单那一项调的是 Explorer 的
        // 删除，Rove 记不了账（撤消栈里没有它），所以默认把它藏掉。
        ActionEntry {
            id: "builtin.trash".into(),
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
        // 「属性」/「重命名」紧跟在「删除」下面：系统菜单也有这两项，但系统
        // 项只是对话框，Rove 自己的重命名要接 F2 那套行内编辑，属性要在
        // 系统菜单被藏掉时仍然可用。
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
        // 「复制路径」放在「剪切」下面：菜单按本函数顺序输出一级项，
        // 复制 / 剪切 / 复制路径 三个密切相关，摆在一起更好认。
        ActionEntry {
            id: "builtin.copy_path".into(),
            label: "复制路径".into(),
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
        // 「打开方式」：走 Shell 的 openas 动词，弹出跟资源管理器同一个原生选择器。
        // 系统菜单里带省略号的那一项默认被关键词藏掉；不带省略号的（展开程序
        // 子菜单的那种）保留原样。
        ActionEntry {
            id: "builtin.open_with".into(),
            label: "打开方式".into(),
            tool: "".into(),
            program: "".into(),
            args: "".into(),
            working_dir: "".into(),
            // 只对文件出现：Shell 的 openas 动词是给文件用的，文件夹右键系统
            // 菜单里也没有「打开方式」这一项。
            target: "file".into(),
            extensions: vec![],
            enabled: true,
            group: "".into(),
            builtin: true,
        },
        // 「批量重命名」紧跟在「复制路径」下面：它同样作用于选中项，
        // 而且是 Rove 自己最容易找不到的一个功能。
        ActionEntry {
            id: "builtin.batch_rename".into(),
            label: "批量重命名".into(),
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
        // 两个快捷方式动作紧跟在「批量重命名」下面：都是「对一个/一组文件产出一个
        // 新东西」的操作，摆在一起更好认。
        ActionEntry {
            id: "builtin.create_shortcut".into(),
            label: "创建快捷方式".into(),
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
            id: "builtin.send_to_desktop".into(),
            label: "桌面快捷方式".into(),
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
    /// 「这一项是文件还是文件夹」优先由调用方给的快照回答。
    ///
    /// `is_dir_of` 返回 `None` 表示快照里没有这一项（搜索结果、虚拟视图等），
    /// 这时才退回磁盘查询 —— `is_dir()` / `is_file()` 是真正的磁盘访问，
    /// 网络路径上可能卡几秒，而右键菜单是在界面线程上构建的。
    pub fn matches_selection(
        &self,
        paths: &[PathBuf],
        is_dir_of: impl Fn(&Path) -> Option<bool>,
    ) -> bool {
        let Some(first) = paths.first() else {
            return false;
        };
        // 压缩软件关联项：没探测到程序就不进菜单（设置页里仍可编辑/更新）。
        if is_archive_action(&self.id) && self.program.is_empty() {
            return false;
        }
        if is_archive_action(&self.id) {
            let ext_of = |p: &Path| {
                p.extension()
                    .map(|e| e.to_string_lossy().to_lowercase())
                    .unwrap_or_default()
            };
            let listed = |ext: &str, exts: &[String]| {
                exts.iter().any(|e| e.eq_ignore_ascii_case(ext))
            };
            let packed = |ext: &str| ARCHIVE_EXTS.iter().any(|e| *e == ext);
            // 扩展名留空 = 用内置默认；用户在设置页填了扩展名就按他填的来。
            if self.id == ARCHIVE_ADD_ID {
                // 压缩：文件和文件夹都收（文件夹本身没有扩展名，永远算「不是压缩包」），
                // 只要选中项里还有能压的东西就露出来。
                return paths.iter().any(|p| {
                    if is_dir_of(p).unwrap_or_else(|| p.is_dir()) {
                        return true;
                    }
                    let ext = ext_of(p);
                    if self.extensions.is_empty() {
                        !packed(&ext)
                    } else {
                        listed(&ext, &self.extensions)
                    }
                });
            }
            // 解压类只认压缩包文件（分卷的各卷也算，挑卷交给 extractable_archives）。
            let is_dir = is_dir_of(first).unwrap_or_else(|| first.is_dir());
            if is_dir {
                return false;
            }
            if !self.extensions.is_empty() {
                return listed(&ext_of(first), &self.extensions);
            }
            return paths
                .iter()
                .any(|p| !is_dir_of(p).unwrap_or_else(|| p.is_dir()) && is_archive_file(p));
        }
        let want_dir = match self.target.as_str() {
            "file" => false,
            "dir" => true,
            _ => return true,
        };
        // 快照优先，查不到才查盘。
        let is_dir = is_dir_of(first).unwrap_or_else(|| first.is_dir());
        if want_dir {
            return is_dir;
        }
        if is_dir {
            return false;
        }
        if self.extensions.is_empty() {
            return true;
        }
        let ext = first
            .extension()
            .map(|e| e.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        self.extensions.iter().any(|e| e.eq_ignore_ascii_case(&ext))
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
/// 加载动作配置（没有就建）。`prefer` = 默认压缩软件，首扫时优先用它。
pub fn load_or_create(prefer: Option<crate::sys::archive::ToolKind>) -> Vec<ActionEntry> {
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
    let (mut entries, needs_archive_seed) = heal_and_load(&path);
    if needs_archive_seed {
        // 首次启动只扫一次：装了压缩软件就默认开启（挑优先级最高的那款），
        // 没装就生成两行关闭状态的条目，用户点「更新」随时能接上。
        let began = std::time::Instant::now();
        let tool = crate::sys::archive::seed_on_first_run(prefer);
        logln!(
            "[archive] 首次探测 {}（{}ms）",
            tool.as_ref()
                .map(|t| format!("{} → {}", t.kind.id(), t.program.display()))
                .unwrap_or_else(|| "未找到压缩软件".into()),
            began.elapsed().as_millis()
        );
        let fresh = archive_actions(tool.as_ref());
        for entry in fresh {
            entries.retain(|a| a.id != entry.id);
            entries.push(entry);
        }
        if let Err(e) = save(&entries) {
            logln!("actions.toml seed write failed: {e}");
        }
    }
    entries
}

/// 把一次探测结果并回动作表。
///
/// `only = Some(id)` 时只动那一项（设置页单条「更新」），别的项原样保留；
/// `None` 时是「更新全部」。
pub fn merge_archive_entries(
    actions: &mut Vec<ActionEntry>,
    fresh: Vec<ActionEntry>,
    only: Option<&str>,
) {
    for entry in fresh {
        if let Some(want) = only {
            if entry.id != want {
                continue;
            }
        }
        let mut entry = entry;
        match actions.iter().position(|a| a.id == entry.id) {
            Some(idx) => {
                // 找到程序就沿用用户原来的开关，找不到一律关掉。
                entry.enabled = !entry.program.is_empty() && actions[idx].enabled;
                actions[idx] = entry;
            }
            None => {
                entry.enabled = !entry.program.is_empty();
                actions.push(entry);
            }
        }
    }
}

/// 扫描不到任何压缩软件时，把所有压缩项关掉。
///
/// 留着开着的项只会指向已被卸载的 exe，点下去什么都不发生（连错误都没有）。
pub fn disable_all_archive_entries(actions: &mut [ActionEntry]) {
    for a in actions.iter_mut().filter(|a| is_archive_action(&a.id)) {
        a.enabled = false;
    }
}

/// 压缩关联项改版时的补齐：按当前模板重建，保留用户改过的开关与参数。
fn upgrade_archive_entries(entries: &mut Vec<ActionEntry>) {
    let old: Vec<ActionEntry> = entries
        .iter()
        .filter(|a| is_archive_action(&a.id))
        .cloned()
        .collect();
    if old.is_empty() {
        return;
    }
    let kind = old
        .iter()
        .find_map(|a| crate::sys::archive::ToolKind::from_id(&a.tool));
    let program = old
        .iter()
        .find(|a| a.id == ARCHIVE_ADD_ID && !a.program.is_empty())
        .or_else(|| old.iter().find(|a| !a.program.is_empty()))
        .map(|a| PathBuf::from(&a.program));
    let tool = match (kind, program) {
        (Some(kind), Some(program)) => Some(crate::sys::archive::Tool { kind, program }),
        _ => None,
    };
    for mut entry in archive_actions(tool.as_ref()) {
        if let Some(o) = old.iter().find(|o| o.id == entry.id) {
            entry.enabled = o.enabled;
            if !o.program.is_empty() {
                entry.program = o.program.clone();
            }
            // 参数一律换成新模板：老版本的 WinRAR 模板缺尾反斜杠，留着等于留个坏命令。
        }
        entries.retain(|a| a.id != entry.id);
        entries.push(entry);
    }
}

/// 把「压缩关联已初始化」标记写进配置（只在首扫后调用一次）。
fn mark_archive_seeded_text(doc: &mut toml_edit::DocumentMut) {
    doc["rove_archive_seeded"] = toml_edit::value(true);
}

/// `rove_archive_seeded` 是否已写过（没写过 = 首次启动，需要扫压缩软件）。
fn archive_seeded(doc: &toml_edit::DocumentMut) -> bool {
    doc.get("rove_archive_seeded")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

/// 用 toml_edit 无损校验并加载。
fn heal_and_load(path: &Path) -> (Vec<ActionEntry>, bool) {
    let (entries, needs_archive_seed, upgraded) = heal_and_load_ex(path);
    // 压缩项按新模板重建过：必须写回文件，否则下次启动读到的还是旧参数，
    // 而代数已经补齐，就再也不会重建了（旧模板永远留在盘上）。
    if upgraded {
        if let Err(e) = save_to(path, &entries) {
            logln!("actions.toml upgrade write failed: {e}");
        }
    }
    (entries, needs_archive_seed)
}

fn heal_and_load_ex(path: &Path) -> (Vec<ActionEntry>, bool, bool) {
    let Ok(text) = std::fs::read_to_string(path) else {
        return (Vec::new(), false, false);
    };
    let Ok(mut doc) = text.parse::<toml_edit::DocumentMut>() else {
        logln!("actions.toml parse failed");
        return (Vec::new(), false, false);
    };
    // 必须先读：后面 `get_mut` 会可变借用 doc。
    let needs_archive_seed = !archive_seeded(&doc);
    let archive_gen = doc
        .get("rove_archive_gen")
        .and_then(|v| v.as_integer())
        .unwrap_or(0);

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
        return (Vec::new(), needs_archive_seed, false);
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
    // 条数/默认参数改过的版本：按新模板重建，旧的那几条里用户改过的开关和
    // 参数原样搬过去，多出来的补上。没种过（needs_archive_seed）就不动，
    // 交给首扫去生成。
    let mut upgraded = false;
    if !needs_archive_seed && archive_gen < ARCHIVE_GEN {
        upgrade_archive_entries(&mut entries);
        upgraded = true;
    }
    // 首扫之后才写标记：写早了就等于「以后再也不扫」，
    // 用户装好压缩软件重启也不会有菜单。
    if !needs_archive_seed {
        mark_archive_seeded_text(&mut doc);
    }
    let _ = std::fs::write(path, doc.to_string());
    (entries, needs_archive_seed, upgraded)
}

/// 无损写回全部动作配置（设置页开关 / 新建 / 编辑 / 删除后保存）。
/// 保留原有注释：先重读原始 DocumentMut，替换 action 数组。
pub fn save(entries: &[ActionEntry]) -> Result<(), String> {
    let Some(path) = config_path() else {
        return Err("无法定位配置目录".into());
    };
    save_to(&path, entries)
}

/// 写到指定路径（`save` 与升级回写共用，测试也能给临时文件）。
fn save_to(path: &Path, entries: &[ActionEntry]) -> Result<(), String> {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let mut doc = text
        .parse::<toml_edit::DocumentMut>()
        .unwrap_or_else(|_| toml_edit::DocumentMut::new());

    mark_archive_seeded_text(&mut doc);
    doc["rove_archive_gen"] = toml_edit::value(ARCHIVE_GEN);
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

    // 2) 剩余条目：用户自定义动作 + 压缩软件关联项（这两条由探测生成，
    //    但程序/参数/开关用户可以自己改，不能被丢弃或重置）。
    for e in entries {
        if !by_id.contains_key(&e.id) {
            continue;
        }
        let custom = !e.builtin && e.tool.is_empty();
        let archive = is_archive_action(&e.id);
        if custom || archive {
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
///
/// 只做「不碰文件系统」的配置检查，真正的启动在后台线程里做：工作目录和程序
/// 路径都可能落在网络盘 / WSL 共享上，`CreateProcess` 打开它们能卡几十秒，
/// 放在界面线程上就是整窗未响应（和终端那条路径同一个坑）。启动失败由后台
/// 线程塞进 `ops::report_launch_error`，状态栏轮询显示。
pub fn run_action(entry: &ActionEntry, targets: &[PathBuf]) -> Result<(), String> {
    run_action_with(entry, targets, None)
}

/// 「解压文件」：解到用户在选择框里挑的目录（模板里的 `{dest}`）。
pub fn run_action_into_dir(
    entry: &ActionEntry,
    targets: &[PathBuf],
    dest: &str,
) -> Result<(), String> {
    run_action_with(entry, targets, Some(dest))
}

fn run_action_with(
    entry: &ActionEntry,
    targets: &[PathBuf],
    dest: Option<&str>,
) -> Result<(), String> {
    if targets.is_empty() {
        return Err("没有选中项".into());
    }
    if entry.program.is_empty() {
        return Err("未配置可执行文件".into());
    }
    let entry = entry.clone();
    let targets = targets.to_vec();
    let dest = dest.map(str::to_string);
    std::thread::Builder::new()
        .name("rove-action".into())
        .spawn(move || {
            if let Err(e) = launch_action_with(&entry, &targets, dest.as_deref()) {
                crate::ops::report_launch_error(format!("执行失败：{e}"));
            }
        })
        .map(|_| ())
        .map_err(|e| format!("无法创建线程：{e}"))
}

/// 真正的启动动作（只在后台线程里跑，允许阻塞在慢速磁盘/网络上）。
fn launch_action_with(
    entry: &ActionEntry,
    targets: &[PathBuf],
    dest: Option<&str>,
) -> Result<(), String> {
    let target_dir = targets[0]
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."));
    let mut args = build_substituted(&entry.args, targets);
    if let Some(dest) = dest {
        args = args.replace("{dest}", dest);
    }
    let working_dir = if entry.working_dir.is_empty() {
        target_dir.clone()
    } else {
        PathBuf::from(build_substituted(&entry.working_dir, targets))
    };

    let argv = split_args(&args);
    if !Path::new(&entry.program).is_file() {
        return Err(format!("程序不存在：{}", entry.program));
    }
    // 工作目录不存在（回收站里的虚拟路径、掉线的网络盘）先给一句人话，
    // 否则只会得到 CreateProcess 的 os error 267。检查放在后台线程里，
    // 慢速磁盘上卡住也不影响界面。
    if !working_dir.is_dir() {
        return Err(format!("工作目录不存在：{}", working_dir.display()));
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
    let archive_name = if targets.len() > 1 {
        folder_archive_name(&target_dir)
    } else {
        stem.clone()
    };
    template
        .replace("{archive_name}", &archive_name)
        .replace("{target_paths}", &all)
        .replace("{target_path}", &targets[0].to_string_lossy().into_owned())
        .replace("{target_dir}", &target_dir.to_string_lossy().into_owned())
        .replace("{name}", &name)
        .replace("{stem}", &stem)
}

/// 多选时给压缩包取的名字：当前文件夹名；盘根（`D:\`）没有文件夹名，
/// 退成盘符（`D`）。
fn folder_archive_name(dir: &Path) -> String {
    if let Some(name) = dir.file_name() {
        let name = name.to_string_lossy();
        // 盘根在某些写法下会带出 `D:`（冒号进文件名是非法的），只取盘符。
        let trimmed = name.trim_end_matches(':');
        if !trimmed.is_empty() && !trimmed.contains(':') {
            return trimmed.to_string();
        }
        if let Some(letter) = trimmed.chars().find(|c| c.is_alphanumeric()) {
            return letter.to_string();
        }
    }
    let letter: String = dir
        .to_string_lossy()
        .chars()
        .find(|c| c.is_alphanumeric())
        .map(|c| c.to_string())
        .unwrap_or_default();
    if letter.is_empty() {
        "归档".to_string()
    } else {
        letter
    }
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

    fn file(name: &str) -> PathBuf {
        PathBuf::from(name)
    }

    fn winrar() -> crate::sys::archive::Tool {
        crate::sys::archive::Tool {
            kind: crate::sys::archive::ToolKind::WinRar,
            program: PathBuf::from("D:/winrar/WinRAR.exe"),
        }
    }

    /// 压缩项升级必须写回磁盘：只补代数不写参数的话，下次启动会读到旧参数，
    /// 而代数已经补齐 —— 旧模板就永远留在盘上了。
    #[test]
    fn archive_upgrade_is_persisted() {
        let dir = std::env::temp_dir().join(format!("rove-actions-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("actions.toml");
        std::fs::write(
            &path,
            r#"rove_archive_seeded = true
rove_archive_gen = 1

[[action]]
id = "builtin.archive_extract_here"
label = '解压到当前文件夹'
program = 'D:/winrar/WinRAR.exe'
args = 'x {target_paths} {target_dir}'
enabled = true
builtin = true
tool = "winrar"
"#,
        )
        .unwrap();

        let (entries, seeded) = heal_and_load(&path);
        assert!(!seeded);
        let here = entries
            .iter()
            .find(|a| a.id == ARCHIVE_EXTRACT_HERE_ID)
            .unwrap();
        assert!(here.args.ends_with(r#"\""#), "{}", here.args);

        // 落盘了才会在下次启动生效：重读一次仍是新模板。
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains(r#"x {target_paths} "{target_dir}\""#), "{text}");
        assert!(text.contains(&format!("rove_archive_gen = {ARCHIVE_GEN}")), "{text}");

        let (again, _) = heal_and_load(&path);
        let here = again
            .iter()
            .find(|a| a.id == ARCHIVE_EXTRACT_HERE_ID)
            .unwrap();
        assert!(here.args.ends_with(r#"\""#), "{}", here.args);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 没装压缩软件时也要生成这四条（关闭状态），否则设置页里没有入口可更新。
    #[test]
    fn archive_actions_exist_even_without_tool() {
        let entries = archive_actions(None);
        let ids: Vec<&str> = entries.iter().map(|a| a.id.as_str()).collect();
        assert_eq!(ids, ARCHIVE_IDS.to_vec());
        for e in &entries {
            assert!(e.program.is_empty());
            assert!(e.args.is_empty());
            assert!(!e.enabled);
            assert!(e.builtin);
            assert!(e.tool.is_empty());
        }
    }

    /// 探测到工具后：程序/参数就位、默认开启、解压标签带 `{name}` 占位符。
    #[test]
    fn archive_actions_fill_templates_for_tool() {
        let entries = archive_actions(Some(&winrar()));
        for e in &entries {
            assert!(e.enabled);
            assert_eq!(e.tool, "winrar");
            assert!(e.program.ends_with("WinRAR.exe"));
            assert!(!e.args.is_empty());
        }
        let add = &entries[0];
        assert!(add.args.contains("{target_paths}"));
        let whole = &entries[3];
        assert!(whole.label.contains("{name}"));
        assert!(whole.args.contains("{target_paths}"));
        // WinRAR 的目标目录必须带尾反斜杠，否则它只报「诊断信息」。
        assert!(entries[2].args.ends_with(r#"\""#), "{}", entries[2].args);
    }

    /// 压缩包名走 `{archive_name}`，不能带扩展名：`{name}` 是含扩展名的
    /// 全名，会压出 `a.txt.rar` 这种东西。
    #[test]
    fn add_archive_name_placeholder() {
        let entries = archive_actions(Some(&winrar()));
        let add = &entries[0];
        assert!(add.args.contains("{archive_name}"), "{}", add.args);
        assert!(!add.args.contains("{name}"), "{}", add.args);
    }

    /// 单选 = 那一项的主名；多选 = 当前文件夹名；盘根 = 盘符。
    #[test]
    fn archive_name_single_multi_and_drive_root() {
        let one = build_substituted("{archive_name}", &[file("/work/report.txt")]);
        assert_eq!(one, "report");
        let one_dir = build_substituted("{archive_name}", &[file("/work/photos")]);
        assert_eq!(one_dir, "photos");
        let many = build_substituted(
            "{archive_name}",
            &[file("/work/a.txt"), file("/work/b.txt")],
        );
        assert_eq!(many, "work");
        let root = build_substituted("{archive_name}", &[file("/D:/a.txt"), file("/D:/b.txt")]);
        assert_eq!(root, "D");
    }

    /// 三款压缩软件全没了：所有压缩项一起关掉（含「更新」只刷一项的情况）。
    #[test]
    fn missing_tools_turn_off_every_archive_entry() {
        let mut actions = archive_actions(Some(&winrar()));
        assert!(actions.iter().all(|a| a.enabled));
        disable_all_archive_entries(&mut actions);
        assert!(actions.iter().all(|a| !a.enabled));

        // 只剩一项被「更新」刷新时，其它项也必须一起关。
        let mut actions = archive_actions(Some(&winrar()));
        let fresh = archive_actions(None);
        disable_all_archive_entries(&mut actions);
        merge_archive_entries(&mut actions, fresh, Some(ARCHIVE_EXTRACT_ID));
        assert!(actions.iter().all(|a| !a.enabled));
        // 被刷新的那一项程序被清空；其它项只是开关关掉，下次「更新全部」才清。
    }

    /// 后续卷要能自己推出第一卷（纯推导，不看磁盘）。
    #[test]
    fn later_volume_finds_first_volume_candidates() {
        assert_eq!(
            first_volume_candidates(&file("/d/multi.part2.rar")).unwrap(),
            vec![file("/d/multi.part1.rar")]
        );
        assert_eq!(
            first_volume_candidates(&file("/d/multi.part02.rar")).unwrap(),
            vec![file("/d/multi.part01.rar"), file("/d/multi.part1.rar")]
        );
        assert_eq!(
            first_volume_candidates(&file("/d/x.7z.003")).unwrap(),
            vec![file("/d/x.7z.001")]
        );
        assert_eq!(
            first_volume_candidates(&file("/d/x.r01")).unwrap(),
            vec![file("/d/x.rar"), file("/d/x.zip"), file("/d/x.7z")]
        );
        assert!(first_volume_candidates(&file("/d/x.7z.001")).is_none());
        assert!(first_volume_candidates(&file("/d/a.zip")).is_none());
    }

    /// 真建几卷，验证单选 part2 / 混选整组都会自己找第一卷并去重。
    #[test]
    fn selection_resolves_to_first_volume_on_disk() {
        let dir = std::env::temp_dir().join(format!("rove-vol-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for n in ["multi.part1.rar", "multi.part2.rar", "multi.part3.rar"] {
            std::fs::write(dir.join(n), b"x").unwrap();
        }
        for n in ["x.7z.001", "x.7z.002"] {
            std::fs::write(dir.join(n), b"x").unwrap();
        }
        std::fs::write(dir.join("a.zip"), b"x").unwrap();

        assert_eq!(
            extractable_archives(&[dir.join("multi.part2.rar")]),
            vec![dir.join("multi.part1.rar")]
        );
        assert_eq!(
            extractable_archives(&[
                dir.join("multi.part2.rar"),
                dir.join("multi.part3.rar"),
                dir.join("multi.part1.rar"),
                dir.join("x.7z.002"),
                dir.join("a.zip"),
            ]),
            vec![
                dir.join("multi.part1.rar"),
                dir.join("x.7z.001"),
                dir.join("a.zip")
            ]
        );
        assert_eq!(
            skipped_volumes(&[
                dir.join("multi.part2.rar"),
                dir.join("multi.part3.rar"),
                dir.join("multi.part1.rar"),
                dir.join("a.zip"),
            ]),
            0
        );

        std::fs::remove_file(dir.join("multi.part1.rar")).unwrap();
        assert!(extractable_archives(&[dir.join("multi.part2.rar")]).is_empty());
        assert_eq!(skipped_volumes(&[dir.join("multi.part2.rar")]), 1);
        // 残缺分卷 + 正常包：只解正常包。
        assert_eq!(
            extractable_archives(&[dir.join("multi.part2.rar"), dir.join("a.zip")]),
            vec![dir.join("a.zip")]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 分卷的各卷都要能让「解压」在菜单里出现。
    #[test]
    fn split_volumes_count_as_archives() {
        for name in [
            "/d/multi.part1.rar",
            "/d/multi.part2.rar",
            "/d/x.7z.001",
            "/d/x.zip.001",
            "/d/x.r00",
            "/d/x.z01",
        ] {
            assert!(is_archive_file(&file(name)), "{name}");
        }
        assert!(!is_archive_file(&file("/d/note.txt")));
    }

    /// 「解压文件」三款都用 `{dest}` 占位（Rove 自己问目录），别的项不许有。
    #[test]
    fn extract_files_uses_dest_placeholder() {
        for kind in [
            crate::sys::archive::ToolKind::WinRar,
            crate::sys::archive::ToolKind::SevenZip,
            crate::sys::archive::ToolKind::Bandizip,
        ] {
            let tool = crate::sys::archive::Tool {
                kind,
                program: PathBuf::from("C:/tool/t.exe"),
            };
            let entries = archive_actions(Some(&tool));
            let files = entries
                .iter()
                .find(|a| a.id == ARCHIVE_EXTRACT_FILES_ID)
                .unwrap();
            assert!(files.args.contains("{dest}"), "{kind:?} 少 {{dest}}");
            for a in &entries {
                if a.id != ARCHIVE_EXTRACT_FILES_ID {
                    assert!(!a.args.contains("{dest}"), "{:?} 多 {{dest}}", a.id);
                }
            }
        }
    }

    /// 加压缩包文件、文件夹都收（压缩包本身除外）；解压类只认压缩包。
    #[test]
    fn archive_actions_split_by_archive_extension() {
        let entries = archive_actions(Some(&winrar()));
        let add = &entries[0];
        let not_dir = |_: &Path| Some(false);
        let dir = |_: &Path| Some(true);

        assert!(add.matches_selection(&[file("a.txt")], not_dir));
        assert!(!add.matches_selection(&[file("a.rar")], not_dir));
        // 文件夹也能压。
        assert!(add.matches_selection(&[file("folder")], dir));
        // 混合选择里只要还有能压的文件就露出来。
        assert!(add.matches_selection(&[file("a.rar"), file("b.txt")], not_dir));

        for e in &entries[1..] {
            assert!(e.matches_selection(&[file("a.rar")], not_dir));
            assert!(e.matches_selection(&[file("a.ZIP")], not_dir));
            assert!(!e.matches_selection(&[file("a.txt")], not_dir));
            assert!(!e.matches_selection(&[file("a.rar")], dir));
        }
    }

    /// 程序缺失（没探测到）时这几条不进菜单，但设置页里仍在。
    #[test]
    fn archive_actions_hidden_without_program() {
        let entries = archive_actions(None);
        let not_dir = |_: &Path| Some(false);
        for e in &entries {
            assert!(!e.matches_selection(&[file("a.txt")], not_dir));
            assert!(!e.matches_selection(&[file("a.rar")], not_dir));
        }
    }

    /// 自愈不能把压缩关联项丢掉或重置成默认（用户的程序/参数/开关要留住）。
    #[test]
    fn heal_full_keeps_archive_entries() {
        let mut raw = archive_actions(Some(&winrar()));
        raw[0].enabled = false;
        raw[0].program = "E:/elsewhere/WinRAR.exe".into();
        let healed = heal_full(&raw);
        let add = healed.iter().find(|a| a.id == ARCHIVE_ADD_ID).unwrap();
        assert_eq!(add.program, "E:/elsewhere/WinRAR.exe");
        assert!(!add.enabled);
        assert_eq!(add.tool, "winrar");
        for id in ARCHIVE_IDS {
            assert!(healed.iter().any(|a| a.id == id), "缺了 {id}");
        }
    }

    /// 单条「更新」只能动那一项，别的项一个字节都不许变。
    #[test]
    fn merge_updates_only_the_requested_entry() {
        let mut actions = archive_actions(Some(&winrar()));
        // 用户手工把「解压到当前文件夹」改成别的程序与参数。
        let here = actions
            .iter_mut()
            .find(|a| a.id == ARCHIVE_EXTRACT_HERE_ID)
            .unwrap();
        here.program = "E:/mine/other.exe".into();
        here.args = "--mine".into();
        here.enabled = true;

        let fresh = archive_actions(Some(&winrar()));
        merge_archive_entries(&mut actions, fresh, Some(ARCHIVE_ADD_ID));

        let here = actions
            .iter()
            .find(|a| a.id == ARCHIVE_EXTRACT_HERE_ID)
            .unwrap();
        assert_eq!(here.program, "E:/mine/other.exe");
        assert_eq!(here.args, "--mine");
        let add = actions.iter().find(|a| a.id == ARCHIVE_ADD_ID).unwrap();
        assert_eq!(add.program, "D:/winrar/WinRAR.exe");
    }

    /// 「更新全部」才把每一项都刷成新值。
    #[test]
    fn merge_updates_every_entry_for_rescan_all() {
        let mut actions = archive_actions(Some(&winrar()));
        for a in actions.iter_mut() {
            a.program = "E:/old/x.exe".into();
        }
        let fresh = archive_actions(Some(&winrar()));
        merge_archive_entries(&mut actions, fresh, None);
        for id in ARCHIVE_IDS {
            let a = actions.iter().find(|a| a.id == id).unwrap();
            assert_eq!(a.program, "D:/winrar/WinRAR.exe", "{id} 没刷到");
        }
    }

    /// 老配置只有两条时，升级要补齐四条，并留住用户改过的开关（参数换成新模板）。
    #[test]
    fn upgrade_fills_missing_archive_entries() {
        let mut entries = archive_actions(Some(&winrar()));
        entries.retain(|a| a.id == ARCHIVE_ADD_ID || a.id == ARCHIVE_EXTRACT_ID);
        entries[0].enabled = false;
        upgrade_archive_entries(&mut entries);
        let ids: Vec<&str> = entries
            .iter()
            .filter(|a| is_archive_action(&a.id))
            .map(|a| a.id.as_str())
            .collect();
        assert_eq!(ids.len(), ARCHIVE_IDS.len());
        for id in ARCHIVE_IDS {
            assert!(ids.contains(&id), "缺了 {id}");
        }
        let add = entries.iter().find(|a| a.id == ARCHIVE_ADD_ID).unwrap();
        assert!(!add.enabled, "用户关掉的不能被升级打开");
        let whole = entries.iter().find(|a| a.id == ARCHIVE_EXTRACT_ID).unwrap();
        assert!(whole.args.ends_with(r#"\""#), "参数要换成新模板");
        assert_eq!(whole.program, "D:/winrar/WinRAR.exe");
    }

    /// 「删除」必须紧跟在「剪切」下面，而且必须存在：系统菜单那一项不记账，
    /// 删掉这一条等价于把右键菜单里的删除功能弄丢。
    #[test]
    fn trash_sits_right_below_cut() {
        let entries = builtin_actions();
        let ids: Vec<&str> = entries.iter().map(|a| a.id.as_str()).collect();
        let cut = ids
            .iter()
            .position(|id| *id == "builtin.move")
            .expect("剪切项不见了");
        assert_eq!(ids[cut + 1], "builtin.trash");
        let entry = builtin_actions()
            .into_iter()
            .find(|a| a.id == "builtin.trash")
            .unwrap();
        assert_eq!(entry.label, "删除");
        assert!(entry.enabled && entry.target == "any");
    }

    fn test_entry() -> ActionEntry {
        ActionEntry {
            id: "user.test".into(),
            label: "测试".into(),
            tool: String::new(),
            program: r"C:\Windows\System32\cmd.exe".into(),
            args: String::new(),
            working_dir: String::new(),
            target: "any".into(),
            extensions: Vec::new(),
            enabled: true,
            group: String::new(),
            builtin: false,
        }
    }

    /// 空选中 / 没填程序 这两类配置错误必须在界面线程上立刻返回，
    /// 不能先起个后台线程再报错。
    #[test]
    fn run_action_reports_config_errors_synchronously() {
        let mut entry = test_entry();
        assert!(run_action(&entry, &[]).is_err());
        entry.program = String::new();
        assert!(run_action(&entry, &[PathBuf::from(r"C:\a.txt")]).is_err());
    }

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

    /// 右键菜单判断「适用对象」时先用已加载的条目快照，查不到才查盘。
    #[test]
    fn target_matching_prefers_loaded_snapshot() {
        // 用肯定不存在的路径：退回磁盘那一步必须答「不是文件夹」。
        let dir = PathBuf::from("D:\\rove-no-such-dir-42");
        let file = PathBuf::from("D:\\rove-no-such-dir-42\\a.txt");
        // 内置动作里没有 target = file 的项，这里自己造一条最小动作。
        let pick = |target: &str| ActionEntry {
            id: "t".into(),
            label: "t".into(),
            tool: "".into(),
            program: "".into(),
            args: "".into(),
            working_dir: "".into(),
            target: target.into(),
            extensions: vec![],
            enabled: true,
            group: "".into(),
            builtin: false,
        };

        let d = pick("dir");
        // 快照说是文件夹 → 显示。
        assert!(d.matches_selection(std::slice::from_ref(&dir), |_| Some(true)));
        // 快照说是文件 → 不显示（不会再去 stat 一次盘）。
        assert!(!d.matches_selection(std::slice::from_ref(&dir), |_| Some(false)));
        // 快照没有这一项 → 退回磁盘：两个路径都不存在，is_dir() = false。
        assert!(!d.matches_selection(std::slice::from_ref(&dir), |_| None));

        let mut f = pick("file");
        assert!(!f.matches_selection(std::slice::from_ref(&file), |_| Some(true)));
        assert!(f.matches_selection(std::slice::from_ref(&file), |_| Some(false)));
        // 扩展名过滤照旧生效。
        f.extensions = vec!["txt".into()];
        assert!(f.matches_selection(std::slice::from_ref(&file), |_| Some(false)));
        f.extensions = vec!["png".into()];
        assert!(!f.matches_selection(std::slice::from_ref(&file), |_| Some(false)));

        // 空选中集：任何动作都不匹配。
        assert!(!f.matches_selection(&[], |_| None));
    }

    /// 一级菜单前六项的顺序：复制 / 剪切 / 删除 / 属性 / 重命名 / 复制路径。
    /// heal_full 会把内置动作按本函数顺序重排，所以只靠这里的顺序就成。
    #[test]
    fn copy_path_sits_right_below_cut() {
        let actions = builtin_actions();
        let ids: Vec<&str> = actions.iter().map(|a| a.id.as_str()).collect();
        assert_eq!(
            &ids[..7],
            &[
                "builtin.copy",
                "builtin.move",
                // 「删除」插在剪切下面（系统那一项不记账，见 default_context_menu_hide）。
                "builtin.trash",
                // 「属性」/「重命名」紧跟删除，与系统菜单的习惯顺序一致。
                "builtin.properties",
                "builtin.rename",
                // 「复制路径」跟在重命名后面，「打开方式」再跟在它后面。
                "builtin.copy_path",
                "builtin.open_with",
            ],
        );
    }

    /// 「打开方式」只对文件出现（文件夹没有这个概念）。
    #[test]
    fn open_with_is_file_only() {
        let entry = builtin_actions()
            .into_iter()
            .find(|a| a.id == "builtin.open_with")
            .unwrap();
        assert_eq!(entry.target, "file");
        let file = [PathBuf::from(r"C:.txt")];
        let dir = [PathBuf::from(r"C:")];
        assert!(entry.matches_selection(&file, |_| Some(false)));
        assert!(!entry.matches_selection(&dir, |_| Some(true)));
    }

    /// 两个快捷方式动作必须紧跟在「批量重命名」下面。
    #[test]
    fn shortcuts_sit_right_below_batch_rename() {
        let actions = builtin_actions();
        let ids: Vec<&str> = actions.iter().map(|a| a.id.as_str()).collect();
        let batch = ids
            .iter()
            .position(|id| *id == "builtin.batch_rename")
            .expect("批量重命名项不见了");
        assert_eq!(ids[batch + 1], "builtin.create_shortcut");
        assert_eq!(ids[batch + 2], "builtin.send_to_desktop");
    }
}
