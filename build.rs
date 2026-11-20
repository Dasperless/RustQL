//! Build script for embedding Windows PE Application Manifest (`asInvoker`, Windows 10/11
//! compatibility, UTF-8 codepage) and `VS_VERSION_INFO` version metadata into release binaries.
//!
//! This prevents Windows SmartScreen, UAC, and antivirus heuristics from flagging downloaded
//! release binaries as unidentified or legacy executables, while maintaining zero external
//! build dependencies.

use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=Cargo.toml");

    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let target_env = env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();

    if target_os == "windows" && target_env == "msvc" {
        embed_windows_metadata();
    }
}

fn embed_windows_metadata() {
    let out_dir = match env::var_os("OUT_DIR") {
        Some(d) => PathBuf::from(d),
        None => return,
    };

    let pkg_version = env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "0.1.0".to_string());
    let major = env::var("CARGO_PKG_VERSION_MAJOR")
        .ok()
        .and_then(|v| v.parse::<u16>().ok())
        .unwrap_or(0);
    let minor = env::var("CARGO_PKG_VERSION_MINOR")
        .ok()
        .and_then(|v| v.parse::<u16>().ok())
        .unwrap_or(1);
    let patch = env::var("CARGO_PKG_VERSION_PATCH")
        .ok()
        .and_then(|v| v.parse::<u16>().ok())
        .unwrap_or(0);

    // 1. Write Windows Application Manifest declaring standard user execution (asInvoker),
    //    Windows 10/11 compatibility GUIDs, longPathAware, and UTF-8 activeCodePage.
    let manifest_path = out_dir.join("rustql.exe.manifest");
    let manifest_xml = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<assembly xmlns="urn:schemas-microsoft-com:asm.v1" manifestVersion="1.0">
  <assemblyIdentity
    type="win32"
    name="Dasperless.RustQL.CLI"
    version="{}.{}.{}.0"
    processorArchitecture="*"
  />
  <description>RustQL - Pure-Rust Terminal UI and CLI for SQL Queries on JSON Files</description>
  <trustInfo xmlns="urn:schemas-microsoft-com:asm.v3">
    <security>
      <requestedPrivileges>
        <requestedExecutionLevel level="asInvoker" uiAccess="false"/>
      </requestedPrivileges>
    </security>
  </trustInfo>
  <compatibility xmlns="urn:schemas-microsoft-com:compatibility.v1">
    <application>
      <!-- Windows 10 and Windows 11 -->
      <supportedOS Id="{{8e0f7a12-bfb3-4fe8-b9a5-48fd50a15a9a}}"/>
    </application>
  </compatibility>
  <application xmlns="urn:schemas-microsoft-com:asm.v3">
    <windowsSettings>
      <activeCodePage xmlns="http://schemas.microsoft.com/SMI/2019/WindowsSettings">UTF-8</activeCodePage>
      <longPathAware xmlns="http://schemas.microsoft.com/SMI/2016/WindowsSettings">true</longPathAware>
    </windowsSettings>
  </application>
</assembly>
"#,
        major, minor, patch
    );

    if fs::write(&manifest_path, manifest_xml).is_ok() {
        println!("cargo:rustc-link-arg-bins=/MANIFEST:EMBED");
        println!(
            "cargo:rustc-link-arg-bins=/MANIFESTINPUT:{}",
            manifest_path.display()
        );
    }

    // 2. Write and compile Windows VERSIONINFO resource (.rc -> .res) if rc.exe or llvm-rc is available
    let rc_path = out_dir.join("rustql.rc");
    let res_path = out_dir.join("rustql.res");
    let rc_content = format!(
        r#"#pragma code_page(65001)
1 VERSIONINFO
FILEVERSION {major},{minor},{patch},0
PRODUCTVERSION {major},{minor},{patch},0
FILEFLAGSMASK 0x3fL
FILEFLAGS 0x0L
FILEOS 0x40004L
FILETYPE 0x1L
FILESUBTYPE 0x0L
BEGIN
    BLOCK "StringFileInfo"
    BEGIN
        BLOCK "040904b0"
        BEGIN
            VALUE "CompanyName", "Darío Vargas (Dasperless)\0"
            VALUE "FileDescription", "RustQL - Pure-Rust TUI and CLI for SQL Queries on JSON Files\0"
            VALUE "FileVersion", "{pkg_version}\0"
            VALUE "InternalName", "rustql\0"
            VALUE "LegalCopyright", "Copyright (c) 2026 Darío Vargas. MIT License.\0"
            VALUE "OriginalFilename", "rustql.exe\0"
            VALUE "ProductName", "RustQL\0"
            VALUE "ProductVersion", "{pkg_version}\0"
        END
    END
    BLOCK "VarFileInfo"
    BEGIN
        VALUE "Translation", 0x409, 1200
    END
END
"#
    );

    if fs::write(&rc_path, rc_content).is_ok() {
        let compiled = Command::new("rc.exe")
            .args(["/nologo", "/fo"])
            .arg(&res_path)
            .arg(&rc_path)
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
            || Command::new("llvm-rc")
                .arg("/FO")
                .arg(&res_path)
                .arg(&rc_path)
                .status()
                .map(|s| s.success())
                .unwrap_or(false);

        if compiled && res_path.exists() {
            println!("cargo:rustc-link-arg-bins={}", res_path.display());
        }
    }
}
