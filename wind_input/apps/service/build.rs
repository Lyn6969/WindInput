// 为 wind_input.exe 嵌入 Windows 资源：图标 / 版本信息 / manifest（DPI 感知）。
// 字段值与原 Go 项目 winres/winres.json 基本一致（产品名/版权/描述/图标）。
// 版本号取 docs/VERSION（与 release tag、旧项目单一 VERSION 真值源一致），
// 而非 Cargo 版本（二者当前不同步）。仅 Windows 目标生效，其它目标为空操作。

use std::path::PathBuf;

fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    // wind_input/apps/service → 仓库根 WindInput
    let product_root = manifest_dir.join("..").join("..").join("..");
    let icon = product_root.join("wind_tsf/res/wind_input.ico");
    let version_file = product_root.join("docs/VERSION");

    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed={}", version_file.display());
    println!("cargo:rerun-if-changed={}", icon.display());

    // 版本号：X.Y.Z[-suffix] → 数值 FIXEDFILEINFO + 原始字符串
    let ver_str = std::fs::read_to_string(&version_file)
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "0.0.0".to_string());
    let core = ver_str.split('-').next().unwrap_or("0.0.0");
    let mut it = core.split('.').map(|s| s.parse::<u16>().unwrap_or(0));
    let (maj, min, pat) = (
        it.next().unwrap_or(0),
        it.next().unwrap_or(0),
        it.next().unwrap_or(0),
    );
    let ver_u64 = ((maj as u64) << 48) | ((min as u64) << 32) | ((pat as u64) << 16);

    // 调试变体：与 release 共存，产品名加「开发版」后缀以便区分。
    let debug_variant = std::env::var("CARGO_FEATURE_DEBUG_VARIANT").is_ok();
    let product_name = if debug_variant {
        "清风输入法开发版"
    } else {
        "清风输入法"
    };
    let original_filename = if debug_variant {
        "wind_input_debug.exe"
    } else {
        "wind_input.exe"
    };

    // manifest：asInvoker + PerMonitorV2 DPI 感知 + win10 + 长路径（对齐旧项目）。
    let manifest = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<assembly xmlns="urn:schemas-microsoft-com:asm.v1" manifestVersion="1.0" xmlns:asmv3="urn:schemas-microsoft-com:asm.v3">
  <assemblyIdentity type="win32" name="com.windinput.service" version="1.0.0.0"/>
  <trustInfo xmlns="urn:schemas-microsoft-com:asm.v3">
    <security>
      <requestedPrivileges>
        <requestedExecutionLevel level="asInvoker" uiAccess="false"/>
      </requestedPrivileges>
    </security>
  </trustInfo>
  <compatibility xmlns="urn:schemas-microsoft-com:compatibility.v1">
    <application>
      <supportedOS Id="{8e0f7a12-bfb3-4fe8-b9a5-48fd50a15a9a}"/>
    </application>
  </compatibility>
  <asmv3:application>
    <asmv3:windowsSettings>
      <dpiAware xmlns="http://schemas.microsoft.com/SMI/2005/WindowsSettings">true/pm</dpiAware>
      <dpiAwareness xmlns="http://schemas.microsoft.com/SMI/2016/WindowsSettings">PerMonitorV2, PerMonitor</dpiAwareness>
      <longPathAware xmlns="http://schemas.microsoft.com/SMI/2016/WindowsSettings">true</longPathAware>
    </asmv3:windowsSettings>
  </asmv3:application>
</assembly>
"#;

    let mut res = winresource::WindowsResource::new();
    res.set_icon(icon.to_str().expect("icon 路径含非 UTF-8"));
    res.set("ProductName", product_name);
    res.set("CompanyName", "清风输入法");
    res.set("FileDescription", "清风输入法服务进程");
    res.set("InternalName", "wind_input");
    res.set("OriginalFilename", original_filename);
    res.set("LegalCopyright", "Copyright © 2026 清风输入法");
    res.set("ProductVersion", &ver_str);
    res.set("FileVersion", &ver_str);
    res.set_version_info(winresource::VersionInfo::FILEVERSION, ver_u64);
    res.set_version_info(winresource::VersionInfo::PRODUCTVERSION, ver_u64);
    res.set_manifest(manifest);

    if let Err(e) = res.compile() {
        // 不致命：缺资源不影响功能，仅图标/版本元数据缺失。
        println!("cargo:warning=Windows 资源嵌入失败: {e}");
    }
}
