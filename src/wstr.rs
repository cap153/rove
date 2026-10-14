/// Windows API 字符串工具
pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

#[cfg(windows)]
pub fn pcw(v: &[u16]) -> windows::core::PCWSTR {
    windows::core::PCWSTR(v.as_ptr())
}
