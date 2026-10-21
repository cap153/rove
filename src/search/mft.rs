//! 任务 3.2：通过 NTFS USN/FSCTL 直读全盘 MFT 记录（需管理员权限）。
//!
//! 放弃直接 open `\\.\X:\$MFT`（该路径被 Windows 内核独占锁死，常规权限会
//! Access Denied）。采用工业级方案：通过卷句柄发送 `FSCTL_ENUM_USN_DATA`
//! 枚举 MFT 记录，再由文件引用号（fid/parent_fid）重构完整路径。
//!
//! - 仅 NTFS/ReFS 固定盘可用；需管理员权限以打开卷设备。
//! - 任意盘打开失败（无权限 / 非 NTFS / 外接盘）即跳过该盘；
//!   **全部**盘都失败则返回 `None`，由调用方回退到原有 `std::fs` 递归 walker。
//! - 与 walker 保持一致的跳过策略（系统噪声目录）。

// 仅在 Windows 实现中使用的导入按平台拆分，避免 Linux 宿主构建产生
// unused import 警告（本模块在 Linux 下是 no-op，直接回退 walker）。
use std::sync::atomic::AtomicBool;

#[cfg(windows)]
use std::collections::HashSet;
#[cfg(windows)]
use std::path::{Path, PathBuf};
#[cfg(windows)]
use std::sync::atomic::Ordering;

use super::index::SharedIndex;
#[cfg(windows)]
use super::index::{is_skipped, IndexEntry, MAX_ENTRIES};
#[cfg(windows)]
use super::pinyin_fields;
#[cfg(windows)]
use crate::logln;

/// 尝试用 USN/MFT 构建全盘索引；成功索引到条目返回 `Some(条目数)`，
/// 失败（无权限 / 非 NTFS）返回 `None`（回退 walker）。
#[cfg(windows)]
pub fn build_via_mft(idx: &SharedIndex, stop: &AtomicBool) -> Option<usize> {
    let mut total = 0usize;
    let mut any_drive = false;
    for drive in super::index::list_roots() {
        if stop.load(Ordering::Relaxed) || total >= MAX_ENTRIES {
            break;
        }
        if let Some(n) = scan_drive_mft(&drive, idx, stop, total) {
            any_drive = true;
            total += n;
        }
    }
    if any_drive {
        Some(total)
    } else {
        None
    }
}

/// 非 Windows 平台无 MFT，直接回退 walker。
#[cfg(not(windows))]
pub fn build_via_mft(_idx: &SharedIndex, _stop: &AtomicBool) -> Option<usize> {
    None
}

/// 按**完整路径**判定是否为系统噪音子树。
///
/// MFT 是扁平枚举：即便跳过了 `C:\Windows` 这条目录记录，它的子项仍是独立的
/// MFT 记录、照样会被枚举到。因此只靠目录名 + `depth <= 1` 判定会大量漏网
/// （`WinSxS`、`AppData\Local\Temp`、`Servicing` 等会占满索引名额）。
/// 这里对解析出的绝对路径做大小写不敏感的子串判定，与 walker 的剪枝范围对齐。
#[cfg(windows)]
fn path_is_noise(path: &Path) -> bool {
    let p = path.to_string_lossy().to_lowercase();
    // `\program files\` 已覆盖 `\program files (x86)\`；`\appdata\` 已覆盖
    // `\appdata\local\temp\`（保留显式项以示意图）。
    p.contains(r"\windows\")
        || p.contains(r"\$recycle.bin\")
        || p.contains(r"\system volume information\")
        || p.contains(r"\appdata\local\temp\")
        || p.contains(r"\appdata\")
        || p.contains(r"\program files\")
}

/// 解析单个盘的 MFT/USN 并写入索引；该盘不可用时返回 `None`。
#[cfg(windows)]
fn scan_drive_mft(
    drive: &Path,
    idx: &SharedIndex,
    stop: &AtomicBool,
    start_total: usize,
) -> Option<usize> {
    let letter = drive
        .to_string_lossy()
        .trim_end_matches([':', '\\'])
        .to_string();
    if letter.is_empty() {
        return None;
    }

    let drive_char = letter.chars().next()?;
    let t0 = std::time::Instant::now();

    // 1. 打开卷设备（需管理员权限；失败多为无权限或非 NTFS/ReFS）。
    let volume = match usn_journal_rs::volume::Volume::from_drive_letter(drive_char) {
        Ok(v) => v,
        Err(e) => {
            logln!("mft: {letter}: open volume handle failed (need admin / non-NTFS): {e}");
            return None;
        }
    };

    let mft = volume.mft();
    // 必须用带 LRU 缓存的解析器：无缓存版每条记录都会 OpenFileById 打开
    // 父目录句柄（百万级条目 = 百万次句柄打开，慢到不可用）；缓存版把目录
    // 路径入 LRU（4096 条），同目录下的文件直接复用，syscall 降至约每目录一次。
    // resolve_path 需要 &mut self，故必须为 mut。
    let mut resolver = volume.path_resolver_with_cache();
    // 已判定为噪音的目录 fid 集合。`FSCTL_ENUM_USN_DATA` 按 fid 升序枚举，
    // 父目录必先于子孙出现，因此记下一次就能让整棵子树后续零成本剪枝。
    // 这是 C 盘耗时的大头：WinSxS/System32 等近百万文件根本不该走到路径解析。
    let mut noise_fids: HashSet<u64> = HashSet::new();
    let mut count = 0usize;

    // 2. 流式遍历内核吐出的 MFT 记录（内部按 64KB 缓冲批量读取）。
    for entry_res in mft.iter() {
        if stop.load(Ordering::Relaxed) || start_total + count >= MAX_ENTRIES {
            break;
        }
        let entry = match entry_res {
            Ok(e) => e,
            Err(_) => continue,
        };

        // 剪枝前置（一）：父目录已在噪音集合 → 登记自身 fid（若是目录）并跳过。
        //
        // 登记自身 fid 是**必需**的：不登记就会「断链」——孙辈的 parent_fid
        // 不在集合内，会重新掉回昂贵的 resolve_path。实测 C:\Windows 下
        // System32/drivers/WinSxS 等深层子树正是因此全量解析，剪枝形同虚设
        // （C 盘 8.3s，比不优化还慢）。登记后噪音标记沿目录链一路下传。
        // 严禁对已确定舍弃的系统文件执行昂贵的路径回溯（OpenFileById）。
        if noise_fids.contains(&entry.parent_fid) {
            if entry.is_dir() {
                noise_fids.insert(entry.fid);
            }
            continue;
        }

        // MftEntry.file_name 是 OsString 字段（无 file_name() 方法）。
        let file_name = entry.file_name.to_string_lossy();
        // 跳过 $MFT / $LogFile / $Bitmap 等系统元文件。
        if file_name.starts_with('$') {
            continue;
        }

        // is_dir()（无 is_directory() 方法）。
        let is_dir = entry.is_dir();
        // 剪枝前置（二）：目录名命中噪音表 → 记下 fid 后跳过，
        // 其整棵子树都会被上面（一）拦下，无需再解析任何一个子孙。
        if is_dir && is_skipped(&file_name) {
            noise_fids.insert(entry.fid);
            continue;
        }

        // 3. 将文件引用号解析为完整绝对路径；解析不出时退化为盘根 + 文件名。
        let path: PathBuf = match resolver.resolve_path(&entry) {
            Some(p) => p,
            None => drive.join(&*file_name),
        };

        // 兜底：目录名看不出、但解析出的路径落在噪音子树内（理论上已被
        // （一）（二）覆盖，此处防枚举顺序异常）。目录同样登记 fid 惠及子孙。
        if path_is_noise(&path) {
            if is_dir {
                noise_fids.insert(entry.fid);
            }
            continue;
        }

        let fields = pinyin_fields(&file_name);
        if let Ok(mut w) = idx.write() {
            w.entries.push(IndexEntry {
                path,
                name: file_name.into_owned(),
                fields,
                is_dir,
            });
        }
        count += 1;
    }

    if count == 0 {
        logln!(
            "mft: {letter}: 0 usable entries in {:.1} ms -> treated as unavailable",
            t0.elapsed().as_secs_f64() * 1000.0
        );
        None
    } else {
        logln!(
            "mft: {letter}: {} entries in {:.1} ms",
            count,
            t0.elapsed().as_secs_f64() * 1000.0
        );
        Some(count)
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[test]
    fn noise_paths_are_filtered() {
        // Windows 核心系统目录（任意盘符、任意大小写）。
        assert!(path_is_noise(Path::new(r"C:\Windows\System32\drivers\etc\hosts")));
        assert!(path_is_noise(Path::new(r"c:\windows\winsxs\amd64_x.dll")));
        assert!(path_is_noise(Path::new(r"D:\$Recycle.Bin\S-1-5-21\desktop.ini")));
        assert!(path_is_noise(Path::new(
            r"C:\System Volume Information\tracking.log"
        )));
        assert!(path_is_noise(Path::new(
            r"C:\Users\me\AppData\Local\Temp\scratch.tmp"
        )));
        assert!(path_is_noise(Path::new(r"C:\Program Files\Git\cmd\git.exe")));
        // 用户数据不应被误伤。
        assert!(!path_is_noise(Path::new(r"D:\Projects\rove\src\main.rs")));
        assert!(!path_is_noise(Path::new(r"D:\文档\报告.docx")));
        assert!(!path_is_noise(Path::new(r"D:\MyWindows\backup.iso")));
    }
}
