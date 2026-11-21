//! 系统应用主题查询：Rove 的「跟随系统」和原生右键菜单的深浅都靠它。

/// 系统**应用**主题是否是深色。
///
/// 读 `HKCU\Software\Microsoft\Windows\CurrentVersion\Themes\Personalize`
/// 的 `AppsUseLightTheme`：`0` = 深色，`1` / 值不存在 = 浅色。
///
/// 特意不读 `SystemUsesLightTheme` —— 那是任务栏与开始菜单的开关，
/// 用户可以「系统深色、应用浅色」，与窗口内容无关。
///
/// 读失败一律按浅色处理：宁可菜单和界面偏亮，也不要在亮色系统上强行变深。
#[cfg(windows)]
pub fn system_dark() -> bool {
    use windows_sys::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_DWORD};

    const KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize";
    const VALUE: &str = "AppsUseLightTheme";

    let wide = |s: &str| s.encode_utf16().chain(Some(0)).collect::<Vec<u16>>();
    let (key, name) = (wide(KEY), wide(VALUE));
    let mut data: u32 = 1;
    let mut size = std::mem::size_of::<u32>() as u32;
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            key.as_ptr(),
            name.as_ptr(),
            RRF_RT_REG_DWORD,
            std::ptr::null_mut(),
            &mut data as *mut u32 as *mut _,
            &mut size,
        )
    };
    status == 0 && data == 0
}

/// 非 Windows 构建（宿主机 `cargo check` / 测试）没有系统主题概念。
#[cfg(not(windows))]
pub fn system_dark() -> bool {
    false
}

#[cfg(test)]
mod tests {
    /// 只验证取值域与调用不会 panic：真实的深浅由系统决定，无法断言。
    #[test]
    fn system_dark_is_a_plain_bool() {
        let _ = super::system_dark();
    }
}
