use std::path::PathBuf;

fn main() {
    // 产品版本唯一真源 = WindInput/docs/VERSION（与 wind_input.exe 资源、安装包一致）,
    // 供 system.* RPC 上报（system_info.version / engine / appVersion）。本 crate 位于
    // wind_input/crates/wind-rpc, 上溯三级即仓库根 WindInput。缺失时回退 CARGO_PKG_VERSION。
    let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let version_file = manifest_dir
        .join("..")
        .join("..")
        .join("..")
        .join("docs")
        .join("VERSION");
    let ver = std::fs::read_to_string(&version_file)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| std::env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "0.0.0".into()));
    println!("cargo:rustc-env=WIND_APP_VERSION={ver}");
    println!("cargo:rerun-if-changed={}", version_file.display());
}
