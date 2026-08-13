//! UDS / SHM 端点路径（macOS 与 Linux 共用）
//!
//! 与 Go `internal/bridge/endpoint_darwin.go` 及 Swift `ProtocolTypes.swift`
//! 的 `BridgeEndpoints` 对齐。
use std::path::PathBuf;

/// 把**管道后缀**（`""` / `"_dev"`，见 `wind_config::variant::pipe_suffix`）映射成
/// **变体后缀**（`""` / `"Dev"`）。
///
/// 这两种后缀是两套命名风格，混用会让 dev 变体的两端各说各话：
/// 调用方一路传下来的是管道风格的 `_dev`（它给 `wind_input_ctrl_dev` 这类管道名用），
/// 但 Application Support 下的目录名与 SHM 名走的是变体风格 `Dev` ——
/// Swift `BridgeEndpoints.variantSuffix`、`wind_config::variant::app_dir_name()`
/// 和 `scripts/mac/dev.sh` 三处都是 `WindInputDev`。
///
/// 曾经此处直接把 `_dev` 拼进目录名，于是服务把 socket bind 到 `WindInput_dev/`，
/// 而 .app 去 `WindInputDev/` 连——dev 变体的 bridge/SHM 全程握不上手。
/// 改名须三处同步。
///
/// 未知后缀（测试/临时场景的 `_debug` 等）原样透传：它们不是正式变体，
/// 静默改名只会让排查现场更难看懂。
fn variant_suffix(pipe_suffix: &str) -> &str {
    match pipe_suffix {
        "_dev" => "Dev",
        other => other,
    }
}

/// runtime 目录：env 覆盖 → ~/Library/Application Support/WindInput{变体后缀} → /tmp/wind_input{管道后缀}
///
/// 两段刻意用不同风格的后缀：Application Support 段是**面向用户的应用目录名**
/// （与 .app、dev.sh、用户配置目录同名），/tmp 段是无 HOME 时的兜底，
/// 沿用管道风格的 snake 命名。
pub fn runtime_dir(suffix: &str) -> PathBuf {
    if let Ok(env) = std::env::var("WIND_INPUT_RUNTIME_DIR") {
        if !env.is_empty() {
            return PathBuf::from(env);
        }
    }
    if let Some(home) = std::env::var_os("HOME") {
        if !home.is_empty() {
            let dir = variant_suffix(suffix);
            return PathBuf::from(home)
                .join("Library")
                .join("Application Support")
                .join(format!("WindInput{dir}"));
        }
    }
    PathBuf::from(format!("/tmp/wind_input{suffix}"))
}

pub fn request_socket_path(suffix: &str) -> PathBuf {
    runtime_dir(suffix).join("bridge.sock")
}

pub fn push_socket_path(suffix: &str) -> PathBuf {
    runtime_dir(suffix).join("bridge_push.sock")
}

/// POSIX shm 名（须以 '/' 开头，长度 <=31）。
///
/// 后缀与 socket 目录同源（`variant_suffix`），对齐 Swift
/// `CandidatePanelHost` 的 `"/WindInput_SHM\(BridgeEndpoints.variantSuffix)"`；
/// 不一致的话 dev 变体开出的是两段互不相干的共享内存，候选框永远拿不到帧。
pub fn shm_name(suffix: &str) -> String {
    let suffix = variant_suffix(suffix);
    let name = format!("/WindInput_SHM{suffix}");
    debug_assert!(name.len() <= 31, "shm name too long: {name}");
    name
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // 串行化所有动 env 的测试（env 是进程全局，默认并行会互扰）
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// 保存并在 Drop 时恢复指定 env 变量（panic-safe）
    struct EnvRestore {
        keys: Vec<(&'static str, Option<std::ffi::OsString>)>,
    }
    impl EnvRestore {
        fn capture(keys: &[&'static str]) -> Self {
            Self {
                keys: keys.iter().map(|k| (*k, std::env::var_os(k))).collect(),
            }
        }
    }
    impl Drop for EnvRestore {
        fn drop(&mut self) {
            for (k, v) in &self.keys {
                match v {
                    Some(val) => unsafe { std::env::set_var(k, val) },
                    None => unsafe { std::env::remove_var(k) },
                }
            }
        }
    }

    #[test]
    fn runtime_dir_honors_env_override() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _restore = EnvRestore::capture(&["WIND_INPUT_RUNTIME_DIR", "HOME"]);
        unsafe { std::env::set_var("WIND_INPUT_RUNTIME_DIR", "/tmp/wind_test_rt") };
        assert_eq!(
            runtime_dir(""),
            std::path::PathBuf::from("/tmp/wind_test_rt")
        );
        assert_eq!(
            request_socket_path(""),
            std::path::PathBuf::from("/tmp/wind_test_rt/bridge.sock")
        );
        assert_eq!(
            push_socket_path(""),
            std::path::PathBuf::from("/tmp/wind_test_rt/bridge_push.sock")
        );
    }

    #[test]
    fn runtime_dir_tmp_fallback_with_suffix() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _restore = EnvRestore::capture(&["WIND_INPUT_RUNTIME_DIR", "HOME"]);
        unsafe { std::env::remove_var("WIND_INPUT_RUNTIME_DIR") };
        unsafe { std::env::remove_var("HOME") };
        assert_eq!(
            runtime_dir("_debug"),
            std::path::PathBuf::from("/tmp/wind_input_debug")
        );
        assert_eq!(
            request_socket_path("_debug"),
            std::path::PathBuf::from("/tmp/wind_input_debug/bridge.sock")
        );
        assert_eq!(
            push_socket_path("_debug"),
            std::path::PathBuf::from("/tmp/wind_input_debug/bridge_push.sock")
        );
    }

    #[test]
    fn shm_name_has_leading_slash_and_suffix() {
        assert_eq!(shm_name(""), "/WindInput_SHM");
        assert_eq!(shm_name("_debug"), "/WindInput_SHM_debug");
    }

    /// dev 变体的目录名必须是 `WindInputDev`（变体风格），不是把管道后缀 `_dev`
    /// 直接拼上去的 `WindInput_dev`。三处对齐：Swift `BridgeEndpoints.runtimeDir`、
    /// `wind_config::variant::app_dir_name()`、`scripts/mac/dev.sh` 的 `APP_SUPPORT`。
    ///
    /// 这条曾经是真实故障：服务 bind 在 `WindInput_dev/bridge.sock`，
    /// .app 连 `WindInputDev/bridge.sock`，dev 变体的 IPC 从来没通过。
    #[test]
    fn dev_runtime_dir_uses_app_dir_style_suffix() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _restore = EnvRestore::capture(&["WIND_INPUT_RUNTIME_DIR", "HOME"]);
        unsafe { std::env::remove_var("WIND_INPUT_RUNTIME_DIR") };
        unsafe { std::env::set_var("HOME", "/Users/tester") };

        assert_eq!(
            runtime_dir("_dev"),
            std::path::PathBuf::from("/Users/tester/Library/Application Support/WindInputDev")
        );
        assert_eq!(
            request_socket_path("_dev"),
            std::path::PathBuf::from(
                "/Users/tester/Library/Application Support/WindInputDev/bridge.sock"
            )
        );
        assert_eq!(
            push_socket_path("_dev"),
            std::path::PathBuf::from(
                "/Users/tester/Library/Application Support/WindInputDev/bridge_push.sock"
            )
        );
    }

    /// 正式变体（空后缀）不受映射影响 —— 已装机用户的 socket 路径不能因这次对齐而变。
    #[test]
    fn release_runtime_dir_unchanged() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _restore = EnvRestore::capture(&["WIND_INPUT_RUNTIME_DIR", "HOME"]);
        unsafe { std::env::remove_var("WIND_INPUT_RUNTIME_DIR") };
        unsafe { std::env::set_var("HOME", "/Users/tester") };

        assert_eq!(
            runtime_dir(""),
            std::path::PathBuf::from("/Users/tester/Library/Application Support/WindInput")
        );
    }

    /// SHM 名与 socket 目录同源：dev 走变体风格，未知后缀原样透传。
    #[test]
    fn shm_name_maps_dev_suffix_like_socket_dir() {
        assert_eq!(shm_name("_dev"), "/WindInput_SHMDev");
        // POSIX shm 名上限 31 字节，映射后不得越界。
        assert!(shm_name("_dev").len() <= 31);
    }
}
