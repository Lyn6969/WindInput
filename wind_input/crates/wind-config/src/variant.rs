//! 运行时变体探测：按自身 exe 文件名判断 dev/release 身份，开发期可用环境变量覆盖。
//!
//! 身份与编译画像解耦——无论用哪个 cargo profile 编译，产物都叫 `wind_input.exe`；
//! 被复制改名为 `wind_input_dev.exe` 后，此模块在运行时据文件名识别为 dev 变体。
use std::path::PathBuf;
use std::sync::OnceLock;

/// 纯逻辑：给定 exe 文件名主干（file_stem），判断是否 dev 变体。抽出以便单测。
fn is_dev_from_stem(stem: &str) -> bool {
    stem.ends_with("_dev")
}

/// 当前进程是否为 dev 变体。优先级：
/// 1. 环境变量 `WIND_VARIANT`（开发覆盖）——`=dev`（忽略大小写）强制 dev，其它值强制 release；
/// 2. 否则按自身 exe 文件名（去扩展名）是否以 `_dev` 结尾（生产部署以此为准）。
///
/// 仅开发用：生产部署严禁设置 `WIND_VARIANT`。用 OnceLock 缓存——进程内结果不变，只算一次。
pub fn is_dev() -> bool {
    static IS_DEV: OnceLock<bool> = OnceLock::new();
    *IS_DEV.get_or_init(|| {
        if let Ok(v) = std::env::var("WIND_VARIANT") {
            return v.eq_ignore_ascii_case("dev");
        }
        std::env::current_exe()
            .ok()
            .and_then(|p| {
                p.file_stem()
                    .map(|s| is_dev_from_stem(&s.to_string_lossy()))
            })
            .unwrap_or(false)
    })
}

/// 管道/产物后缀：dev 为 `"_dev"`，release 为 `""`。
pub fn pipe_suffix() -> &'static str {
    if is_dev() { "_dev" } else { "" }
}

/// 应用数据目录名：dev `WindInputDev`，release `WindInput`。
pub fn app_dir_name() -> &'static str {
    if is_dev() {
        "WindInputDev"
    } else {
        "WindInput"
    }
}

/// 便携模式标记文件名。
pub const PORTABLE_MARKER_NAME: &str = "portable_mode";

/// 用 OnceLock 缓存，进程内只检测一次。
pub fn is_portable() -> bool {
    static IS_PORTABLE: OnceLock<bool> = OnceLock::new();
    *IS_PORTABLE.get_or_init(|| {
        std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|d| d.join(PORTABLE_MARKER_NAME).is_file()))
            .unwrap_or(false)
    })
}

/// 便携模式下的用户数据根目录（exe 同目录/userdata）。
pub fn portable_userdata_dir() -> Option<PathBuf> {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("userdata")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dev_stem_detected() {
        assert!(is_dev_from_stem("wind_input_dev"));
        assert!(is_dev_from_stem("wind_tsf_dev"));
    }

    #[test]
    fn non_dev_stem_rejected() {
        assert!(!is_dev_from_stem("wind_input"));
        // 旧 debug 命名不再被识别为变体
        assert!(!is_dev_from_stem("wind_input_debug"));
    }
}
