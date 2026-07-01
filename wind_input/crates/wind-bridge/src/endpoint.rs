//! UDS / SHM 端点路径（macOS 与 Linux 共用）
//!
//! 与 Go `internal/bridge/endpoint_darwin.go` 及 Swift `ProtocolTypes.swift`
//! 的 `BridgeEndpoints` 对齐。
use std::path::PathBuf;

/// runtime 目录：env 覆盖 → ~/Library/Application Support/WindInput{suffix} → /tmp/wind_input{suffix}
pub fn runtime_dir(suffix: &str) -> PathBuf {
    if let Ok(env) = std::env::var("WIND_INPUT_RUNTIME_DIR") {
        if !env.is_empty() {
            return PathBuf::from(env);
        }
    }
    if let Some(home) = std::env::var_os("HOME") {
        if !home.is_empty() {
            return PathBuf::from(home)
                .join("Library")
                .join("Application Support")
                .join(format!("WindInput{suffix}"));
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

/// POSIX shm 名（须以 '/' 开头，长度 <=31）
pub fn shm_name(suffix: &str) -> String {
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
}
