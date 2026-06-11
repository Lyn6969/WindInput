//! 模糊音配置
//!
//! 与 Go 版本 `wind_input/internal/engine/pinyin/fuzzy.go` 对齐。

/// 模糊音配置
#[derive(Debug, Clone, Default)]
pub struct FuzzyConfig {
    pub zh_z: bool,
    pub ch_c: bool,
    pub sh_s: bool,
    pub n_l: bool,
    pub f_h: bool,
    pub r_l: bool,
    pub an_ang: bool,
    pub en_eng: bool,
    pub in_ing: bool,
    pub ian_iang: bool,
    pub uan_uang: bool,
}
