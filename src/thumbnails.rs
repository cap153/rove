//! Windows Shell 缩略图异步管线 + LRU 纹理缓存。
//!
//! * 主线程只发起请求，绝不阻塞 I/O；
//! * 后台线程池调用 `IShellItemImageFactory::GetImage` 抽取位图并转成 RGBA 字节
//!   （`slint::Image` 不可跨线程传递，因此线程间只传像素数据）；
//! * 结果经 `poll()` 在主线程取回并构造成 Slint 图像，更新模型对应行。

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::Duration;

use slint::{Image, SharedPixelBuffer};

#[cfg(windows)]
use crate::wstr::wide;

#[cfg(windows)]
use windows::Win32::Foundation::SIZE;
#[cfg(windows)]
use windows::Win32::Graphics::Gdi::{DIBSECTION, DeleteObject, GetObjectW};
#[cfg(windows)]
use windows::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx};
#[cfg(windows)]
use windows::Win32::UI::Shell::{
    IShellItemImageFactory, SHCreateItemFromParsingName, SIIGBF, SIIGBF_BIGGERSIZEOK,
};
#[cfg(windows)]
use crate::wstr::pcw;

/// 缩略图边长（像素）
pub const THUMB_SIZE: u32 = 32;
/// 后台工作线程数
const WORKERS: usize = 8;
/// 抽取失败（Shell 缓存被占用等）时的重试次数
const EXTRACT_RETRIES: u32 = 3;
/// LRU 容量
const CACHE_CAPACITY: usize = 512;

/// 解码后的 RGBA 像素字节（可跨线程传递）
pub type Pixels = (Vec<u8>, u32, u32);

pub fn pixels_to_image(pixels: &Pixels) -> Image {
    let (data, width, height) = pixels;
    Image::from_rgba8(SharedPixelBuffer::clone_from_slice(data, *width, *height))
}

struct LruCache {
    map: HashMap<PathBuf, Pixels>,
    order: VecDeque<PathBuf>,
}

impl LruCache {
    fn new() -> Self {
        Self {
            map: HashMap::with_capacity(CACHE_CAPACITY),
            order: VecDeque::new(),
        }
    }

    fn get(&mut self, key: &Path) -> Option<Pixels> {
        let pixels = self.map.get(key)?.clone();
        self.touch(key);
        Some(pixels)
    }

    fn insert(&mut self, key: PathBuf, pixels: Pixels) {
        if self.map.insert(key.clone(), pixels).is_none() {
            self.order.push_back(key);
        }
        while self.order.len() > CACHE_CAPACITY {
            if let Some(oldest) = self.order.pop_front() {
                self.map.remove(&oldest);
            }
        }
    }

    fn touch(&mut self, key: &Path) {
        if let Some(position) = self.order.iter().position(|p| p == key) {
            let key = self.order.remove(position).unwrap();
            self.order.push_back(key);
        }
    }
}

struct Inner {
    cache: LruCache,
    in_flight: HashSet<PathBuf>,
    results: Receiver<(PathBuf, Option<Pixels>)>,
}

pub struct ThumbnailService {
    inner: Mutex<Inner>,
    work_tx: Sender<PathBuf>,
}

static SERVICE: OnceLock<ThumbnailService> = OnceLock::new();

pub fn global() -> &'static ThumbnailService {
    SERVICE.get_or_init(ThumbnailService::new)
}

impl ThumbnailService {
    fn new() -> Self {
        let (work_tx, work_rx) = channel::<PathBuf>();
        let (result_tx, result_rx) = channel::<(PathBuf, Option<Pixels>)>();
        let work_rx = Arc::new(Mutex::new(work_rx));

        for _ in 0..WORKERS {
            let work_rx = Arc::clone(&work_rx);
            let result_tx = result_tx.clone();
            thread::spawn(move || loop {
                let path = {
                    let guard = work_rx.lock().unwrap_or_else(|e| e.into_inner());
                    guard.recv()
                };
                let Ok(path) = path else { break };
                // Shell 的 thumbcache 可能被 Explorer 占用导致偶发失败，重试数次
                let mut pixels = extract(&path);
                for attempt in 1..=EXTRACT_RETRIES {
                    if pixels.is_some() {
                        break;
                    }
                    thread::sleep(Duration::from_millis(100 * attempt as u64));
                    pixels = extract(&path);
                }
                let _ = result_tx.send((path, pixels));
            });
        }

        Self {
            inner: Mutex::new(Inner {
                cache: LruCache::new(),
                in_flight: HashSet::new(),
                results: result_rx,
            }),
            work_tx,
        }
    }

    /// 请求缩略图；命中缓存立即返回，否则后台异步抽取
    pub fn request(&self, path: &Path) -> Option<Image> {
        let path = path.to_path_buf();
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(pixels) = inner.cache.get(&path) {
            return Some(pixels_to_image(&pixels));
        }
        if inner.in_flight.insert(path.clone()) {
            let _ = self.work_tx.send(path);
        }
        None
    }

    /// 主线程轮询已完成的结果
    pub fn poll(&self) -> Vec<(PathBuf, Image)> {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let mut done = Vec::new();
        while let Ok((path, pixels)) = inner.results.try_recv() {
            inner.in_flight.remove(&path);
            if let Some(pixels) = pixels {
                let image = pixels_to_image(&pixels);
                inner.cache.insert(path.clone(), pixels);
                done.push((path, image));
            }
        }
        done
    }

    /// 清理在途请求集合（例如 UI 销毁后）
    pub fn reset(&self) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.in_flight.clear();
    }
}

/// 通过 Shell 图像工厂抽取缩略图并转换为 RGBA 像素（仅 Windows）
#[cfg(windows)]
fn extract(path: &Path) -> Option<Pixels> {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);

        let path_wide = wide(&path.display().to_string());
        let factory: IShellItemImageFactory =
            SHCreateItemFromParsingName(pcw(&path_wide), None).ok()?;
        let bitmap = factory
            .GetImage(
                SIZE {
                    cx: THUMB_SIZE as i32,
                    cy: THUMB_SIZE as i32,
                },
                SIIGBF(SIIGBF_BIGGERSIZEOK.0),
            )
            .ok()?;

        let mut section: DIBSECTION = std::mem::zeroed();
        let copied = GetObjectW(
            bitmap.into(),
            std::mem::size_of::<DIBSECTION>() as i32,
            Some(&mut section as *mut DIBSECTION as *mut core::ffi::c_void),
        );
        if copied == 0 {
            let _ = DeleteObject(bitmap.into());
            return None;
        }

        let width = section.dsBm.bmWidth;
        let height = section.dsBm.bmHeight;
        let bits = section.dsBm.bmBits;
        let depth = section.dsBmih.biBitCount;

        if width <= 0 || height <= 0 || bits.is_null() || depth != 32 {
            let _ = DeleteObject(bitmap.into());
            return None;
        }

        let width = width as usize;
        let height = height as usize;
        let stride = (width * 4 + 3) / 4 * 4;
        let source = std::slice::from_raw_parts(bits as *const u8, stride * height);

        // Shell 返回的是自下而上的 BGRA，转换为自上而下的 RGBA 字节
        let mut pixels: Vec<u8> = vec![0u8; width * height * 4];
        for y in 0..height {
            let row = (height - 1 - y) * stride;
            for x in 0..width {
                let source_offset = row + x * 4;
                let target_offset = (y * width + x) * 4;
                pixels[target_offset] = source[source_offset + 2];
                pixels[target_offset + 1] = source[source_offset + 1];
                pixels[target_offset + 2] = source[source_offset];
                pixels[target_offset + 3] = source[source_offset + 3];
            }
        }

        let _ = DeleteObject(bitmap.into());
        Some((pixels, width as u32, height as u32))
    }
}

/// 非 Windows 平台无缩略图来源
#[cfg(not(windows))]
fn extract(_path: &Path) -> Option<Pixels> {
    None
}
