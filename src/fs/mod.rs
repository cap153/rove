//! 文件系统抽象层：条目模型、目录枚举、驱动器/回收站/网络位置查询。

pub mod dir;
pub mod drives;
pub mod entry;
pub mod time;

pub use dir::{SortKey, SortState, list_directory, sort_entries};
pub use drives::{
    DriveInfo, list_drives, list_network_locations, recycle_bin_stats, quick_access_dirs,
};
pub use entry::{
    FileEntry, format_size, format_size_bytes, format_time, icon_color, icon_text_for,
};
