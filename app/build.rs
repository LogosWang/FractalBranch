use embed_manifest::{embed_manifest, new_manifest};

use std::path::PathBuf;
use std::process::Command;

fn main() {
    if std::env::var_os("CARGO_CFG_WINDOWS").is_some() {
        embed_manifest(new_manifest("FractalBranch.App")).expect("embed manifest");
        embed_icon();
    }
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=../assets/app.ico");
}

/// 把图标和版本信息编进 exe。resource.h 之类都不需要，一个 rc 就够了。
fn embed_icon() {
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let ico = manifest.join("../assets/app.ico").display().to_string().replace('\\', "/");
    let version = std::env::var("CARGO_PKG_VERSION").unwrap();
    let nums = version.replace('.', ",");
    let rc = out.join("app.rc");
    std::fs::write(
        &rc,
        format!(
            r#"1 ICON "{ico}"
1 VERSIONINFO
FILEVERSION {nums},0
PRODUCTVERSION {nums},0
BEGIN
  BLOCK "StringFileInfo"
  BEGIN
    BLOCK "040904B0"
    BEGIN
      VALUE "FileDescription", "FractalBranch desktop organizer"
      VALUE "ProductName", "FractalBranch"
      VALUE "FileVersion", "{version}"
      VALUE "ProductVersion", "{version}"
    END
  END
  BLOCK "VarFileInfo"
  BEGIN
    VALUE "Translation", 0x0409, 1200
  END
END
"#
        ),
    )
    .unwrap();
    let obj = out.join("app_res.o");
    let status = Command::new("windres").arg(&rc).args(["-O", "coff", "-o"]).arg(&obj).status();
    match status {
        Ok(s) if s.success() => println!("cargo:rustc-link-arg-bins={}", obj.display()),
        // 没有 windres 也能编译，只是 exe 没有图标。
        _ => println!("cargo:warning=windres unavailable, building without icon"),
    }
}
