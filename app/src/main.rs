//! 分形树枝：桌面分类整理和分区。
#![windows_subsystem = "windows"]

mod app;
mod desktop;
mod gfx;
mod icons;
mod install;
mod layout;
mod shell;
mod snapshot;
mod util;
mod view;
mod watch;
mod xz;

fn arg_value(args: &[String], key: &str) -> Option<String> {
    args.iter().position(|a| a == key).and_then(|i| args.get(i + 1)).cloned()
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if let Some(pid) = arg_value(&args, "--guard").and_then(|v| v.parse().ok()) {
        let crashes = arg_value(&args, "--crashes").and_then(|v| v.parse().ok()).unwrap_or(0);
        desktop::run_guard(pid, crashes);
        return;
    }
    let quiet = args.iter().any(|a| a == "--quiet");
    if args.iter().any(|a| a == "--uninstall") {
        std::process::exit(install::uninstall(quiet));
    }
    // 安装包就是程序本身，靠文件名区分。--run 用于强制按普通程序运行。
    if args.iter().any(|a| a == "--install") || (install::is_setup_exe() && !args.iter().any(|a| a == "--run")) {
        std::process::exit(install::install(quiet));
    }
    if let Some(out) = arg_value(&args, "--snapshot") {
        if let Err(e) = snapshot::run(&out) {
            util::log(format!("生成预览失败：{e}"));
            std::process::exit(2);
        }
        return;
    }
    let crashes = arg_value(&args, "--crashes").and_then(|v| v.parse().ok()).unwrap_or(0);
    let autostart = args.iter().any(|a| a == "--autostart");
    if let Err(e) = app::run(crashes, autostart) {
        util::log(format!("启动失败：{e}"));
        // 没能接管桌面，要确保原生图标还在，然后正常退出，不让守护重启。
        desktop::set_icons_visible(true);
    }
}
