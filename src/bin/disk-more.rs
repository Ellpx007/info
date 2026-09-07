use std::path::PathBuf;

use info::browse;

fn main() {
    // 第一个参数作为起始目录，缺省为当前目录
    let start = std::env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));

    if let Err(e) = browse::run(&start) {
        eprintln!("浏览失败: {e}");
    }
}
