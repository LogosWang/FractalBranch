use embed_manifest::{embed_manifest, new_manifest};

fn main() {
    if std::env::var_os("CARGO_CFG_WINDOWS").is_some() {
        // 默认清单已含 PerMonitorV2 和 Win8+ 兼容声明；子分层窗口依赖后者。
        embed_manifest(new_manifest("FractalBranch.DesktopMount")).expect("embed manifest");
    }
    println!("cargo:rerun-if-changed=build.rs");
}
