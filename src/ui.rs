use bytesize::ByteSize;
use colored::Colorize;

use crate::disk::DiskInfo;

/// 表头与进度条下方的分隔线宽度
const RULE_WIDTH: usize = 105;

/// 打印磁盘使用情况表格
pub fn print_disk_usage_table(disks: &[DiskInfo]) {
    println!(
        "{:<15} {:<15} {:<10} {:<10} {:<10} {:<10} Usage Bar",
        "Name", "Mount", "FS", "Total", "Used", "Free"
    );
    println!("{}", "-".repeat(RULE_WIDTH));

    for disk in disks {
        println!(
            "{:<15} {:<15} {:<10} {:<10} {:<10} {:<10} {}",
            truncate(&disk.name, 14),
            truncate(&disk.mount_point, 14),
            truncate(&disk.fs, 9),
            format_bytes(disk.total),
            format_bytes(disk.used),
            format_bytes(disk.available),
            generate_progress_bar(disk.usage_percent),
        );
    }
}

/// 生成“落日余晖”渐变极细进度条
fn generate_progress_bar(percent: f64) -> String {
    const BAR_LENGTH: usize = 30;
    const FULL_BLOCK: &str = "━";
    const EMPTY_BLOCK: &str = "─";

    // 根据使用比例计算已填充 / 未填充的长度
    let filled = ((percent / 100.0) * BAR_LENGTH as f64).round() as usize;
    let empty_count = BAR_LENGTH.saturating_sub(filled);

    let mut bar = "[".bright_black().to_string();

    for i in 0..filled {
        // 根据字符在进度条中的位置着色，实现从左到右的渐变
        let pos = (i as f64 / BAR_LENGTH as f64) * 100.0;
        let (r, g, b) = sunset_gradient(pos);
        bar.push_str(&FULL_BLOCK.truecolor(r, g, b).to_string());
    }

    for _ in 0..empty_count {
        bar.push_str(&EMPTY_BLOCK.truecolor(60, 60, 60).to_string());
    }

    bar.push_str(&"]".bright_black().to_string());

    format!("{} {}", bar, style_percent(percent))
}

/// 根据使用比例给百分比数字上色：越接近满盘越醒目
fn style_percent(percent: f64) -> String {
    let text = format!("{:>5.1}%", percent);

    if percent > 90.0 {
        text.red().bold().to_string()
    } else if percent > 75.0 {
        text.yellow().to_string()
    } else {
        text.normal().to_string()
    }
}

/// 计算“落日余晖”(Sunset) 风格的 RGB 渐变色，percent 取值 0~100
fn sunset_gradient(percent: f64) -> (u8, u8, u8) {
    let p = percent.clamp(0.0, 100.0) / 100.0;

    let start = (255.0, 184.0, 0.0); // 橙
    let end = (255.0, 0.0, 128.0); // 玫红
    let lerp = |s: f64, e: f64| (s + (e - s) * p) as u8;

    (
        lerp(start.0, end.0),
        lerp(start.1, end.1),
        lerp(start.2, end.2),
    )
}

/// 将字节数格式化为易于阅读的尺寸字符串（去掉空格）
pub(crate) fn format_bytes(bytes: u64) -> String {
    ByteSize::b(bytes).to_string().replace(" ", "")
}

/// 超过最大宽度时截断并在末尾加上省略号
fn truncate(s: &str, max_width: usize) -> String {
    if s.chars().count() > max_width {
        let mut truncated: String = s.chars().take(max_width.saturating_sub(1)).collect();
        truncated.push('…');
        truncated
    } else {
        s.to_string()
    }
}
