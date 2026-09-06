use std::collections::HashSet;
use sysinfo::Disks;

/// 单块磁盘的展示数据，由 ui 模块负责渲染
pub struct DiskInfo {
    pub name: String,
    pub mount_point: String,
    pub fs: String,
    pub total: u64,
    pub used: u64,
    pub available: u64,
    pub usage_percent: f64,
}

/// 收集所有需要展示的磁盘信息（已应用过滤规则）
pub fn collect_disks() -> Vec<DiskInfo> {
    let disks = Disks::new_with_refreshed_list();
    let mut seen_devices = HashSet::new();

    disks
        .iter()
        .filter_map(|disk| collect_disk(disk, &mut seen_devices))
        .collect()
}

/// 把一条 sysinfo 磁盘记录转换为 DiskInfo；需要跳过的返回 None
fn collect_disk(disk: &sysinfo::Disk, seen_devices: &mut HashSet<String>) -> Option<DiskInfo> {
    let fs = disk.file_system().to_string_lossy().to_lowercase();
    let mount_point = disk.mount_point().to_string_lossy().to_string();
    let device_name = disk.name().to_string_lossy().to_string();
    let total = disk.total_space();

    if should_skip(&mount_point, &fs, total) {
        return None;
    }

    // Btrfs 去重：同一物理设备只保留最先扫描到的挂载点（通常是 / 根目录）
    if fs == "btrfs" && !seen_devices.insert(device_name.clone()) {
        return None;
    }

    let available = disk.available_space();
    let used = total.saturating_sub(available);
    let usage_percent = (used as f64 / total as f64) * 100.0;

    Some(DiskInfo {
        name: device_name,
        mount_point,
        fs,
        total,
        used,
        available,
        usage_percent,
    })
}

/// 判断是否应跳过此磁盘：容量为 0、临时/Snap 挂载点、或只读虚拟文件系统
fn should_skip(mount_point: &str, fs: &str, total: u64) -> bool {
    total == 0
        || mount_point.starts_with("/tmp/.mount_")
        || mount_point.starts_with("/snap")
        || fs.contains("squashfs")
        || fs.contains("fuse")
        || fs.contains("overlay")
}
