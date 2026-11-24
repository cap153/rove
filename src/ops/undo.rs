//! 撤消 / 重做栈。
//!
//! 为什么自己记账，而不是借 Windows Shell 的撤消栈：Shell 那份历史挂在
//! **资源管理器自己的视图实例**上（`FOFX_ADDUNDORECORD` 只是往当前视图的
//! 撤消管理器里登记）。Rove 的列表不是 Shell 视图，操作又发生在后台线程，
//! 进程里没有那个视图，所以「问 Shell 有没有可撤消项」永远问不到东西。
//!
//! 记自己的账换来的是：复制 / 移动 / 重命名 / 新建 / 删除到回收站全都能撤，
//! 而且不依赖 Shell 有没有记录（别的程序删的文件我们当然管不了）。
//!
//! 每条记录同时描述「正向」和「反向」两个动作，所以撤消与重做共用一份数据：
//! - 撤消：把记录反向执行一遍，然后压进重做栈；
//! - 重做：把记录正向再执行一遍，然后压回撤消栈。

use crate::logv;
use std::path::{Path, PathBuf};

/// 撤消栈最多保留多少步。每条记录只存路径，40 条撑死几百 KB。
pub const MAX_UNDO: usize = 40;

/// 单条记录允许携带的路径条数上限。
///
/// 一次粘贴五万个文件时，把这些路径全记下来会长期占着几 MB —— 与 Rove
/// 「常驻低内存」的目标冲突，且这种批量操作本来也不太可能靠 Ctrl+Z 回退。
pub const MAX_ENTRY_PATHS: usize = 2_000;

/// 一个「新建出来的文件夹」的身份。
///
/// 卷序列号 + 文件索引（NTFS 的 FileId）是对象本身的名字，不是路径的名字：
/// 用户在我们撤消之前把那个文件夹删了、又新建了一个同名的，路径一模一样，
/// 但 FileId 不同 —— 这时候绝不能拿它当「我新建的那个」删掉。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DirId {
    pub volume: u32,
    pub index_high: u32,
    pub index_low: u32,
}

/// 新建出来的文件夹：路径 + 建它那一刻的身份（采不到时为 None）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CreatedDir {
    pub path: PathBuf,
    pub id: Option<DirId>,
}

/// 采集一个目录的身份。取不到（权限 / 网络路径 / 已经被删）返回 None。
///
/// 打开时带 `FILE_FLAG_OPEN_REPARSE_POINT`：不跟随符号链接 / 目录联接，
/// 拿到的是那个链接自己的身份。`FILE_FLAG_BACKUP_SEMANTICS` 才能打开目录句柄。
#[cfg(windows)]
pub fn capture_dir_id(path: &Path) -> Option<DirId> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION, FILE_FLAG_BACKUP_SEMANTICS,
        FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ,
        FILE_SHARE_WRITE, OPEN_EXISTING,
    };

    let wide: Vec<u16> = path.as_os_str().encode_wide().chain([0]).collect();
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            FILE_READ_ATTRIBUTES,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return None;
    }
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    let ok = unsafe { GetFileInformationByHandle(handle, &mut info) } != 0;
    unsafe { CloseHandle(handle) };
    if !ok {
        return None;
    }
    Some(DirId {
        volume: info.dwVolumeSerialNumber,
        index_high: info.nFileIndexHigh,
        index_low: info.nFileIndexLow,
    })
}

#[cfg(not(windows))]
pub fn capture_dir_id(_path: &Path) -> Option<DirId> {
    None
}

/// 一次可撤消的文件操作。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Kind {
    /// 删除到回收站。
    Trash { paths: Vec<PathBuf> },
    /// 复制：`sources` 复制到 `dest`，落地后是 `created`。
    Copy {
        sources: Vec<PathBuf>,
        dest: PathBuf,
        created: Vec<PathBuf>,
    },
    /// 移动：`(原来的路径, 现在的位置)`。
    Move { pairs: Vec<(PathBuf, PathBuf)> },
    /// 重命名：`(旧路径, 新路径)`。
    Rename { pairs: Vec<(PathBuf, PathBuf)> },
    /// 新建的文件夹（带身份，撤消前要核对）。
    Create { dirs: Vec<CreatedDir> },
}

impl Kind {
    /// 这条记录一共涉及多少个路径（用于决定要不要记）。
    pub fn path_count(&self) -> usize {
        match self {
            Kind::Trash { paths } => paths.len(),
            Kind::Create { dirs } => dirs.len(),
            Kind::Copy { created, .. } => created.len(),
            Kind::Move { pairs } | Kind::Rename { pairs } => pairs.len(),
        }
    }

    /// 这条记录执行完毕后，哪些目录需要重新列举。
    pub fn affected_dirs(&self) -> Vec<PathBuf> {
        let mut dirs: Vec<PathBuf> = Vec::new();
        let mut add = |p: Option<&Path>| {
            if let Some(p) = p {
                if !dirs.iter().any(|d| d == p) {
                    dirs.push(p.to_path_buf());
                }
            }
        };
        match self {
            Kind::Trash { paths } => {
                for p in paths {
                    add(p.parent());
                }
            }
            Kind::Create { dirs } => {
                for d in dirs {
                    add(d.path.parent());
                }
            }
            Kind::Copy { sources, dest, created } => {
                add(Some(dest));
                for p in sources.iter().chain(created.iter()) {
                    add(p.parent());
                }
            }
            Kind::Move { pairs } => {
                for (from, to) in pairs {
                    add(from.parent());
                    add(to.parent());
                }
            }
            Kind::Rename { pairs } => {
                for (old, new) in pairs {
                    add(old.parent());
                    add(new.parent());
                }
            }
        }
        dirs
    }

    /// 空白处右键菜单的「撤消 X」文字（中文键，交给 `i18n::text` 翻译）。
    ///
    /// 用整串而不是「撤消」+动词拼：英文要的是 "Undo copy"，中文要的是
    /// 「撤消复制」，拼起来得先知道当前语言，查表最省事。
    pub fn undo_label(&self) -> &'static str {
        match self {
            Kind::Trash { .. } => "撤消删除",
            Kind::Copy { .. } => "撤消复制",
            Kind::Move { .. } => "撤消移动",
            Kind::Rename { .. } => "撤消重命名",
            Kind::Create { .. } => "撤消新建",
        }
    }

    /// 同上，重做那一侧。
    pub fn redo_label(&self) -> &'static str {
        match self {
            Kind::Trash { .. } => "恢复删除",
            Kind::Copy { .. } => "恢复复制",
            Kind::Move { .. } => "恢复移动",
            Kind::Rename { .. } => "恢复重命名",
            Kind::Create { .. } => "恢复新建",
        }
    }
}

/// 撤消 / 重做两个栈。
#[derive(Default)]
pub struct UndoStack {
    undo: Vec<Kind>,
    redo: Vec<Kind>,
}

impl UndoStack {
    pub fn new() -> Self {
        Self::default()
    }

    /// 记下一次新操作：撤消栈增长，重做栈作废（标准的撤消语义）。
    ///
    /// 路径太多的记录直接丢掉：记下来也回退不动，还白占内存。
    pub fn record(&mut self, kind: Kind) {
        // 先作废重做栈，再判断要不要真的记：这一次操作确实发生了，
        // 「重做」指向的那一步已经被它盖掉 —— 跟记不记得下来无关。
        self.redo.clear();
        // 空记录（一条路径都没有）记了也没用：它只会占着栈顶显示一个
        // 什么都撤不掉的「撤消 X」。
        if kind.path_count() == 0 {
            return;
        }
        if kind.path_count() > MAX_ENTRY_PATHS {
            logv!(
                "[undo] 记录过大（{} 条路径），不加入撤消栈",
                kind.path_count()
            );
            return;
        }
        self.undo.push(kind);
        if self.undo.len() > MAX_UNDO {
            self.undo.remove(0);
        }
    }

    /// 撤消栈顶的记录（不弹出，等任务真的提交成功再弹）。
    pub fn peek_undo(&self) -> Option<&Kind> {
        self.undo.last()
    }

    /// 重做栈顶的记录（同上）。
    pub fn peek_redo(&self) -> Option<&Kind> {
        self.redo.last()
    }

    /// 撤消任务已经提交：从撤消栈挪到重做栈。
    pub fn take_undo(&mut self) -> Option<Kind> {
        let kind = self.undo.pop()?;
        self.redo.push(kind.clone());
        Some(kind)
    }

    /// 重做任务已经提交：从重做栈挪回撤消栈。
    pub fn take_redo(&mut self) -> Option<Kind> {
        let kind = self.redo.pop()?;
        self.undo.push(kind.clone());
        Some(kind)
    }

    /// 任务失败被退回时用：把记录放回它原来那一侧。
    pub fn restore_undo(&mut self) {
        if let Some(kind) = self.redo.pop() {
            self.undo.push(kind);
        }
    }

    /// 任务失败被退回时用：把记录放回它原来那一侧。
    pub fn restore_redo(&mut self) {
        if let Some(kind) = self.undo.pop() {
            self.redo.push(kind);
        }
    }

    /// 撤消 / 重做**彻底失败**（不是用户取消）时用：把记录从另一侧扔掉。
    ///
    /// 不扔的话它会一直顶在栈顶，每按一次 Ctrl+Z 都重试同一条、每次都失败，
    /// 更早的历史就永远撤不到了（「回收站里已经找不到」这种失败是永久的）。
    pub fn drop_undo(&mut self) {
        self.undo.pop();
    }

    /// 同上，重做那一侧。
    pub fn drop_redo(&mut self) {
        self.redo.pop();
    }

    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    /// 清空两个栈。不可撤消的操作（彻底删除）之后必须调用：
    /// 否则下一次 Ctrl+Z 会悄悄跳过它，去撤消更早的那次操作 ——
    /// 用户以为「删掉的那批又回来了」，其实动的是别的东西。
    pub fn clear(&mut self) {
        self.undo.clear();
        self.redo.clear();
    }
}

/// 把「复制 / 移动之后多出来的路径」对回它们的来源。
///
/// 没有同名冲突时 `dest/源文件名` 就是答案；系统给冲突项改名成
/// `名字 (2).后缀` 时按词干前缀再找一次。对不上的源直接跳过——
/// 宁可少一条可撤消的项，也不能把 A 的位置记成 B 的。
pub fn pair_created(sources: &[PathBuf], created: &[PathBuf]) -> Vec<(PathBuf, PathBuf)> {
    let mut used = vec![false; created.len()];
    let mut pairs = Vec::new();
    // 第一轮：文件名完全一致。
    for src in sources {
        let want = src.file_name();
        if let Some(index) = created
            .iter()
            .enumerate()
            .position(|(i, c)| !used[i] && c.file_name() == want)
        {
            used[index] = true;
            pairs.push((src.clone(), created[index].clone()));
        }
    }
    // 第二轮：被系统改过名的（`报告 (2).txt`）。
    for src in sources {
        if pairs.iter().any(|(s, _)| s == src) {
            continue;
        }
        let Some(stem) = src.file_stem().map(|s| s.to_string_lossy().to_lowercase()) else {
            continue;
        };
        let ext = src
            .extension()
            .map(|e| e.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        let hit = created.iter().enumerate().position(|(i, c)| {
            if used[i] {
                return false;
            }
            let c_stem = c
                .file_stem()
                .map(|s| s.to_string_lossy().to_lowercase())
                .unwrap_or_default();
            let c_ext = c
                .extension()
                .map(|e| e.to_string_lossy().to_lowercase())
                .unwrap_or_default();
            c_ext == ext && (c_stem == stem || c_stem.starts_with(&format!("{stem} (")))
        });
        if let Some(index) = hit {
            used[index] = true;
            pairs.push((src.clone(), created[index].clone()));
        }
    }
    pairs
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    fn rename(old: &str, new: &str) -> Kind {
        Kind::Rename {
            pairs: vec![(p(old), p(new))],
        }
    }

    /// 新操作必须清空重做栈，否则「撤消 → 做点别的 → 重做」会把中间那次
    /// 操作的结果覆盖掉。
    #[test]
    fn new_record_clears_redo() {
        let mut stack = UndoStack::new();
        stack.record(rename("a", "b"));
        stack.take_undo();
        assert!(stack.can_redo());
        stack.record(rename("c", "d"));
        assert!(!stack.can_redo());
    }

    /// 撤消 / 重做必须在两个栈之间来回搬，不能丢也不能重复。
    #[test]
    fn undo_and_redo_move_between_stacks() {
        let mut stack = UndoStack::new();
        stack.record(rename("a", "b"));
        assert_eq!(stack.peek_undo(), Some(&rename("a", "b")));
        stack.take_undo();
        assert!(!stack.can_undo());
        assert_eq!(stack.peek_redo(), Some(&rename("a", "b")));
        stack.take_redo();
        assert!(stack.can_undo());
        assert!(!stack.can_redo());
    }

    /// 提交失败要能原样退回原来那一侧。
    #[test]
    fn failed_submit_is_restored() {
        let mut stack = UndoStack::new();
        stack.record(rename("a", "b"));
        stack.take_undo();
        stack.restore_undo();
        assert_eq!(stack.peek_undo(), Some(&rename("a", "b")));
        assert!(!stack.can_redo());
    }

    /// 栈深有上限，且丢的是最老的那步。
    #[test]
    fn stack_is_capped() {
        let mut stack = UndoStack::new();
        for i in 0..MAX_UNDO + 5 {
            stack.record(rename(&format!("a{i}"), &format!("b{i}")));
        }
        let top = format!("b{}", MAX_UNDO + 4);
        match stack.peek_undo() {
            Some(Kind::Rename { pairs }) => assert_eq!(pairs[0].1, p(&top)),
            other => panic!("unexpected top: {other:?}"),
        }
    }

    /// 路径太多的记录不记（省内存），但不能把已有的记录清掉。
    #[test]
    fn huge_records_are_skipped() {
        let mut stack = UndoStack::new();
        stack.record(rename("a", "b"));
        let paths: Vec<PathBuf> = (0..MAX_ENTRY_PATHS + 1).map(|i| p(&format!("x{i}"))).collect();
        stack.record(Kind::Create {
            dirs: paths
                .into_iter()
                .map(|path| CreatedDir { path, id: None })
                .collect(),
        });
        match stack.peek_undo() {
            Some(Kind::Rename { .. }) => {}
            other => panic!("大记录不该覆盖栈顶: {other:?}"),
        }
    }

    /// 复制/移动后的新路径要对回它们的来源：同名直接对上，被系统改名的
    /// （`报告 (2).txt`）也要对上，对不上的不能瞎配。
    #[test]
    fn created_pairs_match_by_name_then_by_stem() {
        let sources = vec![p("D:\\a\\报告.txt"), p("D:\\a\\照片.jpg"), p("D:\\a\\不在.txt")];
        let created = vec![p("E:\\b\\照片.jpg"), p("E:\\b\\报告 (2).txt")];
        // 顺序按两轮匹配的先后：先同名，再按词干补上被系统改名的那条。
        let mut pairs = pair_created(&sources, &created);
        pairs.sort();
        let mut want = vec![
            (p("D:\\a\\报告.txt"), p("E:\\b\\报告 (2).txt")),
            (p("D:\\a\\照片.jpg"), p("E:\\b\\照片.jpg")),
        ];
        want.sort();
        assert_eq!(pairs, want);
    }

    /// 彻底失败的记录必须从另一侧扔掉：留在栈顶会让每次 Ctrl+Z 都重试
    /// 同一条、次次失败，更早的历史永远撤不到。
    #[test]
    fn fatal_failure_drops_the_record_instead_of_pinning_it() {
        let mut stack = UndoStack::new();
        stack.record(rename("a", "b"));
        stack.record(rename("c", "d"));
        stack.take_undo();
        assert!(stack.can_redo());
        stack.drop_redo();
        assert!(!stack.can_redo(), "失败的那条不能留在重做栈里");
        assert!(stack.can_undo(), "更早的那条必须还能撤");
    }

    /// 记录被丢弃（路径太多）时也要作废重做栈：那一步重做指向的状态已经被
    /// 这次操作盖掉了，还留着它 Ctrl+Y 就会在错误的基础上往前推。
    #[test]
    fn oversized_record_still_clears_redo() {
        let mut stack = UndoStack::new();
        stack.record(rename("a", "b"));
        stack.take_undo();
        assert!(stack.can_redo());
        let huge: Vec<PathBuf> = (0..MAX_ENTRY_PATHS + 1)
            .map(|i| PathBuf::from(format!("C:\\x\\{i}.txt")))
            .collect();
        stack.record(Kind::Trash { paths: huge });
        assert!(!stack.can_redo(), "丢弃记录也必须清掉重做栈");
        let _ = stack;
    }

    /// 身份必须是「对象」的身份，不是「路径」的身份：同名文件夹删了重建，
    /// 路径一样但身份必须不同 —— 撤消就靠这个区别决定能不能删。
    #[test]
    #[cfg(windows)]
    fn dir_identity_distinguishes_a_recreated_folder() {
        let root = std::env::temp_dir().join(format!("rove-undo-id-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let target = root.join("新建文件夹");
        std::fs::create_dir(&target).unwrap();
        let first = capture_dir_id(&target).expect("新建的目录必须能采到身份");
        // 同一个对象重复采集要稳定，否则撤消永远对不上。
        assert_eq!(Some(first), capture_dir_id(&target));
        std::fs::remove_dir(&target).unwrap();
        std::fs::create_dir(&target).unwrap();
        let second = capture_dir_id(&target).expect("重建的目录也必须能采到身份");
        assert_ne!(first, second, "路径相同、对象不同，身份不能相等");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 撤消要刷新的目录：移动要同时刷源目录和目标目录。
    #[test]
    fn affected_dirs_cover_both_sides() {
        let kind = Kind::Move {
            pairs: vec![(p("C:\\a\\x.txt"), p("D:\\b\\x.txt"))],
        };
        let dirs = kind.affected_dirs();
        assert!(dirs.contains(&p("C:\\a")));
        assert!(dirs.contains(&p("D:\\b")));
    }
}
