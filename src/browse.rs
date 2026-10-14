use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// 目录中的一个条目（Phase 1 仅做基础枚举，缩略图见 Phase 2）
#[derive(Debug, Clone)]
pub struct Entry {
    pub name: String,
    pub path: PathBuf,
    pub is_dir: bool,
    pub size: u64,
    pub modified: Option<SystemTime>,
}

impl Entry {
    pub fn kind(&self) -> String {
        if self.is_dir {
            return "Folder".to_string();
        }
        match self.path.extension().and_then(|e| e.to_str()) {
            Some(ext) if !ext.is_empty() => ext.to_uppercase(),
            _ => "File".to_string(),
        }
    }
}

/// 列出目录内容：目录优先，其次按名称（忽略大小写）排序
pub fn list(path: &Path) -> std::io::Result<Vec<Entry>> {
    let mut out = Vec::new();
    for item in fs::read_dir(path)? {
        let item = item?;
        let meta = item.metadata();
        let (is_dir, size, modified) = match &meta {
            Ok(m) => (m.is_dir(), m.len(), m.modified().ok()),
            Err(_) => (false, 0, None),
        };
        out.push(Entry {
            name: item.file_name().to_string_lossy().to_string(),
            path: item.path(),
            is_dir,
            size,
            modified,
        });
    }
    out.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    Ok(out)
}

pub fn parent_dir(path: &Path) -> Option<PathBuf> {
    path.parent().filter(|p| !p.as_os_str().is_empty()).map(|p| p.to_path_buf())
}

pub fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{} {}", bytes, UNITS[0])
    } else {
        format!("{:.1} {}", value, UNITS[unit])
    }
}

/// 将 `SystemTime` 格式化为 `YYYY-MM-DD HH:MM`（不引入日期库，使用公历换算）
pub fn format_time(time: SystemTime) -> String {
    let secs = match time.duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_secs() as i64,
        Err(_) => return "-".to_string(),
    };
    let days = secs.div_euclid(86_400);
    let secs_of_day = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}",
        y,
        m,
        d,
        secs_of_day / 3600,
        (secs_of_day % 3600) / 60
    )
}

/// Howard Hinnant 的 days -> civil 算法
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}
