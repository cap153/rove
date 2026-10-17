//! 任务 2：系统级拖放（`src/sys/dnd`）。
//!
//! - 拖出（Rove → 桌面/外部）：构造携带 `CF_HDROP` 的 Shell `IDataObject`
//!   （`SHCreateDataObject`，无需手写 COM 数据对象）+ 自定义 `IDropSource`，
//!   经 Win32 `DoDragDrop` 广播标准拖放循环。
//! - 移动语义：写入「Preferred DropEffect」= COPY|MOVE，由放置目标按盘符
//!   与修饰键决定；目标返回 MOVE 而源文件仍在时，由调用方删除源（标准协议）。
//! - 拖入（外部 → Rove）：由 winit `DroppedFile` 事件驱动，见 app.rs。
//!
//! 严禁在此模块操作 Slint UI；仅做 Win32 交互并返回结果。

use std::path::PathBuf;
#[cfg(windows)]
use std::time::{Duration, Instant};

/// 拖出判定阈值：按下后位移超过该值（逻辑像素）才触发拖放。
pub const DRAG_THRESHOLD: f32 = 4.0;

/// 拖出结果。
pub enum DragOutOutcome {
    /// 目标接受了文件。
    Dropped(DropEffect),
    /// 用户取消（Esc / 释放过快）。
    Cancelled,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum DropEffect {
    Copy,
    Move,
    Link,
    None,
}

/// 发起一次拖出循环。调用方处于左键按下的手势中时调用；
/// 本函数阻塞至拖放结束（内部为嵌套消息循环）。
///
/// `paths`：拖动的源文件列表（非空）。
#[cfg(windows)]
pub fn begin_drag_out(paths: &[PathBuf]) -> DragOutOutcome {
    if paths.is_empty() {
        return DragOutOutcome::Cancelled;
    }
    // OLE 拖放必须在 STA 线程执行。主线程可能被其他库初始化为 MTA，
    // 导致 DoDragDrop 跨进程无法封送数据对象（目标一律拒绝 → 禁止光标）。
    // 故在独立 STA 线程中运行拖放循环。
    let paths = paths.to_vec();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        unsafe {
            use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
            let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        }
        let outcome = drag_out_impl(&paths);
        let _ = tx.send(outcome);
    });
    rx.recv().unwrap_or(DragOutOutcome::Cancelled)
}

#[cfg(not(windows))]
pub fn begin_drag_out(_paths: &[PathBuf]) -> DragOutOutcome {
    DragOutOutcome::Cancelled
}

#[cfg(windows)]
fn drag_out_impl(paths: &[PathBuf]) -> DragOutOutcome {
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{DRAGDROP_S_CANCEL, DRAGDROP_S_DROP, DRAGDROP_S_USEDEFAULTCURSORS};
    use windows::Win32::System::Com::{
        CoTaskMemFree, DVASPECT_CONTENT, FORMATETC, IDataObject, STGMEDIUM, TYMED_HGLOBAL,
    };
    use windows::Win32::System::DataExchange::RegisterClipboardFormatW;
    use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
    use windows::Win32::System::Ole::{DoDragDrop, IDropSource, DROPEFFECT};
    use windows::Win32::System::Ole::{DROPEFFECT_COPY, DROPEFFECT_LINK, DROPEFFECT_MOVE};
    use windows::Win32::System::SystemServices::{MK_LBUTTON, MODIFIERKEYS_FLAGS};
    use windows::Win32::UI::Shell::{ILGetSize, SHCreateDataObject, SHParseDisplayName};
    use windows::Win32::UI::Shell::Common::ITEMIDLIST;
    use windows::Win32::System::Ole::IDropSource_Impl;
    use windows_core::implement;

    fn to_wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    // ---- 自定义 IDropSource：左键释放 → DROP；Esc → CANCEL；光标交给系统。----
    #[implement(IDropSource)]
    struct DropSource {
        started: Instant,
    }

    impl IDropSource_Impl for DropSource_Impl {
        fn QueryContinueDrag(
            &self,
            escape_pressed: windows::Win32::Foundation::BOOL,
            key_state: MODIFIERKEYS_FLAGS,
        ) -> windows::core::HRESULT {
            if escape_pressed.as_bool() {
                return DRAGDROP_S_CANCEL;
            }
            // 左键仍按住 → 继续拖动；释放 → DROP（释放过快视为误触，取消）。
            if key_state & MK_LBUTTON == MODIFIERKEYS_FLAGS(0) {
                if self.started.elapsed() > Duration::from_millis(150) {
                    return DRAGDROP_S_DROP;
                }
                return DRAGDROP_S_CANCEL;
            }
            windows::core::HRESULT(0)
        }

        fn GiveFeedback(&self, _effect: DROPEFFECT) -> windows::core::HRESULT {
            DRAGDROP_S_USEDEFAULTCURSORS
        }
    }

    fn free_pidl(p: *mut ITEMIDLIST) {
        if !p.is_null() {
            unsafe { CoTaskMemFree(Some(p as *const _)) };
        }
    }

    unsafe {
        // 路径 → 绝对 PIDL（*mut ITEMIDLIST），用于后续生成相对子项 PIDL / 回退。
        let mut full_pidls: Vec<*mut ITEMIDLIST> = Vec::with_capacity(paths.len());
        for p in paths {
            let wide = to_wide(&p.to_string_lossy());
            let mut pidl: *mut ITEMIDLIST = std::ptr::null_mut();
            if SHParseDisplayName(PCWSTR(wide.as_ptr()), None, &mut pidl, 0, None).is_err() {
                for f in &full_pidls {
                    free_pidl(*f);
                }
                return DragOutOutcome::Cancelled;
            }
            full_pidls.push(pidl);
        }

        // 判断所有文件是否同处一个文件夹；是则使用规范形式（父目录 PIDL + 相对子项 PIDL），
        // 该形式生成的 CF_HDROP 最稳定，可被桌面/资源管理器正确识别。
        let parents: std::collections::HashSet<String> = paths
            .iter()
            .filter_map(|p| p.parent().map(|pp| pp.to_string_lossy().to_string()))
            .collect();
        let data_obj: IDataObject = if parents.len() == 1 {
            let parent = paths[0].parent().unwrap();
            let parent_wide = to_wide(&parent.to_string_lossy());
            let mut pparent: *mut ITEMIDLIST = std::ptr::null_mut();
            if SHParseDisplayName(PCWSTR(parent_wide.as_ptr()), None, &mut pparent, 0, None).is_err()
            {
                for f in &full_pidls {
                    free_pidl(*f);
                }
                return DragOutOutcome::Cancelled;
            }
            let parent_len = ILGetSize(Some(pparent)) as usize;
            // 子项相对 PIDL = 绝对 PIDL 去掉父目录前缀。
            let child_ptrs: Vec<*const ITEMIDLIST> = full_pidls
                .iter()
                .map(|&p| (p as *const u8).add(parent_len) as *const ITEMIDLIST)
                .collect();
            let obj = SHCreateDataObject::<Option<&IDataObject>, IDataObject>(
                Some(pparent as *const ITEMIDLIST),
                Some(&child_ptrs),
                None,
            );
            free_pidl(pparent);
            match obj {
                Ok(d) => d,
                Err(_) => {
                    for f in &full_pidls {
                        free_pidl(*f);
                    }
                    return DragOutOutcome::Cancelled;
                }
            }
        } else {
            // 跨多目录：回退到绝对 PIDL 形式。
            let apidl: Vec<*const ITEMIDLIST> =
                full_pidls.iter().map(|p| *p as *const ITEMIDLIST).collect();
            match SHCreateDataObject::<Option<&IDataObject>, IDataObject>(None, Some(&apidl), None)
            {
                Ok(d) => d,
                Err(_) => {
                    for f in &full_pidls {
                        free_pidl(*f);
                    }
                    return DragOutOutcome::Cancelled;
                }
            }
        };

        for f in &full_pidls {
            free_pidl(*f);
        }

        // 「Preferred DropEffect」= COPY|MOVE：由放置目标按盘符/修饰键决定。
        let fmt_name = to_wide("Preferred DropEffect");
        let fmt_id = RegisterClipboardFormatW(PCWSTR(fmt_name.as_ptr()));
        let fmt = FORMATETC {
            cfFormat: fmt_id as u16,
            dwAspect: DVASPECT_CONTENT.0,
            lindex: -1,
            tymed: TYMED_HGLOBAL.0 as u32,
            ptd: std::ptr::null_mut(),
        };
        let effects: u32 = DROPEFFECT_COPY.0 | DROPEFFECT_MOVE.0;
        if let Ok(hg) = GlobalAlloc(GMEM_MOVEABLE, std::mem::size_of::<u32>()) {
            let ptr = GlobalLock(hg);
            if !ptr.is_null() {
                std::ptr::copy_nonoverlapping(&effects as *const u32, ptr as *mut u32, 1);
                let _ = GlobalUnlock(hg);
                let medium = STGMEDIUM {
                    tymed: TYMED_HGLOBAL.0 as u32,
                    u: windows::Win32::System::Com::STGMEDIUM_0 { hGlobal: hg },
                    pUnkForRelease: std::mem::ManuallyDrop::new(None),
                };
                let _ = data_obj.SetData(&fmt, &medium, true);
            } else {
                let _ = windows::Win32::Foundation::GlobalFree(Some(hg));
            }
        }

        let source: IDropSource = DropSource { started: Instant::now() }.into();
        let mut out_effect = DROPEFFECT(0);
        let hr = DoDragDrop(
            &data_obj,
            &source,
            DROPEFFECT(DROPEFFECT_COPY.0 | DROPEFFECT_MOVE.0 | DROPEFFECT_LINK.0),
            &mut out_effect,
        );
        if hr.is_err() {
            return DragOutOutcome::Cancelled;
        }
        if out_effect & DROPEFFECT_MOVE == DROPEFFECT_MOVE {
            DragOutOutcome::Dropped(DropEffect::Move)
        } else if out_effect & DROPEFFECT_COPY == DROPEFFECT_COPY {
            DragOutOutcome::Dropped(DropEffect::Copy)
        } else if out_effect & DROPEFFECT_LINK == DROPEFFECT_LINK {
            DragOutOutcome::Dropped(DropEffect::Link)
        } else {
            DragOutOutcome::Dropped(DropEffect::None)
        }
    }
}
