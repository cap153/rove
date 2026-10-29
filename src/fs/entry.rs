//! 文件条目模型、类型识别（扩展名 → 类型徽标/图标配色）与体积/时间格式化。

use std::path::Path;
use std::time::SystemTime;

/// 条目大类，决定图标配色与类型徽标配色。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum IconStyle {
    Folder,
    Image,
    Video,
    Audio,
    Doc,
    Pdf,
    Sheet,
    Archive,
    Code,
    Exe,
    Font,
    Disk,
    Generic,
}

impl IconStyle {
    pub fn color(self) -> slint::Color {
        let (r, g, b) = match self {
            IconStyle::Folder => (0xE8, 0xB3, 0x3C),
            IconStyle::Image => (0x4C, 0x9F, 0x70),
            IconStyle::Video => (0x8E, 0x6C, 0xC9),
            IconStyle::Audio => (0xD9, 0x69, 0x4E),
            IconStyle::Doc => (0x3E, 0x7B, 0xC4),
            IconStyle::Pdf => (0xC4, 0x45, 0x3C),
            IconStyle::Sheet => (0x2E, 0x9E, 0x6B),
            IconStyle::Archive => (0xB0, 0x7A, 0x2E),
            IconStyle::Code => (0x3A, 0xA6, 0xA6),
            IconStyle::Exe => (0x6B, 0x7B, 0x8C),
            IconStyle::Font => (0x9C, 0x6A, 0xDE),
            IconStyle::Disk => (0xC0, 0x44, 0x44),
            IconStyle::Generic => (0x8A, 0x8F, 0x98),
        };
        slint::Color::from_rgb_u8(r, g, b)
    }
}

/// 一个文件/目录/驱动器条目。
#[derive(Clone, Debug)]
pub struct FileEntry {
    pub name: String,
    pub path: std::path::PathBuf,
    pub is_dir: bool,
    pub size: u64,
    pub modified: Option<SystemTime>,
    /// 小写扩展名（不含点）；目录为空。
    pub ext: String,
    pub style: IconStyle,
    /// 类型徽标文本，如 "PNG 图像" / "文件夹" / "本地磁盘"。
    pub type_text: String,
    /// 驱动器视图下的附加文本（容量等）。
    pub extra: String,
    /// 是否隐藏（Windows 隐藏属性，或 Unix 以 '.' 开头的文件）。
    pub is_hidden: bool,
    /// 文件夹体积是否已完成统计（文件默认为 true，文件夹初始为 false）。
    pub dir_size_loaded: bool,
}

impl FileEntry {
    pub fn from_path(
        path: &Path,
        is_dir: bool,
        size: u64,
        modified: Option<SystemTime>,
        is_hidden: bool,
    ) -> Self {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.to_string_lossy().into_owned());
        let ext = if is_dir {
            String::new()
        } else {
            path.extension()
                .map(|e| e.to_string_lossy().to_ascii_lowercase())
                .unwrap_or_default()
        };
        let style = if is_dir {
            IconStyle::Folder
        } else {
            style_for_ext(&ext)
        };
        let type_text = if is_dir {
            "文件夹".to_string()
        } else {
            type_description(&ext)
        };
        Self {
            name,
            path: path.to_path_buf(),
            is_dir,
            size,
            modified,
            ext,
            style,
            type_text,
            extra: String::new(),
            is_hidden,
            dir_size_loaded: !is_dir, // 文件即时可用，文件夹需等待异步统计
        }
    }

    /// 驱动器条目（"此电脑"视图）。
    pub fn drive(drive: &super::drives::DriveInfo) -> Self {
        // 名称与侧边栏保持一致：卷标为空时回落类型标签。
        let label = if drive.label.is_empty() {
            drive.drive_type.label().to_string()
        } else {
            drive.label.clone()
        };
        Self {
            name: format!("{label} ({}:)", drive.letter),
            path: std::path::PathBuf::from(format!("{}:\\", drive.letter)),
            is_dir: true,
            size: drive.total_bytes,
            modified: None,
            ext: String::new(),
            style: IconStyle::Disk,
            type_text: drive.drive_type.label().to_string(),
            extra: format!(
                "{} / {}",
                format_size_bytes(drive.free_bytes),
                format_size_bytes(drive.total_bytes)
            ),
            is_hidden: false,
            dir_size_loaded: true, // 驱动器容量即时可得
        }
    }
}

/// 扩展名 → 图标风格。
pub fn style_for_ext(ext: &str) -> IconStyle {
    match ext {
        "png" | "jpg" | "jpeg" | "gif" | "bmp" | "webp" | "svg" | "ico" | "tif" | "tiff" | "psd"
        | "avif" | "heic" => IconStyle::Image,
        "mp4" | "mkv" | "avi" | "mov" | "wmv" | "flv" | "webm" | "m4v" | "rmvb" => {
            IconStyle::Video
        }
        "mp3" | "wav" | "flac" | "aac" | "ogg" | "wma" | "m4a" | "ape" | "cue" => IconStyle::Audio,
        "pdf" => IconStyle::Pdf,
        "xls" | "xlsx" | "csv" | "ods" | "et" => IconStyle::Sheet,
        "zip" | "rar" | "7z" | "tar" | "gz" | "bz2" | "xz" | "zst" | "cab" | "iso" | "tgz" => {
            IconStyle::Archive
        }
        "exe" | "msi" | "bat" | "cmd" | "com" | "scr" | "appx" | "msix" => IconStyle::Exe,
        "ttf" | "otf" | "ttc" | "woff" | "woff2" => IconStyle::Font,
        "rs" | "py" | "js" | "ts" | "tsx" | "jsx" | "c" | "h" | "cpp" | "hpp" | "cc" | "cs"
        | "java" | "go" | "rb" | "php" | "swift" | "kt" | "lua" | "sh" | "ps1" | "sql" | "json"
        | "toml" | "yaml" | "yml" | "xml" | "html" | "css" | "scss" | "vue" | "slint" | "cmake"
        | "mk" | "gradle" | "dockerfile" | "ipynb" => IconStyle::Code,
        "txt" | "md" | "doc" | "docx" | "rtf" | "log" | "nfo" => IconStyle::Doc,
        _ => IconStyle::Generic,
    }
}

/// 扩展名 → 类型徽标文本（未知类型回落 "XXX 文件"）。
pub fn type_description(ext: &str) -> String {
    let text = match ext {
        "" => "文件",
        "png" => "PNG 图像",
        "jpg" | "jpeg" => "JPG 图像",
        "gif" => "GIF 图像",
        "bmp" => "BMP 图像",
        "webp" => "WebP 图像",
        "svg" => "SVG 矢量图",
        "ico" => "图标",
        "psd" => "PSD 工程文件",
        "mp4" => "MP4 视频",
        "mkv" => "MKV 视频",
        "avi" => "AVI 视频",
        "mov" => "MOV 视频",
        "flv" => "FLV 视频",
        "mp3" => "MP3 音频",
        "wav" => "WAV 音频",
        "flac" => "FLAC 音频",
        "aac" => "AAC 音频",
        "ogg" => "OGG 音频",
        "pdf" => "PDF 文档",
        "txt" => "文本文档",
        "md" => "Markdown 文档",
        "doc" => "Word 文档",
        "docx" => "Word 文档",
        "xls" => "Excel 工作表",
        "xlsx" => "Excel 工作表",
        "csv" => "CSV 表格",
        "ppt" => "PowerPoint 演示文稿",
        "pptx" => "PowerPoint 演示文稿",
        "zip" => "ZIP 压缩文件",
        "rar" => "RAR 压缩文件",
        "7z" => "7z 压缩文件",
        "tar" => "TAR 归档",
        "gz" => "GZIP 归档",
        "iso" => "光盘镜像",
        "exe" => "应用程序",
        "msi" => "安装程序",
        "bat" => "批处理脚本",
        "cmd" => "命令脚本",
        "ps1" => "PowerShell 脚本",
        "lnk" => "快捷方式",
        "url" => "网址",
        "ttf" => "TrueType 字体",
        "otf" => "OpenType 字体",
        "rs" => "Rust 源文件",
        "py" => "Python 脚本",
        "js" => "JavaScript 脚本",
        "ts" => "TypeScript 脚本",
        "c" => "C 源文件",
        "cpp" => "C++ 源文件",
        "h" => "C 头文件",
        "java" => "Java 源文件",
        "go" => "Go 源文件",
        "json" => "JSON 文件",
        "toml" => "TOML 配置",
        "yaml" | "yml" => "YAML 配置",
        "xml" => "XML 文档",
        "html" => "HTML 文档",
        "css" => "样式表",
        "slint" => "Slint 界面",
        "sln" => "Visual Studio 解决方案",
        "dll" => "系统扩展",
        "sys" => "系统文件",
        _ => return format!("{} 文件", ext.to_uppercase()),
    };
    text.to_string()
}

/// 图标方块内的角标文本：文件夹为空，文件显示扩展名（截断到 4 字符）。
pub fn icon_text_for(entry: &FileEntry) -> String {
    if entry.is_dir {
        return String::new();
    }
    let ext = if entry.ext.is_empty() {
        entry.name.rsplit('.').next().unwrap_or("")
    } else {
        &entry.ext
    };
    ext.chars().take(4).collect()
}

/// 条目图标配色。
pub fn icon_color(entry: &FileEntry) -> slint::Color {
    entry.style.color()
}

/// 体积格式化：1024 进制，一位小数（"34.5 KB"）。
/// 文件夹在异步统计完成前返回空串，完成后显示真实体积。
pub fn format_size(entry: &FileEntry) -> String {
    if entry.is_dir {
        if entry.dir_size_loaded {
            format_size_bytes(entry.size)
        } else {
            String::new()
        }
    } else {
        format_size_bytes(entry.size)
    }
}

pub fn format_size_bytes(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KB", "MB", "GB", "TB", "PB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64;
    let mut unit = 0usize;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    // 整数部分大于 100 时不再保留小数，避免 "1023.5 MB" 这类过宽文本。
    if value >= 100.0 {
        format!("{:.0} {}", value, UNITS[unit])
    } else {
        format!("{:.1} {}", value, UNITS[unit])
    }
}

/// 修改时间格式化为本地时间 "yyyy-MM-dd HH:mm"。
pub fn format_time(time: Option<SystemTime>) -> String {
    let Some(time) = time else {
        return String::new();
    };
    super::time::format_local(time)
}
