//! 快速空格预览（QuickLook）：按类型抽取可预览内容。
//!
//! * 图片：交给 Slint 图像加载器；
//! * 文本 / 代码 / Markdown：读取文本（限 2MB）等宽显示；
//! * CSV / TSV：解析为对齐表格；Excel：经 calamine 读取首个工作表；
//! * PDF / 音视频：展示元信息并提供外部打开入口。

use std::path::Path;

use calamine::{Reader, open_workbook_auto};

/// 预览内容类别
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreviewKind {
    Image,
    Text,
    Table,
    Info,
}

impl PreviewKind {
    pub fn as_str(self) -> &'static str {
        match self {
            PreviewKind::Image => "image",
            PreviewKind::Text => "text",
            PreviewKind::Table => "table",
            PreviewKind::Info => "info",
        }
    }
}

/// 表格行（固定 8 列，超出截断）
#[derive(Debug, Clone, Default)]
pub struct PreviewRow {
    pub cells: [String; 8],
    pub count: usize,
}

/// 一次预览的完整内容
#[derive(Debug, Clone)]
pub struct Preview {
    pub kind: PreviewKind,
    pub title: String,
    pub info: String,
    pub image: Option<slint::Image>,
    pub text: Option<String>,
    pub rows: Vec<PreviewRow>,
    pub message: Option<String>,
}

/// 文本读取上限
const TEXT_LIMIT: u64 = 2 * 1024 * 1024;
/// 表格行数上限
const TABLE_ROWS: usize = 200;

const IMAGE_EXTENSIONS: [&str; 10] = [
    "png", "jpg", "jpeg", "gif", "bmp", "ico", "webp", "svg", "tif", "tiff",
];
const TEXT_EXTENSIONS: [&str; 23] = [
    "txt", "md", "markdown", "log", "json", "xml", "yml", "yaml", "toml", "ini", "cfg", "conf",
    "rs", "py", "js", "ts", "tsx", "jsx", "html", "css", "c", "cpp", "h",
];
const TABLE_EXTENSIONS: [&str; 3] = ["csv", "tsv", "xlsx"];
const INFO_EXTENSIONS: [&str; 14] = [
    "pdf", "mp3", "wav", "flac", "aac", "ogg", "m4a", "mp4", "mkv", "avi", "mov", "wmv", "exe",
    "dll",
];

/// 加载预览内容
pub fn load(path: &Path) -> std::io::Result<Preview> {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    let extension = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();

    let metadata = std::fs::metadata(path).ok();
    let size = metadata.as_ref().map(|m| m.len()).unwrap_or(0);
    let kind_label = if extension.is_empty() {
        "文件".to_string()
    } else {
        extension.to_uppercase()
    };
    let info = format!("{}  ·  {}", crate::browse::format_size(size), kind_label);

    let mut preview = Preview {
        kind: PreviewKind::Info,
        title: name,
        info,
        image: None,
        text: None,
        rows: Vec::new(),
        message: None,
    };

    if IMAGE_EXTENSIONS.contains(&extension.as_str()) {
        match slint::Image::load_from_path(path) {
            Ok(image) => {
                preview.kind = PreviewKind::Image;
                preview.image = Some(image);
            }
            Err(e) => {
                preview.kind = PreviewKind::Info;
                preview.message = Some(format!("图片加载失败：{e}"));
            }
        }
        return Ok(preview);
    }

    if TABLE_EXTENSIONS.contains(&extension.as_str()) {
        if extension == "xlsx" || extension == "xls" {
            return load_spreadsheet(path, preview);
        }
        return load_delimited(path, if extension == "tsv" { '\t' } else { ',' }, preview);
    }

    if TEXT_EXTENSIONS.contains(&extension.as_str()) {
        if size > TEXT_LIMIT {
            preview.kind = PreviewKind::Info;
            preview.message = Some("文件过大，仅显示元信息".to_string());
            return Ok(preview);
        }
        match std::fs::read_to_string(path) {
            Ok(text) => {
                preview.kind = PreviewKind::Text;
                preview.text = Some(text);
            }
            Err(_) => {
                // 非 UTF-8：按字节宽松解码
                match std::fs::read(path) {
                    Ok(bytes) => {
                        preview.kind = PreviewKind::Text;
                        preview.text = Some(String::from_utf8_lossy(&bytes).to_string());
                    }
                    Err(e) => {
                        preview.kind = PreviewKind::Info;
                        preview.message = Some(format!("读取失败：{e}"));
                    }
                }
            }
        }
        return Ok(preview);
    }

    if INFO_EXTENSIONS.contains(&extension.as_str()) {
        preview.kind = PreviewKind::Info;
        preview.message = Some(format!(
            "{} 文件暂不支持内嵌预览，可点击“用默认程序打开”。",
            extension.to_uppercase()
        ));
        return Ok(preview);
    }

    // 未知类型：探测是否为文本
    if let Ok(bytes) = std::fs::read(path) {
        let sample = &bytes[..bytes.len().min(1024)];
        if !sample.contains(&0) && std::str::from_utf8(sample).is_ok() && size <= TEXT_LIMIT {
            preview.kind = PreviewKind::Text;
            preview.text = Some(String::from_utf8_lossy(&bytes).to_string());
            return Ok(preview);
        }
    }
    preview.kind = PreviewKind::Info;
    preview.message = Some("二进制文件，无法预览".to_string());
    Ok(preview)
}

/// CSV / TSV -> 对齐表格
fn load_delimited(
    path: &Path,
    delimiter: char,
    mut preview: Preview,
) -> std::io::Result<Preview> {
    let content = std::fs::read_to_string(path)
        .map_err(|e| std::io::Error::other(format!("读取失败：{e}")))?;
    let mut rows = Vec::new();
    for (index, line) in content.lines().enumerate() {
        if index >= TABLE_ROWS {
            break;
        }
        let mut cells = std::array::from_fn(|_| String::new());
        let mut count = 0;
        for (position, value) in line.split(delimiter).enumerate() {
            if position >= 8 {
                break;
            }
            cells[position] = value.trim().to_string();
            count += 1;
        }
        rows.push(PreviewRow { cells, count });
    }
    preview.kind = PreviewKind::Table;
    preview.rows = rows;
    Ok(preview)
}

/// Excel -> 首个工作表表格
fn load_spreadsheet(path: &Path, mut preview: Preview) -> std::io::Result<Preview> {
    let mut workbook = open_workbook_auto(path)
        .map_err(|e| std::io::Error::other(format!("打开工作簿失败：{e}")))?;
    let Some(sheet) = workbook.sheet_names().first().cloned() else {
        preview.kind = PreviewKind::Info;
        preview.message = Some("工作簿为空".to_string());
        return Ok(preview);
    };
    let Ok(range) = workbook.worksheet_range(&sheet) else {
        preview.kind = PreviewKind::Info;
        preview.message = Some("无法读取工作表".to_string());
        return Ok(preview);
    };

    let mut rows = Vec::new();
    for (index, row) in range.rows().enumerate() {
        if index >= TABLE_ROWS {
            break;
        }
        let mut cells = std::array::from_fn(|_| String::new());
        let mut count = 0;
        for (position, cell) in row.iter().enumerate() {
            if position >= 8 {
                break;
            }
            cells[position] = cell.to_string();
            count += 1;
        }
        rows.push(PreviewRow { cells, count });
    }
    preview.kind = PreviewKind::Table;
    preview.rows = rows;
    preview.info = format!("{}  ·  工作表 {}", preview.info, sheet);
    Ok(preview)
}
