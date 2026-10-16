//! 本地时间格式化：SystemTime → "yyyy-MM-dd HH:mm"。
//!
//! 不引入 chrono：经 FILETIME → SYSTEMTIME(UTC) →
//! SystemTimeToTzSpecificLocalTime 三步得到本地时间，再手工排版。

use std::time::{SystemTime, UNIX_EPOCH};

/// FILETIME 纪元（1601-01-01）与 UNIX 纪元之差，单位 100ns。
#[cfg_attr(not(windows), allow(dead_code))]
const FILETIME_UNIX_DIFF: u64 = 11_644_473_600;

#[cfg(windows)]
pub fn format_local(time: SystemTime) -> String {
    let Ok(duration) = time.duration_since(UNIX_EPOCH) else {
        return String::new();
    };
    let file_time = duration.as_secs() * 10_000_000 + duration.subsec_nanos() as u64 / 100
        + FILETIME_UNIX_DIFF * 10_000_000;

    let utc = filetime_to_systemtime(file_time);
    let local = to_local(&utc);
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}",
        local.year, local.month, local.day, local.hour, local.minute
    )
}

#[cfg(not(windows))]
pub fn format_local(time: SystemTime) -> String {
    // 非 Windows 构建仅用于代码检查：按 UTC 排版。
    let Ok(duration) = time.duration_since(UNIX_EPOCH) else {
        return String::new();
    };
    let total = duration.as_secs();
    let days = (total / 86_400) as i64;
    let secs_of_day = total % 86_400;
    let (year, month, day) = civil_from_days(days);
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}",
        year,
        month,
        day,
        secs_of_day / 3600,
        (secs_of_day % 3600) / 60
    )
}

#[cfg(windows)]
struct SystemTimeParts {
    year: u16,
    month: u16,
    day: u16,
    hour: u16,
    minute: u16,
    second: u16,
}

/// 100ns 计数 → SYSTEMTIME 分量（UTC）。
#[cfg(windows)]
fn filetime_to_systemtime(file_time: u64) -> SystemTimeParts {
    // FILETIME 是 1601 纪元，先折算回 Unix 秒（1970 纪元），
    // 否则 civil_from_days 会把 369 年的纪元差叠进结果（显示成 2395 年）。
    let unix_secs = file_time / 10_000_000 - FILETIME_UNIX_DIFF;
    let days = (unix_secs / 86_400) as i64;
    let secs_of_day = unix_secs % 86_400;

    let (year, month, day) = civil_from_days(days);
    SystemTimeParts {
        year: year as u16,
        month: month as u16,
        day: day as u16,
        hour: (secs_of_day / 3600) as u16,
        minute: ((secs_of_day % 3600) / 60) as u16,
        second: (secs_of_day % 60) as u16,
    }
}

/// 经 Win32 转换为本地时间；失败时回落 UTC。
#[cfg(windows)]
fn to_local(utc: &SystemTimeParts) -> SystemTimeParts {
    use windows_sys::Win32::Foundation::SYSTEMTIME;
    use windows_sys::Win32::System::Time::SystemTimeToTzSpecificLocalTime;

    let st = SYSTEMTIME {
        wYear: utc.year,
        wMonth: utc.month,
        wDayOfWeek: 0,
        wDay: utc.day,
        wHour: utc.hour,
        wMinute: utc.minute,
        wSecond: utc.second,
        wMilliseconds: 0,
    };
    let mut local = SYSTEMTIME {
        wYear: 0,
        wMonth: 0,
        wDayOfWeek: 0,
        wDay: 0,
        wHour: 0,
        wMinute: 0,
        wSecond: 0,
        wMilliseconds: 0,
    };
    let ok = unsafe {
        SystemTimeToTzSpecificLocalTime(std::ptr::null(), &st, &mut local)
    };
    if ok == 0 {
        return SystemTimeParts {
            year: utc.year,
            month: utc.month,
            day: utc.day,
            hour: utc.hour,
            minute: utc.minute,
            second: utc.second,
        };
    }
    SystemTimeParts {
        year: local.wYear,
        month: local.wMonth,
        day: local.wDay,
        hour: local.wHour,
        minute: local.wMinute,
        second: local.wSecond,
    }
}

/// 儒略日序 → (年, 月, 日)。算法见 Howard Hinnant 的 civil_from_days。
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs::entry::format_size_bytes;

    #[test]
    fn civil_from_days_matches_known_dates() {
        // 1970-01-01 = day 0
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        // 2000-02-29（闰日）= 第 11016 天
        assert_eq!(civil_from_days(11_016), (2000, 2, 29));
        // 2024-10-15 = 第 20011 天
        assert_eq!(civil_from_days(20_011), (2024, 10, 15));
        // 2000-03-01 = 闰日后一天
        assert_eq!(civil_from_days(11_017), (2000, 3, 1));
    }

    #[test]
    fn format_size_scales() {
        assert_eq!(format_size_bytes(0), "0 B");
        assert_eq!(format_size_bytes(512), "512 B");
        assert_eq!(format_size_bytes(1024), "1.0 KB");
        assert_eq!(format_size_bytes(1024 * 1024 * 3 + 1024 * 512), "3.5 MB");
        assert_eq!(format_size_bytes(1024u64.pow(3) * 2), "2.0 GB");
    }
}
