//! 内部目录变量 `${APP_DIR}` / `${USER_DATA}` / `${LOCAL_DATA}` 的**单一真相源**。
//!
//! 这三个目录都含用户名或安装位置，硬编码进短语/脚本即不可移植，故给出变量形式。
//!
//! 消费方有两个，语法域不同但变量集必须一致：
//! - CLI 文件参数：`wind_input backup create ${USER_DATA}\x.zip`（见 `cli_util::resolve_path`）
//! - 命令栏短语字符串：`$CC("[打开安装目录]", open("${APP_DIR}"))`（见 `wind_cmdbar` 词法层）
//!
//! 两侧若各维护一份映射，新增变量时必然漏一边，用户侧表现为「同样的写法这里能用那里不能」，
//! 且没有任何编译期约束会提示——故映射只此一份（[`DIR_TABLE`]），两侧一律委托。
//!
//! 目录语义（便携模式、`datadir.conf` 自定义数据目录重定向）全部沿用
//! [`Config::user_config_dir`] / [`Config::local_dir`]，不在此另立判定。

use crate::config::Config;
use std::path::{Path, PathBuf};

/// 变量名 → 目录定位函数。**新增变量只改这张表**，名字集与解析逻辑天然同步，
/// 不存在「认得这个名字但展不开」的漂移窗口。
type DirResolver = fn() -> Option<PathBuf>;
const DIR_TABLE: &[(&str, DirResolver)] = &[
    // 程序安装目录（wind_input.exe 所在目录）
    ("APP_DIR", app_dir),
    // 漫游用户数据目录（%APPDATA%\WindInput[Dev]，受便携模式 / datadir.conf 影响）
    ("USER_DATA", Config::user_config_dir),
    // 本机用户数据目录（%LOCALAPPDATA%\WindInput[Dev]，**不**跟随自定义数据目录）
    ("LOCAL_DATA", Config::local_dir),
];

fn app_dir() -> Option<PathBuf> {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
}

/// 支持的内部目录变量名，顺序即错误提示与帮助文档里的展示顺序。
pub fn dir_var_names() -> Vec<&'static str> {
    DIR_TABLE.iter().map(|(k, _)| *k).collect()
}

/// 支持的变量名连成一行，供错误提示直接嵌入（如「支持: APP_DIR / USER_DATA / LOCAL_DATA」）。
pub fn dir_var_help() -> String {
    dir_var_names().join(" / ")
}

/// 把内部目录变量名解析为绝对目录。
///
/// `None` 有两种成因，调用方通常无需区分（都得不到路径），需要区分时先用 [`is_dir_var`]：
/// 该名字不是内部目录变量，或它是变量但目录当前无法定位。
pub fn dir_var(name: &str) -> Option<PathBuf> {
    DIR_TABLE
        .iter()
        .find(|(k, _)| *k == name)
        .and_then(|(_, f)| f())
}

/// [`dir_var`] 的字符串形式，供路径拼接与词法层直接嵌入。
pub fn dir_var_str(name: &str) -> Option<String> {
    dir_var(name).map(|p| p.to_string_lossy().into_owned())
}

/// 该名字是否为内部目录变量（不关心目录此刻是否可定位）。
///
/// 词法层据此区分「不是变量」（原样保留字面，如旧模板的 `${YC}`）与
/// 「是变量」（展开；定位失败时的处置另说）——两者混为一谈会让写错的变量名
/// 静默拼出错误路径。
pub fn is_dir_var(name: &str) -> bool {
    DIR_TABLE.iter().any(|(k, _)| *k == name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_names_are_recognized_and_unknown_rejected() {
        for name in dir_var_names() {
            assert!(is_dir_var(name), "{name} 应被识别为内部目录变量");
        }
        // 大小写与下划线变体不得被认成变量（写错就该原样留字面，好让用户看出来）
        assert!(!is_dir_var("APPDIR"));
        assert!(!is_dir_var("app_dir"));
        assert!(!is_dir_var("APP_DIR "));
        assert!(!is_dir_var(""));
    }

    #[test]
    fn app_dir_resolves_to_exe_parent() {
        // APP_DIR 不依赖用户目录探测，任何平台的测试进程都能定位，故可强断言实际取值。
        // （USER_DATA / LOCAL_DATA 依赖 HOME/APPDATA，在部分 CI 环境不可定位，
        //   对它们只锁「被识别」而不锁「必有值」。）
        let want = std::env::current_exe()
            .unwrap()
            .parent()
            .unwrap()
            .to_path_buf();
        assert_eq!(dir_var("APP_DIR").expect("APP_DIR 应可定位"), want);
        assert_eq!(dir_var_str("APP_DIR").unwrap(), want.to_string_lossy());
    }

    #[test]
    fn unknown_name_yields_none() {
        assert!(dir_var("NOPE").is_none());
        assert!(dir_var_str("NOPE").is_none());
    }

    #[test]
    fn help_lists_every_supported_name() {
        let help = dir_var_help();
        for name in dir_var_names() {
            assert!(help.contains(name), "帮助串漏了 {name}");
        }
    }
}
