use info::setting;

fn main() {
    if let Err(e) = setting::run() {
        eprintln!("设置界面启动失败: {e}");
    }
}
