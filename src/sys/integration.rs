//! 「接管资源管理器」：让文件夹 / 磁盘 / 此电脑的默认打开程序变成 Rove。
//!
//! 只写 `HKCU\Software\Classes`（当前用户，不需要管理员权限），不碰 `HKLM`，
//! 也不改 explorer.exe。做法是在三个类下登记一个 `Rove` 谓词，再把该类的
//! `shell` 默认值指向它：
//!
//! - `Directory`：普通文件夹
//! - `Drive`：磁盘
//! - `CLSID\{20D04FE0-…}`：此电脑
//!
//! 写入前把每个受影响值的原样（类型 + 数据，或「本来不存在」）记到
//! `%APPDATA%\Rove\shell_integration.json`。关闭开关时只还原「当前仍是我们
//! 写的值」的那些，用户后来自己改过的值一律不动；我们自己新建的空键也会删掉。
//!
//! 配置项 `explorer_takeover` 是唯一真源：每次启动按它同步一次，所以 exe
//! 换了路径、或被别的程序覆盖后，下次启动会自动纠正。

use std::io;
use std::path::{Path, PathBuf};

use crate::{logln, logv};

/// 谓词名（也是注册表键名）。改名会让旧注册无法识别，别动。
const VERB: &str = "Rove";
const CLASSES: &str = r"Software\Classes";
/// 此电脑的 CLSID 类。
const THIS_PC_CLASS: &str = r"CLSID\{20D04FE0-3AEA-1069-A2D8-08002B30309D}";
/// REG_SZ 的类型号（与注册表 API 一致）。
const REG_SZ_U32: u32 = 1;

// ---------------------------------------------------------------------------
// 注册表读写（Windows 实现 / 其他平台桩）
// ---------------------------------------------------------------------------

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

/// REG_SZ 的字节形态：UTF-16LE + 结尾 0。
fn string_bytes(text: &str) -> Vec<u8> {
    wide(text).iter().flat_map(|u| u.to_le_bytes()).collect()
}

/// REG_SZ 字节 → 字符串（读到别人的值时用来比较）。
pub(super) fn bytes_to_string(data: &[u8]) -> String {
    let units: Vec<u16> = data
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .take_while(|&c| c != 0)
        .collect();
    String::from_utf16_lossy(&units)
}

#[cfg(windows)]
pub(super) mod reg {
    use super::wide;
    use std::io;
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::ERROR_SUCCESS;
    use windows::Win32::System::Registry::{
        RegCloseKey, RegDeleteKeyValueW, RegDeleteKeyW, RegOpenKeyExW, RegQueryInfoKeyW,
        RegQueryValueExW, RegSetKeyValueW, HKEY, HKEY_CURRENT_USER, KEY_READ, REG_VALUE_TYPE,
    };

    /// ERROR_FILE_NOT_FOUND：键或值不存在（不算错误）。
    const ERROR_NOT_FOUND: u32 = 2;

    fn name_ptr(name: Option<&Vec<u16>>) -> PCWSTR {
        name.as_ref().map_or(PCWSTR::null(), |n| PCWSTR(n.as_ptr()))
    }

    fn open(key: &str) -> Option<HKEY> {
        let path = wide(key);
        let mut handle = HKEY::default();
        let status = unsafe {
            RegOpenKeyExW(HKEY_CURRENT_USER, PCWSTR(path.as_ptr()), None, KEY_READ, &mut handle)
        };
        if status == ERROR_SUCCESS {
            Some(handle)
        } else {
            None
        }
    }

    pub fn key_exists(key: &str) -> bool {
        let Some(handle) = open(key) else {
            return false;
        };
        let _ = unsafe { RegCloseKey(handle) };
        true
    }

    /// `Ok(None)` = 键或值不存在；`Err` = 真的读不了。
    /// 只读打开：键不存在就返回 None，**不会顺手把键建出来**。
    /// 启动时的清理探测用它，避免每查一次残留就凭空造出一个键。
    pub fn peek_value(key: &str, name: Option<&str>) -> io::Result<Option<(u32, Vec<u8>)>> {
        read_value_inner(key, name)
    }

    pub fn read_value(key: &str, name: Option<&str>) -> io::Result<Option<(u32, Vec<u8>)>> {
        read_value_inner(key, name)
    }

    fn read_value_inner(key: &str, name: Option<&str>) -> io::Result<Option<(u32, Vec<u8>)>> {
        let value_name = name.map(wide);
        let Some(handle) = open(key) else {
            return Ok(None);
        };
        let result = (|| {
            let mut kind = REG_VALUE_TYPE(0);
            let mut size = 0u32;
            let status = unsafe {
                RegQueryValueExW(
                    handle,
                    name_ptr(value_name.as_ref()),
                    None,
                    Some(&mut kind),
                    None,
                    Some(&mut size),
                )
            };
            if status.0 == ERROR_NOT_FOUND {
                return Ok(None);
            }
            if status != ERROR_SUCCESS {
                return Err(io::Error::from_raw_os_error(status.0 as i32));
            }
            let mut data = vec![0u8; size as usize];
            let status = unsafe {
                RegQueryValueExW(
                    handle,
                    name_ptr(value_name.as_ref()),
                    None,
                    Some(&mut kind),
                    Some(data.as_mut_ptr()),
                    Some(&mut size),
                )
            };
            if status != ERROR_SUCCESS {
                return Err(io::Error::from_raw_os_error(status.0 as i32));
            }
            data.truncate(size as usize);
            Ok(Some((kind.0, data)))
        })();
        let _ = unsafe { RegCloseKey(handle) };
        result
    }

    /// 写 REG_SZ；`RegSetKeyValueW` 会连带创建缺失的键。
    pub fn write_string(key: &str, name: Option<&str>, text: &str) -> io::Result<()> {
        let path = wide(key);
        let value_name = name.map(wide);
        let data = super::string_bytes(text);
        let status = unsafe {
            RegSetKeyValueW(
                HKEY_CURRENT_USER,
                PCWSTR(path.as_ptr()),
                name_ptr(value_name.as_ref()),
                super::REG_SZ_U32,
                Some(data.as_ptr().cast()),
                data.len() as u32,
            )
        };
        if status == ERROR_SUCCESS {
            Ok(())
        } else {
            Err(io::Error::from_raw_os_error(status.0 as i32))
        }
    }

    /// 写入任意类型的数据（还原用户原值时用，保证类型不被改成 REG_SZ）。
    pub fn write_raw(key: &str, name: Option<&str>, kind: u32, data: &[u8]) -> io::Result<()> {
        let path = wide(key);
        let value_name = name.map(wide);
        let status = unsafe {
            RegSetKeyValueW(
                HKEY_CURRENT_USER,
                PCWSTR(path.as_ptr()),
                name_ptr(value_name.as_ref()),
                kind,
                Some(data.as_ptr().cast()),
                data.len() as u32,
            )
        };
        if status == ERROR_SUCCESS {
            Ok(())
        } else {
            Err(io::Error::from_raw_os_error(status.0 as i32))
        }
    }

    pub fn delete_value(key: &str, name: &str) -> io::Result<()> {
        let path = wide(key);
        let value_name = wide(name);
        let status = unsafe {
            RegDeleteKeyValueW(
                HKEY_CURRENT_USER,
                PCWSTR(path.as_ptr()),
                PCWSTR(value_name.as_ptr()),
            )
        };
        if status == ERROR_SUCCESS || status.0 == ERROR_NOT_FOUND {
            Ok(())
        } else {
            Err(io::Error::from_raw_os_error(status.0 as i32))
        }
    }

    /// 只在键已经空了（无子键、无值）时才删除，避免连带别人的内容。
    pub fn delete_key_if_empty(key: &str) {
        let Some(handle) = open(key) else {
            return;
        };
        let mut subkeys = 0u32;
        let mut values = 0u32;
        let status = unsafe {
            RegQueryInfoKeyW(
                handle,
                None,
                None,
                None,
                Some(&mut subkeys),
                None,
                None,
                Some(&mut values),
                None,
                None,
                None,
                None,
            )
        };
        let _ = unsafe { RegCloseKey(handle) };
        if status != ERROR_SUCCESS || subkeys != 0 || values != 0 {
            return;
        }
        let path = wide(key);
        let _ = unsafe { RegDeleteKeyW(HKEY_CURRENT_USER, PCWSTR(path.as_ptr())) };
    }
}

#[cfg(not(windows))]
pub(super) mod reg {
    use std::io;

    pub fn key_exists(_key: &str) -> bool {
        false
    }
    pub fn peek_value(_key: &str, _name: Option<&str>) -> io::Result<Option<(u32, Vec<u8>)>> {
        Ok(None)
    }
    pub fn read_value(_key: &str, _name: Option<&str>) -> io::Result<Option<(u32, Vec<u8>)>> {
        Ok(None)
    }
    pub fn write_string(_key: &str, _name: Option<&str>, _text: &str) -> io::Result<()> {
        Err(io::Error::new(io::ErrorKind::Unsupported, "仅 Windows 支持"))
    }
    pub fn write_raw(_key: &str, _name: Option<&str>, _kind: u32, _data: &[u8]) -> io::Result<()> {
        Err(io::Error::new(io::ErrorKind::Unsupported, "仅 Windows 支持"))
    }
    pub fn delete_value(_key: &str, _name: &str) -> io::Result<()> {
        Ok(())
    }
    pub fn delete_key_if_empty(_key: &str) {}
}

// ---------------------------------------------------------------------------
// 备份记录
// ---------------------------------------------------------------------------

/// 一个被写过的注册表值：`kind/data` 为 None 表示「原本不存在，需要删除」。
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct SavedValue {
    key: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    kind: Option<u32>,
    #[serde(default)]
    data: Option<Vec<u8>>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct Backup {
    /// 接管之前系统里的原值，关闭时还原用。
    values: Vec<SavedValue>,
    /// 我们写下去的值，关闭时判断「没人动过」用。
    applied: Vec<SavedValue>,
    /// 我们新建的键（关闭时若已空则删除）。
    created_keys: Vec<String>,
}

fn backup_path() -> Option<PathBuf> {
    let appdata = std::env::var_os("APPDATA").map(PathBuf::from)?;
    Some(appdata.join("Rove").join("shell_integration.json"))
}

fn load_backup() -> Option<Backup> {
    let path = backup_path()?;
    let bytes = std::fs::read(&path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn save_backup(backup: &Backup) -> io::Result<()> {
    let Some(path) = backup_path() else {
        return Err(io::Error::new(io::ErrorKind::NotFound, "APPDATA 不可用"));
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let bytes = serde_json::to_vec_pretty(backup).map_err(io::Error::other)?;
    std::fs::write(&path, bytes)
}

// ---------------------------------------------------------------------------
// 对外接口
// ---------------------------------------------------------------------------

/// 按配置开 / 关接管。`label` 是右键菜单里显示的文字（跟随界面语言）。
/// 返回是否真的改动了注册表。
pub fn sync(enabled: bool, label: &str) -> Result<bool, String> {
    if enabled {
        let exe = std::env::current_exe().map_err(|e| format!("取当前 exe 路径失败：{e}"))?;
        register(&exe, label)
    } else {
        unregister(label)
    }
}

fn command_for(exe: &Path, this_pc: bool) -> String {
    if this_pc {
        format!("\"{}\" --this-pc", exe.display())
    } else {
        format!("\"{}\" --shell-open \"%1\"", exe.display())
    }
}

/// 我们打算写下去的全部值：(键, 值名, 文本)。
///
/// 只登记自己的谓词并把类的默认谓词指过来，**不改**内置的 `open` / `explore`：
/// 改了之后资源管理器右键会同时出现「Explore」和「用 Rove 打开」两个做同一件事
/// 的条目（用户实测反馈），而且会抢掉系统原本的浏览/新窗口语义。
/// 默认谓词足以让双击打开 Rove。
fn plan(exe: &Path, label: &str) -> Vec<(String, Option<&'static str>, String)> {
    let icon = format!("\"{}\",0", exe.display());
    let mut out = Vec::new();
    for (class, this_pc) in [("Directory", false), ("Drive", false), (THIS_PC_CLASS, true)] {
        let verb = format!(r"{CLASSES}\{class}\shell\{VERB}");
        out.push((format!(r"{CLASSES}\{class}\shell"), None, VERB.to_string()));
        out.push((verb.clone(), None, label.to_string()));
        out.push((verb.clone(), Some("Icon"), icon.clone()));
        out.push((format!(r"{verb}\command"), None, command_for(exe, this_pc)));
    }
    out
}

/// 通知资源管理器刷新图标与打开方式缓存，否则新设置要等下次登录才生效。
fn notify_shell() {
    #[cfg(windows)]
    unsafe {
        use windows::Win32::UI::Shell::{SHChangeNotify, SHCNE_ASSOCCHANGED, SHCNF_IDLIST};
        SHChangeNotify(SHCNE_ASSOCCHANGED, SHCNF_IDLIST, None, None);
    }
}

/// 旧版本会覆盖内置 `open` / `explore` 的命令（现已取消）。
fn legacy_builtin_keys() -> Vec<String> {
    let mut out = Vec::new();
    for class in ["Directory", "Drive"] {
        for builtin in ["open", "explore"] {
            out.push(format!(r"{CLASSES}\{class}\shell\{builtin}\command"));
        }
    }
    out
}

/// 这条命令是不是我们（任何版本的 Rove）写下的。
///
/// 不能只认当前 exe 的完整路径：exe 换过位置、版本又变过之后，旧值里记的是
/// 老的路径，按文本比对就会漏掉 —— 表现就是右键「Explore」报「此文件没有
/// 与之关联的应用」（命令指向一个已经不存在的 rove.exe）。
fn is_our_shell_command(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    lower.contains("rove.exe") && (lower.contains("--shell-open") || lower.contains("--this-pc"))
}

/// 清掉旧版本对内置 `open` / `explore` 命令的覆盖。
///
/// 有备份原值就还原，没有就删掉这个值，让系统回落到自己的内置谓词。
/// 开关**打开时**也要做：这些残留跟当前开关状态无关，留着就是坏的。
fn cleanup_legacy_overrides(backup: &Backup) -> usize {
    let mut cleaned = 0usize;
    for key in legacy_builtin_keys() {
        let Ok(Some((kind, data))) = reg::peek_value(&key, None) else {
            continue;
        };
        if kind != REG_SZ_U32 || !is_our_shell_command(&bytes_to_string(&data)) {
            continue;
        }
        let original = backup
            .values
            .iter()
            .find(|v| v.key == key && v.name.is_none());
        let (done, deleted) = match original.and_then(|v| v.kind.zip(v.data.as_ref())) {
            Some((kind, data)) => (reg::write_raw(&key, None, kind, data).is_ok(), false),
            None => (reg::delete_value(&key, "").is_ok(), true),
        };
        if !done {
            continue;
        }
        if deleted {
            // 值删空后把键也收掉：`...\shell\explore` 这种键只要还留在 HKCU，
            // 资源管理器就会把键名当成菜单项显示出来，点下去照样报错。
            // 只在键里确实没有别的内容时才删，不碰用户自己加的值。
            let _ = reg::delete_key_if_empty(&key);
            if let Some(parent) = key.strip_suffix(r"\command") {
                let _ = reg::delete_key_if_empty(parent);
            }
        }
        logln!("[takeover] 清理旧版内置谓词覆盖 {key}");
        cleaned += 1;
    }
    cleaned
}

/// 把 plan 里尚未记进备份的项补齐，并把备份落盘。
///
/// 为什么需要：接管规则会随版本变化（例如后来才补上内置 `open`/`explore` 的
/// 覆盖）。老版本写下的备份里没有这些项，关闭开关时就会漏还原，表现为
/// 「开关已经关了，双击还是打开 Rove、右键还留着菜单」。
///
/// 补记录时如果发现该项**已经指向我们**（老版本留下的、原值已不可知），
/// 就把原值记成「不存在」——关闭时删掉该值，让系统回落到内置行为。
fn merge_missing_into_backup(
    backup: &mut Backup,
    planned: &[(String, Option<&'static str>, String)],
) -> Result<bool, String> {
    let mut changed = false;
    for (key, name, text) in planned {
        let known = backup
            .applied
            .iter()
            .any(|v| v.key == *key && v.name.as_deref() == *name);
        if known {
            continue;
        }
        changed = true;
        let current = reg::read_value(key, *name).map_err(|e| format!("读取 {key} 失败：{e}"))?;
        let already_ours = current
            .as_ref()
            .is_some_and(|(kind, data)| *kind == REG_SZ_U32 && bytes_to_string(data) == *text);
        // 原本存在且不是我们写的 → 记下原值；否则（不存在 / 已被我们占用）→ 记「不存在」。
        let original = match current {
            Some((kind, data)) if !already_ours => SavedValue {
                key: key.clone(),
                name: name.map(str::to_string),
                kind: Some(kind),
                data: Some(data),
            },
            _ => SavedValue {
                key: key.clone(),
                name: name.map(str::to_string),
                kind: None,
                data: None,
            },
        };
        backup.values.push(original);
        backup.applied.push(SavedValue {
            key: key.clone(),
            name: name.map(str::to_string),
            kind: Some(REG_SZ_U32),
            data: Some(string_bytes(text)),
        });
        if !reg::key_exists(key) && !backup.created_keys.iter().any(|k| k == key) {
            backup.created_keys.push(key.clone());
        }
    }
    Ok(changed)
}

fn register(exe: &Path, label: &str) -> Result<bool, String> {
    let planned = plan(exe, label);
    // 旧版本改过内置 open/explore 的命令：那是坏的（右键 Explore 会弹
    // 「此文件没有与之关联的应用」），跟开关状态无关，看到就清掉。
    let prev_backup = load_backup();
    let backup_before = prev_backup.clone().unwrap_or_default();
    let cleaned = cleanup_legacy_overrides(&backup_before);
    // 已有备份说明之前就接管过：保留最初的原值，别把我们现在写的东西当成
    // 「用户原来的设置」记下来（否则关闭后会还原成 Rove 自己）。
    // 但规则会随版本增加（内置 open/explore 覆盖是后补的），所以老备份要补齐，
    // 否则关闭开关时那些新增项永远还原不回去。
    let mut backup = load_backup().unwrap_or_default();
    backup.values.retain(|v| planned.iter().any(|(k, n, _)| v.key == *k && v.name.as_deref() == *n));
    backup.applied.retain(|v| planned.iter().any(|(k, n, _)| v.key == *k && v.name.as_deref() == *n));
    merge_missing_into_backup(&mut backup, &planned)?;
    let backup_changed = prev_backup.as_ref().map(|prev| *prev != backup).unwrap_or(true);
    // 先落盘再改注册表：中途崩溃也还有还原依据。
    if backup_changed {
        save_backup(&backup).map_err(|e| format!("写入备份失败：{e}"))?;
    }
    // 只写「值不对」的那些项。每次启动都无脑重写 12 个值会让资源管理器
    // 收到一次关联变更通知、重读一遍图标缓存 —— 完全没必要。
    let mut written = 0usize;
    // 关闭时会按 created_keys 逆序删除键，所以写入顺序必须是父键在前。
    for (key, name, text) in &planned {
        let unchanged = reg::peek_value(key, *name)
            .ok()
            .flatten()
            .is_some_and(|(kind, data)| kind == REG_SZ_U32 && bytes_to_string(&data) == *text);
        if unchanged {
            continue;
        }
        if let Err(err) = reg::write_string(key, *name, text) {
            return Err(format!("写入 {key} 失败：{err}"));
        }
        written += 1;
    }
    if written > 0 || cleaned > 0 {
        notify_shell();
    }
    if written == 0 && cleaned == 0 {
        logv!(
            "接管资源管理器：登记 {} 个值，无需改动",
            planned.len()
        );
    } else {
        // 真的有写入/清理才值得进 level 1：正常热启动这行不该出现。
        logln!(
            "接管资源管理器：登记 {} 个值（本次写入 {written} 个，清理旧版残留 {cleaned} 个）",
            planned.len()
        );
    }
    Ok(true)
}

/// 当前注册表值是否仍然是「我们写的」。
///
/// 认两种文本：备份里记下的旧文本、以及按**当前** exe / 语言重新算出来的文本。
/// 后者必须要有 —— exe 换过位置或用户切过语言后，备份里那条旧文本对不上，
/// 只比对备份就会漏还原（开关关了，右键菜单和双击却还是 Rove）。
fn still_ours(
    expected: &SavedValue,
    fresh_text: Option<&str>,
    current: Option<&(u32, Vec<u8>)>,
) -> bool {
    let fresh = fresh_text.map(string_bytes);
    current.is_some_and(|(kind, data)| {
        Some(*kind) == expected.kind
            && (Some(data.as_slice()) == expected.data.as_deref()
                || fresh.as_deref() == Some(data.as_slice()))
    })
}

fn unregister(label: &str) -> Result<bool, String> {
    // 关闭时的 plan：用来兜底清理「备份里没记过、但其实是我们写的」那些项
    // （老版本备份 + 新版本规则 = 漏还原）。
    let exe = std::env::current_exe().ok();
    let planned = exe.as_deref().map(|exe| plan(exe, label)).unwrap_or_default();
    let mut backup = load_backup().unwrap_or_default();
    if backup.applied.is_empty() && planned.is_empty() {
        return Ok(false);
    }
    let mut orphaned: Vec<(String, Option<String>)> = Vec::new();
    for (key, name, text) in &planned {
        let known = backup
            .applied
            .iter()
            .any(|v| v.key == *key && v.name.as_deref() == *name);
        if known {
            continue;
        }
        // 备份不知道这一项：只要它当前确实指向我们，就必须清掉。
        let ours = reg::read_value(key, *name)
            .ok()
            .flatten()
            .is_some_and(|(kind, data)| {
                if kind != REG_SZ_U32 {
                    return false;
                }
                let current = bytes_to_string(&data);
                current == *text || is_our_shell_command(&current)
            });
        if ours {
            logln!(
                "[takeover] 备份外的残留 {} [{}] 仍指向我们，直接清理",
                key,
                name.unwrap_or("<默认值>")
            );
            orphaned.push((key.clone(), name.map(str::to_string)));
        }
    }
    // 旧版本对内置 open/explore 的覆盖：不在 plan 里，单独按「看起来像我们的
    // 命令」识别并清掉（exe 路径可能已经变了）。
    let mut restored = cleanup_legacy_overrides(&backup);
    // 还原失败的项：只要还有一条没还原成功，就必须保留备份文件，
    // 否则「还原依据」被删掉，这些值以后再也回不到原样。
    let mut failed = 0usize;
    for expected in &backup.applied {
        let name = expected.name.as_deref();
        let current = match reg::read_value(&expected.key, name) {
            Ok(value) => value,
            Err(err) => return Err(format!("读取 {} 失败：{err}", expected.key)),
        };
        logv!(
            "[takeover] 还原 {} [{}] 当前={:?}",
            expected.key,
            name.unwrap_or("<默认值>"),
            current
                .as_ref()
                .map(|(kind, data)| (*kind, bytes_to_string(data)))
        );
        let fresh = planned
            .iter()
            .find(|(k, n, _)| *k == expected.key && *n == name.map(|s| s.to_string()).as_deref())
            .map(|(_, _, text)| text.as_str());
        let ours = still_ours(expected, fresh, current.as_ref());
        if !ours {
            // 被别人改过了：保持用户/其他程序的选择，不动。
            continue;
        }
        let original = backup
            .values
            .iter()
            .find(|v| v.key == expected.key && v.name == expected.name);
        match original.and_then(|v| v.kind.zip(v.data.as_ref())) {
            Some((kind, data)) => {
                // 按原类型原数据写回：不把 REG_EXPAND_SZ 之类悄悄改成 REG_SZ。
                if let Err(err) = reg::write_raw(&expected.key, name, kind, data) {
                    logln!("!! 还原 {} 失败：{err}", expected.key);
                    failed += 1;
                    continue;
                }
            }
            None => {
                // 原值本来不存在 → 删掉我们写的那个值。
                // 注意 name 为 None 表示「默认值」，必须用空名字去删；
                // 早先这里写成 `if let Some(name)`，默认值被整个跳过，
                // 于是 `...\shell\Rove` 的标签和 `...\Rove\command` 的命令
                // 全都留着（图标是具名值所以删掉了）—— 表现为「关了开关，
                // 右键菜单还在、双击还是打开 Rove」。
                if let Err(err) = reg::delete_value(&expected.key, name.unwrap_or("")) {
                    logln!("!! 删除 {} 失败：{err}", expected.key);
                    failed += 1;
                    continue;
                }
            }
        }
        restored += 1;
    }
    // 备份之外、但确实是我们的残留：直接删值，让系统回落到内置行为。
    for (key, name) in &orphaned {
        match name {
            Some(name) => {
                if reg::delete_value(key, name).is_ok() {
                    restored += 1;
                }
            }
            None => {
                if reg::delete_value(key, "").is_ok() {
                    restored += 1;
                }
            }
        }
    }
    // 谓词键本身也清掉：老备份可能没记 created_keys。
    for (key, name, _) in &planned {
        if name.is_none() {
            backup.created_keys.push(key.clone());
        }
    }
    // 旧版本覆盖过内置 open/explore 的命令，写那个值会把 `...\shell\open`、
    // `...\shell\explore` 两个键一并建出来。值删空后键还留着，资源管理器就会
    // 把键名当菜单项显示（点它报「此文件没有与之关联的应用」）—— 必须一起收掉。
    for key in legacy_builtin_keys() {
        if let Some(parent) = key.strip_suffix(r"\command") {
            backup.created_keys.push(parent.to_string());
        }
        backup.created_keys.push(key);
    }
    backup.created_keys.sort();
    backup.created_keys.dedup();
    // 逆序删除：先删子的 command 键，再删谓词键，空的不会留在注册表里。
    for key in backup.created_keys.iter().rev() {
        reg::delete_key_if_empty(key);
    }
    if failed == 0 {
        if let Some(path) = backup_path() {
            let _ = std::fs::remove_file(path);
        }
    } else {
        // 保留备份：下次关闭开关（或下次启动）还能再试一次还原。
        logln!("!! 接管资源管理器：{failed} 项未能还原，已保留备份以便重试");
    }
    notify_shell();
    logv!(
        "接管资源管理器：已还原 {restored} 个值（备份 {} 项 + 残留 {} 项）",
        backup.applied.len(),
        orphaned.len()
    );
    if failed > 0 {
        return Err(format!("{failed} 个注册表值未能还原（已保留备份）"));
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_targets_folders_drives_and_this_pc() {
        let exe = Path::new(r"C:\Apps\rove.exe");
        let planned = plan(exe, "Open with Rove");
        let key = |k: &str| {
            planned
                .iter()
                .find(|(key, name, _)| key == k && name.is_none())
                .map(|(_, _, text)| text.clone())
        };
        // 三类的 shell 默认谓词都指向 Rove。
        for class in ["Directory", "Drive", THIS_PC_CLASS] {
            assert_eq!(key(&format!(r"{CLASSES}\{class}\shell")).as_deref(), Some(VERB));
        }
        // 文件夹/磁盘走 --shell-open，此电脑走 --this-pc。
        assert_eq!(
            key(&format!(r"{CLASSES}\Directory\shell\{VERB}\command")).as_deref(),
            Some(r#""C:\Apps\rove.exe" --shell-open "%1""#)
        );
        assert_eq!(
            key(&format!(r"{CLASSES}\{THIS_PC_CLASS}\shell\{VERB}\command")).as_deref(),
            Some(r#""C:\Apps\rove.exe" --this-pc"#)
        );
        // 不能覆盖内置 open / explore：否则右键会同时出现「Explore」和
        // 「用 Rove 打开」两个做同一件事的条目（用户实测反馈），
        // 还会抢掉系统原本的浏览/新窗口语义。
        for class in ["Directory", "Drive"] {
            for builtin in ["open", "explore"] {
                let k = format!(r"{CLASSES}\{class}\shell\{builtin}\command");
                assert!(key(&k).is_none(), "{k} 不应被接管");
            }
        }
        // 谓词名用传进来的本地化文本。
        let label = planned
            .iter()
            .find(|(k, n, _)| *k == format!(r"{CLASSES}\Directory\shell\{VERB}") && n.is_none())
            .map(|(_, _, text)| text.as_str());
        assert_eq!(label, Some("Open with Rove"));
    }

    /// unregister 判断「当前值还是不是我们写的」时的规则：
    /// 备份里记的旧文本，或按**当前** exe / 语言重新算出来的文本，
    /// 任一匹配就算我们的。exe 换过路径、用户切过语言之后都必须仍能还原。
    #[test]
    fn moved_exe_and_changed_language_still_restore() {
        let old_text = r#""C:\old\rove.exe" --shell-open "%1""#;
        let new_text = r#""C:\new\rove.exe" --shell-open "%1""#;
        let expected = SavedValue {
            key: format!(r"{CLASSES}\Directory\shell\{VERB}\command"),
            name: None,
            kind: Some(REG_SZ_U32),
            data: Some(string_bytes(old_text)),
        };
        let reg = |text: &str| (REG_SZ_U32, string_bytes(text));

        // exe 换了位置：备份文本对不上，但按当前 exe 算出来的文本对上了。
        assert!(still_ours(&expected, Some(new_text), Some(&reg(new_text))));
        // 注册表里还是旧路径（这次启动没重写成功）：靠备份文本认出来。
        assert!(still_ours(&expected, Some(new_text), Some(&reg(old_text))));
        // 用户自己改成别的程序：绝不能动。
        assert!(!still_ours(
            &expected,
            Some(new_text),
            Some(&reg(r#""C:\Tools\far.exe" "%1""#))
        ));
        // 值被删了：没什么可还原的。
        assert!(!still_ours(&expected, Some(new_text), None));
        // 类型被改了（REG_EXPAND_SZ）：不是我们写的，别碰。
        assert!(!still_ours(
            &expected,
            Some(new_text),
            Some(&(2, string_bytes(new_text)))
        ));
    }

    #[test]
    fn recognizes_our_shell_commands_from_any_old_path() {
        // 老版本写的覆盖：exe 早就换地方了，但仍然是我们的，必须能认出来。
        assert!(is_our_shell_command(
            r#""D:\old\build\rove.exe" --shell-open "%1""#
        ));
        assert!(is_our_shell_command(
            r#""C:\Program Files\Rove\ROVE.EXE" --shell-open "%1""#
        ));
        assert!(is_our_shell_command(r#""C:\r\rove.exe" --this-pc"#));
        // 系统内置谓词不能被误判成我们的。
        assert!(!is_our_shell_command(r#"%SystemRoot%\Explorer.exe /e,"%1""#));
        assert!(!is_our_shell_command(r#""C:\Tools\far.exe" "%1""#));
        assert!(!is_our_shell_command("Rove"));
    }

    #[test]
    fn string_bytes_are_utf16_with_nul() {
        let text = "D:\\重";
        let bytes = string_bytes(text);
        // 每个 UTF-16 单元 2 字节，末尾补一个 NUL 单元。
        assert_eq!(bytes.len(), (text.encode_utf16().count() + 1) * 2);
        assert_eq!(&bytes[bytes.len() - 2..], &[0, 0]);
    }
}
