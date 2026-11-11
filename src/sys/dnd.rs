//! 任务 2：系统级拖放（`src/sys/dnd`）。
//!
//! - 拖出（Rove → 桌面/外部）：
//!   1. 在主 UI 线程同步运行（严禁 spawn 子线程，DoDragDrop 依赖当前线程的鼠标输入捕获与消息循环）。
//!   2. 必须初始化 OleInitialize（STA 模式）。
//!   3. 使用 SHCreateDataObject（None, full_pidls）构建标准 Shell 数据对象。
//!   4. 显式写入 CF_HDROP 数据与 Preferred DropEffect，确保桌面及任意第三方程序均可识别。
//!
//! 严禁在此模块操作 Slint UI；仅做 Win32 交互并返回结果。

use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
#[cfg(windows)]
use std::sync::atomic::Ordering;

/// 拖出判定阈值：按下后位移超过该值（逻辑像素）才触发拖放。
pub const DRAG_THRESHOLD: f32 = 4.0;

/// 正在执行 `DoDragDrop` 的全局标志。
///
/// OLE 拖放期间系统仍会向窗口派发 `WM_NCHITTEST`；若窗口边缘此时返回
/// `HTLEFT/HTTOP...`，系统会用缩放光标覆盖 OLE 的拖放光标（表现为拖到
/// Rove 边缘时变成调整大小的箭头）。`WM_NCHITTEST` 钩子据此强制 `HTCLIENT`。
#[allow(dead_code)]
pub static IS_DRAGGING: AtomicBool = AtomicBool::new(false);

/// 本次拖放是否由「松开左键」结束（而不是 Esc 取消）。
///
/// 拖到没有注册 `IDropTarget` 的区域（例如 rove 自己的面包屑）时，`DoDragDrop`
/// 返回 `DRAGDROP_S_CANCEL`，和用户按 Esc 完全一样，只能靠 `QueryContinueDrag`
/// 里记下这个标志来区分「用户松手了」。
#[allow(dead_code)]
pub static DRAG_RELEASED: AtomicBool = AtomicBool::new(false);

/// 最近一次拖放是否是「松手」结束的。
#[cfg(windows)]
#[allow(dead_code)]
pub fn drag_released() -> bool {
    DRAG_RELEASED.load(Ordering::SeqCst)
}

#[cfg(not(windows))]
#[allow(dead_code)]
pub fn drag_released() -> bool {
    false
}

/// 拖出结果。
#[allow(dead_code)]
pub enum DragOutOutcome {
    /// 目标接受了文件。
    Dropped(DropEffect),
    /// 用户取消（Esc / 释放过快）。
    Cancelled,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[allow(dead_code)]
pub enum DropEffect {
    Copy,
    Move,
    Link,
    None,
}

/// 发起一次拖出循环。调用方处于左键按下的手势中时调用；
/// 本函数在主 UI 线程阻塞至拖放结束（内部为 Windows 模态消息循环，期间正常刷新光标并派发绘制）。
#[cfg(windows)]
pub fn begin_drag_out<F: Fn() + 'static>(
    paths: &[PathBuf],
    icon_px: u32,
    on_feedback: F,
) -> DragOutOutcome {
    if paths.is_empty() {
        return DragOutOutcome::Cancelled;
    }
    DRAG_RELEASED.store(false, Ordering::SeqCst);
    IS_DRAGGING.store(true, Ordering::SeqCst);
    let res = drag_out_impl(paths, icon_px, Box::new(on_feedback));
    IS_DRAGGING.store(false, Ordering::SeqCst);
    res
}

#[cfg(not(windows))]
pub fn begin_drag_out<F: Fn() + 'static>(
    _paths: &[PathBuf],
    _icon_px: u32,
    _on_feedback: F,
) -> DragOutOutcome {
    DragOutOutcome::Cancelled
}

#[cfg(windows)]
fn drag_out_impl(paths: &[PathBuf], icon_px: u32, on_feedback: Box<dyn Fn()>) -> DragOutOutcome {
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{
        BOOL, DRAGDROP_S_CANCEL, DRAGDROP_S_DROP, DRAGDROP_S_USEDEFAULTCURSORS,
    };
    use windows::Win32::System::Com::{
        CoTaskMemFree, DVASPECT_CONTENT, FORMATETC, IDataObject, STGMEDIUM, TYMED_HGLOBAL,
    };
    use windows::Win32::System::Memory::{
        GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE, GMEM_ZEROINIT,
    };
    use windows::Win32::System::Ole::{
        DoDragDrop, IDropSource, IDropSource_Impl, OleInitialize, DROPEFFECT, DROPEFFECT_COPY,
        DROPEFFECT_LINK, DROPEFFECT_MOVE,
    };
    use windows::Win32::System::SystemServices::{MK_LBUTTON, MODIFIERKEYS_FLAGS};
    use windows::Win32::UI::Shell::Common::ITEMIDLIST;
    use windows::Win32::UI::Shell::{DROPFILES, ILFindChild, SHCreateDataObject, SHParseDisplayName};
    // 拖拽图像：让 Shell 创建并跟随光标的半透明图标浮层。
    use windows::Win32::Foundation::{COLORREF, POINT, SIZE};
    use windows::Win32::Graphics::Gdi::{
        CreateDIBSection, HBITMAP, BITMAPINFO, BITMAPINFOHEADER, DIB_RGB_COLORS,
    };
    use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance};
    use windows::Win32::UI::Shell::{
        CLSID_DragDropHelper, IDragSourceHelper, IDropTargetHelper, SHDRAGIMAGE,
    };
    use windows::Win32::UI::WindowsAndMessaging::{GetCursorPos, GetForegroundWindow};
    use windows_core::implement;

    fn to_wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    // ---- 自定义 IDropSource：左键释放 → DROP；Esc → CANCEL；光标交给 Windows 原生绘制 ----
    #[implement(IDropSource)]
    struct DropSource {
        on_feedback: Box<dyn Fn()>,
        helper: Option<IDropTargetHelper>,
    }

    impl IDropSource_Impl for DropSource_Impl {
        fn QueryContinueDrag(
            &self,
            escape_pressed: BOOL,
            key_state: MODIFIERKEYS_FLAGS,
        ) -> windows::core::HRESULT {
            if escape_pressed.as_bool() {
                return DRAGDROP_S_CANCEL;
            }
            // 左键已松开 → 触发放置
            if key_state & MK_LBUTTON == MODIFIERKEYS_FLAGS(0) {
                // 记下来：拖到 rove 自己的面包屑这类「无 IDropTarget」的区域，
                // DoDragDrop 返回值与 Esc 取消无法区分，只能靠这个标志判断。
                DRAG_RELEASED.store(true, Ordering::SeqCst);
                return DRAGDROP_S_DROP;
            }
            windows::core::HRESULT(0) // S_OK: 继续拖拽
        }

        fn GiveFeedback(&self, effect: DROPEFFECT) -> windows::core::HRESULT {
            (self.on_feedback)();
            // DoDragDrop 是主线程模态循环，Slint 的 Timer/事件循环此时不刷新；
            // GiveFeedback 是期间唯一稳定触发的回调，在这里让 Shell 的图像浮层跟随光标。
            if let Some(helper) = &self.helper {
                unsafe {
                    let mut pt = POINT::default();
                    if GetCursorPos(&mut pt).is_ok() {
                        let _ = helper.DragOver(&pt, effect);
                    }
                }
            }
            DRAGDROP_S_USEDEFAULTCURSORS
        }
    }

    fn free_pidl(p: *mut ITEMIDLIST) {
        if !p.is_null() {
            unsafe { CoTaskMemFree(Some(p as *const _)) };
        }
    }

    // 构造标准 CF_HDROP 内存块（适用于桌面、资源管理器及所有普通 Windows 软件）
    fn create_hdrop_medium(paths: &[PathBuf]) -> Option<STGMEDIUM> {
        let mut wide_data: Vec<u16> = Vec::new();
        for p in paths {
            let s = p.to_string_lossy();
            wide_data.extend(s.encode_utf16());
            wide_data.push(0);
        }
        wide_data.push(0); // 双空终止符结尾 \0\0

        let dropfiles_size = std::mem::size_of::<DROPFILES>();
        let total_bytes = dropfiles_size + wide_data.len() * 2;

        unsafe {
            let hg = GlobalAlloc(GMEM_MOVEABLE | GMEM_ZEROINIT, total_bytes).ok()?;
            let ptr = GlobalLock(hg);
            if ptr.is_null() {
                let _ = windows::Win32::Foundation::GlobalFree(Some(hg));
                return None;
            }

            let df = ptr as *mut DROPFILES;
            (*df).pFiles = dropfiles_size as u32;
            (*df).fWide = BOOL(1);

            let dest = (ptr as *mut u8).add(dropfiles_size) as *mut u16;
            std::ptr::copy_nonoverlapping(wide_data.as_ptr(), dest, wide_data.len());

            let _ = GlobalUnlock(hg);

            Some(STGMEDIUM {
                tymed: TYMED_HGLOBAL.0 as u32,
                u: windows::Win32::System::Com::STGMEDIUM_0 { hGlobal: hg },
                pUnkForRelease: std::mem::ManuallyDrop::new(None),
            })
        }
    }

    /// 取与文件列表同一管线、同一物理像素尺寸的图标，转成 32bpp top-down DIB。
    /// 必须用取图返回的**真实 (w,h)** 建位图：SIIGBF_RESIZETOFIT 可能返回小于
    /// 请求尺寸的位图，按请求尺寸写会裁掉图标一角。
    fn make_drag_bitmap(path: &std::path::Path, icon_px: u32) -> Option<(HBITMAP, SIZE)> {
        let (w, h, rgba) = match crate::ops::thumbs::get_thumbnail_timed(path, icon_px) {
            Ok((w, h, rgba, _)) if w > 0 && h > 0 => (w, h, rgba),
            // 大尺寸请求被缩略图管线拒绝时退回系统图标（较小但完整）。
            _ => crate::ops::thumbs::get_system_icon(path)?,
        };
        let pixels = (w as usize).checked_mul(h as usize)?;
        if pixels == 0 || rgba.len() < pixels * 4 {
            return None;
        }
        unsafe {
            let mut bmi: BITMAPINFO = std::mem::zeroed();
            bmi.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
            bmi.bmiHeader.biWidth = w as i32;
            bmi.bmiHeader.biHeight = -(h as i32); // top-down，行序与 RGBA 一致
            bmi.bmiHeader.biPlanes = 1;
            bmi.bmiHeader.biBitCount = 32;
            let mut bits: *mut std::ffi::c_void = std::ptr::null_mut();
            let hbmp = CreateDIBSection(None, &bmi, DIB_RGB_COLORS, &mut bits, None, 0).ok()?;
            let dst = std::slice::from_raw_parts_mut(bits as *mut u8, pixels * 4);
            for i in 0..pixels {
                let r = rgba[i * 4] as u32;
                let g = rgba[i * 4 + 1] as u32;
                let b = rgba[i * 4 + 2] as u32;
                let a = rgba[i * 4 + 3] as u32;
                // 32bpp 拖拽图像按 BGRA、预乘 alpha 写入
                dst[i * 4] = ((b * a) / 255) as u8;
                dst[i * 4 + 1] = ((g * a) / 255) as u8;
                dst[i * 4 + 2] = ((r * a) / 255) as u8;
                dst[i * 4 + 3] = a as u8;
            }
            Some((hbmp, SIZE { cx: w as i32, cy: h as i32 }))
        }
    }

    unsafe {
        // 1. 确保当前主线程完全初始化 OLE 拖放子系统
        let _ = OleInitialize(None);

        // 2. 将文件路径转换为绝对 PIDL（清理路径尾部斜杠，防止文件夹解析失败）
        let mut full_pidls: Vec<*mut ITEMIDLIST> = Vec::with_capacity(paths.len());
        for p in paths {
            let s = p.to_string_lossy();
            let trimmed = s.trim_end_matches(['\\', '/']);
            let path_clean = if trimmed.len() == 2 && trimmed.ends_with(':') {
                format!("{trimmed}\\")
            } else {
                trimmed.to_string()
            };
            let wide = to_wide(&path_clean);
            let mut pidl: *mut ITEMIDLIST = std::ptr::null_mut();
            if SHParseDisplayName(PCWSTR(wide.as_ptr()), None, &mut pidl, 0, None).is_err() {
                for f in &full_pidls {
                    free_pidl(*f);
                }
                return DragOutOutcome::Cancelled;
            }
            full_pidls.push(pidl);
        }

        // 3. 构建 Shell 官方数据对象（同目录下使用 pparent + ILFindChild，确保文件夹和文件均可被正确识别）
        let parents: std::collections::HashSet<PathBuf> =
            paths.iter().filter_map(|p| p.parent().map(|d| d.to_path_buf())).collect();

        let data_obj: IDataObject = if parents.len() == 1 {
            let parent_s = paths[0].parent().unwrap().to_string_lossy();
            let parent_clean = parent_s.trim_end_matches(['\\', '/']);
            let parent_clean = if parent_clean.len() == 2 && parent_clean.ends_with(':') {
                format!("{parent_clean}\\")
            } else {
                parent_clean.to_string()
            };
            let parent_wide = to_wide(&parent_clean);
            let mut pparent: *mut ITEMIDLIST = std::ptr::null_mut();

            if SHParseDisplayName(PCWSTR(parent_wide.as_ptr()), None, &mut pparent, 0, None).is_ok() {
                let child_ptrs: Vec<*const ITEMIDLIST> = full_pidls
                    .iter()
                    .map(|&p| {
                        let rel = ILFindChild(pparent, p);
                        if rel.is_null() { p as *const ITEMIDLIST } else { rel as *const ITEMIDLIST }
                    })
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
                        let apidls: Vec<*const ITEMIDLIST> = full_pidls.iter().map(|p| *p as *const ITEMIDLIST).collect();
                        SHCreateDataObject(None, Some(&apidls), None).unwrap()
                    }
                }
            } else {
                let apidls: Vec<*const ITEMIDLIST> = full_pidls.iter().map(|p| *p as *const ITEMIDLIST).collect();
                SHCreateDataObject(None, Some(&apidls), None).unwrap()
            }
        } else {
            let apidls: Vec<*const ITEMIDLIST> = full_pidls.iter().map(|p| *p as *const ITEMIDLIST).collect();
            SHCreateDataObject(None, Some(&apidls), None).unwrap()
        };

        // 4. 显式注入 CF_HDROP 数据（保证跨进程通用兼容性）
        if let Some(medium) = create_hdrop_medium(paths) {
            let fmt_hdrop = FORMATETC {
                cfFormat: 15, // CF_HDROP = 15
                dwAspect: DVASPECT_CONTENT.0,
                lindex: -1,
                tymed: TYMED_HGLOBAL.0 as u32,
                ptd: std::ptr::null_mut(),
            };
            let _ = data_obj.SetData(&fmt_hdrop, &medium, true);
        }

        // 5. 拖拽图像：把图标交给 Shell 的 DragDropHelper，由系统创建并维持
        //    跟随光标的半透明图标浮层（Slint 浮层在模态循环里不会刷新）。
        let mut drag_bitmap: Option<HBITMAP> = None;
        if let Some((hbmp, size)) = make_drag_bitmap(&paths[0], icon_px) {
            let src: windows::core::Result<IDragSourceHelper> =
                CoCreateInstance(&CLSID_DragDropHelper, None, CLSCTX_INPROC_SERVER);
            if let Ok(src) = src {
                let image = SHDRAGIMAGE {
                    sizeDragImage: size,
                    ptOffset: POINT { x: size.cx / 2, y: size.cy / 2 },
                    hbmpDragImage: hbmp,
                    crColorKey: COLORREF(0xFFFFFFFF), // CLR_NONE：使用 32bpp alpha
                };
                let _ = src.InitializeFromBitmap(&image, &data_obj);
            }
            drag_bitmap = Some(hbmp);
        }

        // 目标侧 helper 只负责「显示/移动/销毁」图像窗口，不注册 IDropTarget：
        // 外部拖入仍由 winit 处理，两者互不冲突（与 zsdesk 的 SHDoDragDrop 同机制）。
        let target_helper: windows::core::Result<IDropTargetHelper> =
            CoCreateInstance(&CLSID_DragDropHelper, None, CLSCTX_INPROC_SERVER);
        let drag_helper = target_helper.ok();
        if let Some(h) = &drag_helper {
            let mut pt = POINT::default();
            let _ = GetCursorPos(&mut pt);
            let effects = DROPEFFECT(DROPEFFECT_COPY.0 | DROPEFFECT_MOVE.0 | DROPEFFECT_LINK.0);
            let _ = h.DragEnter(GetForegroundWindow(), &data_obj, &pt, effects);
        }

        // 6. 发起原生 OLE 拖放
        let source: IDropSource = DropSource { on_feedback, helper: drag_helper.clone() }.into();
        let mut out_effect = DROPEFFECT(0);
        let hr = DoDragDrop(
            &data_obj,
            &source,
            DROPEFFECT(DROPEFFECT_COPY.0 | DROPEFFECT_MOVE.0 | DROPEFFECT_LINK.0),
            &mut out_effect,
        );

        // 7. 收尾：先让 Shell 撤掉图像窗口，再释放我们创建的位图。
        if let Some(h) = &drag_helper {
            let _ = h.DragLeave();
        }
        if let Some(hbmp) = drag_bitmap {
            let _ = windows_sys::Win32::Graphics::Gdi::DeleteObject(hbmp.0);
        }

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
