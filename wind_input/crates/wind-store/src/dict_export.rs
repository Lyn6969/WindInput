//! 词库多段导入导出编排（用户词库 / 临时词库 / 词频 / 候选调整）。
//!
//! 把 store 各数据域原语组合成「一个 .wdict 文件多段、按类型可选」的导入导出，对齐旧 Go
//! wind_dict 一文件多段的形态。段与引擎类型的适用关系（码表四段 / 混输仅候选调整 /
//! 拼音三段）由调用方（RPC / UI）决定，本模块只按传入的 sections 处理。

use crate::store::Store;
use crate::user_words::WordsImportCounts;
use crate::wdict::{self, DictWdict, FreqIo, WordIo};

/// 词库数据段类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DictSection {
    /// 用户词库
    UserWords,
    /// 临时词库
    TempWords,
    /// 词频
    Freq,
    /// 候选调整（shadow）
    Shadow,
}

impl DictSection {
    /// RPC/UI 稳定标识（camelCase）。
    pub fn key(&self) -> &'static str {
        match self {
            DictSection::UserWords => "userWords",
            DictSection::TempWords => "tempWords",
            DictSection::Freq => "freq",
            DictSection::Shadow => "shadow",
        }
    }

    /// wdict 段标签（`--- !<tag>`）。
    pub fn tag(&self) -> &'static str {
        match self {
            DictSection::UserWords => "words",
            DictSection::TempWords => "temp_words",
            DictSection::Freq => "freq",
            DictSection::Shadow => "shadow",
        }
    }

    /// 从 RPC key / wdict 段名解析（宽松：兼容多种写法）。
    pub fn from_key(k: &str) -> Option<Self> {
        match k {
            "userWords" | "words" | "user_words" => Some(DictSection::UserWords),
            "tempWords" | "temp_words" => Some(DictSection::TempWords),
            "freq" => Some(DictSection::Freq),
            "shadow" => Some(DictSection::Shadow),
            _ => None,
        }
    }
}

/// 单段导入结果（供 RPC 序列化）。
#[derive(Debug, Clone, Default)]
pub struct SectionImport {
    pub key: &'static str,
    /// 用户词库分类计数（仅 UserWords）。
    pub words: Option<WordsImportCounts>,
    /// 其余段的导入条数。
    pub imported: usize,
    pub skipped: usize,
}

/// 多段导入结果。
#[derive(Debug, Clone, Default)]
pub struct DictImportReport {
    pub sections: Vec<SectionImport>,
}

impl Store {
    /// 词频记录的导出编码：查用户词 / 临时词表补出音节边界，渲染成带空格的音节码。
    ///
    /// 词频表是唯一不带 boundary 的持久层（value 仅 `count + last_used`），边界只能反查。
    /// **store 层查不到系统词典**（那需要 engine），故系统词的词频记录保持扁平码——
    /// 设置页列表另有一条经引擎的反查路径（`Coordinator::freq_display_code`）覆盖得更全。
    ///
    /// 查不到即原样返回，不猜。
    fn spaced_freq_code(&self, schema: &str, code: &str, text: &str) -> String {
        let pick = |recs: Vec<crate::user_words::UserWordRecord>| {
            recs.into_iter()
                .find(|w| w.text == text)
                .map(|w| w.boundary)
                .filter(|b| *b != 0)
        };
        let b = self
            .get_user_words(schema, code)
            .ok()
            .and_then(&pick)
            .or_else(|| self.get_temp_words(schema, code).ok().and_then(&pick))
            .unwrap_or(0);
        crate::wdict::join_code_by_boundary(code, b)
    }

    /// 导出所选数据段为单个多段 wdict 文本。缺数据的段仍会写出（空段），保证文件自描述。
    /// `engine_type` 写入头部供导入时校验（防跨引擎类型误导）；调用方（RPC）解析后传入。
    pub fn export_dict_sections_wdict(
        &self,
        schema: &str,
        sections: &[DictSection],
        exported_at: &str,
        engine_type: &str,
    ) -> anyhow::Result<String> {
        let mut d = DictWdict::default();
        for sec in sections {
            match sec {
                DictSection::UserWords => d.words = Some(self.collect_user_word_rows(schema)?),
                DictSection::TempWords => {
                    let recs = self.search_temp_words_prefix(schema, "", 0)?;
                    d.temp_words = Some(
                        recs.into_iter()
                            // 带空格的音节码，与 collect_user_word_rows 同款（导入端
                            // import_temp_word_rows 会拆回 flat + 边界）。
                            .map(|r| WordIo {
                                code: wdict::join_code_by_boundary(&r.code, r.boundary),
                                text: r.text,
                                weight: r.weight,
                                count: r.count,
                                boundary: None,
                            })
                            .collect(),
                    );
                }
                DictSection::Freq => {
                    let (rows, _total) = self.list_freq_paged(schema, "", 0, 0)?;
                    d.freq = Some(
                        rows.into_iter()
                            .map(|(code, text, rec)| FreqIo {
                                // 与 words / temp_words 两段同形，输出带空格的音节码。
                                // 词频表自己不存 boundary，只能反查——这里只查得到用户词与
                                // 临时词（store 层没有系统词典），系统词的记录保持扁平。
                                // 对功能无影响：导入端会拆回扁平 key，词频表也不存边界；
                                // 带空格纯为文件可读性与三段格式一致。
                                code: self.spaced_freq_code(schema, &code, &text),
                                text,
                                count: rec.count,
                                last_used: rec.last_used,
                            })
                            .collect(),
                    );
                }
                DictSection::Shadow => d.shadow = Some(self.export_shadow_actions(schema)?),
            }
        }
        Ok(wdict::export_dict_sections(
            &d,
            exported_at,
            schema,
            engine_type,
        ))
    }

    /// 从多段 wdict 文本导入所选数据段。**只处理文件中实际存在的段**（防 replace 误清空
    /// 文件未携带的段）。replace=先清该段再导入；否则 Merge。
    ///
    /// `contract` 是**词条准入闸口**：解析之后、落库之前，对 `words` 与 `temp_words` 两段
    /// 的行做过滤与补齐（拼音词条的音节边界求解在此发生）。
    ///
    /// ★ 为什么是注入而不是在本层实现：准入判据要跑引擎的求解链，而 `wind-store` 拿不到
    /// `engine_mgr`（同 `CodePolicy` 的理由）。本层只负责「少了几行」，不关心为什么少。
    /// 被闸口丢掉的行计入该段的 `skipped`，`imported` 取闸口之后的条数。
    ///
    /// ⚠️ `freq` 段**有意不过闸**：词频表按既定决策不带 boundary 字段，且它的 code 来自
    /// 用户既有词条、不是新入库的词。`shadow` 段不是词条。
    pub fn import_dict_sections_wdict(
        &self,
        schema: &str,
        text: &str,
        sections: &[DictSection],
        replace: bool,
        contract: &mut dyn FnMut(DictSection, Vec<wdict::WordIo>) -> Vec<wdict::WordIo>,
    ) -> anyhow::Result<DictImportReport> {
        let present = wdict::sections_present(text);
        let has = |tag: &str| present.iter().any(|t| t == tag);
        let mut rep = DictImportReport::default();
        for sec in sections {
            if !has(sec.tag()) {
                continue; // 文件无此段，跳过（不清空、不计入）
            }
            match sec {
                DictSection::UserWords => {
                    let (rows, skipped) =
                        wdict::parse_words_wdict(text).map_err(|e| anyhow::anyhow!(e))?;
                    let parsed = rows.len();
                    let rows = contract(*sec, rows);
                    let rejected = parsed - rows.len();
                    if replace {
                        self.clear_user_words(schema)?;
                    }
                    let counts = self.import_user_words(schema, &rows)?;
                    rep.sections.push(SectionImport {
                        key: sec.key(),
                        words: Some(counts),
                        imported: rows.len(),
                        skipped: skipped + rejected,
                    });
                }
                DictSection::TempWords => {
                    let (rows, skipped) =
                        wdict::parse_temp_words_wdict(text).map_err(|e| anyhow::anyhow!(e))?;
                    let parsed = rows.len();
                    // 临时词同样过闸：它会随 `promote_temp_word` 晋升成用户词并**带着
                    // boundary 一起走**，放它进来等于给不变量开一条绕行路。
                    let rows = contract(*sec, rows);
                    let rejected = parsed - rows.len();
                    if replace {
                        self.clear_temp_words(schema)?;
                    }
                    let n = self.import_temp_word_rows(schema, &rows)?;
                    rep.sections.push(SectionImport {
                        key: sec.key(),
                        words: None,
                        imported: n,
                        skipped: skipped + rejected,
                    });
                }
                DictSection::Freq => {
                    let (rows, skipped) =
                        wdict::parse_freq_wdict(text).map_err(|e| anyhow::anyhow!(e))?;
                    if replace {
                        self.clear_freq(schema)?;
                    }
                    let n = self.import_freq_rows(schema, &rows)?;
                    rep.sections.push(SectionImport {
                        key: sec.key(),
                        words: None,
                        imported: n,
                        skipped,
                    });
                }
                DictSection::Shadow => {
                    let (actions, skipped) =
                        wdict::parse_shadow_wdict(text).map_err(|e| anyhow::anyhow!(e))?;
                    if replace {
                        self.clear_shadow(schema)?;
                    }
                    let n = self.import_shadow_actions(schema, &actions)?;
                    rep.sections.push(SectionImport {
                        key: sec.key(),
                        words: None,
                        imported: n,
                        skipped,
                    });
                }
            }
        }
        Ok(rep)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 恒等闸口：本模块的用例测的是多段编排（段的存在性、replace 清空、计数），
    /// 准入判据要引擎、属 `wind-webdata` 的测试范围。
    fn pass_through(_sec: DictSection, rows: Vec<WordIo>) -> Vec<WordIo> {
        rows
    }

    /// 准入闸口作用于 **words 与 temp_words 两段**，被丢掉的行如实进 `skipped`、
    /// 不进 `imported`。
    ///
    /// ★ 为什么这条测试有必要：闸口是**注入**的，接错段或漏接一段都不会有任何编译错误
    /// 或运行时报错——表现是「导入成功，但那批词悄悄进去了」。临时词那段尤其容易漏：
    /// 它会随 `promote_temp_word` 晋升成用户词，是绕过不变量的一条现成后门。
    #[test]
    fn contract_gates_both_word_sections_and_is_counted_as_skipped() {
        let path = tmp("wind_dict_contract_gate.redb");
        let s = Store::open(&path).unwrap();
        s.add_user_word("wb", "a", "好", 100, 0).unwrap();
        s.add_user_word("wb", "b", "坏", 100, 0).unwrap();
        s.learn_temp_word("wb", "c", "好", 50, 0).unwrap();
        s.learn_temp_word("wb", "d", "坏", 50, 0).unwrap();

        let all = [DictSection::UserWords, DictSection::TempWords];
        let text = s
            .export_dict_sections_wdict("wb", &all, "2026-08-22T00:00:00+08:00", "codetable")
            .unwrap();

        let path2 = tmp("wind_dict_contract_gate2.redb");
        let s2 = Store::open(&path2).unwrap();
        let mut seen: Vec<&'static str> = Vec::new();
        let rep = s2
            .import_dict_sections_wdict("wb", &text, &all, false, &mut |sec, rows| {
                seen.push(sec.key());
                rows.into_iter().filter(|r| r.text != "坏").collect()
            })
            .unwrap();

        assert_eq!(
            seen,
            vec!["userWords", "tempWords"],
            "两段都必须过闸，且闸口要知道自己在处理哪一段"
        );
        let uw = rep.sections.iter().find(|s| s.key == "userWords").unwrap();
        assert_eq!(uw.imported, 1, "imported 取闸口之后的条数");
        assert_eq!(uw.skipped, 1, "被闸口丢掉的行必须进 skipped");
        let tw = rep.sections.iter().find(|s| s.key == "tempWords").unwrap();
        assert_eq!(tw.imported, 1);
        assert_eq!(tw.skipped, 1);

        // 落库端的真凭据：被丢的词一条都不能在库里。
        assert!(
            s2.get_user_words("wb", "b").unwrap().is_empty(),
            "坏 不该入库"
        );
        assert!(s2.get_temp_words("wb", "d").unwrap().is_empty());
        assert_eq!(
            s2.get_user_words("wb", "a").unwrap().len(),
            1,
            "放行的照常入库"
        );
    }

    fn tmp(name: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(name);
        let _ = std::fs::remove_file(&p);
        p
    }

    /// **freq 段的 code 列也要带音节空格**，与 words / temp_words 两段同形；
    /// 且导入端必须拆回扁平 key。
    ///
    /// 词频表自己不存 boundary，导出时只能反查——store 层查得到用户词与临时词，
    /// 查不到系统词（那需要 engine），故系统词的词频记录保持扁平，属正常降级。
    ///
    /// ⚠️ **导入端拆空格比导出更要紧**：不拆就会写进一条永不匹配任何候选的死键
    /// （查询侧拿的是候选的扁平 code），用户只会看到「调频不生效」，且毫无痕迹。
    #[test]
    fn freq_section_uses_spaced_code_and_imports_flat() {
        let path = tmp("wind_dict_freq_spaced.redb");
        let s = Store::open(&path).unwrap();
        // 用户词提供边界：ni|hao
        s.add_user_word("pinyin", "nihao", "你好", 500, 0b101)
            .unwrap();
        s.record_freq("pinyin", "nihao", "你好").unwrap();
        // 无处可查边界 → 保持扁平
        s.record_freq("pinyin", "wubian", "无边").unwrap();

        let text = s
            .export_dict_sections_wdict("pinyin", &[DictSection::Freq], "t", "pinyin")
            .unwrap();
        assert!(
            text.contains("ni hao\t你好"),
            "freq 段的 code 应带音节空格，实际:\n{text}"
        );
        assert!(
            text.contains("wubian\t无边"),
            "查不到边界的记录保持扁平，实际:\n{text}"
        );

        // 导入到新库：key 必须是扁平的，否则这条记录永远匹配不到候选
        let p2 = tmp("wind_dict_freq_spaced2.redb");
        let s2 = Store::open(&p2).unwrap();
        s2.import_dict_sections_wdict(
            "pinyin",
            &text,
            &[DictSection::Freq],
            false,
            &mut pass_through,
        )
        .unwrap();
        let (rows, _) = s2.list_freq_paged("pinyin", "", 0, 0).unwrap();
        let mut codes: Vec<String> = rows.into_iter().map(|(c, _, _)| c).collect();
        codes.sort();
        assert_eq!(
            codes,
            vec!["nihao".to_string(), "wubian".to_string()],
            "导入端须把带空格的 code 拆回扁平 key"
        );
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&p2);
    }

    #[test]
    fn all_sections_roundtrip() {
        let path = tmp("wind_dict_sections_io.redb");
        let s = Store::open(&path).unwrap();
        s.add_user_word("wb", "a", "工", 100, 0).unwrap();
        s.on_word_selected("wb", "a", "工", 0, 0).unwrap(); // user count -> 1
        s.learn_temp_word("wb", "ab", "临时", 50, 0).unwrap();
        s.learn_temp_word("wb", "ab", "临时", 50, 0).unwrap(); // temp count -> 2
        s.record_freq("wb", "a", "工").unwrap();
        s.record_freq("wb", "a", "工").unwrap(); // freq count -> 2
        s.pin_shadow("wb", "aaaa", "恭", None, 0).unwrap();
        s.delete_shadow("wb", "bbbb", "见").unwrap();

        let all = [
            DictSection::UserWords,
            DictSection::TempWords,
            DictSection::Freq,
            DictSection::Shadow,
        ];
        let text = s
            .export_dict_sections_wdict("wb", &all, "2026-07-15T00:00:00+08:00", "codetable")
            .unwrap();
        for tag in ["--- !words", "--- !temp_words", "--- !freq", "--- !shadow"] {
            assert!(text.contains(tag), "缺段 {tag}");
        }

        // 导入新库，全段还原
        let path2 = tmp("wind_dict_sections_io2.redb");
        let s2 = Store::open(&path2).unwrap();
        let rep = s2
            .import_dict_sections_wdict("wb", &text, &all, false, &mut pass_through)
            .unwrap();
        assert_eq!(rep.sections.len(), 4);

        let uw = s2.get_user_words("wb", "a").unwrap();
        assert_eq!(uw[0].weight, 100);
        assert_eq!(uw[0].count, 1, "用户词 count 流转");
        let tw = s2.get_temp_words("wb", "ab").unwrap();
        assert_eq!(tw[0].count, 2, "临时词 count 保真");
        assert_eq!(
            s2.get_freq("wb", "a", "工").unwrap().unwrap().count,
            2,
            "词频 count 流转"
        );
        assert!(
            s2.get_shadow_rules("wb", "aaaa").unwrap().is_some(),
            "pin 还原"
        );
        assert_eq!(
            s2.get_shadow_rules("wb", "bbbb").unwrap().unwrap().deleted,
            vec!["见".to_string()],
            "del 还原"
        );
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&path2);
    }

    #[test]
    fn selective_export_and_present_guard() {
        let path = tmp("wind_dict_sel.redb");
        let s = Store::open(&path).unwrap();
        s.add_user_word("wb", "a", "工", 100, 0).unwrap();
        s.record_freq("wb", "a", "工").unwrap();

        // 只导词频
        let text = s
            .export_dict_sections_wdict("wb", &[DictSection::Freq], "t", "codetable")
            .unwrap();
        assert!(text.contains("--- !freq"));
        assert!(!text.contains("--- !words"), "未选用户词库不应写出");

        // 目标库已有用户词；即便选了 UserWords，但文件无 words 段 → replace 也不清空
        let path2 = tmp("wind_dict_sel2.redb");
        let s2 = Store::open(&path2).unwrap();
        s2.add_user_word("wb", "z", "旧", 9, 0).unwrap();
        let all = [DictSection::UserWords, DictSection::Freq];
        let rep = s2
            .import_dict_sections_wdict("wb", &text, &all, true, &mut pass_through)
            .unwrap();
        assert_eq!(rep.sections.len(), 1, "只处理文件存在的 freq 段");
        assert_eq!(rep.sections[0].key, "freq");
        assert!(
            !s2.get_user_words("wb", "z").unwrap().is_empty(),
            "文件无 words 段，replace 不应清空既有用户词"
        );
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&path2);
    }
}
