use std::path::{Path, PathBuf};
use walkdir::WalkDir;

/// 当前目录下的一个条目（文件或子目录）
#[derive(Debug, Clone)]
pub struct FileEntry {
    pub name: String,
    pub path: PathBuf,
    pub is_dir: bool,
    /// 子目录为其整棵子树的递归总大小；文件为其自身大小
    pub size: u64,
}

/// 列出目录下的所有条目，按递归大小降序排列
/// 
///
/// 子目录的 `size` 是对它整棵子树递归求和的结果（du/ncdu 风格），
/// 这样能一眼看出哪个目录最占空间。计算按需进行：只统计当前目录的
/// 各项，不会预扫整棵磁盘树。
pub fn list_directory(dir: &Path) -> Vec<FileEntry> {
    let mut entries: Vec<FileEntry> = Vec::new();

    let read_dir = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(_) => return entries,
    };

    for item in read_dir.flatten() {
        let path = item.path();
        let name = item.file_name().to_string_lossy().to_string();

        // symlink 不跟随、不统计，避免循环和越界
        let file_type = match item.file_type() {
            Ok(ft) if !ft.is_symlink() => ft,
            _ => continue,
        };

        if file_type.is_dir() {
            let size = dir_size(&path);
            entries.push(FileEntry {
                name,
                path,
                is_dir: true,
                size,
            });
        } else if file_type.is_file() {
            let size = item.metadata().map(|m| m.len()).unwrap_or(0);
            entries.push(FileEntry {
                name,
                path,
                is_dir: false,
                size,
            });
        }
    }

    entries.sort_by_key(|a| std::cmp::Reverse(a.size));
    entries
}

/// 递归统计一个目录整棵子树的总大小；无权限/出错的项记为 0
fn dir_size(dir: &Path) -> u64 {
    WalkDir::new(dir)
        .follow_links(false)
        .into_iter()
        .filter_map(Result::ok)
        .filter_map(|entry| {
            if entry.file_type().is_file() {
                entry.metadata().ok().map(|m| m.len())
            } else {
                None
            }
        })
        .sum()
}
