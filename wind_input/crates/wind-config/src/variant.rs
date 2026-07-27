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
pub const PORTABLE_MARKER_NAME: &str = "wind_portable_mode";

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

/// 安装器写下的自定义用户数据目录配置文件名。
///
/// 真源是 `WindInput/config/app.toml` 的 `[datadir] conf_file`（安装器据此写盘），
/// 本常量是读端的同名副本——两仓无法共享常量，改名时须同步两处。
pub const DATADIR_CONF_NAME: &str = "datadir.conf";

/// 覆盖 `datadir.conf` 的完整路径。仅供测试与开发排查，生产部署严禁设置。
const DATADIR_CONF_ENV: &str = "WIND_DATADIR_CONF";

/// `datadir.conf` 的所在路径：`%LOCALAPPDATA%\WindInput[Dev]\datadir.conf`。
///
/// 与安装器 `userdata::datadir_conf_path()` 同址：那边用 `app.toml` 的 `[app] id`
/// （dev 打包时为 `WindInputDev`），本侧用 [`app_dir_name`]，两者取值一一对应，
/// 故 dev 与正式版各读各的配置、互不干扰。
fn datadir_conf_path() -> Option<PathBuf> {
    if let Ok(p) = std::env::var(DATADIR_CONF_ENV) {
        return Some(PathBuf::from(p));
    }
    dirs::data_local_dir().map(|d| d.join(app_dir_name()).join(DATADIR_CONF_NAME))
}

/// 解析 `datadir.conf` 的内容（纯函数，不碰盘，便于单测）。
///
/// 文件内容就是一行数据目录绝对路径明文（安装器 `write_datadir_conf` 直接写
/// `to_string_lossy()`，无引号无键名）。校验不通过一律返回 `None` → 调用方回落默认位置：
/// 这个路径会被当成用户全部配置与词库的家，宁可退回默认，也不能拿一个可疑路径去建目录。
fn parse_datadir_conf(content: &str) -> Option<PathBuf> {
    // 安装器用 UTF-8 无 BOM 写入，但用户可能用记事本改过（记事本另存为 UTF-8 会加 BOM）。
    let s = content.trim_start_matches('\u{feff}').trim();
    if s.is_empty() {
        return None;
    }
    let p = PathBuf::from(s);
    // 必须是绝对路径。这一条同时挡住 Windows 的**驱动器相对路径** `X:name`——
    // 它看着像绝对路径，`is_absolute()` 却为 false，解析时会落到该盘的当前目录上。
    if !p.is_absolute() {
        return None;
    }
    // `..` 在这里没有正当用途，出现即视为可疑。
    if p.components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return None;
    }
    Some(p)
}

/// 纯决策：要不要用自定义目录、用哪个。副作用（读盘、建目录）留在调用方。
///
/// `conf` 为 `None` 表示配置文件不存在或读不出来。便携优先级高于配置文件——
/// 便携包自带 `userdata`，若还去读机器全局的 `datadir.conf`，一台装过正式版的
/// 机器上便携包就会把数据写回安装版的目录，「便携」即名存实亡。
fn decide_custom_dir(portable: bool, conf: Option<&str>) -> Option<PathBuf> {
    if portable {
        return None;
    }
    parse_datadir_conf(conf?)
}

/// 用户自定义的用户数据目录（安装向导选定、由安装器写入 `datadir.conf`）。
///
/// 返回 `None` 表示「用默认位置」：便携模式、无配置文件、内容非法、目录建不出来，
/// 四种情况都归到这一个出口。用 OnceLock 缓存——进程内结果不变，只读一次盘。
///
/// **只重定向漫游那份用户数据**（`user_config_dir`）：`local_dir` 系（cache/logs/
/// state.toml）不跟随。这与安装器卸载侧的语义严格对齐——`cleanup.rs` 的
/// `user_data_dir()` 读本文件、`local_cache_dir()` 恒为 `%LOCALAPPDATA%\{id}\cache`
/// 从不读它；两侧口径若不一致，卸载就会删错目录。
pub fn custom_userdata_dir() -> Option<PathBuf> {
    static CUSTOM_DIR: OnceLock<Option<PathBuf>> = OnceLock::new();
    CUSTOM_DIR
        .get_or_init(|| {
            let conf = datadir_conf_path()?;
            // 便携判定先于读盘：读不到文件与「便携故不该读」是两件事，日志要分得清。
            let content = if is_portable() {
                None
            } else {
                std::fs::read_to_string(&conf).ok()
            };
            let Some(dir) = decide_custom_dir(is_portable(), content.as_deref()) else {
                if content.is_some() {
                    tracing::warn!(
                        "datadir.conf 内容非法，回落默认用户数据目录: {}",
                        conf.display()
                    );
                }
                return None;
            };
            // 安装器只写配置、不建目录，故首次启动时它多半还不存在。建不出来
            // （盘符不存在、无权限、被占用）就回落默认——否则配置与词库将全部读写失败。
            if let Err(e) = std::fs::create_dir_all(&dir) {
                tracing::warn!(
                    "自定义用户数据目录不可用（{}），回落默认: {}",
                    e,
                    dir.display()
                );
                return None;
            }
            tracing::info!("使用自定义用户数据目录: {}", dir.display());
            Some(dir)
        })
        .clone()
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

    /// 安装器写的就是一行裸路径，尾随换行由 trim 吃掉。
    #[test]
    fn datadir_conf_plain_path_accepted() {
        assert_eq!(
            parse_datadir_conf(r"D:\MyData\WindInput"),
            Some(PathBuf::from(r"D:\MyData\WindInput"))
        );
        assert_eq!(
            parse_datadir_conf("D:\\MyData\\WindInput\r\n"),
            Some(PathBuf::from(r"D:\MyData\WindInput"))
        );
    }

    /// 记事本另存为 UTF-8 会加 BOM，不能因此把整份用户配置判废。
    #[test]
    fn datadir_conf_bom_stripped() {
        assert_eq!(
            parse_datadir_conf("\u{feff}D:\\MyData"),
            Some(PathBuf::from(r"D:\MyData"))
        );
    }

    #[test]
    fn datadir_conf_empty_rejected() {
        assert_eq!(parse_datadir_conf(""), None);
        assert_eq!(parse_datadir_conf("   \r\n"), None);
    }

    /// 驱动器相对路径 `X:name` 看着像绝对路径，实则解析到该盘当前目录。
    /// 这是本仓踩过的坑：`Path::is_absolute` 对 `C:foo` 返回 false，正是靠它挡住。
    #[test]
    fn datadir_conf_drive_relative_rejected() {
        assert_eq!(parse_datadir_conf("C:data"), None);
        assert_eq!(parse_datadir_conf(r"data\WindInput"), None);
    }

    #[test]
    fn datadir_conf_parent_traversal_rejected() {
        assert_eq!(parse_datadir_conf(r"D:\MyData\..\..\Windows"), None);
    }

    /// 便携模式自带 userdata，绝不能再去读机器全局的 datadir.conf——
    /// 否则一个装过正式版的机器上，便携包会把数据写回安装版的目录。
    ///
    /// 断言的是纯决策函数而非 `custom_userdata_dir()`：后者的便携判定来自
    /// `current_exe()`，测试进程里恒为非便携，写成条件断言就成了永不执行的假测试。
    #[test]
    fn portable_beats_datadir_conf() {
        let conf = Some(r"D:\MyData");
        assert_eq!(decide_custom_dir(true, conf), None, "便携必须忽略配置文件");
        assert_eq!(
            decide_custom_dir(false, conf),
            Some(PathBuf::from(r"D:\MyData")),
            "非便携才认配置文件"
        );
    }

    /// 无配置文件 = 用默认位置，这是绝大多数用户的路径。
    #[test]
    fn absent_conf_falls_back_to_default() {
        assert_eq!(decide_custom_dir(false, None), None);
    }
}
