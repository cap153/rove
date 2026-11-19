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
/// 用于调度隔离：慢格式的在途数有上限（线程数 >2 时只占 `n-1` 条 worker，
/// 给普通图片永远留 1 条空闲线程，避免「图片+视频混排」时图片被视频卡死）。
/// 纯视频文件夹本身解码就慢，调度救不了（那是解码速度的物理上限），
/// 此隔离主要救混排场景。
///
/// **绝不碰文件系统**：曾经的 `path.is_dir()` 是一次真实 stat，网络盘上实测
/// 25–30 ms/次，而它被派发线程和每条 worker 各调用一次 —— 派发是单线程的，
/// 等于整个缩略图管道被限速到「一个请求 / 60 ms」，大幅滚动后新窗口那 117 行
/// 要 7 秒才派发完，用户看到的就是「滚过去之后整屏空白」。
///
/// 目录没有视频扩展名，所以按扩展名判和原来等价（目录封面照旧走快格式）。
pub fn is_slow_thumbnail(path: &Path) -> bool {
    const SLOW: [&str; 7] = ["mp4", "mkv", "avi", "mov", "webm", "wmv", "m4v"];
    let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
        return false;
    };
    SLOW.iter().any(|s| ext.eq_ignore_ascii_case(s))
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

    // 图标类文件（快捷方式/可执行）先走系统图列表（Pulse 的做法）：
    // `GetImage` 对它们返回的是**不透明整帧**，画稿小也没法裁。
    // 文件夹只有「设了自定义图标」才走这条，其余保留 `GetImage` 的封面缩略图。
    let use_image_list = if path.is_dir() {
        folder_has_custom_icon(path)
    } else {
        is_icon_like(path)
    };
    if use_image_list {
        if let Some((w, h, rgba)) = icon_from_image_list(path, size) {
            let art = art_bounds(w, h, &rgba);
            let (cw, ch, cbuf) = crop_undersized_icon(w, h, rgba, false);
            icon_diag_log(path, 0, w, h, art, cw != w || ch != h);
            return Ok((cw, ch, cbuf));
        }
    }

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
        let art = art_bounds(bw, bh, &buf);
        let matte = is_icon_like(path);
        let (cw, ch, cbuf) = crop_undersized_icon(bw, bh, buf, matte);
        icon_diag_log(path, size, bw, bh, art, cw != bw || ch != bh);
        Ok((cw, ch, cbuf))
    }
}

/// 裁掉「小图塞在大透明画布」多出来的空白。
///
/// Shell 的图列表对**没有大图**的图标（很多 `.lnk`、老程序）不会放大，而是把它
/// 最大的一张图原尺寸放进一块大透明画布（左上角或居中）。整帧按单元格拉伸绘制
/// 时，真正有像素的部分只占一小块，别的图标铺满格子，它却小一圈——「图标偏小」
/// 就是这么来的。思路抄 Pulse 的 `ui/icon_artwork_bounds.h`：量出实际画稿的
/// 包围盒，明显小于画布（< 55%）就裁到包围盒本身；比例接近满格的原样返回。
///
/// 裁到**包围盒**而不是正方形：绘制端是 `image-fit: contain`，包围盒能保证
/// 同等比例下画稿尽量大，且不会切掉偏移绘制（居中）的图形。
///
/// 返回新的 (宽, 高, 像素)；不需要裁时原样返回。
/// alpha 低于此值视为透明。
///
/// 不能只认 `== 0`：Shell 给小图铺的那层衬底 alpha 常常是 1–4（肉眼不可见，
/// 但按"非零即有像素"量出来就是整帧），于是画稿包围盒 = 整帧、裁切失效。
const ALPHA_MIN: u8 = 8;

/// 量实际画稿的包围盒（像素，右下为开区间）。全透明返回 None。
fn art_bounds(w: u32, h: u32, buf: &[u8]) -> Option<(u32, u32, u32, u32)> {
    if w == 0 || h == 0 || buf.len() < (w as usize) * (h as usize) * 4 {
        return None;
    }
    let (mut left, mut top, mut right, mut bottom) = (w, h, 0u32, 0u32);
    for y in 0..h {
        let row = (y as usize) * (w as usize) * 4;
        for x in 0..w {
            if buf[row + (x as usize) * 4 + 3] < ALPHA_MIN {
                continue;
            }
            if x < left {
                left = x;
            }
            if y < top {
                top = y;
            }
            if x + 1 > right {
                right = x + 1;
            }
            if y + 1 > bottom {
                bottom = y + 1;
            }
        }
    }
    if right <= left || bottom <= top {
        return None;
    }
    Some((left, top, right, bottom))
}

/// 按「主色衬底」量画稿包围盒。
///
/// 有些图标（无 256px 版本的 .lnk/.exe）Shell 交回来的是**不透明整帧**：真正的
/// 画稿只占中间一小块，四周被填成一种均匀底色（通常是白色，肉眼在白色单元格里
/// 看不出来）。这种帧按 alpha 量就是整帧，只能按颜色找：取占比最大的量化色当
/// 衬底，再量「与衬底明显不同」的像素包围盒。
///
/// 只在图标类文件上调用。照片/视频封面/文件夹封面不走这里——照片里大片同色
/// 天空、电影上下黑边都会被误裁。
fn matte_bounds(w: u32, h: u32, buf: &[u8]) -> Option<(u32, u32, u32, u32)> {
    let (wus, hus) = (w as usize, h as usize);
    if wus == 0 || hus == 0 || buf.len() < wus * hus * 4 {
        return None;
    }
    // 5 位/通道量化直方图（32³ 桶）：找占比最大的底色。
    let mut hist = vec![0u32; 32 * 32 * 32];
    for px in buf.chunks_exact(4) {
        hist[((px[0] as usize >> 3) << 10) | ((px[1] as usize >> 3) << 5) | (px[2] as usize >> 3)] += 1;
    }
    let (best, count) = hist
        .iter()
        .enumerate()
        .max_by_key(|(_, &n)| n)
        .map(|(i, &n)| (i, n))
        .unwrap_or((0, 0));
    // 底色不足 30%：这不是"衬底 + 小画稿"，而是本来就铺满的图案，别动它。
    if (count as usize) * 10 < wus * hus * 3 {
        return None;
    }
    let bg = [
        ((best >> 10) as u8 & 0x1F) << 3,
        ((best >> 5) as u8 & 0x1F) << 3,
        (best as u8 & 0x1F) << 3,
    ];
    let differs = |px: &[u8]| -> bool {
        let d = |a: u8, b: u8| (a as i32 - b as i32).abs();
        // 量化会把颜色截到桶左下角，容差取 24 才能覆盖整个桶 + 抗锯齿边。
        d(px[0], bg[0]).max(d(px[1], bg[1])).max(d(px[2], bg[2])) > 24
    };
    let (mut left, mut top, mut right, mut bottom) = (w, h, 0u32, 0u32);
    for y in 0..h {
        let row = (y as usize) * (w as usize) * 4;
        for x in 0..w {
            if !differs(&buf[row + (x as usize) * 4..row + (x as usize) * 4 + 4]) {
                continue;
            }
            if x < left {
                left = x;
            }
            if y < top {
                top = y;
            }
            if x + 1 > right {
                right = x + 1;
            }
            if y + 1 > bottom {
                bottom = y + 1;
            }
        }
    }
    if right <= left || bottom <= top {
        return None;
    }
    Some((left, top, right, bottom))
}

/// 图标帧诊断（详细级，`ROVE_LOG=2`）。
///
/// 记「Shell 到底给了什么」：帧多大、画稿多小、裁没裁。`ROVE_LOG` 未开启时
/// 第一行就返回，正常运行连扫描都不做。
fn icon_diag_log(path: &Path, req: u32, w: u32, h: u32, art: Option<(u32, u32, u32, u32)>, cropped: bool) {
    if !crate::app::log_enabled(2) {
        return;
    }
    use std::sync::Mutex;
    static SEEN: Mutex<Option<std::collections::HashSet<String>>> = Mutex::new(None);
    let Some((l, t, r, b)) = art else { return };
    let (aw, ah) = (r - l, b - t);
    let ext = path
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    // 图标类（快捷方式/可执行）一律记录——「显小」的抱怨全部落在这一类里，
    // 而且它们数量少。其他格式只在画稿明显小（<80%）时记，避免图库刷屏。
    let icon_class = matches!(
        ext.as_str(),
        "lnk" | "exe" | "url" | "msi" | "bat" | "cmd" | "com" | "scr"
    );
    if w == 0 || h == 0 || (!icon_class && aw * 10 >= w * 8 && ah * 10 >= h * 8) {
        return;
    }
    let key = path.to_string_lossy().to_string();
    {
        let Ok(mut guard) = SEEN.lock() else { return };
        let set = guard.get_or_insert_with(Default::default);
        if set.len() >= 200 || !set.insert(key.clone()) {
            return;
        }
    }
    crate::logv!(
        "[icon] req={req}{} frame={w}x{h} art={aw}x{ah} box={l},{t} fill={:.2} cropped={cropped} alpha_full={} {}",
        if req == 0 { " src=imagelist" } else { "" },
        (aw as f32 / w.max(1) as f32).min(ah as f32 / h.max(1) as f32),
        aw * 100 > w * 98 && ah * 100 > h * 98,
        key
    );
}

fn crop_undersized_icon(w: u32, h: u32, buf: Vec<u8>, matte: bool) -> (u32, u32, Vec<u8>) {
    // 下限 32px：再小的帧里画稿不可能有明显留白，裁了只会放大噪点。
    // 不要求正方形：绘制端是 `image-fit: contain`，裁到包围盒不会变形。
    if w < 32 || h < 32 || buf.len() < (w as usize) * (h as usize) * 4 {
        return (w, h, buf);
    }
    let alpha_box = art_bounds(w, h, &buf);
    let full = |(l, t, r, b): (u32, u32, u32, u32)| -> bool {
        (r - l) * 100 > w * 98 && (b - t) * 100 > h * 98
    };
    // 1. 透明留白：照片/封面/大多数图标走这条（画稿 = 不透明区域）。
    // 2. 不透明衬底：alpha 铺满整帧时才考虑，且只对图标类文件
    //    （照片里的大片同色、影片上下黑边都不能按颜色裁）。
    let box_ = match alpha_box {
        Some(b) if !full(b) => Some(b),
        _ if matte => matte_bounds(w, h, &buf).filter(|&b| {
            // 衬底色块本身占满整帧（纯色图标）时没必要裁，也别把纯色图裁成一条线。
            let (l, t, r, b2) = b;
            (r - l) >= 8
                && (b2 - t) >= 8
                && ((r - l) * 100 < w * 96 || (b2 - t) * 100 < h * 96)
        }),
        _ => None,
    };
    let Some((left, top, right, bottom)) = box_ else {
        return (w, h, buf); // 没有可裁的留白（或全透明，交给 UI 画占位色块）
    };
    let (aw, ah) = (right - left, bottom - top);
    if aw < 8 || ah < 8 {
        return (w, h, buf);
    }
    let stride = (w as usize) * 4;
    let mut out = Vec::with_capacity((aw as usize) * (ah as usize) * 4);
    for y in top..bottom {
        let start = (y as usize) * stride + (left as usize) * 4;
        out.extend_from_slice(&buf[start..start + (aw as usize) * 4]);
    }
    crate::logv!(
        "[thumb] icon cropped {}x{} -> {}x{} box={},{} matte={}",
        w,
        h,
        aw,
        ah,
        left,
        top,
        matte
    );
    (aw, ah, out)
}

/// HICON → RGBA（已反预乘）。成功后销毁图标句柄。
#[cfg(windows)]
fn hicon_to_rgba(
    hicon: windows::Win32::UI::WindowsAndMessaging::HICON,
) -> Option<(u32, u32, Vec<u8>)> {
    use windows::Win32::Graphics::Gdi::{
        DeleteObject, GetDC, GetDIBits, GetObjectW, ReleaseDC, BITMAP, BITMAPINFO,
        BITMAPINFOHEADER, DIB_RGB_COLORS,
    };
    use windows::Win32::UI::WindowsAndMessaging::{DestroyIcon, GetIconInfo, ICONINFO};

    unsafe {
        let mut ii = ICONINFO::default();
        if GetIconInfo(hicon, &mut ii).is_err() {
            let _ = DestroyIcon(hicon);
            return None;
        }
        let hbm = ii.hbmColor;
        if hbm.is_invalid() {
            let _ = DestroyIcon(hicon);
            let _ = DeleteObject(ii.hbmMask.into());
            return None;
        }
        // 高 DPI / 带大图标的 exe 上颜色位图未必等于请求尺寸，先查真实尺寸。
        let mut bmp = BITMAP::default();
        let got = GetObjectW(
            hbm.into(),
            std::mem::size_of::<BITMAP>() as i32,
            Some(&mut bmp as *mut _ as *mut _),
        );
        if got == 0 || bmp.bmWidth <= 0 || bmp.bmHeight <= 0 {
            let _ = DeleteObject(hbm.into());
            let _ = DeleteObject(ii.hbmMask.into());
            let _ = DestroyIcon(hicon);
            return None;
        }
        let (bw, bh) = (bmp.bmWidth as u32, bmp.bmHeight as u32);
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
        let hdc = GetDC(None);
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
            return None;
        }
        // 老式图标只有 AND 掩码、没有 alpha 通道，GetDIBits 读回来整幅 alpha=0。
        // 不补这一步，图标会被当成"全透明"画成空白（裁切也会因为找不到画稿而失效）。
        let any_alpha = buf.chunks_exact(4).any(|px| px[3] != 0);
        // BGRA→RGBA + 反预乘（HICON 位图为预乘 alpha）。
        for px in buf.chunks_exact_mut(4) {
            let a = px[3] as u32;
            if !any_alpha {
                px[3] = 255;
            } else if a > 0 && a < 255 {
                px[0] = ((px[0] as u32 * 255) / a).min(255) as u8;
                px[1] = ((px[1] as u32 * 255) / a).min(255) as u8;
                px[2] = ((px[2] as u32 * 255) / a).min(255) as u8;
            }
            px.swap(0, 2);
        }
        Some((bw, bh, buf))
    }
}

/// 取图标本体：走系统图列表（Pulse 的做法），不是让 Shell 渲染整帧。
///
/// `IShellItemImageFactory` 对没有大图的图标（很多 `.lnk` / 老程序）会返回
/// **不透明整帧**（`[icon]` 日志里的 `alpha_full=true`、`fill=1.00`）：真正画稿
/// 只占中间一小块，四周被衬底填满，按 alpha 裁切无从下手 → 图标显小。
/// 系统图列表给的是图标自己的位图（透明留白 + 画稿原尺寸摆放），裁切才有效。
/// 文件夹是否设了自定义图标（desktop.ini 的 IconResource 之类）。
///
/// 判定办法：同一个路径查两次系统图标索引——
/// - 带 `SHGFI_USEFILEATTRIBUTES` 时 Shell 不读 desktop.ini，给的是**通用文件夹图标**；
/// - 不带时读真实属性，给的是**这个文件夹实际显示的图标**。
/// 两者索引不同 ⇒ 有自定义图标。这样不必猜「有封面还是没封面」。
///
/// 自定义图标的文件夹同样会被 Shell 渲染成不透明整帧（画稿小、衬底填满），
/// 只能改走系统图列表；没有自定义图标的文件夹继续走 `GetImage` 保留封面缩略图。
#[cfg(windows)]
fn folder_has_custom_icon(path: &Path) -> bool {
    use windows::core::PCWSTR;
    use windows::Win32::Storage::FileSystem::FILE_ATTRIBUTE_DIRECTORY;
    use windows::Win32::UI::Shell::{
        SHGetFileInfoW, SHFILEINFOW, SHGFI_FLAGS, SHGFI_SYSICONINDEX, SHGFI_USEFILEATTRIBUTES,
    };

    let wide: Vec<u16> = path
        .as_os_str()
        .to_string_lossy()
        .encode_utf16()
        .chain([0])
        .collect();
    let index_of = |extra: SHGFI_FLAGS| -> Option<i32> {
        let mut info = SHFILEINFOW::default();
        let ok = unsafe {
            SHGetFileInfoW(
                PCWSTR(wide.as_ptr()),
                FILE_ATTRIBUTE_DIRECTORY,
                Some(&mut info),
                std::mem::size_of::<SHFILEINFOW>() as u32,
                SHGFI_SYSICONINDEX | extra,
            )
        };
        (ok != 0 && info.iIcon >= 0).then_some(info.iIcon)
    };
    // 差异只在 USEFILEATTRIBUTES：带它 = 不读 desktop.ini（通用文件夹图标）。
    let generic = index_of(SHGFI_USEFILEATTRIBUTES);
    let real = index_of(SHGFI_FLAGS(0));
    match (generic, real) {
        (Some(g), Some(r)) => {
            let custom = g != r;
            if custom {
                crate::logv!(
                    "[icon] folder custom icon idx={r} (generic {g}) {}",
                    path.display()
                );
            }
            custom
        }
        // 查不到就当作没有自定义图标：宁可显示封面/默认图标，也不要乱换。
        _ => false,
    }
}

#[cfg(windows)]
fn icon_from_image_list(path: &Path, want_px: u32) -> Option<(u32, u32, Vec<u8>)> {
    use windows::core::PCWSTR;
    use windows::Win32::Storage::FileSystem::FILE_ATTRIBUTE_NORMAL;
    use windows::Win32::UI::Controls::{IImageList, ILD_TRANSPARENT};
    use windows::Win32::UI::Shell::{
        SHGetFileInfoW, SHGetImageList, SHFILEINFOW, SHGFI_SYSICONINDEX, SHIL_EXTRALARGE,
        SHIL_JUMBO, SHIL_LARGE, SHIL_SMALL,
    };

    let wide: Vec<u16> = path
        .as_os_str()
        .to_string_lossy()
        .encode_utf16()
        .chain([0])
        .collect();
    unsafe {
        let mut info = SHFILEINFOW::default();
        let ok = SHGetFileInfoW(
            PCWSTR(wide.as_ptr()),
            FILE_ATTRIBUTE_NORMAL,
            Some(&mut info),
            std::mem::size_of::<SHFILEINFOW>() as u32,
            SHGFI_SYSICONINDEX,
        );
        if ok == 0 || info.iIcon < 0 {
            return None;
        }
        // 要多大就先要多大，不够再往上退（Pulse 的 `ImageListId` 同思路）。
        // 别一律取 JUMBO：详情/列表视图只要 24 逻辑像素，256² 的图标
        // 一张就是 262KB，几百行滚动下来会把缩略图 LRU 挤掉。
        // 列表名义边长：SMALL 16 / LARGE 32 / EXTRALARGE 48 / JUMBO 256。
        let order: [u32; 4] = if want_px <= 20 {
            [SHIL_SMALL, SHIL_LARGE, SHIL_EXTRALARGE, SHIL_JUMBO]
        } else if want_px <= 40 {
            [SHIL_LARGE, SHIL_EXTRALARGE, SHIL_JUMBO, SHIL_SMALL]
        } else if want_px <= 96 {
            [SHIL_EXTRALARGE, SHIL_JUMBO, SHIL_LARGE, SHIL_SMALL]
        } else {
            [SHIL_JUMBO, SHIL_EXTRALARGE, SHIL_LARGE, SHIL_SMALL]
        };
        for shil in order {
            let Ok(list) = SHGetImageList::<IImageList>(shil as i32) else {
                continue;
            };
            let Ok(hicon) = list.GetIcon(info.iIcon, ILD_TRANSPARENT.0) else {
                continue;
            };
            if hicon.is_invalid() {
                continue;
            }
            if let Some(rgba) = hicon_to_rgba(hicon) {
                crate::logv!(
                    "[icon] image-list shil={shil} {}x{} {}",
                    rgba.0,
                    rgba.1,
                    path.display()
                );
                return Some(rgba);
            }
        }
        None
    }
}

/// 图标类扩展名：只有这些文件允许用「主色衬底」裁切。
fn is_icon_like(path: &Path) -> bool {
    let ext = path
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    matches!(
        ext.as_str(),
        "lnk" | "exe" | "url" | "msi" | "bat" | "cmd" | "com" | "scr" | "ico" | "dll"
    )
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
    use windows::Win32::UI::Shell::{
        SHGetFileInfoW, SHFILEINFOW, SHGFI_ICON, SHGFI_LARGEICON, SHGFI_USEFILEATTRIBUTES,
    };

    let mut pure_ms: u128 = 0;
    if ensure_com().is_err() {
        return (None, pure_ms);
    }    let wide: Vec<u16> = path
        .as_os_str()
        .to_string_lossy()
        .encode_utf16()
        .chain([0])
        .collect();
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
        let out = hicon_to_rgba(hicon);
        (out, pure_ms + tail_ms())
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

#[cfg(test)]
mod tests {
    use super::{crop_undersized_icon, is_slow_thumbnail};
    use std::path::Path;

    /// 慢格式判定只看扩展名，**不许碰文件系统**：一次网络 stat 是 25–30 ms，
    /// 而派发线程是单线程的，一屏 117 行就会被限速成 3 秒以上。
    #[test]
    fn slow_thumbnail_is_decided_by_extension_only() {
        assert!(is_slow_thumbnail(Path::new("C:/x/clip.MP4")));
        assert!(is_slow_thumbnail(Path::new(r"\\nas\share\clip.mkv")));
        assert!(is_slow_thumbnail(Path::new("C:/x/无后缀也能判.m4v")));
        assert!(!is_slow_thumbnail(Path::new("C:/x/photo.jpg")));
        assert!(!is_slow_thumbnail(Path::new("C:/x/noext")));
        // 目录名恰好带视频后缀也按「慢」处理：宁可多限一个目录，
        // 也不要在判定里加一次 stat。
        assert!(is_slow_thumbnail(Path::new("C:/x/folder.mp4")));
    }

    /// 造一张 `frame` 见方的透明画布，在 (x, y) 画一块 `art` 见方的实心像素。
    fn frame_with_art(frame: u32, x: u32, y: u32, art: u32) -> Vec<u8> {
        let mut buf = vec![0u8; (frame * frame * 4) as usize];
        for yy in y..(y + art) {
            for xx in x..(x + art) {
                let i = ((yy * frame + xx) * 4) as usize;
                buf[i..i + 4].copy_from_slice(&[0x40, 0x80, 0xC0, 0xFF]);
            }
        }
        buf
    }

    #[test]
    fn crops_artwork_with_padding() {
        // 132 帧、画稿 97×116（实测 data 文件夹那类）——留白要被裁掉，
        // 否则它比别人小一圈。
        let (w, h, out) = crop_undersized_icon(132, 132, frame_with_art(132, 22, 13, 97), false);
        assert_eq!((w, h), (97, 97));
        assert_eq!(out.len(), 97 * 97 * 4);
    }

    #[test]
    fn crops_artwork_off_center() {
        let (w, h, _) = crop_undersized_icon(264, 264, frame_with_art(264, 57, 9, 191), false);
        assert_eq!((w, h), (191, 191));
    }

    #[test]
    fn keeps_artwork_that_fills_the_frame() {
        // 照片/封面类：铺满整帧，不做任何裁切。
        let (w, h, out) = crop_undersized_icon(256, 256, frame_with_art(256, 0, 0, 256), false);
        assert_eq!((w, h), (256, 256));
        assert_eq!(out.len(), 256 * 256 * 4);
        // 只差 1px 的边缘也视为铺满。
        let (w, h, _) = crop_undersized_icon(256, 256, frame_with_art(256, 1, 1, 254), false);
        assert_eq!((w, h), (256, 256));
    }

    #[test]
    fn keeps_tiny_frames_and_wide_images() {
        // 32px 以下不裁：画稿不可能有明显留白。
        let (w, h, _) = crop_undersized_icon(24, 24, frame_with_art(24, 0, 0, 12), false);
        assert_eq!((w, h), (24, 24));
        // 不透明宽幅图（视频封面等）：不裁。
        let wide = vec![0xFFu8; 200 * 100 * 4];
        let (w, h, _) = crop_undersized_icon(200, 100, wide, false);
        assert_eq!((w, h), (200, 100));
    }

    /// 不透明白色衬底 + 中间一小块图案：这是 Shell 给「没有 256 图」的
    /// .lnk/.exe 的样子（alpha 铺满整帧，画稿只占中间）。
    fn white_matte_frame(frame: u32, x: u32, y: u32, art: u32) -> Vec<u8> {
        let mut buf = vec![0u8; (frame * frame * 4) as usize];
        for px in buf.chunks_exact_mut(4) {
            px.copy_from_slice(&[0xFF, 0xFF, 0xFF, 0xFF]);
        }
        for yy in y..(y + art) {
            for xx in x..(x + art) {
                let i = ((yy * frame + xx) * 4) as usize;
                buf[i..i + 4].copy_from_slice(&[0x20, 0x60, 0xC0, 0xFF]);
            }
        }
        buf
    }

    #[test]
    fn crops_opaque_matte_for_icon_files() {
        let (w, h, _) = crop_undersized_icon(264, 264, white_matte_frame(264, 90, 90, 80), true);
        assert_eq!((w, h), (80, 80));
    }

    #[test]
    fn keeps_opaque_matte_for_other_files() {
        // 同一帧但路径不是图标类（例如一张照片）：绝不能按颜色裁。
        let (w, h, _) = crop_undersized_icon(264, 264, white_matte_frame(264, 90, 90, 80), false);
        assert_eq!((w, h), (264, 264));
    }

    #[test]
    fn keeps_fully_transparent_frame() {
        let (w, h, _) = crop_undersized_icon(256, 256, vec![0u8; 256 * 256 * 4], false);
        assert_eq!((w, h), (256, 256));
    }
}
