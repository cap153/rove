//! 索引遍历的统一过滤模块。
//!
//! 将「用户自定义规则」在扫描启动前一次性预编译为 `globset::GlobSet`（工业标准
//! 通配符引擎），避免在百万级遍历的每条记录上都重复解析字符串；同时内置
//! 默认垃圾文件后缀与系统/缓存目录剪枝（这些本就是 Rove 旧版 walker / MFT
//! 的剪枝范围，必须一并迁移进来，否则全盘扫描会重新把 C: 的系统目录灌满索引）。

use std::path::Path;

use globset::{GlobBuilder, GlobSet, GlobSetBuilder};

/// 默认屏蔽的高频垃圾/临时文件后缀（小写，不带点）。
const NOISE_FILE_EXTS: &[&str] = &[
    "log", "tmp", "temp", "bak", "dmp", "cache", "swp", "swo", "part", "crdownload",
];

/// 内置系统/缓存目录的剪枝通配符（任意盘符、任意大小写、任意层级）。
///
/// 旧版 MFT 的 `path_is_noise` 用 `\windows\` 这类子串匹配覆盖整棵子树；这里
/// 等价地表达为 `{?}:/**/<dir>/**`，让 GlobSet 一次性判定。顺序与旧版注释对应：
/// `\program files\` 已覆盖 `(x86)`/`ARM64` 等变体，故只列主名。
const NOISE_DIR_GLOBS: &[&str] = &[
    "{?}:/**/windows/**",
    "{?}:/**/$recycle.bin/**",
    "{?}:/**/system volume information/**",
    "{?}:/**/appdata/**",
    "{?}:/**/program files/**",
    "{?}:/**/node_modules/**",
    "{?}:/**/.git/**",
    "{?}:/**/target/**",
];

pub struct IndexFilter {
    globset: GlobSet,
}

impl IndexFilter {
    /// 构造过滤器：编译内置系统/缓存目录通配符 + 用户自定义规则。
    ///
    /// 用户规则若不含通配符（`*`/`?`），默认当作目录前缀并自动追加 `/**`
    /// 以匹配整棵子树（与旧版「子树剪枝」语义一致）；含通配符则原样使用。
    pub fn new(user_patterns: &[String]) -> Self {
        // Windows 路径大小写不敏感，且旧版剪枝统一做了 to_lowercase；这里把所有
        // 通配符编译为大小写不敏感，保证 `C:/Windows` 与 `c:/windows` 同等拦截。
        let mut builder = GlobSetBuilder::new();

        for pat in NOISE_DIR_GLOBS {
            if let Ok(glob) = GlobBuilder::new(pat).case_insensitive(true).build() {
                builder.add(glob);
            }
        }

        for pat in user_patterns {
            let p = pat.trim().replace('\\', "/");
            if p.is_empty() {
                continue;
            }
            // 没有通配符 → 当作目录前缀：既匹配该目录自身，也匹配整棵子树
            // （与旧版「子树剪枝」语义一致：`D:/Cache` 应同时剪掉自身与其子孙）。
            if !p.contains('*') && !p.contains('?') {
                let base = p.trim_end_matches('/');
                if let Ok(glob) = GlobBuilder::new(base).case_insensitive(true).build() {
                    builder.add(glob);
                }
                if let Ok(glob) = GlobBuilder::new(&format!("{}/**", base))
                    .case_insensitive(true)
                    .build()
                {
                    builder.add(glob);
                }
            } else if let Ok(glob) = GlobBuilder::new(&p).case_insensitive(true).build() {
                builder.add(glob);
            }
        }

        let globset = builder.build().unwrap_or_else(|_| GlobSet::empty());
        Self { globset }
    }

    /// 针对目录的拦截判定（内置系统/缓存目录 + 用户自定义）。
    pub fn is_skipped_dir(&self, path: &Path) -> bool {
        let p = path.to_string_lossy().replace('\\', "/");
        self.globset.is_match(&p)
    }

    /// 针对文件的拦截判定（默认垃圾后缀 + 用户自定义通配符）。
    pub fn is_skipped_file(&self, path: &Path) -> bool {
        // 1. 默认垃圾后缀过滤。
        if let Some(ext) = path.extension() {
            let ext_lower = ext.to_string_lossy().to_lowercase();
            if NOISE_FILE_EXTS.contains(&ext_lower.as_str()) {
                return true;
            }
        }

        // 2. 用户自定义通配符过滤。
        let p = path.to_string_lossy().replace('\\', "/");
        self.globset.is_match(&p)
    }
}
