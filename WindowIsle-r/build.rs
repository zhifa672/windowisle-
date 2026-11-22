//! 把窗屿图标作为 RT_GROUP_ICON #1 编进 exe，供 LoadImageW(hInst, 1, ...) 取用。
//! 不引入额外依赖：直接调用 Windows SDK 的 rc.exe，把 .res 交给链接器。

use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

fn find_rc() -> Option<PathBuf> {
    let roots = [
        r"C:\Program Files (x86)\Windows Kits\10\bin",
        r"C:\Program Files\Windows Kits\10\bin",
    ];
    let mut found: Vec<PathBuf> = Vec::new();
    for root in roots {
        let Ok(entries) = fs::read_dir(root) else { continue };
        for entry in entries.flatten() {
            let dir = entry.path();
            if !dir.is_dir() {
                continue;
            }
            let rc = dir.join("x64").join("rc.exe");
            if rc.exists() {
                found.push(rc);
            }
        }
    }
    found.sort();
    found.pop()
}

fn find_windres() -> Option<PathBuf> {
    // PATH 里的 windres 或 E:\Developer_Tools\mingw64\bin\windres.exe
    if let Ok(path) = Command::new("where").arg("windres").output() {
        if path.status.success() {
            let first = String::from_utf8_lossy(&path.stdout).lines().next()?.to_string();
            return Some(PathBuf::from(first));
        }
    }
    let fallback = PathBuf::from(r"E:\Developer_Tools\mingw64\bin\windres.exe");
    fallback.exists().then_some(fallback)
}

fn main() {
    println!("cargo:rerun-if-changed=app.rc");
    println!("cargo:rerun-if-changed=assets/windowisle.ico");
    println!("cargo:rerun-if-changed=build.rs");
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let out = PathBuf::from(env::var("OUT_DIR").unwrap()).join("windowisle.res");

    // 首选 Windows SDK 的 rc.exe；没有时用 MinGW 的 windres 兜底
    //（windres 输出 COFF 格式，MSVC 链接器可直接消费）。
    if let Some(rc) = find_rc() {
        let status = Command::new(&rc)
            .arg("/nologo")
            .arg("/fo")
            .arg(&out)
            .arg("app.rc")
            .status();
        if status.map(|s| s.success()).unwrap_or(false) {
            println!("cargo:rustc-link-arg={}", out.display());
            return;
        }
        println!("cargo:warning=rc.exe 编译 app.rc 失败");
        return;
    }

    if let Some(windres) = find_windres() {
        let status = Command::new(&windres)
            .arg("--input")
            .arg("app.rc")
            .arg("--output")
            .arg(&out)
            .arg("--output-format=coff")
            .status();
        if status.map(|s| s.success()).unwrap_or(false) {
            println!("cargo:rustc-link-arg={}", out.display());
            return;
        }
        println!("cargo:warning=windres 编译 app.rc 失败");
        return;
    }

    println!("cargo:warning=rc.exe 和 windres 都未找到，跳过图标资源");
}
