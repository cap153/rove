//! 缩略图管线（Phase 5，6.2）：Win32 `IShellItemImageFactory`。
//!
//! - 图片 / 视频文件与文件夹统一走系统合成（文件夹返回封面卡片）；
//! - 输出 32bpp BGRA 位图 → RGBA `SharedPixelBuffer`，供 Slint 直接显示；
//! - 调用方需在 worker 线程执行（本模块负责该线程的 COM 初始化）。

use std::path::Path;

/// 缩略图边长（与列表图标区域一致）。
pub const THUMB_SIZE: u32 = 44;

/// 是否值得为该路径取缩略图：
#[allow(dead_code)] // Linux 宿主测试构建不引用（Windows 侧使用）
/// - 图片 / 视频扩展名；
/// - 文件夹（系统会为含媒体的文件夹合成封面卡片，Explorer 同款）。
pub fn is_thumbnail_candidate(path: &Path) -> bool {
    if path.is_dir() {
        // 系统目录封面合成开销大且无意义。
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        return !matches!(
            name.as_str(),
            "windows" | "program files" | "program files (x86)" | "$recycle.bin"
                | "system volume information" | "appdata"
        );
    }
    let ext = path
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    matches!(
        ext.as_str(),
        "jpg" | "jpeg" | "png" | "gif" | "webp" | "bmp" | "tif" | "tiff" | "mp4" | "mkv"
            | "avi" | "mov" | "webm" | "wmv" | "m4v"
    )
}

/// 是否为「慢格式」：解码开销明显大于普通图片（视频海报 / 这类媒体）。
///
/// 用于主池调度隔离：慢格式最多只占 `lanes-1` 条 worker，永远给普通图片
/// 留 1 条空闲线程，避免滚到「图片+视频混排」文件夹时图片被视频卡死
/// （抄 Pulse 的 `slow_cap = lanes-1` 思路）。纯视频文件夹本身解码就慢，
/// 调度救不了（那是解码速度的物理上限），此隔离主要救混排场景。
pub fn is_slow_thumbnail(path: &Path) -> bool {
    if path.is_dir() {
        return false; // 文件夹封面不按“慢”处理，保持原优先逻辑
    }
    let ext = path
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    matches!(
        ext.as_str(),
        "mp4" | "mkv" | "avi" | "mov" | "webm" | "wmv" | "m4v"
    )
}

/// 每线程一次的 COM 初始化（CoInitializeEx 幂等：已初始化返回 S_FALSE 也算成功）。
fn ensure_com() -> Result<(), String> {
    use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
    unsafe {
        // S_OK(0)/S_FALSE(1) 都算可用；RPC_E_CHANGED_MODE 时改用多线程模型重试。
        let hr = CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok();
        let _ = hr;
    }
    Ok(())
}

/// 取缩略图，并额外返回「纯解码耗时」毫秒数。
///
/// 纯解码耗时 **不含** 退避重试的 sleep：只有这样才能用 `>= 30ms` 准确判断
/// “这张图是否值得落盘”。否则启动期 E_PENDING 的 250ms/500ms 退避会污染
/// 计时，让所有廉价格式（本地 jpg/png/小图标）也被误判为慢图而写满磁盘。
pub fn get_thumbnail_timed(path: &Path, size: u32) -> Result<(u32, u32, Vec<u8>, u128), String> {
    // 瞬时错误限次重试：启动初期 Shell 缩略图缓存尚未就绪，GetImage 会返回
    // 0x8000000A（E_PENDING，“完成该操作所需的数据还不可使用”）。这是可自愈的，
    // 重试几次即可成功。退避 250ms 起步、每次加倍（250/500/1000/2000/4000ms）。
    // 注意：重试必须放在这里（thumbs 模块）而非调用方，否则驱动器这类「永远
    // 拿不到 264px 缩略图」的条目会在调用方空转十几秒才落定，期间一直空白。
    const MAX_ATTEMPTS: u32 = 6;
    let mut attempt: u32 = 0;
    let mut pure_decode_ms: u128 = 0;
    loop {
        attempt += 1;
        let t0 = std::time::Instant::now();
        let res = get_thumbnail_impl(path, size);
        pure_decode_ms += t0.elapsed().as_millis();
        match res {
            Ok(r) => {
                crate::logv!(
                    "[thumb] ok {} req_size={} attempt={} pure_ms={} -> {}x{}",
                    path.display(),
                    size,
                    attempt,
                    pure_decode_ms,
                    r.0,
                    r.1
                );
                return Ok((r.0, r.1, r.2, pure_decode_ms));
            }
            Err(e) => {
                let transient = e.contains("0x8000000A")
                    || e.contains("E_PENDING")
                    || e.contains("数据还不可使用");
                if transient && attempt < MAX_ATTEMPTS {
                    let wait = 250u64.saturating_mul(1 << (attempt - 1).min(5));
                    crate::logv!(
                        "[thumb] transient E_PENDING (attempt {}/{}) {} size={}: {} -> retry in {}ms",
                        attempt,
                        MAX_ATTEMPTS,
                        path.display(),
                        size,
                        e,
                        wait
                    );
                    std::thread::sleep(std::time::Duration::from_millis(wait));
                    continue;
                }
                crate::logv!(
                    "[thumb] impl FAILED {} req_size={} attempt={}: {}",
                    path.display(),
                    size,
                    attempt,
                    e
                );
                let (icon_res, icon_ms) = get_system_icon_timed(path);
                pure_decode_ms += icon_ms;
                match icon_res {
                    Some(r) => {
                        // 核心修复：如果降级返回的图标尺寸远小于请求尺寸
                        // （例如请求 264 返回 32），强行塞进大容器会因透明/缩放产生
                        // 不可见的白块。直接返回 Err，让上层走彩色方块（fallback）。
                        if r.0 * 2 < size || r.1 * 2 < size {
                            crate::logv!(
                                "[thumb] fallback system icon too small ({}x{}) for req_size={}, rejecting -> UI colored block",
                                r.0, r.1, size
                            );
                            return Err(e);
                        }
                        crate::logv!(
                            "[thumb] fallback system icon {} req_size={} -> {}x{}",
                            path.display(),
                            size,
                            r.0,
                            r.1
                        );
                        return Ok((r.0, r.1, r.2, pure_decode_ms));
                    }
                    None => {
                        crate::logv!("[thumb] fallback system icon FAILED {}", path.display());
                        return Err(e);
                    }
                }
            }
        }
    }
}

fn get_thumbnail_impl(path: &Path, size: u32) -> Result<(u32, u32, Vec<u8>), String> {
    ensure_com()?;
    use windows::core::{PCWSTR, Interface};
    use windows::Win32::UI::Shell::{
        SHCreateItemFromParsingName, IShellItemImageFactory, SIIGBF_RESIZETOFIT,
    };
    use windows::Win32::Graphics::Gdi::{
        DeleteObject, GetDC, GetDIBits, ReleaseDC, BITMAPINFO, BITMAPINFOHEADER, DIB_RGB_COLORS,
    };

    let wide: Vec<u16> = path
        .as_os_str()
        .to_string_lossy()
        .encode_utf16()
        .chain([0])
        .collect();
    unsafe {
        let item: windows::Win32::UI::Shell::IShellItem =
            SHCreateItemFromParsingName(PCWSTR(wide.as_ptr()), None)
                .map_err(|e| format!("SHCreateItemFromParsingName: {e}"))?;
        let factory: IShellItemImageFactory = item
            .cast()
            .map_err(|e| format!("cast to ImageFactory: {e}"))?;

        let size_h = windows::Win32::Foundation::SIZE {
            cx: size as i32,
            cy: size as i32,
        };
        let hbitmap = factory
            .GetImage(size_h, SIIGBF_RESIZETOFIT)
            .map_err(|e| format!("GetImage: {e}"))?;

        // 查询位图实际尺寸（SIIGBF 可能返回不同于请求的尺寸）。
        let mut bmp = windows::Win32::Graphics::Gdi::BITMAP::default();
        let got = windows::Win32::Graphics::Gdi::GetObjectW(
            hbitmap.into(),
            std::mem::size_of::<windows::Win32::Graphics::Gdi::BITMAP>() as i32,
            Some(&mut bmp as *mut _ as *mut _),
        );
        if got == 0 || bmp.bmWidth <= 0 || bmp.bmHeight <= 0 {
            let _ = DeleteObject(hbitmap.into());
            return Err("GetObjectW failed".into());
        }
        let (bw, bh) = (bmp.bmWidth as u32, bmp.bmHeight as u32);

        // 提取 32bpp 像素（top-down）。
        let hdc = GetDC(None);
        let mut bmi = BITMAPINFO::default();
        bmi.bmiHeader = BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: bw as i32,
            biHeight: -(bh as i32), // top-down
            biPlanes: 1,
            biBitCount: 32,
            biCompression: 0, // BI_RGB
            ..Default::default()
        };
        let mut buf = vec![0u8; (bw * bh * 4) as usize];
        let lines = GetDIBits(
            hdc,
            hbitmap,
            0,
            bh,
            Some(buf.as_mut_ptr().cast()),
            &mut bmi,
            DIB_RGB_COLORS,
        );
        ReleaseDC(None, hdc);
        let _ = DeleteObject(hbitmap.into());
        if lines == 0 {
            return Err("GetDIBits failed".into());
        }
        // BGRA → RGBA。
        for px in buf.chunks_exact_mut(4) {
            px.swap(0, 2);
        }
        drop(item);
        Ok((bw, bh, buf))
    }
}

/// 系统图标兜底：`SHGetFileInfoW`（不打开文件，受保护路径也能拿到图标）。
/// 返回 (宽, 高, RGBA)。尺寸为系统大图标 32px，显示端按需拉伸。
/// 系统图标兜底（不计时版本，供不需要落盘判定的调用方使用）。
#[allow(dead_code)]
pub fn get_system_icon(path: &Path) -> Option<(u32, u32, Vec<u8>)> {
    get_system_icon_timed(path).0
}

/// 系统图标兜底，并返回「纯取图耗时」毫秒数（不含退避 sleep）。
///
/// 计时不含 retry 睡眠：调用方用它判断是否值得落盘，退避睡眠属于「环境慢」
/// 而非「这张图解码慢」，不应被算入。
#[cfg(windows)]
fn get_system_icon_timed(path: &Path) -> (Option<(u32, u32, Vec<u8>)>, u128) {
    use windows::core::PCWSTR;
    use windows::Win32::Graphics::Gdi::{
        DeleteObject, GetDC, GetDIBits, GetObjectW, ReleaseDC, BITMAP, BITMAPINFO,
        BITMAPINFOHEADER, DIB_RGB_COLORS,
    };
    use windows::Win32::UI::Shell::{
        SHGetFileInfoW, SHFILEINFOW, SHGFI_ICON, SHGFI_LARGEICON, SHGFI_USEFILEATTRIBUTES,
    };
    use windows::Win32::UI::WindowsAndMessaging::{DestroyIcon, GetIconInfo, ICONINFO};

    let mut pure_ms: u128 = 0;
    if ensure_com().is_err() {
        return (None, pure_ms);
    }    let wide: Vec<u16> = path
        .as_os_str()
        .to_string_lossy()
        .encode_utf16()
        .chain([0])
        .collect();
    let size = 32u32;
    // 目录 / 盘根：不要传 FILE_ATTRIBUTE_NORMAL（那是“普通文件”），
    // 用真实属性 + SHGFI_USEFILEATTRIBUTES，才能拿到文件夹/驱动器图标；
    // 并且 SHGetFileInfoW 在 Shell 初始化早期也可能瞬时失败，做限次重试。
    let is_dir = std::fs::metadata(path).map(|m| m.is_dir()).unwrap_or(false);
    let file_attrs = if is_dir {
        windows::Win32::Storage::FileSystem::FILE_ATTRIBUTE_DIRECTORY
    } else {
        windows::Win32::Storage::FileSystem::FILE_ATTRIBUTE_NORMAL
    };
    unsafe {
        let mut info = SHFILEINFOW::default();
        let mut ret = 0;
        // 最多 5 次，250ms 起步指数退避（覆盖 Shell 就绪前的窗口期）。
        for attempt in 0..5u32 {
            let t_call = std::time::Instant::now();
            ret = SHGetFileInfoW(
                PCWSTR(wide.as_ptr()),
                file_attrs,
                Some(&mut info as *mut _ as *mut _),
                std::mem::size_of::<SHFILEINFOW>() as u32,
                SHGFI_ICON | SHGFI_LARGEICON | SHGFI_USEFILEATTRIBUTES,
            );
            pure_ms += t_call.elapsed().as_millis();
            if ret != 0 && !info.hIcon.is_invalid() {
                if attempt > 0 {
                    crate::logv!(
                        "[sysicon] retry OK on attempt {} {}",
                        attempt + 1,
                        path.display()
                    );
                }
                break;
            }
            if attempt < 4 {
                crate::logv!(
                    "[sysicon] attempt {} failed for {} -> retry",
                    attempt + 1,
                    path.display()
                );
                std::thread::sleep(std::time::Duration::from_millis(250 << attempt));
            }
        }
        if ret == 0 || info.hIcon.is_invalid() {
            return (None, pure_ms);
        }
        let hicon = info.hIcon;
        let t_tail = std::time::Instant::now();
        let tail_ms = || t_tail.elapsed().as_millis();

        let mut ii = ICONINFO::default();
        if GetIconInfo(hicon, &mut ii).is_err() {
            let _ = DestroyIcon(hicon);
            return (None, pure_ms + tail_ms());
        }
        let hbm = ii.hbmColor;
        if hbm.is_invalid() {
            let _ = DestroyIcon(hicon);
            let _ = DeleteObject(ii.hbmMask.into());
            return (None, pure_ms + tail_ms());
        }

        let hdc = GetDC(None);
        // SHGFI_LARGEICON 返回的 HICON 颜色位图在高 DPI / 带大图标的 exe 上
        // 未必是 32×32。硬按 32 调 GetDIBits 只会取到左上角一角，正是拖拽图
        // 「缺一块」的来源；先查真实尺寸再按它读取。
        let mut bmp = BITMAP::default();
        let got = GetObjectW(
            hbm.into(),
            std::mem::size_of::<BITMAP>() as i32,
            Some(&mut bmp as *mut _ as *mut _),
        );
        let (bw, bh) = if got > 0 && bmp.bmWidth > 0 && bmp.bmHeight > 0 {
            (bmp.bmWidth as u32, bmp.bmHeight as u32)
        } else {
            (size, size)
        };
        let mut bmi = BITMAPINFO::default();
        bmi.bmiHeader = BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: bw as i32,
            biHeight: -(bh as i32),
            biPlanes: 1,
            biBitCount: 32,
            biCompression: 0,
            ..Default::default()
        };
        let mut buf = vec![0u8; (bw * bh * 4) as usize];
        let lines = GetDIBits(
            hdc,
            hbm,
            0,
            bh,
            Some(buf.as_mut_ptr().cast()),
            &mut bmi,
            DIB_RGB_COLORS,
        );
        ReleaseDC(None, hdc);
        let _ = DeleteObject(hbm.into());
        let _ = DeleteObject(ii.hbmMask.into());
        let _ = DestroyIcon(hicon);
        if lines == 0 {
            return (None, pure_ms + tail_ms());
        }
        // BGRA→RGBA + 反预乘（HICON 位图为预乘 alpha）。
        for px in buf.chunks_exact_mut(4) {
            let a = px[3] as u32;
            if a > 0 && a < 255 {
                px[0] = ((px[0] as u32 * 255) / a).min(255) as u8;
                px[1] = ((px[1] as u32 * 255) / a).min(255) as u8;
                px[2] = ((px[2] as u32 * 255) / a).min(255) as u8;
            }
            px.swap(0, 2);
        }
        (
            Some((bw, bh, buf)),
            pure_ms + tail_ms(),
        )
    }
}

// ---------------------------------------------------------------------------
// 缩略图磁盘缓存编解码（WIC）
//
// 对标 Pulse：不透明图用 JPEG（quality 0.9），带 alpha 用 PNG。
// 裸 RGBA 落盘（px²×4，132px 就 69 KB）比 JPEG 大约 20-30 倍，是缓存膨胀的主因。
// WIC 是系统自带组件，无需新增第三方依赖。
// ---------------------------------------------------------------------------

/// JPEG 质量参数名写入编码器 options（对标 Pulse 的 `ImageQuality = 0.9`）。
#[cfg(windows)]
unsafe fn write_jpeg_quality(
    options: &windows::Win32::System::Com::StructuredStorage::IPropertyBag2,
    quality: f32,
) {
    use windows::Win32::System::Com::StructuredStorage::PROPBAG2;
    use windows::Win32::System::Variant::{VARIANT, VT_R4};

    let name: Vec<u16> = "ImageQuality\0".encode_utf16().collect();
    let mut prop = PROPBAG2::default();
    prop.pstrName = windows::core::PWSTR(name.as_ptr() as *mut u16);

    let mut var = VARIANT::default();
    // VARIANT_0::Anonymous 是 ManuallyDrop，需显式解引用才能原地赋值。
    let inner = &mut *var.Anonymous.Anonymous;
    inner.vt = VT_R4;
    inner.Anonymous.fltVal = quality;

    let _ = options.Write(1, &prop, &var);
}

/// 把 RGBA 像素编码写入文件（JPEG 或 PNG，由是否含 alpha 决定）。
/// 若 `max_size` 小于原图边长，先用 WIC 的 BitmapScaler 等比缩小再编码。
#[cfg(windows)]
pub fn encode_wic_to_file(
    path: &Path,
    w: u32,
    h: u32,
    rgba: &[u8],
    max_size: Option<u32>,
) -> Result<(), String> {
    use windows::core::{HSTRING, Interface};
    use windows::Win32::Graphics::Imaging::{
        CLSID_WICImagingFactory, GUID_ContainerFormatJpeg, GUID_ContainerFormatPng,
        GUID_WICPixelFormat24bppBGR, GUID_WICPixelFormat32bppBGRA, IWICBitmap,
        IWICBitmapFrameEncode, IWICBitmapSource, IWICImagingFactory, IWICPalette,
        WICBitmapDitherTypeNone, WICBitmapEncoderNoCache, WICBitmapInterpolationModeFant,
        WICBitmapPaletteTypeCustom,
    };
    use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_INPROC_SERVER};

    if w == 0 || h == 0 || rgba.len() != (w as usize) * (h as usize) * 4 {
        return Err(format!("bad dims {w}x{h} bytes={}", rgba.len()));
    }
    ensure_com()?;

    unsafe {
        let factory: IWICImagingFactory =
            CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)
                .map_err(|e| format!("WIC factory: {e}"))?;

        // WIC 原生顺序是 BGRA；Slint 侧是 RGBA。
        let mut bgra = Vec::with_capacity(rgba.len());
        for px in rgba.chunks_exact(4) {
            bgra.extend_from_slice(&[px[2], px[1], px[0], px[3]]);
        }
        let bitmap: IWICBitmap = factory
            .CreateBitmapFromMemory(w, h, &GUID_WICPixelFormat32bppBGRA, w * 4, &bgra)
            .map_err(|e| format!("CreateBitmap: {e}"))?;

        // 智能缩放：超限时等比缩小。
        // 注意：IWICBitmapScaler 只实现 IWICBitmapSource（不是 IWICBitmap），
        // 所以统一用 IWICBitmapSource 作为转换器输入，不能 cast 成 IWICBitmap。
        let mut final_w = w;
        let mut final_h = h;
        let mut source: IWICBitmapSource = bitmap
            .cast()
            .map_err(|e| format!("BitmapToSource: {e}"))?;
        if let Some(max) = max_size {
            if w > max || h > max {
                let ratio = (max as f64) / (w.max(h) as f64);
                final_w = ((w as f64) * ratio).round().max(1.0) as u32;
                final_h = ((h as f64) * ratio).round().max(1.0) as u32;
                let scaler = factory
                    .CreateBitmapScaler()
                    .map_err(|e| format!("CreateBitmapScaler: {e}"))?;
                scaler
                    .Initialize(
                        &bitmap,
                        final_w,
                        final_h,
                        WICBitmapInterpolationModeFant,
                    )
                    .map_err(|e| format!("ScalerInit: {e}"))?;
                source = scaler
                    .cast()
                    .map_err(|e| format!("ScalerToSource: {e}"))?;
            }
        }

        let has_alpha = rgba.chunks_exact(4).any(|px| px[3] != 255);
        let container = if has_alpha {
            &GUID_ContainerFormatPng
        } else {
            &GUID_ContainerFormatJpeg
        };

        let stream = factory
            .CreateStream()
            .map_err(|e| format!("CreateStream: {e}"))?;
        // GENERIC_WRITE = 0x4000_0000
        stream
            .InitializeFromFilename(&HSTRING::from(path.as_os_str()), 0x4000_0000)
            .map_err(|e| format!("InitStream: {e}"))?;

        let encoder = factory
            .CreateEncoder(container, std::ptr::null())
            .map_err(|e| format!("CreateEncoder: {e}"))?;
        encoder
            .Initialize(&stream, WICBitmapEncoderNoCache)
            .map_err(|e| format!("InitEncoder: {e}"))?;

        let mut frame_opt: Option<IWICBitmapFrameEncode> = None;
        let mut options_opt = None;
        encoder
            .CreateNewFrame(&mut frame_opt, &mut options_opt)
            .map_err(|e| format!("NewFrame: {e}"))?;
        let frame = frame_opt.ok_or_else(|| "NewFrame returned none".to_string())?;

        if !has_alpha {
            if let Some(options) = options_opt.as_ref() {
                write_jpeg_quality(options, 0.9);
            }
        }

        frame
            .Initialize(options_opt.as_ref())
            .map_err(|e| format!("InitFrame: {e}"))?;
        frame
            .SetSize(final_w, final_h)
            .map_err(|e| format!("SetSize: {e}"))?;

        // JPEG 不支持 alpha，用 24bppBGR；PNG 保留 alpha，用 32bppBGRA。
        let mut target = if has_alpha {
            GUID_WICPixelFormat32bppBGRA
        } else {
            GUID_WICPixelFormat24bppBGR
        };
        frame
            .SetPixelFormat(&mut target)
            .map_err(|e| format!("SetPixelFormat: {e}"))?;

        // 让 WIC 自己做格式转换，避免手工处理 stride/padding。
        let converter = factory
            .CreateFormatConverter()
            .map_err(|e| format!("CreateConverter: {e}"))?;
        converter
            .Initialize(
                &source,
                &target,
                WICBitmapDitherTypeNone,
                None::<&IWICPalette>,
                0.0,
                WICBitmapPaletteTypeCustom,
            )
            .map_err(|e| format!("InitConverter: {e}"))?;

        frame
            .WriteSource(&converter, std::ptr::null())
            .map_err(|e| format!("WriteSource: {e}"))?;
        frame.Commit().map_err(|e| format!("CommitFrame: {e}"))?;
        encoder.Commit().map_err(|e| format!("CommitEncoder: {e}"))?;
    }
    Ok(())
}

/// 从文件解码出 RGBA 像素（JPEG / PNG 均可，WIC 自动识别容器）。
#[cfg(windows)]
pub fn decode_wic_from_file(path: &Path) -> Result<(u32, u32, Vec<u8>), String> {
    use windows::core::HSTRING;
    use windows::Win32::Foundation::GENERIC_READ;
    use windows::Win32::Graphics::Imaging::{
        CLSID_WICImagingFactory, GUID_WICPixelFormat32bppBGRA, IWICImagingFactory, IWICPalette,
        WICBitmapDitherTypeNone, WICBitmapPaletteTypeCustom, WICDecodeMetadataCacheOnDemand,
    };
    use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_INPROC_SERVER};

    ensure_com()?;
    unsafe {
        let factory: IWICImagingFactory =
            CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)
                .map_err(|e| format!("WIC factory: {e}"))?;
        let decoder = factory
            .CreateDecoderFromFilename(
                &HSTRING::from(path.as_os_str()),
                None,
                GENERIC_READ,
                WICDecodeMetadataCacheOnDemand,
            )
            .map_err(|e| format!("Decoder: {e}"))?;

        let frame = decoder.GetFrame(0).map_err(|e| format!("GetFrame: {e}"))?;
        let mut w = 0u32;
        let mut h = 0u32;
        frame
            .GetSize(&mut w, &mut h)
            .map_err(|e| format!("GetSize: {e}"))?;
        if w == 0 || h == 0 {
            return Err(format!("zero size {w}x{h}"));
        }

        let converter = factory
            .CreateFormatConverter()
            .map_err(|e| format!("CreateConverter: {e}"))?;
        converter
            .Initialize(
                &frame,
                &GUID_WICPixelFormat32bppBGRA,
                WICBitmapDitherTypeNone,
                None::<&IWICPalette>,
                0.0,
                WICBitmapPaletteTypeCustom,
            )
            .map_err(|e| format!("InitConverter: {e}"))?;

        let mut buf = vec![0u8; (w as usize) * (h as usize) * 4];
        converter
            .CopyPixels(std::ptr::null(), w * 4, &mut buf)
            .map_err(|e| format!("CopyPixels: {e}"))?;

        // BGRA -> RGBA
        for px in buf.chunks_exact_mut(4) {
            px.swap(0, 2);
        }
        Ok((w, h, buf))
    }
}

#[cfg(not(windows))]
pub fn encode_wic_to_file(
    _path: &Path,
    _w: u32,
    _h: u32,
    _rgba: &[u8],
    _max_size: Option<u32>,
) -> Result<(), String> {
    Err("WIC encoding is Windows-only".into())
}

#[cfg(not(windows))]
pub fn decode_wic_from_file(_path: &Path) -> Result<(u32, u32, Vec<u8>), String> {
    Err("WIC decoding is Windows-only".into())
}
