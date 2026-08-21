//! Web 设置数据 RPC：schema/dict/temp/freq/shadow/stats/theme/phrase 命名空间。
//!
//! 经 wind-rpc 的 `CoreRpc::data_rpc` 转发到此（service 的 RpcCore 适配）。
//! 方法名与前端 `contract.ts` 1:1 一致。
//!
//! 接入进度：契约全部数据域方法均接通真实 store/engine/theme：
//! - schema.*（含三层合并 getConfig/saveConfig/resetConfig/setDictEnabled/delete）
//! - dict.*（含 encode/genPinyin 反查出码）、temp.*、freq.*、shadow.*、phrase.*、stats.*、theme.*
//! - `schema.references` 暂返 `{}`（删除安全检查未用，前端宽松消费）；
//!   无 store/themes 时各方法返回合法空集（降级，不报错）。
//!
//! 本 crate 由 wind-coordinator 的 webdata 模块独立而来：它只经
//! [`WebDataHost`] 窄面（定义在 wind-coordinator::web_host，16 方法）触宿主，
//! 把 wind-transfer/fontdb 等重依赖挡在 Android 闭包之外。

use serde_json::{Value, json};
use wind_coordinator::handle_quick_format::QuickFormatEdit;
/// [`WebData::apply_pinyin_entry_contract`] 的处置统计，逐项对应导入预览的三档
/// （见 `docs/design/pinyin-entry-boundary-contract.md` §5）。
#[derive(Debug, Default)]
pub struct EntryContractStats {
    /// 不合法、未入库的行数。
    pub rejected: usize,
    /// 求解成功、补上了边界的行数。
    pub filled: usize,
    /// 多解已按读音权重择一的行数（是 `filled` 的子集）。
    pub ambiguous: usize,
    /// 入库了但仍无边界的行数（码超 64 字节等）。
    pub no_boundary: usize,
    /// 被拒行的样例（至多 5 条）——UI 要能让用户看出「是不是选错了文件」。
    pub samples: Vec<String>,
}
use wind_coordinator::web_host::WebDataHost;

/// 解析方案的权威引擎类型（schema.toml 的 engine.type 可能为空，
/// 此时按 Schema::is_pinyin/is_mixed 依据默认词库类型推断）。
fn resolve_engine_type(s: &wind_config::Schema) -> &'static str {
    if s.engine.engine_type.eq_ignore_ascii_case("english") {
        // 必须显式分流：english 走 `is_pinyin()` 的兜底分支（主词库 dict_type 是 "english"
        // 而非 "rime_pinyin"）会落到 "codetable"，方案列表就会把英文标成「码表」。
        "english"
    } else if s.is_mixed() {
        "mixed"
    } else if s.is_pinyin() {
        "pinyin"
    } else {
        "codetable"
    }
}

fn str_param<'a>(p: &'a Value, key: &str) -> anyhow::Result<&'a str> {
    p.get(key)
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("缺少参数 {}", key))
}

/// 读取 `sections` 参数（字符串数组）→ 词库数据段；缺省返回 None（由调用方取引擎默认）。
fn dict_sections_param(p: &Value) -> Option<Vec<wind_store::dict_export::DictSection>> {
    let arr = p.get("sections")?.as_array()?;
    Some(
        arr.iter()
            .filter_map(|v| v.as_str())
            .filter_map(wind_store::dict_export::DictSection::from_key)
            .collect(),
    )
}

/// 引擎类型 → 中文标签（错误/提示文案用）。
fn engine_type_label(t: &str) -> &'static str {
    match t {
        "pinyin" => "拼音",
        "mixed" => "混输",
        "codetable" => "码表",
        _ => "未知",
    }
}

/// 按引擎类型的默认适用数据段。
///
/// ⚠️ 与设置页的子标签（`wind-setting` 的 `pages::dict::state::tabs_for_domain_at`）**同源
/// 但不等价**，且这条跨仓契约**没有编译期约束**：
///
/// - 必须同增同减的是「**某类数据在这个引擎下存不存在**」。只改一边的表现是「设置页看得见、
///   导出文件里没有」，用户直到还原时才发现丢了数据。拼音曾是三段（无候选调整），因为那时
///   拼音下调位被整体屏蔽；置顶放开后两边同步补上 Shadow。
/// - 允许不同的是「**这里从宽**」：英文域在设置页不显示临时词库（那张表恒空），这里仍走
///   `_` 分支带上它。导出多一段空数据无害，少一段则是丢数据——两个方向的代价不对称。
fn default_dict_sections(engine_type: &str) -> Vec<wind_store::dict_export::DictSection> {
    use wind_store::dict_export::DictSection::*;
    match engine_type {
        "mixed" => vec![Shadow],
        _ => vec![UserWords, TempWords, Freq, Shadow],
    }
}

/// 多段导入结果 → JSON（`{sections:[{key, added/updated/unchanged | imported, skipped}]}`）。
fn dict_report_json(rep: &wind_store::dict_export::DictImportReport) -> Value {
    let sections: Vec<Value> = rep
        .sections
        .iter()
        .map(|s| {
            let mut o = serde_json::Map::new();
            o.insert("key".into(), json!(s.key));
            if let Some(w) = &s.words {
                o.insert("added".into(), json!(w.added));
                o.insert("updated".into(), json!(w.updated));
                o.insert("unchanged".into(), json!(w.unchanged));
            } else {
                o.insert("imported".into(), json!(s.imported));
            }
            o.insert("skipped".into(), json!(s.skipped));
            Value::Object(o)
        })
        .collect();
    json!({ "sections": sections })
}

fn i32_param(p: &Value, key: &str) -> i32 {
    p.get(key).and_then(|v| v.as_i64()).unwrap_or(0) as i32
}

fn usize_param(p: &Value, key: &str, default: usize) -> usize {
    p.get(key)
        .and_then(|v| v.as_u64())
        .map(|n| n as usize)
        .unwrap_or(default)
}

/// 解析排序参数。sortBy 在 valid_fields 中时返回 (字段名, is_desc)，否则返回 None（保持原顺序）。
fn parse_sort<'a>(params: &'a Value, valid_fields: &[&str]) -> Option<(&'a str, bool)> {
    let by = params.get("sortBy")?.as_str()?;
    if !valid_fields.contains(&by) {
        return None;
    }
    let desc = params.get("sortOrder").and_then(|v| v.as_str()) == Some("desc");
    Some((by, desc))
}

/// 本地今天日期 "YYYY-MM-DD"（统计摘要的参照点）。
fn today_str() -> String {
    chrono::Local::now()
        .date_naive()
        .format("%Y-%m-%d")
        .to_string()
}

/// 设置页数据 RPC 本体：全部方法为默认实现，只能经 [`WebDataHost`] 窄面触宿主——
/// 默认方法看不见 Coordinator 字段，窄面约束由编译期保证。调用方
/// `use 本 trait` 后在 Coordinator 上直接调 `web_data_rpc`。
pub trait WebDataRpc: WebDataHost {
    /// 枚举本机字体：返回 (family, display_name)。family 为匹配/渲染用名(通常英文),
    /// display_name 优先取该字体含 CJK 字符的本地化名(如"微软雅黑"),否则同 family。
    /// 首次调用扫描系统字体目录（fontdb），开销可接受（设置页打开字体选择时一次）。
    fn list_font_families(&self) -> Vec<(String, String)> {
        fn has_cjk(s: &str) -> bool {
            s.chars().any(|c| {
                let u = c as u32;
                (0x4E00..=0x9FFF).contains(&u) // CJK 统一表意
                    || (0x3400..=0x4DBF).contains(&u) // 扩展 A
                    || (0xF900..=0xFAFF).contains(&u) // 兼容表意
            })
        }
        let mut db = fontdb::Database::new();
        db.load_system_fonts();
        // family(英文) → 显示名;同 family 保留首个本地化名。BTreeMap 去重 + 按 family 升序。
        let mut map: std::collections::BTreeMap<String, String> = std::collections::BTreeMap::new();
        for face in db.faces() {
            let Some((family, _)) = face.families.first() else {
                continue;
            };
            if family.is_empty() {
                continue;
            }
            let display = face
                .families
                .iter()
                .map(|(n, _)| n)
                .find(|n| has_cjk(n))
                .cloned()
                .unwrap_or_else(|| family.clone());
            map.entry(family.clone()).or_insert(display);
        }
        map.into_iter().collect()
    }

    /// 数据类 RPC 总分派。方法名以 `<ns>.<method>` 形式分组；未知方法返回 Err。
    fn web_data_rpc(&self, method: &str, params: &Value) -> anyhow::Result<Value> {
        match method {
            // ── schema.* ─────────────────────────────────────────
            "schema.list" => self.web_schema_list(params),
            "schema.layouts" => self.web_schema_layouts(),
            "schema.active" => Ok(json!({ "id": self.engine_mgr().active_schema_id() })),
            "schema.setActive" => {
                let ok = self.engine_mgr().switch_schema(str_param(params, "id")?);
                if ok {
                    self.sync_chaizi_assets(); // 拆字库/字根字体随活跃方案切换
                    self.sync_comment_dicts(); // 方案专属注释库（`schemas` 字段）同理
                }
                Ok(json!({ "ok": ok }))
            }
            // ── 方案配置编辑（三层合并：默认 ← 方案文件 ← override 层）──
            "schema.getConfig" => self.web_schema_get_config(params),
            "schema.saveConfig" => self.web_schema_save_config(params),
            "schema.resetConfig" => self.web_schema_reset_config(params),
            "schema.setDictEnabled" => self.web_schema_set_dict_enabled(params),
            // 失效方案的引擎缓存（未加载时安全 no-op）：CLI `schema set/reset` 后
            // 调用，让 override 改动在下次使用该方案时按新配置重建生效。
            "schema.invalidate" => {
                let id = str_param(params, "id")?;
                self.engine_mgr().invalidate_schema(id);
                Ok(json!({ "ok": true }))
            }
            // 全量强制重建词库缓存（CLI `schema rebuild`）：失效全部引擎后删缓存产物。
            // 面向「指纹判新鲜但内容需重建」的场景（如解析器修复后存量缓存静默过期）。
            "schema.rebuildCache" => {
                let (removed, failed) = self.engine_mgr().rebuild_all_caches();
                Ok(json!({ "removed": removed, "failed": failed }))
            }
            // 重启服务（CLI `wind_input restart`）：与托盘菜单同一条 request_restart
            // 流程。延迟发信号——main 收到即释放单例并 exit，先让本条 RPC 响应写回
            // 客户端，避免 CLI 读响应与进程退出竞争。
            "system.restart" => {
                std::thread::spawn(|| {
                    std::thread::sleep(std::time::Duration::from_millis(200));
                    wind_coordinator::request_restart();
                });
                Ok(json!({ "ok": true }))
            }
            "schema.delete" => self.web_schema_delete(params),
            "schema.references" => Ok(json!({})), // 引用关系（删除安全检查）：暂返空，前端宽松消费
            "scheme.exportPackage" => self.web_scheme_export_package(params),
            "scheme.importPackage" => self.web_scheme_import_package(params),
            "scheme.previewImport" => self.web_scheme_preview_import(params),
            // 文本信封（kind = "schema_text"）：形状与上面两个 path 版完全一致，
            // 设置端复用同一个确认对话框。
            "scheme.previewImportText" => self.web_scheme_preview_import_text(params),
            "scheme.importText" => self.web_scheme_import_text(params),

            // ── backup.*（整机备份，wind-transfer::backup）───────
            "backup.create" => self.web_backup_create(params),
            "backup.inspect" => self.web_backup_inspect(params),
            "backup.restore" => self.web_backup_restore(params),

            // ── dict.*（用户词库，redb 持久化）────────────────────
            "dict.listPaged" => self.web_dict_list_paged(params),
            "dict.search" => self.web_dict_search(params),
            "dict.add" => self.web_dict_add(params),
            "dict.update" => self.web_dict_update(params),
            "dict.remove" => self.web_dict_remove(params),
            "dict.clear" => self.web_dict_clear(params),
            "dict.stats" => self.web_dict_stats(),
            // 加词自动出码：按方案类型选拼音/五笔规则（reverse 反查表）。
            "dict.encode" => self.web_dict_encode(params),
            // 批量出码：纯词列表导入按批调用（设置端每批约 1000 词）。
            "dict.encodeWords" => self.web_dict_encode_words(params),
            "dict.genPinyin" => {
                // 取码要按**真实文本**算：转义形态里的 `\` `n` 会被当成两个待取码的字符。
                let text = str_param(params, "text")?;
                Ok(json!(self.gen_pinyin_word(&store_text(text))))
            }
            "dict.export" => self.web_dict_export(params),
            "dict.import" => self.web_dict_import(params),
            "dict.previewImport" => self.web_dict_preview_import(params),

            // ── temp.*（临时词，redb）─────────────────────────────
            "temp.list" => self.web_temp_list(params),
            "temp.promote" => self.web_temp_promote(params),
            "temp.remove" => self.web_temp_remove(params),
            "temp.promoteAll" => self.web_temp_promote_all(params),
            "temp.clear" => self.web_temp_clear(params),

            // ── freq.*（用户词频，redb 持久化）───────────────────
            "freq.listPaged" => self.web_freq_list_paged(params),
            "freq.delete" => self.web_freq_delete(params),
            "freq.clear" => self.web_freq_clear(params),

            // ── shadow.*（影子规则，redb 持久化）─────────────────
            "shadow.list" => self.web_shadow_list(params),
            "shadow.pin" => self.web_shadow_pin(params),
            "shadow.delete" => self.web_shadow_delete(params),
            "shadow.removeRule" => self.web_shadow_remove_rule(params),
            "shadow.addRule" => self.web_shadow_add_rule(params),

            // ── phrase.*（用户短语，全局，redb 持久化）──────────
            "phrase.list" => self.web_phrase_list(),
            "phrase.add" => self.web_phrase_add(params),
            "phrase.update" => self.web_phrase_update(params),
            "phrase.remove" => self.web_phrase_remove(params),
            "phrase.setEnabled" => self.web_phrase_set_enabled(params),
            "phrase.resetDefault" => self.web_phrase_reset(),
            "phrase.listSystem" => self.web_phrase_list_system(),
            "phrase.listUser" => self.web_phrase_list_user(params),
            "phrase.export" => self.web_phrase_export(),
            "phrase.import" => self.web_phrase_import(params),
            "phrase.previewImportText" => self.web_phrase_preview_import_text(params),
            "phrase.importText" => self.web_phrase_import_text(params),
            "phrase.resetSystem" => self.web_phrase_reset_system(),

            // ── quick.*（快捷输入格式表的用户调整，全局，redb 持久化）──
            // 基表（模板与出厂顺序）在 system.quick.toml，**RPC 一律不写它**：
            // 那会抢走高级用户手写文件的所有权，见 handle_quick_format 模块文档。
            "quick.list" => self.web_quick_list(),
            "quick.move" => self.web_quick_move(params),
            "quick.setEnabled" => self.web_quick_set_enabled(params),
            "quick.resetEntry" => self.web_quick_reset_entry(params),
            "quick.resetKind" => self.web_quick_reset_kind(params),
            "quick.vars" => self.web_quick_vars(),
            "quick.add" => self.web_quick_add(params),
            "quick.setText" => self.web_quick_set_text(params),
            "quick.delete" => self.web_quick_delete(params),
            "quick.export" => self.web_quick_export(),
            "quick.import" => self.web_quick_import(params),
            "quick.previewImport" => self.web_quick_preview_import(params),

            // ── stats.*（输入统计，redb 每日聚合）────────────────
            "stats.summary" => self.web_stats_summary(),
            "stats.daily" => self.web_stats_daily(params),
            "stats.clear" => self.web_stats_clear(),
            "stats.pruneBefore" => self.web_stats_prune(params),

            // ── theme.* ──────────────────────────────────────────
            "theme.list" => self.web_theme_list(),
            "theme.preview" => self.web_theme_preview(params),
            "theme.getText" => self.web_theme_get_text(params),
            "theme.delete" => self.web_theme_delete(params),
            "theme.importFromText" => self.web_theme_import_text(params),
            "theme.importFromUrl" => {
                anyhow::bail!("URL 导入未启用（features.theme.import_url=false）")
            }

            other => anyhow::bail!("unknown method: {}", other),
        }
    }

    /// `schema.list` —— 方案全集（含元信息），供设置页方案管理与主方案下拉。
    ///
    /// `params.includeHidden = true` 时把隐藏方案（英文、快符这类 `[schema].hidden`）
    /// 也列出来，供设置页的「显示特殊方案」开关。默认不含——它们对大多数用户是噪音。
    /// 每项都带 `hidden` 字段，前端据此区分该行能配什么（隐藏方案配引导键，
    /// 普通方案配直达热键）。
    fn web_schema_list(&self, params: &Value) -> anyhow::Result<Value> {
        use std::collections::HashMap;

        let include_hidden = params
            .get("includeHidden")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        // 设置页方案下拉的显示顺序（三段式，段间固定先后）：
        //   ① 已启用的拼音方案（数量少、最常用，置顶）
        //   ② 其余已启用方案，按 config.schema.available 配置顺序
        //   ③ 未启用方案（磁盘扫到但不在 available），按类型分组「拼音→码表→混输→英文」，组内按 id 字典序
        // 底层 installed_schemas() 仍返回 id 字典序全集（做稳定去重锚点），排序只在此展示层重排。

        // 已启用方案 → 配置位置索引，供段①②保持配置顺序。
        let available = self.engine_mgr().available_schemas();
        let avail_pos: HashMap<&str, usize> = available
            .iter()
            .enumerate()
            .map(|(i, s)| (s.as_str(), i))
            .collect();

        // 未启用段的类型分组顺序：拼音→码表→混输→英文。
        // 英文排最后：它是给需要长时间打英文的人用的，绝大多数人不会启用。
        fn type_rank(t: &str) -> i64 {
            match t {
                "pinyin" => 0,
                "codetable" => 1,
                "mixed" => 2,
                "english" => 3,
                _ => 4,
            }
        }

        // 复合排序键 (段号, 段内主键, id)：段号先分档；段内主键在启用段是配置位置、
        // 在未启用段是类型档；id 仅在未启用段做字典序 tiebreak（启用段位置唯一，不参与）。
        let mut rows: Vec<((u8, i64, String), Value)> = self
            .engine_mgr()
            .installed_schemas()
            .iter()
            // 隐藏方案默认不列；已启用的隐藏方案是例外——用户既然把它放进了 available，
            // 藏起来只会让人找不到怎么停用它。
            .filter(|id| {
                include_hidden
                    || avail_pos.contains_key(id.as_str())
                    || !self.engine_mgr().schema_is_hidden(id)
            })
            .map(|id| {
                // 取合并后 Schema 一次，带出方案元信息（备注/版本/图标/作者），供设置页方案列表与详情显示。
                let merged = self.engine_mgr().schema_merged(id);
                let engine_type = merged
                    .as_ref()
                    .map(resolve_engine_type)
                    .unwrap_or("codetable");
                let info = merged.as_ref().map(|s| &s.schema);
                let item = json!({
                    "id": id,
                    "name": self.engine_mgr().schema_name(id),
                    "engineType": merged.as_ref().map(resolve_engine_type),
                    "scheme": merged.as_ref().map(|s| {
                        if resolve_engine_type(s) == "pinyin" {
                            s.engine.pinyin.scheme.clone()
                        } else {
                            String::new()
                        }
                    }).unwrap_or_default(),
                    // 用户目录存在同名 schema.toml 即视为用户方案（可删除）；否则内置。
                    "builtin": !self.engine_mgr().is_user_schema(id),
                    // 隐藏方案（英文/快符）：设置页据此决定该行显示什么、能配什么。
                    "hidden": self.engine_mgr().schema_is_hidden(id),
                    // 是否为 **overlay 方案**（方案文件带 `[overlay]` 段）：可由引导键/直达
                    // 热键临时叠加进入（快符/生僻字那类）。设置页据此枚举 `special:<id>`
                    // 动词的可选项，并决定要不要显示 overlay 那一节配置。
                    //
                    // ⚠️ 与 `hidden` **正交**，不可互推：hidden 只说「不列进方案切换列表」，
                    // 隐藏的码表方案也可能只是 mix 成员、没有 overlay
                    // 生命周期；反过来一个 overlay 方案也可以不隐藏。
                    "overlay": merged.as_ref().is_some_and(|s| s.overlay.is_some()),
                    "description": info.map(|i| i.description.clone()).unwrap_or_default(),
                    "version": info.map(|i| i.version.clone()).unwrap_or_default(),
                    "icon_label": info.map(|i| i.icon_label.clone()).unwrap_or_default(),
                    "author": info.map(|i| i.author.clone()).unwrap_or_default(),
                });

                let key = match avail_pos.get(id.as_str()) {
                    // 已启用：拼音置顶(段0)，其余(段1)，段内按配置位置。
                    Some(&pos) => {
                        let seg = if engine_type == "pinyin" { 0 } else { 1 };
                        (seg, pos as i64, String::new())
                    }
                    // 未启用(段2)：按类型档 + id 字典序。
                    None => (2, type_rank(engine_type), id.clone()),
                };
                (key, item)
            })
            .collect();

        rows.sort_by(|a, b| a.0.cmp(&b.0));
        let items: Vec<Value> = rows.into_iter().map(|(_, item)| item).collect();
        Ok(json!(items))
    }

    /// 双拼布局清单：合并扫描安装目录与用户目录的 `schemas/shuangpin/*.toml`，
    /// 返回 `[{id, name}]`，供设置页"双拼布局"下拉动态取值（取代前端硬编码）。
    fn web_schema_layouts(&self) -> anyhow::Result<Value> {
        let items: Vec<Value> = self
            .engine_mgr()
            .shuangpin_layouts()
            .into_iter()
            .map(|(id, name)| json!({ "id": id, "name": name }))
            .collect();
        Ok(json!(items))
    }

    fn web_dict_list_paged(&self, params: &Value) -> anyhow::Result<Value> {
        let schema = str_param(params, "schemaId")?;
        let schema = self.engine_mgr().data_schema_id(schema); // 拼音族折叠到 "pinyin"
        let prefix = params
            .get("prefix")
            .and_then(|v| v.as_str())
            .or_else(|| params.get("query").and_then(|v| v.as_str()))
            .unwrap_or("");
        let limit = usize_param(params, "limit", 50);
        let offset = usize_param(params, "offset", 0);
        let store = self
            .user_store()
            .ok_or_else(|| anyhow::anyhow!("无持久化存储"))?;
        // 编码前缀须用扁平码（key 是扁平的），用户可能照着列表显示的 `ni hao` 来搜。
        // 下面的**词条内容**搜索仍用原串——那是拿汉字去匹配 text，与音节空格无关。
        let (code_prefix, _) = wind_store::wdict::split_spaced_code(prefix);
        let mut all = store.search_user_words_prefix(&schema, &code_prefix, 0)?;
        // 并入两类补充命中（与上面的编码前缀取并集，去重）：
        //   ① 词条内容包含搜索词（拿汉字匹配 text，用原串）
        //   ② **编码中段包含**搜索词（用拆过的扁平码）—— 前缀扫描只能命中开头，
        //      `haoya` 搜 `ya` 一条也出不来，而用户并不知道搜索框只认前缀。
        // 两者共用这一次全量扫描，仅在有搜索词时才付出该代价。
        if !prefix.is_empty() {
            let q = prefix.to_lowercase();
            let code_q = code_prefix.to_lowercase();
            let seen: std::collections::HashSet<(String, String)> = all
                .iter()
                .map(|w| (w.code.clone(), w.text.clone()))
                .collect();
            for w in store.search_user_words_prefix(&schema, "", 0)? {
                let hit = w.text.to_lowercase().contains(&q)
                    || (!code_q.is_empty() && w.code.to_lowercase().contains(&code_q));
                if hit && !seen.contains(&(w.code.clone(), w.text.clone())) {
                    all.push(w);
                }
            }
        }
        let total = all.len();
        // 有 sortBy 时在切片前排序，实现跨页全局排序
        if let Some((by, desc)) = parse_sort(params, &["code", "text", "weight"]) {
            all.sort_by(|a, b| {
                let ord = match by {
                    "weight" => a.weight.cmp(&b.weight),
                    "text" => a.text.cmp(&b.text),
                    _ => a.code.cmp(&b.code),
                };
                if desc { ord.reverse() } else { ord }
            });
        }
        let items: Vec<Value> = all
            .into_iter()
            .skip(offset)
            .take(limit)
            .map(word_item)
            .collect();
        Ok(json!({ "items": items, "total": total }))
    }

    fn web_dict_search(&self, params: &Value) -> anyhow::Result<Value> {
        let schema = str_param(params, "schemaId")?;
        let schema = self.engine_mgr().data_schema_id(schema); // 拼音族折叠到 "pinyin"
        let query = str_param(params, "query").unwrap_or("");
        let limit = usize_param(params, "limit", 50);
        let store = self
            .user_store()
            .ok_or_else(|| anyhow::anyhow!("无持久化存储"))?;
        // 列表显示的是带空格的音节码（见 word_item），用户很可能照着搜。key 是扁平的，
        // 不拆则 `ni ha` 一条也匹配不到。拆完仍是前缀语义（`ni ha` → `niha`）。
        let (query, _) = wind_store::wdict::split_spaced_code(query);
        let items: Vec<Value> = store
            .search_user_words_prefix(&schema, &query, limit)?
            .into_iter()
            .map(word_item)
            .collect();
        Ok(json!(items))
    }

    fn web_dict_add(&self, params: &Value) -> anyhow::Result<Value> {
        let schema = str_param(params, "schemaId")?;
        let schema = self.engine_mgr().data_schema_id(schema); // 拼音族折叠到 "pinyin"
        let (code, text) = (str_param(params, "code")?, str_param(params, "text")?);
        // 设置页传的是转义形态，还原成存储域（真实文本）后再落库/比对。见 [`store_text`]。
        let text = &store_text(text);
        let weight = i32_param(params, "weight");
        let store = self
            .user_store()
            .ok_or_else(|| anyhow::anyhow!("无持久化存储"))?;
        let (code, boundary) = self.normalize_add_code(&schema, code, text);
        store.add_user_word(&schema, &code, text, weight, boundary)?;
        Ok(json!({ "ok": true }))
    }

    /// 规范化设置端提交的编码，返回 `(扁平 code, boundary)`。
    ///
    /// 两条边界来源，按可信度取：
    /// 1. **用户在码里打了空格**（`ni hao`）—— 显式声明的切分，直接采信。同时必须拆成扁平
    ///    码：带空格的串若原样落库，key 就成了 `ni hao`，前缀查询再也匹配不到它。
    /// 2. 无空格 → 交给 [`wind_engine::Engine::resolve_boundary`] 的四层求解链。
    ///
    /// ★ 这里原本是一个私有的 `infer_boundary_for`，只做「手输码 == 引擎推导码则借用其
    /// 切分」这一条判据——**那正是求解链的层 3**，是重复实现。收敛掉之后手输码额外获得
    /// 层 2（词典点查）与层 4（字数约束求解）两条来源：以前用户手打 `xianning` +「西安宁」
    /// 因推导码对不上而拿 0，现在能解出 `xi|an|ning`。
    ///
    /// ⚠️ **有意不在这里拒收** `Unresolvable`：本函数服务于设置页手动加词，那是用户明确
    /// 的意图，静默拒绝会变成「点了保存没反应」。合法性拦截只放在导入闸口（那里有预览
    /// 可以如实告知）。此处非法码照旧落 `boundary = 0`，与改动前等价。
    fn normalize_add_code(&self, schema: &str, code: &str, text: &str) -> (String, u64) {
        let (flat, explicit) = wind_store::wdict::split_spaced_code(code);
        if explicit != 0 {
            return (flat, explicit);
        }
        let res = self
            .engine_mgr()
            .resolve_boundaries(schema, &[(flat.as_str(), text)])
            .into_iter()
            .next()
            .unwrap_or(wind_engine::BoundaryResolution::NoInfo);
        // 探测器：手输码落在契约之外时留一条痕。**不改变行为**（照旧落 boundary=0），
        // 它是「有多少手输码求解不出」的唯一观测点——这类词的简拼会退化成 DAG 现猜，
        // 而那在歧义码上必错，用户侧的表现是「加了词但简拼召不回」，无从追溯。
        //
        // ⚠️ 用 `debug!` 而非 `info!`：本行含 code 与 text，属用户词库内容。
        // INFO 生产默认开启，不得记录用户输入类信息。
        if res == wind_engine::BoundaryResolution::Unresolvable {
            tracing::debug!(
                "加词：拼音码求解失败，按无边界落库 code={} text={}",
                flat,
                text
            );
        }
        (flat, res.boundary())
    }

    fn web_dict_update(&self, params: &Value) -> anyhow::Result<Value> {
        let schema = str_param(params, "schemaId")?;
        let schema = self.engine_mgr().data_schema_id(schema); // 拼音族折叠到 "pinyin"
        let (code, text) = (str_param(params, "code")?, str_param(params, "text")?);
        // text 在这里是**查找键**（update_user_word_weight 按它匹配记录），
        // 不还原就查不到 → 「改了没反应」。见 [`store_text`]。
        let text = &store_text(text);
        let weight = i32_param(params, "weight");
        let store = self
            .user_store()
            .ok_or_else(|| anyhow::anyhow!("无持久化存储"))?;
        // 存在则改权重（boundary 沿用）；不存在则新增（upsert 语义）。
        // code 同样先规范化，否则带空格的码既查不到既有记录、又会新增出带空格的 key。
        let (code, boundary) = self.normalize_add_code(&schema, code, text);
        if !store.update_user_word_weight(&schema, &code, text, weight)? {
            store.add_user_word(&schema, &code, text, weight, boundary)?;
        }
        Ok(json!({ "ok": true }))
    }

    fn web_dict_remove(&self, params: &Value) -> anyhow::Result<Value> {
        let schema = str_param(params, "schemaId")?;
        let schema = self.engine_mgr().data_schema_id(schema); // 拼音族折叠到 "pinyin"
        let (code, text) = (str_param(params, "code")?, str_param(params, "text")?);
        let store = self
            .user_store()
            .ok_or_else(|| anyhow::anyhow!("无持久化存储"))?;
        // 列表项的 code 带音节空格（见 word_item），而 key 是扁平的——不拆就删不掉。
        // text 同理：列表给的是转义形态、key 是真实文本，不还原一样删不掉。
        let (code, _) = wind_store::wdict::split_spaced_code(code);
        store.remove_user_word(&schema, &code, &store_text(text))?;
        Ok(json!({ "ok": true }))
    }

    fn web_dict_clear(&self, params: &Value) -> anyhow::Result<Value> {
        let schema = str_param(params, "schemaId")?;
        let schema = self.engine_mgr().data_schema_id(schema); // 拼音族折叠到 "pinyin"
        let store = self
            .user_store()
            .ok_or_else(|| anyhow::anyhow!("无持久化存储"))?;
        let n = store.clear_user_words(&schema)?;
        Ok(json!(n))
    }

    /// 导出方案数据为单个多段 wdict 文件。`sections` 参数选类型；缺省按引擎类型取默认适用段。
    fn web_dict_export(&self, params: &Value) -> anyhow::Result<Value> {
        let schema_id = str_param(params, "schemaId")?;
        let data_schema = self.engine_mgr().data_schema_id(schema_id); // 拼音族折叠到 "pinyin"
        let etype = self
            .engine_mgr()
            .schema_merged(schema_id)
            .map(|s| resolve_engine_type(&s))
            .unwrap_or("codetable");
        let store = self
            .user_store()
            .ok_or_else(|| anyhow::anyhow!("无持久化存储"))?;
        let sections = dict_sections_param(params).unwrap_or_else(|| default_dict_sections(etype));
        // engine_type 写入文件头部，供导入时校验来源（防五笔词库导入拼音致编码错乱）。
        let content = store.export_dict_sections_wdict(
            &data_schema,
            &sections,
            &chrono::Local::now().to_rfc3339(),
            etype,
        )?;
        Ok(json!({ "content": content }))
    }

    /// 导入。WindDict 多段：`sections` 选要应用的类型（默认文件所含全部段）；
    /// Rime/TSV：仅用户词库。返回 `{sections:[...]}` 逐段结果。
    /// 目标方案是否拼音族——决定归一化策略与是否执行准入判据。
    fn target_is_pinyin(&self, schema_id: &str) -> bool {
        self.engine_mgr()
            .schema_engine_type(schema_id)
            .map(|t| t == "pinyin")
            .unwrap_or(false)
    }

    /// 按目标引擎挑编码归一化策略（`wind-store` 拿不到 `engine_mgr`，故由这一层决定）。
    fn code_policy_for(&self, schema_id: &str) -> wind_store::import_formats::CodePolicy {
        if self.target_is_pinyin(schema_id) {
            wind_store::import_formats::CodePolicy::PINYIN
        } else {
            wind_store::import_formats::CodePolicy::CODETABLE
        }
    }

    /// 拼音词条入库契约：补齐音节边界、剔除不合法行。
    /// 见 `docs/design/pinyin-entry-boundary-contract.md`。
    ///
    /// 非拼音方案原样放行——码表词组码没有音节语义，`boundary = 0` 是**正确语义**。
    ///
    /// ★ 求解出的边界通过**把 code 写回带空格形态**传给落库端，而不是给
    /// `import_user_words` 加参数：空格本就是本仓的边界载体，`split_spaced_code` 在
    /// 落库时会把它拆回 `flat key + boundary`。既有管线一行都不用改。
    fn apply_pinyin_entry_contract(
        &self,
        schema_id: &str,
        rows: Vec<wind_store::wdict::WordIo>,
    ) -> (Vec<wind_store::wdict::WordIo>, EntryContractStats) {
        let mut st = EntryContractStats::default();
        if !self.target_is_pinyin(schema_id) {
            return (rows, st);
        }
        // 层 1：文件自带空格的行，切分是词库作者写下的真值 —— 直接采信，不进求解。
        let mut flats: Vec<String> = Vec::with_capacity(rows.len());
        let mut pending: Vec<usize> = Vec::new();
        for (i, r) in rows.iter().enumerate() {
            let (flat, explicit) = wind_store::wdict::split_spaced_code(&r.code);
            if explicit == 0 {
                pending.push(i);
            }
            flats.push(flat);
        }
        let pairs: Vec<(&str, &str)> = pending
            .iter()
            .map(|&i| (flats[i].as_str(), rows[i].text.as_str()))
            .collect();
        let solved = self.engine_mgr().resolve_boundaries(schema_id, &pairs);

        let mut verdicts: Vec<Option<wind_engine::BoundaryResolution>> = vec![None; rows.len()];
        for (k, &i) in pending.iter().enumerate() {
            verdicts[i] = solved.get(k).copied();
        }
        let mut out = Vec::with_capacity(rows.len());
        for (i, mut r) in rows.into_iter().enumerate() {
            let Some(v) = verdicts[i] else {
                out.push(r); // 层 1
                continue;
            };
            if v == wind_engine::BoundaryResolution::Unresolvable {
                st.rejected += 1;
                if st.samples.len() < 5 {
                    st.samples.push(format!("{} {}", r.code, r.text));
                }
                continue;
            }
            let b = v.boundary();
            if b != 0 {
                // ★ 走 `WordIo::boundary` 而不是把 code 改写成带空格形态：空格载体
                // 表达不了单音节（`xian` 的 0b1 经 join→split 退化为 0），单字词的
                // 补齐会静默失效、而这里还会把它算进 filled。
                r.boundary = Some(b);
                st.filled += 1;
            } else {
                st.no_boundary += 1;
            }
            if matches!(v, wind_engine::BoundaryResolution::Ambiguous(_)) {
                st.ambiguous += 1;
            }
            out.push(r);
        }
        (out, st)
    }

    fn web_dict_import(&self, params: &Value) -> anyhow::Result<Value> {
        use wind_store::dict_export::DictSection;
        use wind_transfer::merge::Strategy;
        let schema_id = str_param(params, "schemaId")?;
        let data_schema = self.engine_mgr().data_schema_id(schema_id);
        let content = str_param(params, "content")?;
        let replace = Strategy::from_param(
            params
                .get("strategy")
                .and_then(|v| v.as_str())
                .unwrap_or(""),
        ) == Strategy::Replace;
        let store = self
            .user_store()
            .ok_or_else(|| anyhow::anyhow!("无持久化存储"))?;
        let fmt = wind_store::import_formats::detect_dict_format(content);
        if fmt == wind_store::import_formats::DictFormat::WindDict {
            // 校验来源引擎类型：防跨类型误导（如五笔词库导入拼音方案致编码错乱）。
            let target = self
                .engine_mgr()
                .schema_merged(schema_id)
                .map(|s| resolve_engine_type(&s))
                .unwrap_or("codetable");
            if let Some(src) = wind_store::wdict::read_header_field(content, "engine_type")
                && !src.is_empty()
                && src != target
            {
                return Err(anyhow::anyhow!(
                    "该文件为「{}」类型词库，与当前「{}」方案不一致，导入会导致编码错乱，已阻止。",
                    engine_type_label(&src),
                    engine_type_label(target),
                ));
            }
            // 文件实际含的段 ∩ 用户所选（缺省=全部所含段）。
            let present: Vec<DictSection> = wind_store::wdict::sections_present(content)
                .iter()
                .filter_map(|t| DictSection::from_key(t))
                .collect();
            let sections: Vec<DictSection> = match dict_sections_param(params) {
                Some(sel) => sel.into_iter().filter(|s| present.contains(s)).collect(),
                None => present,
            };
            let rep =
                store.import_dict_sections_wdict(&data_schema, content, &sections, replace)?;
            Ok(dict_report_json(&rep))
        } else {
            // Rime/TSV：仅用户词库。
            let (_fmt, rows, skipped) = wind_store::import_formats::parse_words_auto(
                content,
                self.code_policy_for(schema_id),
            )
            .map_err(|e| anyhow::anyhow!(e))?;
            let (rows, contract) = self.apply_pinyin_entry_contract(schema_id, rows);
            if replace {
                store.clear_user_words(&data_schema)?;
            }
            let c = store.import_user_words(&data_schema, &rows)?;
            Ok(json!({ "sections": [ {
                "key": "userWords",
                "added": c.added,
                "updated": c.updated,
                "unchanged": c.unchanged,
                // 解析期跳过的（列数不足/乱码）与准入判据拒收的合并计入 skipped，
                // 另按类别分列——UI 要能告诉用户「少了的词是为什么少的」。
                "skipped": skipped + contract.rejected,
                "rejected": contract.rejected,
                "boundaryFilled": contract.filled,
                "boundaryAmbiguous": contract.ambiguous,
                "noBoundary": contract.no_boundary,
            } ] }))
        }
    }

    /// 导入预览。回报文件含哪些段及各段计数（用户词库另带 willAdd/willUpdate/unchanged/samples）。
    fn web_dict_preview_import(&self, params: &Value) -> anyhow::Result<Value> {
        use wind_store::dict_export::DictSection;
        let schema_id = str_param(params, "schemaId")?;
        let data_schema = self.engine_mgr().data_schema_id(schema_id);
        let content = str_param(params, "content")?;
        let store = self
            .user_store()
            .ok_or_else(|| anyhow::anyhow!("无持久化存储"))?;
        let fmt = wind_store::import_formats::detect_dict_format(content);
        if fmt == wind_store::import_formats::DictFormat::WindDict {
            let present = wind_store::wdict::sections_present(content);
            let mut arr: Vec<Value> = Vec::new();
            for tag in &present {
                let Some(sec) = DictSection::from_key(tag) else {
                    continue;
                };
                match sec {
                    DictSection::UserWords => {
                        let (rows, sk) = wind_store::wdict::parse_words_wdict(content)
                            .map_err(|e| anyhow::anyhow!(e))?;
                        let (c, samples) = store.preview_import_user_words(&data_schema, &rows)?;
                        arr.push(json!({
                            "key": "userWords", "count": rows.len(),
                            "willAdd": c.added, "willUpdate": c.updated, "unchanged": c.unchanged,
                            "skipped": sk, "samples": samples,
                        }));
                    }
                    DictSection::TempWords => {
                        let (rows, sk) = wind_store::wdict::parse_temp_words_wdict(content)
                            .map_err(|e| anyhow::anyhow!(e))?;
                        arr.push(json!({ "key": "tempWords", "count": rows.len(), "skipped": sk }));
                    }
                    DictSection::Freq => {
                        let (rows, sk) = wind_store::wdict::parse_freq_wdict(content)
                            .map_err(|e| anyhow::anyhow!(e))?;
                        arr.push(json!({ "key": "freq", "count": rows.len(), "skipped": sk }));
                    }
                    DictSection::Shadow => {
                        let (rows, sk) = wind_store::wdict::parse_shadow_wdict(content)
                            .map_err(|e| anyhow::anyhow!(e))?;
                        arr.push(json!({ "key": "shadow", "count": rows.len(), "skipped": sk }));
                    }
                }
            }
            // 来源方案/引擎（文件头部）+ 与目标方案的兼容性（引擎类型一致或来源未知）。
            let target = self
                .engine_mgr()
                .schema_merged(schema_id)
                .map(|s| resolve_engine_type(&s))
                .unwrap_or("codetable");
            let source_engine =
                wind_store::wdict::read_header_field(content, "engine_type").unwrap_or_default();
            let source_schema =
                wind_store::wdict::read_header_field(content, "schema_id").unwrap_or_default();
            let compatible = source_engine.is_empty() || source_engine == target;
            Ok(json!({
                "format": "winddict", "sections": arr,
                "sourceSchema": source_schema, "sourceEngine": source_engine,
                "targetEngine": target, "compatible": compatible,
            }))
        } else {
            let (fmt2, rows, skipped) = wind_store::import_formats::parse_words_auto(
                content,
                self.code_policy_for(schema_id),
            )
            .map_err(|e| anyhow::anyhow!(e))?;
            let total = rows.len();
            // 预览必须跑与导入**完全相同**的准入判据，否则「预计入库 N 条」会骗人。
            let (rows, contract) = self.apply_pinyin_entry_contract(schema_id, rows);
            let (c, samples) = store.preview_import_user_words(&data_schema, &rows)?;
            // Rime/TSV 无来源引擎元信息，兼容性交由用户判断（不拦截）。
            Ok(json!({
                "format": fmt2.as_str(),
                "sections": [ {
                    "key": "userWords", "count": total,
                    "willAdd": c.added, "willUpdate": c.updated, "unchanged": c.unchanged,
                    "skipped": skipped + contract.rejected, "samples": samples,
                    // 三档处置的计数（见设计文档 §5）：rejected 一律不入库，
                    // noBoundary 是「入库了但仍缺边界」，ambiguous 是「多解已择一」。
                    "rejected": contract.rejected,
                    "rejectedSamples": contract.samples,
                    "boundaryFilled": contract.filled,
                    "boundaryAmbiguous": contract.ambiguous,
                    "noBoundary": contract.no_boundary,
                } ],
                "compatible": true,
            }))
        }
    }

    fn web_dict_stats(&self) -> anyhow::Result<Value> {
        let store = match self.user_store() {
            Some(s) => s,
            None => return Ok(json!([])),
        };
        let mut out = Vec::new();
        for id in self.engine_mgr().available_schemas().iter() {
            let user_words = store
                .search_user_words_prefix(id, "", 0)
                .map(|v| v.len())
                .unwrap_or(0);
            let temp_words = store
                .search_temp_words_prefix(id, "", 0)
                .map(|v| v.len())
                .unwrap_or(0);
            // 候选调整按 data_schema_id 归属（拼音族折叠到 "pinyin"），与写端 `candidate_op`
            // 和读端 `shadow.list` 同源。此前直传原始 id：双拼方案（`shuangpin_*`）折叠后
            // 才是 "pinyin"，拿原始 id 去查恒得 0 条——设置页的规则计数于是永远显示 0。
            //
            // ⚠️ 上面 user_words / temp_words **刻意保持原始 id**：它们走
            // `write_data_schema_id` 的按来源分桶，与 shadow 不是同一套归属规则，别顺手改。
            let shadow_rules = store
                .list_shadow_rules(&self.engine_mgr().data_schema_id(id))
                .map(|v| {
                    v.iter()
                        .map(|(_, r)| r.pinned.len() + r.deleted.len())
                        .sum::<usize>()
                })
                .unwrap_or(0);
            out.push(json!({
                "schemaId": id,
                "name": self.engine_mgr().schema_name(id),
                "userWords": user_words,
                "tempWords": temp_words,
                "shadowRules": shadow_rules,
            }));
        }
        Ok(json!(out))
    }

    fn web_schema_get_config(&self, params: &Value) -> anyhow::Result<Value> {
        let id = str_param(params, "id")?;
        match self.engine_mgr().schema_merged(id) {
            Some(schema) => {
                let etype = resolve_engine_type(&schema);
                let mut v = serde_json::to_value(schema)?;
                // 确保 engine.type 为解析后的权威类型（schema.toml 可能未显式声明）
                if let Some(eng) = v.get_mut("engine").and_then(|e| e.as_object_mut()) {
                    eng.insert("type".to_string(), json!(etype));
                }
                // 码表方案附带「当前生效值」旁路字段：`engine.codetable` 的行为字段是
                // Option，未设置时是 null，设置页无法把 null 显示成开关——它需要知道
                // 「不设置的话实际是什么」。基线又分普通/特殊两种（见 codetable_baseline），
                // UI 侧算不出来，故由此处随配置一并给出。
                //
                // 与 `engine.codetable` 平级但**不同名**：那份是「显式写了什么」（可为 null，
                // 决定 saveConfig 该不该落盘），这份是「实际按什么跑」（恒为实值，只作 UI 初值）。
                // 合并成一份会让「跟随基线」与「显式等于基线值」无从区分。
                if etype == "codetable" {
                    let eff = self.engine_mgr().effective_codetable(id);
                    if let Some(obj) = v.as_object_mut() {
                        obj.insert("effectiveCodetable".to_string(), serde_json::to_value(eff)?);
                    }
                }
                // 首码集里的符号键（键名形式）。给设置页判「按键功能表绑的这个键，是不是
                // 同一方案的码元首码」——两者同层冲突，内核裁决是绑定优先，于是该符号
                // 起头的编码在本方案静默失效（见 schema-key-actions.md §4.3）。
                //
                // 给**键名**而不是字符集原文：`input_chars` 有区间语法（`a-x/`），设置页
                // 再写一份解析器就是两处慢慢漂移——跨仓契约无编译期约束，本仓已栽过。
                // 边界上传语义结果，不传待解析的原料。
                //
                // 组装放在协调器而不是引擎：引擎层不该依赖 `wind-keys`（层次倒置）。
                // 引擎只出「字符集」，键名↔字符的对照表归按键层。
                //
                // 只报符号键：字母恒在默认码元集里，全列出来是噪音；「字母绑功能键」那条
                // 冲突内核另有活码前缀裁决（`bound_action_key_yields`），不归本判据管。
                let leading_keys: Vec<&str> = match self.engine_mgr().schema_code_char_set(id) {
                    Some(set) => wind_keys::keymap::symbol_keys()
                        .filter(|(_, ch)| set.contains_leading(*ch))
                        .map(|(name, _)| name)
                        .collect(),
                    // 读不到方案就不提示——凭空报一个不存在的冲突比不报更糟。
                    None => Vec::new(),
                };
                // 按键总览（只读旁路）：本方案下每个绑过的键当前干什么、来自哪一层。
                // 组装在这里而不是设置页：全局那份要经 `effective_session_actions` 折算，
                // 展开规则只该有一处。见 [`keys_overview`]。
                let overview = keys_overview(&v);
                if let Some(obj) = v.as_object_mut() {
                    obj.insert("leadingCodeKeys".to_string(), json!(leading_keys));
                    obj.insert("keysOverview".to_string(), Value::Array(overview));
                }
                Ok(v)
            }
            None => Ok(json!({})),
        }
    }

    fn web_schema_save_config(&self, params: &Value) -> anyhow::Result<Value> {
        let id = str_param(params, "id")?;
        let cfg = params
            .get("cfg")
            .ok_or_else(|| anyhow::anyhow!("缺少参数 cfg"))?;
        let cfg = &strip_readonly_fields(cfg);
        let base = self
            .engine_mgr()
            .schema_base(id)
            .ok_or_else(|| anyhow::anyhow!("方案不存在: {}", id))?;
        let base_json = serde_json::to_value(&base)?;
        // 稀疏 diff（仅变化项）写入 override 层，让方案文件后续更新仍能透传未改项。
        let diff = json_diff(&base_json, cfg).unwrap_or(json!({}));
        let mut ov = json_to_toml(&diff);
        // 保留既有 override 的 dictionaries（附加词库开关由 setDictEnabled 单独管理）。
        if let toml::Value::Table(t) = &mut ov
            && !t.contains_key("dictionaries")
            && let Some(prev) = self.engine_mgr().get_schema_override(id)
            && let Some(d) = prev.get("dictionaries")
        {
            t.insert("dictionaries".to_string(), d.clone());
        }
        self.engine_mgr().write_schema_override(id, &ov)?;
        Ok(json!({ "ok": true }))
    }

    fn web_schema_reset_config(&self, params: &Value) -> anyhow::Result<Value> {
        let id = str_param(params, "id")?;
        self.engine_mgr().delete_schema_override(id)?;
        Ok(json!({ "ok": true }))
    }

    /// 构造 `dictionaries` 的 override 值：**每库只落 `{id, enabled}` 稀疏项**。
    ///
    /// 词库的 path/label/base_order/顺序等结构定义始终以方案文件为准（合并侧见
    /// `EngineManager::merge_dict_overrides`）。若在此写入完整数组，override 就会冻结整份
    /// 词库定义——方案后续新增的库透不过来、改过的 path 仍指向旧文件、顺序也停在写快照那一刻。
    ///
    /// 入参取合并后的 dictionaries：`enabled.is_some()` 即"该库有显式启用态"，逐条落盘以
    /// 保留其它库已翻的开关。
    fn sparse_dict_overrides(dicts: &[wind_config::schema::DictSpec]) -> toml::Value {
        toml::Value::Array(
            dicts
                .iter()
                .filter(|d| !d.id.is_empty())
                .filter_map(|d| {
                    let mut t = toml::value::Table::new();
                    t.insert("id".to_string(), toml::Value::String(d.id.clone()));
                    t.insert("enabled".to_string(), toml::Value::Boolean(d.enabled?));
                    Some(toml::Value::Table(t))
                })
                .collect(),
        )
    }

    fn web_schema_set_dict_enabled(&self, params: &Value) -> anyhow::Result<Value> {
        let id = str_param(params, "id")?;
        let dict_id = str_param(params, "dictId")?;
        let enabled = params
            .get("enabled")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);
        let mut merged = self
            .engine_mgr()
            .schema_merged(id)
            .ok_or_else(|| anyhow::anyhow!("方案不存在: {}", id))?;
        let mut found = false;
        for d in merged.dictionaries.iter_mut() {
            if d.id == dict_id {
                d.enabled = Some(enabled);
                found = true;
            }
        }
        if !found {
            anyhow::bail!("方案 {} 无附加词库 {}", id, dict_id);
        }
        let dicts_val = Self::sparse_dict_overrides(&merged.dictionaries);
        let mut ov = self
            .engine_mgr()
            .get_schema_override(id)
            .unwrap_or_else(|| toml::Value::Table(Default::default()));
        if !ov.is_table() {
            ov = toml::Value::Table(Default::default());
        }
        if let toml::Value::Table(t) = &mut ov {
            t.insert("dictionaries".to_string(), dicts_val);
        }
        // 持久化 override（不 invalidate），再对已加载引擎 live 翻该扩展层的 enabled 标志——
        // 扩展词库热插拔：无需重熔大词库即时生效；未加载方案下次构建按新 override 生效。
        self.engine_mgr().persist_schema_override(id, &ov)?;
        let live = self
            .engine_mgr()
            .set_dict_enabled_live(id, dict_id, enabled);
        Ok(json!({ "ok": true, "live": live }))
    }

    fn web_schema_delete(&self, params: &Value) -> anyhow::Result<Value> {
        let id = str_param(params, "id")?;
        if !self.engine_mgr().is_user_schema(id) {
            anyhow::bail!("内置方案不可删除: {id}");
        }
        let user = Self::user_schemas_dir()?;
        let system = Self::system_schemas_dir();
        // 共享检查基准 = 其余已安装方案(含内置——混输可能引用用户资源)。
        let keep: Vec<String> = self
            .engine_mgr()
            .installed_schemas()
            .into_iter()
            .filter(|s| s != id)
            .collect();
        // 镜像导入的收集逻辑删文件:方案文件+引用资源+递归引用的用户方案,共享保留。
        let r = wind_transfer::scheme::delete_package(id, &user, system.as_deref(), &keep)?;
        // 级联清词库数据:仅清数据域=方案自身的(拼音族数据在共享 pinyin 域,
        // data_schema_id≠自身时跳过;文件已删读不到类型时回落自身,清空域无害)。
        if let Some(store) = self.user_store() {
            for sid in &r.schema_ids {
                if self.engine_mgr().data_schema_id(sid) == *sid {
                    store.clear_user_words(sid)?;
                    store.clear_temp_words(sid)?;
                    store.clear_freq(sid)?;
                    store.clear_shadow(sid)?;
                }
            }
        }
        for sid in &r.schema_ids {
            self.engine_mgr().forget_deleted_schema(sid);
        }
        Ok(json!({
            "ok": true,
            "deleted": r.deleted,
            "keptShared": r.kept_shared,
            "schemaIds": r.schema_ids,
        }))
    }

    /// 用户 schemas 根目录(%APPDATA%/WindInput/schemas),不存在则创建。
    fn user_schemas_dir() -> anyhow::Result<std::path::PathBuf> {
        let dir = wind_config::Config::user_config_dir()
            .ok_or_else(|| anyhow::anyhow!("无用户配置目录"))?
            .join("schemas");
        std::fs::create_dir_all(&dir)?;
        Ok(dir)
    }

    /// 系统 schemas 根目录(<exe>/data/schemas),可能不存在(如测试环境)。
    fn system_schemas_dir() -> Option<std::path::PathBuf> {
        wind_config::Config::data_dir()
            .map(|d| d.join("schemas"))
            .filter(|d| d.is_dir())
    }

    fn web_scheme_export_package(&self, params: &Value) -> anyhow::Result<Value> {
        let id = str_param(params, "id")?;
        let out = str_param(params, "path")?;
        let user = Self::user_schemas_dir()?;
        let system = Self::system_schemas_dir();
        // 设置页方案定制层(schema_overrides/<id>.toml,见 write_schema_override):
        // 导出必须带上,否则定制过的方案(如换过双拼布局)导出的是未定制版本,
        // override 新指向的资源文件也会漏打包。
        let override_dir =
            wind_config::Config::user_config_dir().map(|d| d.join("schema_overrides"));
        let r = wind_transfer::scheme::export_package(
            id,
            &user,
            system.as_deref(),
            override_dir.as_deref(),
            std::path::Path::new(out),
            env!("CARGO_PKG_VERSION"),
            std::env::consts::OS,
            &chrono::Local::now().to_rfc3339(),
        )?;
        Ok(json!({
            "path": r.path.to_string_lossy(),
            "packed": r.packed,
            "systemRefs": r.system_refs,
            "missing": r.missing,
        }))
    }

    fn web_scheme_import_package(&self, params: &Value) -> anyhow::Result<Value> {
        use wind_transfer::merge::Strategy;
        let path = str_param(params, "path")?;
        let strategy = Strategy::from_param(
            params
                .get("strategy")
                .and_then(|v| v.as_str())
                .unwrap_or(""),
        );
        let user = Self::user_schemas_dir()?;
        let r = wind_transfer::scheme::import_package(std::path::Path::new(path), &user, strategy)?;
        // 覆盖已加载方案时失效缓存(新方案为安全 no-op);列表可见性由 installed_schemas 实时扫盘天然生效。
        for id in &r.schema_ids {
            self.engine_mgr().invalidate_schema(id);
        }
        Ok(json!({
            "imported": r.imported,
            "conflicts": r.conflicts,
            "schemaIds": r.schema_ids,
        }))
    }

    fn web_scheme_preview_import(&self, params: &Value) -> anyhow::Result<Value> {
        let path = str_param(params, "path")?;
        let user = Self::user_schemas_dir()?;
        let p = wind_transfer::scheme::preview_import(std::path::Path::new(path), &user)?;
        Self::scheme_preview_json(&p)
    }

    /// `scheme.previewImportText { text }`：文本信封的只读预览。
    /// 文本不是信封时错误以 `not_schema_text:` 开头，设置端据此回落配置片段管线。
    fn web_scheme_preview_import_text(&self, params: &Value) -> anyhow::Result<Value> {
        let text = str_param(params, "text")?;
        let user = Self::user_schemas_dir()?;
        let p = wind_transfer::envelope::preview_import_text(text, &user)?;
        Self::scheme_preview_json(&p)
    }

    /// `scheme.importText { text, strategy? }`：文本信封落盘。响应形状与
    /// `scheme.importPackage` 一致；同样**不**应用包内配置片段（设置端两步编排）。
    fn web_scheme_import_text(&self, params: &Value) -> anyhow::Result<Value> {
        use wind_transfer::merge::Strategy;
        let text = str_param(params, "text")?;
        let strategy = Strategy::from_param(
            params
                .get("strategy")
                .and_then(|v| v.as_str())
                .unwrap_or(""),
        );
        let user = Self::user_schemas_dir()?;
        let r = wind_transfer::envelope::import_text(text, &user, strategy)?;
        // 覆盖已加载方案时失效缓存（新方案为安全 no-op），与 path 版同处置。
        for id in &r.schema_ids {
            self.engine_mgr().invalidate_schema(id);
        }
        Ok(json!({
            "imported": r.imported,
            "conflicts": r.conflicts,
            "schemaIds": r.schema_ids,
        }))
    }

    /// 预览响应的公共形状(zip 与文本信封共用,两条路复用同一个确认对话框)。
    ///
    /// **包级说明**(`title` / `description`,来自 package.toml / 信封 `[package]`)提到
    /// 顶层:确认对话框在顶部显示它。空则不输出该字段——前端不必区分"没写"与"写了空串"。
    /// 它与 `configPatch.info`(片段自带的说明)**各显各的、不合并**:包级说的是"这个包
    /// 是什么",片段级说的是"这段配置改了什么"(§2.3)。
    fn scheme_preview_json(
        p: &wind_transfer::scheme::SchemeImportPreview,
    ) -> anyhow::Result<Value> {
        let mut out = json!({
            // v2:包元信息来自可选 package.toml(缺失时各字段为空串,前端显示"未知")。
            "package": serde_json::to_value(&p.meta)?,
            "willAdd": p.will_add,
            "conflicts": p.conflicts,
            "systemRefs": p.system_refs,
            "missing": p.missing,
        });
        if !p.meta.package.title.is_empty() {
            out["title"] = json!(p.meta.package.title);
        }
        if !p.meta.package.description.is_empty() {
            out["description"] = json!(p.meta.package.description);
        }
        if let Some(text) = &p.config_patch {
            out["configPatch"] = Self::config_patch_diff(text)?;
        }
        Ok(out)
    }

    /// 包内配置片段的逐键 diff(`{ text, ok, entries, info? }`),形状与 `config.previewPatch`
    /// 同源。`info` 是**该片段自己**的说明(来自 config_patch.toml 的 `[package]` 段),
    /// 与包级 title/description 无关,不合并也不做优先级判定。
    ///
    /// 只预览、**不应用**:应用编排在设置端(导入方案文件 → `config.applyPatch`),那条路
    /// 才有热重载与镜像回灌。在文件层复刻它们就是第二份真相源。
    fn config_patch_diff(text: &str) -> anyhow::Result<Value> {
        let fragment = wind_config::patch::parse_fragment(text)
            .map_err(|e| anyhow::anyhow!("包内配置片段无法解析: {e}"))?;
        // 说明非法即整体错误(与解析失败同级),判据与 config.previewPatch 完全一致。
        let info = wind_config::patch::extract_info(&fragment)
            .map_err(|e| anyhow::anyhow!("包内配置片段说明非法: {e}"))?;
        let current = toml::Value::try_from(wind_config::Config::load(
            wind_config::Config::data_dir().as_deref(),
        )?)?;
        let entries = wind_config::patch::preview(&fragment, &current);
        let ok = entries.iter().all(|e| e.error.is_none());
        let mut out = json!({ "text": text, "ok": ok, "entries": entries });
        if let Some(info) = info {
            out["info"] = serde_json::to_value(info)?;
        }
        Ok(out)
    }

    fn web_backup_create(&self, params: &Value) -> anyhow::Result<Value> {
        use wind_transfer::backup::{BackupOptions, BackupSources, create_backup};
        let out = str_param(params, "path")?;
        let include_stats = params
            .get("includeStats")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let include_state = params
            .get("includeState")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let store = self
            .user_store()
            .ok_or_else(|| anyhow::anyhow!("无持久化存储"))?;
        let user_dir = wind_config::Config::user_config_dir();
        let cfg_file = user_dir.as_ref().map(|d| d.join("config.toml"));
        let compat_file = user_dir.as_ref().map(|d| d.join("compat.toml"));
        let schemas_dir = user_dir.as_ref().map(|d| d.join("schemas"));
        let schema_overrides_dir = user_dir.as_ref().map(|d| d.join("schema_overrides"));
        let themes_dir = user_dir.as_ref().map(|d| d.join("themes"));
        let state_file = wind_config::Config::local_dir().map(|d| d.join("state.toml"));
        let src = BackupSources {
            user_config_file: cfg_file.as_deref(),
            compat_file: compat_file.as_deref(),
            user_schemas_dir: schemas_dir.as_deref(),
            user_schema_overrides_dir: schema_overrides_dir.as_deref(),
            user_themes_dir: themes_dir.as_deref(),
            state_file: state_file.as_deref(),
        };
        let r = create_backup(
            store,
            &src,
            std::path::Path::new(out),
            env!("CARGO_PKG_VERSION"),
            std::env::consts::OS,
            &chrono::Local::now().to_rfc3339(),
            &BackupOptions {
                include_stats,
                include_state,
            },
        )?;
        let manifest = wind_transfer::bundle::read_manifest(&r.path)?;
        Ok(json!({
            "path": r.path.to_string_lossy(),
            "manifest": serde_json::to_value(&manifest)?,
        }))
    }

    fn web_backup_inspect(&self, params: &Value) -> anyhow::Result<Value> {
        let path = str_param(params, "path")?;
        let manifest = wind_transfer::bundle::read_manifest(std::path::Path::new(path))?;
        Ok(json!({ "manifest": serde_json::to_value(&manifest)? }))
    }

    fn web_backup_restore(&self, params: &Value) -> anyhow::Result<Value> {
        use wind_transfer::backup::{RestoreTargets, restore_backup};
        use wind_transfer::merge::Strategy;
        let path = str_param(params, "path")?;
        let strategy = Strategy::from_param(
            params
                .get("strategy")
                .and_then(|v| v.as_str())
                .unwrap_or(""),
        );
        let sections: Option<Vec<String>> = params.get("sections").and_then(|v| {
            v.as_array().map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(String::from))
                    .collect()
            })
        });
        let store = self
            .user_store()
            .ok_or_else(|| anyhow::anyhow!("无持久化存储"))?;
        let user_dir = wind_config::Config::user_config_dir();
        let cfg_file = user_dir.as_ref().map(|d| d.join("config.toml"));
        let compat_file = user_dir.as_ref().map(|d| d.join("compat.toml"));
        let schemas_dir = user_dir.as_ref().map(|d| d.join("schemas"));
        let schema_overrides_dir = user_dir.as_ref().map(|d| d.join("schema_overrides"));
        let themes_dir = user_dir.as_ref().map(|d| d.join("themes"));
        let state_file = wind_config::Config::local_dir().map(|d| d.join("state.toml"));
        let targets = RestoreTargets {
            user_config_file: cfg_file.as_deref(),
            compat_file: compat_file.as_deref(),
            user_schemas_dir: schemas_dir.as_deref(),
            user_schema_overrides_dir: schema_overrides_dir.as_deref(),
            user_themes_dir: themes_dir.as_deref(),
            state_file: state_file.as_deref(),
        };
        let r = restore_backup(
            std::path::Path::new(path),
            store,
            &targets,
            strategy,
            sections.as_deref(),
        )?;
        // 刷新:config 域生效、短语重建、涉及方案失效缓存(未加载时安全 no-op)。
        let touched_config = r.restored.iter().any(|p| p.starts_with("config/"));
        let touched_phrase = r.restored.iter().any(|p| p == "userdata/phrases.wdict");
        for id in &r.schemas_touched {
            self.engine_mgr().invalidate_schema(id);
        }
        for p in &r.restored {
            if let Some(rel) = p.strip_prefix("schemas/")
                && let Some(id) = rel.strip_suffix(".schema.toml")
                && !id.contains('/')
            {
                self.engine_mgr().invalidate_schema(id);
            }
            // schema_overrides/<id>.toml 同样是「方案配置合并层」的一部分，改动后
            // 必须失效该方案引擎缓存，否则还原后仍沿用还原前的旧 override 跑。
            if let Some(rel) = p.strip_prefix("schema_overrides/")
                && let Some(id) = rel.strip_suffix(".toml")
                && !id.contains('/')
            {
                self.engine_mgr().invalidate_schema(id);
            }
        }
        if touched_phrase {
            // Replace 还原会先 `reset_user_phrases`（见 restore_backup 的 "phrase" 分支），
            // 遮蔽了系统条目的用户行随之被删、那些系统短语一并消失 → 补回缺失的。
            // 与设置页「清空用户短语」同一条约束，漏在这里就是备份还原后系统短语静默少几条。
            self.restore_missing_system_phrases("备份还原");
            self.rebuild_phrases();
        }
        if touched_config {
            self.reload_user_config();
        }
        Ok(json!({
            "restored": r.restored,
            "conflicts": r.conflicts,
            "schemasTouched": r.schemas_touched,
        }))
    }

    fn web_dict_encode(&self, params: &Value) -> anyhow::Result<Value> {
        let schema = str_param(params, "schemaId")?;
        // 同 dict.genPinyin：取码基于真实文本，不能拿转义形态去逐字反查。
        let text = store_text(str_param(params, "text")?);
        let code = self
            .encode_texts(schema, std::slice::from_ref(&text))
            .into_iter()
            .next()
            .unwrap_or_default();
        Ok(json!(code))
    }

    /// 批量出码（纯词列表导入）。规则与 `dict.encode` 完全一致——两者共用
    /// [`Self::encode_texts`]，避免两条出码口径各自漂移。
    ///
    /// 契约：`codes` 与入参 `texts` **同序等长**，出不了码的位置为空串。
    /// 调用方靠下标把码配回词，跳过失败项会让其后所有词错位配到别人的码上。
    /// 故非字符串元素也要占位（按空串处理），不能 filter 掉。
    fn web_dict_encode_words(&self, params: &Value) -> anyhow::Result<Value> {
        let schema = str_param(params, "schemaId")?;
        let texts: Vec<String> = params
            .get("texts")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .map(|x| store_text(x.as_str().unwrap_or("")))
                    .collect()
            })
            .unwrap_or_default();
        Ok(json!({ "codes": self.encode_texts(schema, &texts) }))
    }

    /// 出码统一入口：拼音类方案出拼音码，其余（码表）按方案 `[[encoder.rules]]` 出词组码。
    /// 返回与 `texts` 同序等长，失败位置为空串。
    ///
    /// 一次性准备（读方案 / 取引擎句柄）都在这一层之下完成，故传一个词与传一万个词
    /// 的固定开销相同——`dict.encode` 走单元素切片没有额外代价。
    fn encode_texts(&self, schema: &str, texts: &[String]) -> Vec<String> {
        if texts.is_empty() {
            return Vec::new();
        }
        let refs: Vec<&str> = texts.iter().map(|s| s.as_str()).collect();
        let is_pinyin = self
            .engine_mgr()
            .schema_engine_type(schema)
            .map(|t| t == "pinyin")
            .unwrap_or(false);
        if !is_pinyin {
            // 与自动造词/快捷加词同一取码入口（码源=码表词库自身，规则=方案声明的公式）。
            // 原走 wubi_word_code：拆字表码源 + 硬编码五笔 86 规则，未配拆字的方案恒空、
            // 非五笔方案静默出错。见 docs/design/codetable-auto-phrase.md §2「码源统一」。
            return self.engine_mgr().encode_words(schema, &refs);
        }
        // 优先词级消歧（多音字按词典权重），引擎无果时回退逐字反查表。
        // 回的是**带空格的音节码**，让用户看清拼音词库的音节格式（与 word_item 同形）。
        // 安全前提：写入侧 normalize_add_code 会拆回扁平 key，并把空格当作**显式声明的
        // 切分**采信。逐字反查表回退同样以空格分隔（`gen_pinyin` 以 `.join(" ")` 收尾），
        // 故两条路出来的都是同形的音节码，无需再做区分。
        let generated = self.engine_mgr().generate_words_pinyin(schema, &refs);
        // 反查表的读锁在循环外取一次：逐词加锁在万级批量上纯属浪费。
        let reverse = self
            .reverse_lookup()
            .read()
            .unwrap_or_else(|e| e.into_inner());
        generated
            .into_iter()
            .zip(&refs)
            .map(|(code, text)| code.unwrap_or_else(|| reverse.gen_pinyin(text)))
            .collect()
    }

    /// 为词语生成拼音码：优先用拼音引擎词级消歧（活跃方案→"pinyin"方案），
    /// 都无果时回退逐字反查表（pinyin_map.txt）。用于 dict.genPinyin（无方案上下文）。
    ///
    /// 同 `dict.encode`：回带空格的音节码，写入侧负责拆回扁平 key。
    fn gen_pinyin_word(&self, text: &str) -> String {
        let active = self.engine_mgr().active_schema_id();
        self.engine_mgr()
            .generate_word_pinyin(&active, text)
            .or_else(|| self.engine_mgr().generate_word_pinyin("pinyin", text))
            .unwrap_or_else(|| {
                self.reverse_lookup()
                    .read()
                    .unwrap_or_else(|e| e.into_inner())
                    .gen_pinyin(text)
            })
    }

    fn web_freq_list_paged(&self, params: &Value) -> anyhow::Result<Value> {
        let schema = str_param(params, "schemaId")?;
        let schema = self.engine_mgr().data_schema_id(schema); // 拼音族折叠到 "pinyin"
        let prefix = params.get("prefix").and_then(|v| v.as_str()).unwrap_or("");
        let offset = usize_param(params, "offset", 0);
        let limit = usize_param(params, "limit", 50);
        let store = self
            .user_store()
            .ok_or_else(|| anyhow::anyhow!("无持久化存储"))?;
        let sort = parse_sort(params, &["code", "text", "count", "lastUsed"]);
        // 无搜索且无排序：走 store 分页快路径；否则全量拉取
        //（编码前缀 ∪ 词条内容包含）→ 排序 → 内存切片。
        let (page, total) = if prefix.is_empty() && sort.is_none() {
            store.list_freq_paged(&schema, "", offset, limit)?
        } else {
            // 词频表的 key 是扁平码；用户可能从用户词库列表复制带空格的串来搜，先拆
            //（对无空格串恒等，故无副作用）。
            let (code_prefix, _) = wind_store::wdict::split_spaced_code(prefix);
            let (mut all, _) = store.list_freq_paged(&schema, &code_prefix, 0, 0)?;
            // 并入两类补充命中（与上面的编码前缀取并集，去重）：
            //   ① 词条内容包含搜索词（拿汉字匹配 text，用原串）
            //   ② **编码中段包含**搜索词 —— 与 web_dict_list_paged 同款，前缀扫描只能
            //      命中开头，`haoya` 搜 `ya` 一条也出不来。两者共用这一次全量扫描。
            if !prefix.is_empty() {
                let q = prefix.to_lowercase();
                let code_q = code_prefix.to_lowercase();
                let seen: std::collections::HashSet<(String, String)> =
                    all.iter().map(|(c, t, _)| (c.clone(), t.clone())).collect();
                let (rest, _) = store.list_freq_paged(&schema, "", 0, 0)?;
                for (c, t, rec) in rest {
                    let hit = t.to_lowercase().contains(&q)
                        || (!code_q.is_empty() && c.to_lowercase().contains(&code_q));
                    if hit && !seen.contains(&(c.clone(), t.clone())) {
                        all.push((c, t, rec));
                    }
                }
            }
            let total = all.len();
            if let Some((by, desc)) = sort {
                all.sort_by(|(ca, ta, ra), (cb, tb, rb)| {
                    let ord = match by {
                        "count" => ra.count.cmp(&rb.count),
                        "lastUsed" => ra.last_used.cmp(&rb.last_used),
                        "text" => ta.cmp(tb),
                        _ => ca.cmp(cb),
                    };
                    if desc { ord.reverse() } else { ord }
                });
            }
            let page = all.into_iter().skip(offset).take(limit).collect();
            (page, total)
        };
        let items: Vec<Value> = page
            .into_iter()
            .map(|(code, text, rec)| {
                let code = self.freq_display_code(&schema, &code, &text);
                json!({ "code": code, "text": ui_text(&text), "count": rec.count, "lastUsed": rec.last_used })
            })
            .collect();
        Ok(json!({ "items": items, "total": total }))
    }

    /// 词频列表的编码显示：反查音节边界后渲染成带空格的音节码，与用户词库/临时词库列表同形。
    ///
    /// **词频表是唯一不带 boundary 的持久层**（value 仅 `count + last_used`），边界只能反查。
    /// 三处依次问：系统词典（mmap 点查，最快也最可能命中）→ 用户词表 → 临时词表。
    /// 都查不到即原样返回扁平码——存量的简拼码记录、码表方案、以及词条已被删除的
    /// 遗留记录都会落到这里，属正常降级。
    ///
    /// 只对**当前页**（≤ limit 条）反查，开销与词频表总规模无关。
    ///
    /// 之所以选反查而非给词频表扩容加 boundary 字段：词频是长期积累的数据，扩容只能让
    /// 此后新写入的记录带边界，用户会看到「新词有空格、老词没有」的混杂列表。
    fn freq_display_code(&self, schema: &str, code: &str, text: &str) -> String {
        let mut b = self.engine_mgr().syllable_boundary_of(schema, code, text);
        if b == 0
            && let Some(store) = self.user_store()
        {
            let from = |recs: Vec<wind_store::user_words::UserWordRecord>| {
                recs.into_iter()
                    .find(|w| w.text == text)
                    .map(|w| w.boundary)
                    .filter(|x| *x != 0)
            };
            b = store
                .get_user_words(schema, code)
                .ok()
                .and_then(&from)
                .or_else(|| store.get_temp_words(schema, code).ok().and_then(&from))
                .unwrap_or(0);
        }
        wind_store::wdict::join_code_by_boundary(code, b)
    }

    fn web_freq_delete(&self, params: &Value) -> anyhow::Result<Value> {
        let schema = str_param(params, "schemaId")?;
        let schema = self.engine_mgr().data_schema_id(schema); // 拼音族折叠到 "pinyin"
        let (code, text) = (str_param(params, "code")?, str_param(params, "text")?);
        let store = self
            .user_store()
            .ok_or_else(|| anyhow::anyhow!("无持久化存储"))?;
        // 列表项的 code 带音节空格（见 freq_display_code），而词频表 key 是扁平的。
        let (code, _) = wind_store::wdict::split_spaced_code(code);
        store.delete_freq(&schema, &code, &store_text(text))?;
        Ok(json!({ "ok": true }))
    }

    fn web_freq_clear(&self, params: &Value) -> anyhow::Result<Value> {
        let schema = str_param(params, "schemaId")?;
        let schema = self.engine_mgr().data_schema_id(schema); // 拼音族折叠到 "pinyin"
        let store = self
            .user_store()
            .ok_or_else(|| anyhow::anyhow!("无持久化存储"))?;
        Ok(json!(store.clear_freq(&schema)?))
    }

    fn web_shadow_list(&self, params: &Value) -> anyhow::Result<Value> {
        let schema = str_param(params, "schemaId")?;
        let schema = self.engine_mgr().data_schema_id(schema); // 拼音族折叠到 "pinyin"
        let store = self
            .user_store()
            .ok_or_else(|| anyhow::anyhow!("无持久化存储"))?;
        let mut out = Vec::new();
        for (code, rec) in store.list_shadow_rules(&schema)? {
            for p in rec.pinned {
                out.push(json!({
                    "code": code,
                    "word": p.word,
                    "candId": p.cand_id,
                    "type": "pin",
                    "position": p.position,
                }));
            }
            for d in rec.deleted {
                out.push(json!({
                    "code": code,
                    "word": d,
                    "candId": Value::Null,
                    "type": "delete",
                }));
            }
        }
        Ok(json!(out))
    }

    fn web_shadow_pin(&self, params: &Value) -> anyhow::Result<Value> {
        let (schema, code, word) = (
            str_param(params, "schemaId")?,
            str_param(params, "code")?,
            str_param(params, "word")?,
        );
        let cand_id = params.get("candId").and_then(|v| v.as_str());
        let position = usize_param(params, "position", 0);
        let schema = self.engine_mgr().data_schema_id(schema); // 拼音族折叠到 "pinyin"
        let store = self
            .user_store()
            .ok_or_else(|| anyhow::anyhow!("无持久化存储"))?;
        store.pin_shadow(&schema, code, word, cand_id, position)?;
        Ok(json!({ "ok": true }))
    }

    fn web_shadow_delete(&self, params: &Value) -> anyhow::Result<Value> {
        let (schema, code, word) = (
            str_param(params, "schemaId")?,
            str_param(params, "code")?,
            str_param(params, "word")?,
        );
        let schema = self.engine_mgr().data_schema_id(schema); // 拼音族折叠到 "pinyin"
        let store = self
            .user_store()
            .ok_or_else(|| anyhow::anyhow!("无持久化存储"))?;
        store.delete_shadow(&schema, code, word)?;
        Ok(json!({ "ok": true }))
    }

    fn web_shadow_remove_rule(&self, params: &Value) -> anyhow::Result<Value> {
        let (schema, code, word) = (
            str_param(params, "schemaId")?,
            str_param(params, "code")?,
            str_param(params, "word")?,
        );
        // 设置页删规则时回传 `shadow.list` 给出的 candId：动态短语规则的 word 是写入当天的
        // 求值文本，只按 word 定位会删不掉（列表里看得见、点删除无效）。
        let cand_id = params.get("candId").and_then(|v| v.as_str());
        let schema = self.engine_mgr().data_schema_id(schema); // 拼音族折叠到 "pinyin"
        let store = self
            .user_store()
            .ok_or_else(|| anyhow::anyhow!("无持久化存储"))?;
        store.remove_shadow_rule(&schema, code, word, cand_id)?;
        Ok(json!({ "ok": true }))
    }

    /// 候选调整手动添加/编辑：type="hide" 转屏蔽；否则（pin）按 position 置顶。
    /// 匹配设置端候选调整对话框契约。
    ///
    /// **编辑既有规则时设置端会回传 `candId`，必须透传下去**：短语规则靠稳定 id 跨日命中，
    /// 而这条路会先按 `same_target` 匹配掉同一条旧规则再插入新规则——不带 id 就等于
    /// 把原规则的 id 擦掉（退化成按当日文本匹配，次日必失配）。用户侧表现为
    /// 「在设置页改了一下位置，第二天整条规则就不生效了」。
    fn web_shadow_add_rule(&self, params: &Value) -> anyhow::Result<Value> {
        let (schema, code, word) = (
            str_param(params, "schemaId")?,
            str_param(params, "code")?,
            str_param(params, "word")?,
        );
        let kind = params.get("type").and_then(|v| v.as_str()).unwrap_or("pin");
        let cand_id = params.get("candId").and_then(|v| v.as_str());
        let schema = self.engine_mgr().data_schema_id(schema); // 拼音族折叠到 "pinyin"
        let store = self
            .user_store()
            .ok_or_else(|| anyhow::anyhow!("无持久化存储"))?;
        if kind == "hide" {
            store.delete_shadow(&schema, code, word)?;
        } else {
            let position = usize_param(params, "position", 0);
            store.pin_shadow(&schema, code, word, cand_id, position)?;
        }
        Ok(json!({ "ok": true }))
    }

    fn web_temp_list(&self, params: &Value) -> anyhow::Result<Value> {
        let schema = str_param(params, "schemaId")?;
        let schema = self.engine_mgr().data_schema_id(schema); // 拼音族折叠到 "pinyin"
        let store = self
            .user_store()
            .ok_or_else(|| anyhow::anyhow!("无持久化存储"))?;
        // code 带音节空格，与用户词库列表（word_item）同形。remove/promote 两个入口
        // 会收到这个串，各自拆回扁平码——三处必须同改，否则「显示得了、删不掉」。
        // text 同理：出口投影成转义形态、两个入口用 `store_text` 还原，缺一即同样症状。
        let items: Vec<Value> = store
            .search_temp_words_prefix(&schema, "", 0)?
            .into_iter()
            .map(|r| {
                let code = wind_store::wdict::join_code_by_boundary(&r.code, r.boundary);
                json!({ "code": code, "text": ui_text(&r.text), "count": r.count })
            })
            .collect();
        Ok(json!(items))
    }

    fn web_temp_promote(&self, params: &Value) -> anyhow::Result<Value> {
        let schema = str_param(params, "schemaId")?;
        let schema = self.engine_mgr().data_schema_id(schema); // 拼音族折叠到 "pinyin"
        let (code, text) = (str_param(params, "code")?, str_param(params, "text")?);
        let store = self
            .user_store()
            .ok_or_else(|| anyhow::anyhow!("无持久化存储"))?;
        // 列表项的 code 带音节空格（见 web_temp_list），key 是扁平的。
        let (code, _) = wind_store::wdict::split_spaced_code(code);
        store.promote_temp_word(&schema, &code, &store_text(text))?;
        Ok(json!({ "ok": true }))
    }

    fn web_temp_remove(&self, params: &Value) -> anyhow::Result<Value> {
        let schema = str_param(params, "schemaId")?;
        let schema = self.engine_mgr().data_schema_id(schema); // 拼音族折叠到 "pinyin"
        let (code, text) = (str_param(params, "code")?, str_param(params, "text")?);
        let store = self
            .user_store()
            .ok_or_else(|| anyhow::anyhow!("无持久化存储"))?;
        // 同 promote：列表项的 code 带音节空格，不拆则删不掉。
        let (code, _) = wind_store::wdict::split_spaced_code(code);
        store.remove_temp_word(&schema, &code, &store_text(text))?;
        Ok(json!({ "ok": true }))
    }

    fn web_temp_promote_all(&self, params: &Value) -> anyhow::Result<Value> {
        let schema = str_param(params, "schemaId")?;
        let schema = self.engine_mgr().data_schema_id(schema); // 拼音族折叠到 "pinyin"
        let store = self
            .user_store()
            .ok_or_else(|| anyhow::anyhow!("无持久化存储"))?;
        let mut n = 0u64;
        for r in store.search_temp_words_prefix(&schema, "", 0)? {
            if store.promote_temp_word(&schema, &r.code, &r.text)? {
                n += 1;
            }
        }
        Ok(json!(n))
    }

    fn web_temp_clear(&self, params: &Value) -> anyhow::Result<Value> {
        let schema = str_param(params, "schemaId")?;
        let schema = self.engine_mgr().data_schema_id(schema); // 拼音族折叠到 "pinyin"
        let store = self
            .user_store()
            .ok_or_else(|| anyhow::anyhow!("无持久化存储"))?;
        let all = store.search_temp_words_prefix(&schema, "", 0)?;
        let n = all.len();
        for r in all {
            store.remove_temp_word(&schema, &r.code, &r.text)?;
        }
        Ok(json!(n))
    }

    fn web_phrase_list(&self) -> anyhow::Result<Value> {
        let store = self
            .user_store()
            .ok_or_else(|| anyhow::anyhow!("无持久化存储"))?;
        let items: Vec<Value> = store
            .list_phrases()?
            .into_iter()
            .map(|p| {
                json!({
                    "code": p.code,
                    "text": ui_text(&p.text),
                    "position": p.position,
                    "weight": p.weight,
                    "enabled": p.enabled,
                    "isSystem": p.is_system,
                })
            })
            .collect();
        Ok(json!(items))
    }

    fn web_phrase_add(&self, params: &Value) -> anyhow::Result<Value> {
        let (code, text) = (str_param(params, "code")?, str_param(params, "text")?);
        let position = i32_param(params, "position");
        // 缺省值与取值依据见 `wind_store::phrases::DEFAULT_USER_PHRASE_WEIGHT`
        // （分发导入走同一个常量）。
        let weight = params
            .get("weight")
            .and_then(|v| v.as_i64())
            .unwrap_or(wind_store::phrases::DEFAULT_USER_PHRASE_WEIGHT as i64)
            as i32;
        let store = self
            .user_store()
            .ok_or_else(|| anyhow::anyhow!("无持久化存储"))?;
        store.add_phrase(code, &store_text(text), position, weight)?;
        self.rebuild_phrases();
        Ok(json!({ "ok": true }))
    }

    fn web_phrase_update(&self, params: &Value) -> anyhow::Result<Value> {
        let (code, text) = (str_param(params, "code")?, str_param(params, "text")?);
        let new_code = params.get("newCode").and_then(|v| v.as_str());
        let new_text = params.get("newText").and_then(|v| v.as_str());
        let position = params
            .get("position")
            .and_then(|v| v.as_i64())
            .map(|n| n as i32);
        let weight = params
            .get("weight")
            .and_then(|v| v.as_i64())
            .map(|n| n as i32);
        let store = self
            .user_store()
            .ok_or_else(|| anyhow::anyhow!("无持久化存储"))?;
        // text 是查找键、new_text 是新值，两者都来自设置页，都要还原成存储域。
        let text = store_text(text);
        let new_text = new_text.map(store_text);
        store.update_phrase(code, &text, new_code, new_text.as_deref(), position, weight)?;
        // 若同时携带 enabled，应用到新键。
        if let Some(en) = params.get("enabled").and_then(|v| v.as_bool()) {
            store.set_phrase_enabled(
                new_code.unwrap_or(code),
                new_text.as_deref().unwrap_or(&text),
                en,
            )?;
        }
        // 改 code/text 时 `update_phrase` 内部会 remove 旧键——若改的是一条遮蔽了系统条目的
        // 用户短语，旧键一删那条系统短语也没了。与 `web_phrase_remove` 同一条约束。
        self.restore_missing_system_phrases("编辑短语");
        self.rebuild_phrases();
        Ok(json!({ "ok": true }))
    }

    fn web_phrase_remove(&self, params: &Value) -> anyhow::Result<Value> {
        let (code, text) = (str_param(params, "code")?, str_param(params, "text")?);
        let store = self
            .user_store()
            .ok_or_else(|| anyhow::anyhow!("无持久化存储"))?;
        store.remove_phrase(code, &store_text(text))?;
        // 删掉的可能是一条**遮蔽了系统条目**的用户短语（`overrides_system`）——主键只有
        // 一把，删掉它等于把那条系统短语也删了。用户的预期恰恰相反：删掉自己加的那条
        // 就该露出系统默认那条。故补回缺失的系统条目。
        self.restore_missing_system_phrases("删除短语");
        self.rebuild_phrases();
        Ok(json!({ "ok": true }))
    }

    fn web_phrase_set_enabled(&self, params: &Value) -> anyhow::Result<Value> {
        let (code, text) = (str_param(params, "code")?, str_param(params, "text")?);
        let enabled = params
            .get("enabled")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);
        let store = self
            .user_store()
            .ok_or_else(|| anyhow::anyhow!("无持久化存储"))?;
        // text 是查找键，须还原成存储域（见 `store_text`）——漏了就「开关点了没反应」。
        store.set_phrase_enabled(code, &store_text(text), enabled)?;
        self.rebuild_phrases();
        Ok(json!({ "ok": true }))
    }

    fn web_phrase_reset(&self) -> anyhow::Result<Value> {
        if let Some(store) = self.user_store() {
            store.reset_user_phrases()?;
            // 用户行里可能有遮蔽了系统条目的（`overrides_system`），删掉后那些系统短语
            // 也一并没了 → 补回缺失的，否则要等到 TOML 哈希变动才恢复。
            self.restore_missing_system_phrases("清空用户短语");
        }
        self.rebuild_phrases();
        Ok(json!({ "ok": true }))
    }

    fn web_phrase_list_system(&self) -> anyhow::Result<Value> {
        let store = self
            .user_store()
            .ok_or_else(|| anyhow::anyhow!("无持久化存储"))?;
        let items: Vec<Value> = store
            .list_system_phrases()?
            .into_iter()
            .map(|p| {
                json!({
                    "code": p.code, "text": ui_text(&p.text), "weight": p.weight,
                    "position": p.position, "enabled": p.enabled, "isSystem": true,
                })
            })
            .collect();
        Ok(json!(items))
    }

    fn web_phrase_list_user(&self, params: &Value) -> anyhow::Result<Value> {
        let store = self
            .user_store()
            .ok_or_else(|| anyhow::anyhow!("无持久化存储"))?;
        let prefix = params.get("prefix").and_then(|v| v.as_str());
        let offset = params.get("offset").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
        let limit = params.get("limit").and_then(|v| v.as_u64()).unwrap_or(100) as usize;
        // 带 sortBy 时全量拉取 → 排序 → 内存切片；否则走 store 分页路径
        let (rows, total) = if let Some((by, desc)) =
            parse_sort(params, &["code", "text", "weight", "position", "enabled"])
        {
            let (mut all, total) = store.list_user_phrases_paged(prefix, 0, usize::MAX)?;
            all.sort_by(|a, b| {
                let ord = match by {
                    "weight" => a.weight.cmp(&b.weight),
                    "position" => a.position.cmp(&b.position),
                    "enabled" => a.enabled.cmp(&b.enabled),
                    "text" => a.text.cmp(&b.text),
                    _ => a.code.cmp(&b.code),
                };
                if desc { ord.reverse() } else { ord }
            });
            let page = all.into_iter().skip(offset).take(limit).collect();
            (page, total)
        } else {
            store.list_user_phrases_paged(prefix, offset, limit)?
        };
        let items: Vec<Value> = rows
            .into_iter()
            .map(|p| {
                json!({
                    "code": p.code, "text": ui_text(&p.text), "weight": p.weight,
                    "position": p.position, "enabled": p.enabled, "isSystem": false,
                    // 这条用户短语遮蔽了同码同内容的系统条目（该系统条目已从系统列表隐去，
                    // 输入期生效的是这条）。供 UI 标注来源，并提示「删除本条即恢复系统默认」。
                    "overridesSystem": p.overrides_system,
                })
            })
            .collect();
        Ok(json!({ "items": items, "total": total }))
    }

    fn web_phrase_export(&self) -> anyhow::Result<Value> {
        let store = self
            .user_store()
            .ok_or_else(|| anyhow::anyhow!("无持久化存储"))?;
        let content = store.export_user_phrases_wdict("")?;
        Ok(json!({ "content": content }))
    }

    fn web_phrase_import(&self, params: &Value) -> anyhow::Result<Value> {
        let store = self
            .user_store()
            .ok_or_else(|| anyhow::anyhow!("无持久化存储"))?;
        let content = str_param(params, "content")?;
        let (imported, skipped) = store.import_user_phrases_wdict(content)?;
        self.rebuild_phrases();
        Ok(json!({ "imported": imported, "skipped": skipped }))
    }

    /// 逐行短语文本（`wind:p1` 分发格式）的导入预览。
    ///
    /// 与 `phrase.import`（wdict，备份还原语义）分开是刻意的：那条是整表 upsert、原样写回
    /// position；本条只新增、位置本地重算，且预览要回答「会不会动到我已有的短语」。
    fn web_phrase_preview_import_text(&self, params: &Value) -> anyhow::Result<Value> {
        let content = str_param(params, "content")?;
        let (doc, items, checks, plan) = self.phrase_text_plan(content)?;

        let mut counts = (0usize, 0usize, 0usize);
        let entries: Vec<Value> = doc
            .entries
            .iter()
            .zip(&items)
            .zip(&checks)
            .zip(&plan)
            .map(|(((e, (_, stored)), ck), st)| {
                use wind_store::phrases::PhraseImportStatus as S;
                match st {
                    S::New => counts.0 += 1,
                    S::ExistsUser => counts.1 += 1,
                    S::ShadowsSystem => counts.2 += 1,
                }
                json!({
                    "line": e.line,
                    "code": e.code,
                    // 回显分发原文：分发域与设置页显示域同形，用户在预览里看到的
                    // 与他在群里读到的、在短语列表里看到的是同一串。
                    "text": e.text,
                    "status": match st {
                        S::New => "new",
                        S::ExistsUser => "existsUser",
                        S::ShadowsSystem => "shadowsSystem",
                    },
                    "hints": ck.hints.iter().map(hint_json).collect::<Vec<_>>(),
                    "error": ck.error,
                    // 存储域与分发域不同（普通文本的 `\\`）时给 UI 一个对照位；相同则省略。
                    "storedText": (stored != &e.text).then(|| stored.clone()),
                })
            })
            .collect();

        let problems: Vec<Value> = doc
            .problems
            .iter()
            .map(|p| json!({ "line": p.line, "raw": p.raw, "message": p.reason.message() }))
            .collect();

        Ok(json!({
            "title": doc.title,
            "entries": entries,
            "problems": problems,
            "counts": {
                "new": counts.0,
                "existsUser": counts.1,
                "shadowsSystem": counts.2,
                "skippedLines": doc.problems.len(),
            },
        }))
    }

    /// 应用逐行短语文本。
    ///
    /// `accept` = 要导入的**源行号**数组（来自预览的 `line`）；**缺省导入全部可导入条目**。
    ///
    /// 语法错的条目无论是否被 `accept` 列出都不装——它们装进去只会在触发时失败，
    /// 而失败点离导入很远。
    fn web_phrase_import_text(&self, params: &Value) -> anyhow::Result<Value> {
        let content = str_param(params, "content")?;
        let (doc, items, checks, _) = self.phrase_text_plan(content)?;
        let accept: Option<std::collections::HashSet<u64>> = params
            .get("accept")
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(serde_json::Value::as_u64).collect());

        let mut selected: Vec<(String, String)> = Vec::new();
        let (mut skipped_invalid, mut skipped_unselected) = (0, 0);
        for ((e, item), ck) in doc.entries.iter().zip(&items).zip(&checks) {
            if !ck.is_importable() {
                skipped_invalid += 1;
                continue;
            }
            if accept
                .as_ref()
                .is_some_and(|set| !set.contains(&(e.line as u64)))
            {
                skipped_unselected += 1;
                continue;
            }
            selected.push(item.clone());
        }

        let store = self
            .user_store()
            .ok_or_else(|| anyhow::anyhow!("无持久化存储"))?;
        let rep = store
            .import_phrases_appending(&selected, wind_store::phrases::DEFAULT_USER_PHRASE_WEIGHT)?;
        self.rebuild_phrases();
        Ok(json!({
            "added": rep.added,
            "skippedExisting": rep.skipped_existing,
            "shadowedSystem": rep.shadowed_system,
            "skippedInvalid": skipped_invalid,
            "skippedUnselected": skipped_unselected,
        }))
    }

    /// 解析 + 存储域投影 + 静态检查 + 落点判定。预览与应用共用，保证两次看到的是
    /// 同一份判定——各算各的会让「预览一个样、应用装了另一个」成为可能。
    #[allow(clippy::type_complexity)]
    fn phrase_text_plan(
        &self,
        content: &str,
    ) -> anyhow::Result<(
        wind_store::phrase_text::PhraseTextDoc,
        Vec<(String, String)>,
        Vec<wind_store::phrase_text::EntryCheck>,
        Vec<wind_store::phrases::PhraseImportStatus>,
    )> {
        let doc =
            wind_store::phrase_text::parse_phrase_text(content).map_err(|e| anyhow::anyhow!(e))?;
        let store = self
            .user_store()
            .ok_or_else(|| anyhow::anyhow!("无持久化存储"))?;
        // 存储域投影与手动新增（`web_phrase_add`）走同一个 `store_text`——新增一条入口
        // 就得配一对转换，否则同一条短语经不同入口进库会变成两个字符串。
        let items: Vec<(String, String)> = doc
            .entries
            .iter()
            .map(|e| (e.code.clone(), store_text(&e.text)))
            .collect();
        let plan = store.plan_phrase_import(&items)?;
        let texts: Vec<String> = items.iter().map(|(_, t)| t.clone()).collect();
        let checks = wind_store::phrase_text::check_entries(&texts);
        Ok((doc, items, checks, plan))
    }

    fn web_phrase_reset_system(&self) -> anyhow::Result<Value> {
        let n = self.restore_system_phrases();
        Ok(json!({ "ok": true, "changed": n }))
    }

    // ───────── quick.*（快捷输入格式表）─────────

    /// 全部格式条目，**含被停用的**。
    ///
    /// `displayPos` 与 `moveIndex` 是两个不同的下标，别混用：前者是列表行号（停用项也占
    /// 一行，只给人看），后者是这条在候选里的位置、也是 `quick.move` 唯一认的口径。
    /// 停用项 `moveIndex` 为 null——它不在候选里，移动没有意义。
    fn web_quick_list(&self) -> anyhow::Result<Value> {
        let rows: Vec<Value> = self
            .quick_format_rows()
            .into_iter()
            .map(|r| {
                json!({
                    "kind": r.kind,
                    "id": r.id,
                    "text": r.text,
                    "displayPos": r.display_pos,
                    "moveIndex": r.move_index,
                    "enabled": r.enabled,
                    "adjusted": r.adjusted,
                    "user": r.user,
                    "sample": r.sample,
                })
            })
            .collect();
        Ok(json!(rows))
    }

    fn web_quick_move(&self, params: &Value) -> anyhow::Result<Value> {
        let (kind, id) = (str_param(params, "kind")?, str_param(params, "id")?);
        let index = usize_param(params, "index", 0);
        self.quick_format_edit(kind, id, QuickFormatEdit::MoveTo(index))?;
        Ok(json!({ "ok": true }))
    }

    fn web_quick_set_enabled(&self, params: &Value) -> anyhow::Result<Value> {
        let (kind, id) = (str_param(params, "kind")?, str_param(params, "id")?);
        let enabled = params
            .get("enabled")
            .and_then(|v| v.as_bool())
            .ok_or_else(|| anyhow::anyhow!("缺少参数 enabled"))?;
        self.quick_format_edit(kind, id, QuickFormatEdit::SetEnabled(enabled))?;
        Ok(json!({ "ok": true }))
    }

    fn web_quick_reset_entry(&self, params: &Value) -> anyhow::Result<Value> {
        let (kind, id) = (str_param(params, "kind")?, str_param(params, "id")?);
        self.quick_format_edit(kind, id, QuickFormatEdit::ResetEntry)?;
        Ok(json!({ "ok": true }))
    }

    fn web_quick_reset_kind(&self, params: &Value) -> anyhow::Result<Value> {
        let kind = str_param(params, "kind")?;
        // 整类操作没有单条归属，id 传空串（`QuickFormatEdit::ResetKind` 忽略它）。
        self.quick_format_edit(kind, "", QuickFormatEdit::ResetKind)?;
        Ok(json!({ "ok": true }))
    }

    /// 每类可用的模板变量清单，供设置页的模板输入框提示。
    ///
    /// 静态数据，一次取完即可缓存。真相源在 core 的 `FormatKind::var_hints`——设置仓自己
    /// 硬编码一份会在加新变量时静默过时（照提示写的模板反被拒，或提示里的变量已删）。
    fn web_quick_vars(&self) -> anyhow::Result<Value> {
        let kinds: Vec<Value> = self
            .quick_format_var_hints()
            .into_iter()
            .map(|(kind, vars)| {
                json!({
                    "kind": kind,
                    "vars": vars
                        .into_iter()
                        .map(|(name, desc)| json!({ "name": name, "desc": desc }))
                        .collect::<Vec<_>>(),
                })
            })
            .collect();
        Ok(json!(kinds))
    }

    /// 新增用户自定义条目。返回分配到的 id，好让设置页立刻高亮/滚到那一行。
    ///
    /// 没有对应的「改出厂条目模板」RPC：那条路径被刻意否决，出厂条目只能停用与调序
    /// （见 `docs/design/quick-input-format-table.md` §11.1）。
    fn web_quick_add(&self, params: &Value) -> anyhow::Result<Value> {
        let (kind, text) = (str_param(params, "kind")?, str_param(params, "text")?);
        let id = self.quick_format_add(kind, text)?;
        Ok(json!({ "ok": true, "id": id }))
    }

    fn web_quick_set_text(&self, params: &Value) -> anyhow::Result<Value> {
        let (kind, id) = (str_param(params, "kind")?, str_param(params, "id")?);
        self.quick_format_set_text(kind, id, str_param(params, "text")?)?;
        Ok(json!({ "ok": true }))
    }

    fn web_quick_delete(&self, params: &Value) -> anyhow::Result<Value> {
        let (kind, id) = (str_param(params, "kind")?, str_param(params, "id")?);
        self.quick_format_delete(kind, id)?;
        Ok(json!({ "ok": true }))
    }

    fn web_quick_export(&self) -> anyhow::Result<Value> {
        Ok(json!({ "content": self.quick_format_export()? }))
    }

    fn web_quick_preview_import(&self, params: &Value) -> anyhow::Result<Value> {
        let p = self.quick_format_preview_import(str_param(params, "content")?)?;
        Ok(json!({
            "moved": p.moved,
            "disabled": p.disabled,
            "formats": p.formats,
            "skipped": p.skipped,
            "kinds": p.kinds,
        }))
    }

    fn web_quick_import(&self, params: &Value) -> anyhow::Result<Value> {
        let content = str_param(params, "content")?;
        // 与词库/短语导入同一套参数名：`strategy = "replace"` 先清空，其余为合并。
        let replace = params
            .get("strategy")
            .and_then(|v| v.as_str())
            .is_some_and(|s| s.eq_ignore_ascii_case("replace"));
        let o = self.quick_format_import(content, replace)?;
        Ok(json!({
            "moved": o.moved,
            "disabled": o.disabled,
            "formats": o.formats,
            "skipped": o.skipped,
        }))
    }

    fn web_stats_summary(&self) -> anyhow::Result<Value> {
        use chrono::Datelike;
        let (collector, store) = match (self.stat_collector(), self.user_store()) {
            (Some(c), Some(s)) => (c, s),
            _ => return Ok(Self::empty_stats_summary()),
        };
        // 当日数据来自采集器内存（始终最新、完整）；历史从 store 读。
        let today_stat = collector.get_today_stat();
        let meta = collector.get_meta();
        let today = today_str();
        let today_total = today_stat.total();

        // 活跃天数（DB 天数；今天有数据但未 flush 时 +1）+ 日均。
        let all = store
            .daily_stats("0000-01-01", "9999-12-31")
            .unwrap_or_default();
        let mut active_days = all.iter().filter(|(_, r)| r.total() > 0).count();
        let today_in_db = all.iter().any(|(d, _)| d == &today);
        if today_total > 0 && !today_in_db {
            active_days += 1;
        }
        let daily_avg = if active_days > 0 {
            meta.total_chars / active_days as u64
        } else {
            0
        };

        // 周（周日起）/ 月统计（YYYY-MM-DD 字典序），今天用内存值。
        let now = chrono::Local::now().date_naive();
        let week_start = now - chrono::Duration::days(now.weekday().num_days_from_sunday() as i64);
        let month_start = now.with_day(1).unwrap_or(now);
        let week_chars: u64 = Self::daily_with_today_mem(
            store,
            &week_start.format("%Y-%m-%d").to_string(),
            &today,
            &today_stat,
        )
        .iter()
        .map(|(_, r)| r.total() as u64)
        .sum();
        let month_chars: u64 = Self::daily_with_today_mem(
            store,
            &month_start.format("%Y-%m-%d").to_string(),
            &today,
            &today_stat,
        )
        .iter()
        .map(|(_, r)| r.total() as u64)
        .sum();

        // 近 90 天：最高日 / 平均码长 / 首选率 / 平均速度。
        let recent_from = (now - chrono::Duration::days(90))
            .format("%Y-%m-%d")
            .to_string();
        let recent = Self::daily_with_today_mem(store, &recent_from, &today, &today_stat);
        let (mut max_day_chars, mut max_day_date) = (0u32, String::new());
        let (mut cl_sum, mut cl_cnt, mut first_sel, mut cand_sel) = (0u64, 0u64, 0u64, 0u64);
        let (mut sp_chars, mut sp_active) = (0u64, 0u64);
        for (d, r) in &recent {
            let t = r.total();
            if t > max_day_chars {
                max_day_chars = t;
                max_day_date = d.clone();
            }
            cl_sum += r.code_len_sum as u64;
            cl_cnt += r.code_len_count as u64;
            first_sel += r.cand_pos_dist[0] as u64;
            cand_sel += r.cand_pos_dist.iter().map(|&v| v as u64).sum::<u64>();
            sp_chars += t as u64;
            sp_active += r.active_seconds as u64;
        }
        let avg_code_len = if cl_cnt > 0 {
            cl_sum as f64 / cl_cnt as f64
        } else {
            0.0
        };
        let first_select_rate = if cand_sel > 0 {
            first_sel as f64 / cand_sel as f64
        } else {
            0.0
        };
        let today_speed =
            wind_store::stats::speed_per_minute(today_total, today_stat.active_seconds);
        let overall_speed = wind_store::stats::speed_per_minute(
            sp_chars.min(u32::MAX as u64) as u32,
            sp_active.min(u32::MAX as u64) as u32,
        );

        Ok(json!({
            "today_chars": today_total,
            "today_chinese": today_stat.chinese,
            "today_english": today_stat.english,
            "total_chars": meta.total_chars,
            "active_days": active_days,
            "daily_avg": daily_avg,
            "streak_current": meta.streak_current,
            "streak_max": meta.streak_max,
            "week_chars": week_chars,
            "month_chars": month_chars,
            "max_day_chars": max_day_chars,
            "max_day_date": max_day_date,
            "avg_code_len": avg_code_len,
            "first_select_rate": first_select_rate,
            "today_speed": today_speed,
            "overall_speed": overall_speed,
            "max_speed": meta.max_speed,
        }))
    }

    fn web_stats_daily(&self, params: &Value) -> anyhow::Result<Value> {
        let from = str_param(params, "from")?.to_string();
        let to = str_param(params, "to")?.to_string();
        let store = match self.user_store() {
            Some(s) => s,
            None => return Ok(json!([])),
        };
        // 真实数据按日期索引；今天用采集器内存最新值覆盖（DB 可能未 flush）。
        let mut by_date: std::collections::HashMap<String, wind_store::stats::DailyStats> =
            store.daily_stats(&from, &to)?.into_iter().collect();
        let today = today_str();
        if let Some(c) = self.stat_collector() {
            let ts = c.get_today_stat();
            if today.as_str() >= from.as_str() && today.as_str() <= to.as_str() && ts.total() > 0 {
                by_date.insert(today.clone(), ts);
            }
        }
        // 区间内连续日期补零值，输出完整 DailyStatItem（便于前端绘图）。
        let mut out = Vec::new();
        if let (Ok(f), Ok(t)) = (
            chrono::NaiveDate::parse_from_str(&from, "%Y-%m-%d"),
            chrono::NaiveDate::parse_from_str(&to, "%Y-%m-%d"),
        ) {
            let mut cur = f;
            while cur <= t {
                let key = cur.format("%Y-%m-%d").to_string();
                let rec = by_date.get(&key).cloned().unwrap_or_default();
                out.push(Self::daily_item_json(&key, &rec));
                cur += chrono::Duration::days(1);
            }
        }
        Ok(json!(out))
    }

    fn web_stats_clear(&self) -> anyhow::Result<Value> {
        if let Some(store) = self.user_store() {
            store.clear_stats()?;
        }
        // 同步清空采集器内存（今日 + 元数据），否则 summary 仍读到旧内存值。
        if let Some(c) = self.stat_collector() {
            c.reset();
        }
        Ok(json!({ "ok": true }))
    }

    fn web_stats_prune(&self, params: &Value) -> anyhow::Result<Value> {
        // 参数 days：删除早于 (今天 - days) 的统计。
        let days = params
            .get("days")
            .and_then(|v| v.as_i64())
            .unwrap_or(0)
            .max(0);
        let store = match self.user_store() {
            Some(s) => s,
            None => return Ok(json!({ "pruned": 0 })),
        };
        let before = (chrono::Local::now().date_naive() - chrono::Duration::days(days))
            .format("%Y-%m-%d")
            .to_string();
        let n = store.prune_stats_before(&before)?;
        // 重建元数据（剔除已删历史）并让采集器重载：先 flush 今日落库，recalc 后 resume。
        if let Some(c) = self.stat_collector() {
            c.flush();
            store.recalculate_stats_meta()?;
            c.resume();
        } else {
            store.recalculate_stats_meta()?;
        }
        Ok(json!({ "pruned": n }))
    }

    /// 取 [from, today] 的每日统计，今天用采集器内存值替换/追加（对齐 Go GetSummary 用内存今天）。
    fn daily_with_today_mem(
        store: &wind_store::Store,
        from: &str,
        today: &str,
        today_stat: &wind_store::stats::DailyStats,
    ) -> Vec<(String, wind_store::stats::DailyStats)> {
        let mut days = store.daily_stats(from, today).unwrap_or_default();
        let mut has = false;
        for (d, r) in days.iter_mut() {
            if d == today {
                *r = today_stat.clone();
                has = true;
            }
        }
        if !has && today >= from && today_stat.total() > 0 {
            days.push((today.to_string(), today_stat.clone()));
        }
        days
    }

    /// 组装前端 DailyStatItem JSON（紧凑字段名，含按方案 bs / 按来源 src）。
    fn daily_item_json(date: &str, r: &wind_store::stats::DailyStats) -> Value {
        let bs: serde_json::Map<String, Value> = r
            .by_schema
            .iter()
            .map(|(k, s)| {
                (
                    k.clone(),
                    json!({
                        "tc": s.total_chars,
                        "cn": s.commit_count,
                        "cls": s.code_len_sum,
                        "clc": s.code_len_count,
                        "cpd": s.cand_pos_dist,
                    }),
                )
            })
            .collect();
        json!({
            "d": date,
            "tc": r.total(),
            "cc": r.chinese,
            "ec": r.english,
            "pc": r.punct,
            "oc": r.other,
            "h": r.hours,
            "cn": r.commit_count,
            "cls": r.code_len_sum,
            "clc": r.code_len_count,
            "cld": r.code_len_dist,
            "cpd": r.cand_pos_dist,
            "as": r.active_seconds,
            "bs": bs,
            "src": r.by_source,
        })
    }

    /// 无采集器/存储时的空摘要（17 字段全 0，对齐前端 StatsSummary 形状）。
    fn empty_stats_summary() -> Value {
        json!({
            "today_chars": 0, "today_chinese": 0, "today_english": 0,
            "total_chars": 0, "active_days": 0, "daily_avg": 0,
            "streak_current": 0, "streak_max": 0, "week_chars": 0, "month_chars": 0,
            "max_day_chars": 0, "max_day_date": "", "avg_code_len": 0.0,
            "first_select_rate": 0.0, "today_speed": 0, "overall_speed": 0, "max_speed": 0,
        })
    }

    /// 主题查找目录：用户主题（user_config_dir/themes）优先，回退内置（data/themes）。
    fn theme_dirs(&self) -> Vec<std::path::PathBuf> {
        let mut dirs = Vec::new();
        if let Some(u) = wind_config::Config::user_config_dir() {
            dirs.push(u.join("themes"));
        }
        if let Some(d) = self.themes_dir() {
            dirs.push(d.to_path_buf());
        }
        dirs
    }

    /// 用户主题写入目录（导入/删除）。
    fn user_themes_dir(&self) -> Option<std::path::PathBuf> {
        wind_config::Config::user_config_dir().map(|u| u.join("themes"))
    }

    fn web_theme_preview(&self, params: &Value) -> anyhow::Result<Value> {
        let name = str_param(params, "name")?;
        let dirs = self.theme_dirs();
        // 合并 base 链 + 归一化（扁平人写形态 → 嵌套内存形态）后的主题配置（toml::Value → JSON），
        // 供前端预览渲染（保持历史 views.* 嵌套契约）。
        let merged = wind_theme::load_merged_dirs(&dirs, name, 0)?;
        let normalized = wind_theme::normalize::normalize_theme(merged);
        Ok(serde_json::to_value(&normalized)?)
    }

    fn web_theme_get_text(&self, params: &Value) -> anyhow::Result<Value> {
        let slug = str_param(params, "slug")?;
        if slug.is_empty() || slug.contains('/') || slug.contains('\\') || slug.contains("..") {
            anyhow::bail!("非法主题 slug");
        }
        for dir in self.theme_dirs() {
            let path = dir.join(slug).join("theme.toml");
            if path.is_file() {
                let toml = std::fs::read_to_string(&path)
                    .map_err(|e| anyhow::anyhow!("读取主题失败：{e}"))?;
                return Ok(json!({ "slug": slug, "toml": toml }));
            }
        }
        anyhow::bail!("主题不存在")
    }

    fn web_theme_delete(&self, params: &Value) -> anyhow::Result<Value> {
        let name = str_param(params, "name")?;
        let user_dir = self
            .user_themes_dir()
            .ok_or_else(|| anyhow::anyhow!("无用户主题目录"))?;
        let target = user_dir.join(name);
        if !target.join("theme.toml").exists() {
            anyhow::bail!("内置主题不可删除或主题不存在: {}", name);
        }
        std::fs::remove_dir_all(&target)?;
        Ok(json!({ "ok": true }))
    }

    fn web_theme_import_text(&self, params: &Value) -> anyhow::Result<Value> {
        // 参数键沿用 "yaml"（前端契约未改），内容为 TOML 文本。
        let text = str_param(params, "yaml")?;
        let force = params
            .get("force")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        // 校验可解析为合法主题（仅自身，未校验 base 依赖链）。
        wind_theme::validate_text(text)?;
        let meta = wind_theme::meta_from_text(text)
            .ok_or_else(|| anyhow::anyhow!("主题缺少 meta.name"))?;
        if meta.name.trim().is_empty() {
            anyhow::bail!("主题 meta.name 为空");
        }
        let user_dir = self
            .user_themes_dir()
            .ok_or_else(|| anyhow::anyhow!("无用户主题目录"))?;
        // 目标目录 id：以调用方传入的 slug（主题唯一 id）为准——
        //   传了 slug：目录已存在则就地写回（不新建），否则以 slug 建目录（id 与目录名一致）；
        //   未传 slug（兼容旧客户端）：退回按 meta.name 建目录。
        let slug = params
            .get("slug")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| {
                !s.is_empty() && !s.contains('/') && !s.contains('\\') && !s.contains("..")
            });
        let theme_id = slug.unwrap_or(meta.name.as_str()).to_string();
        let target = user_dir.join(&theme_id);
        let file = target.join("theme.toml");
        let existed_before = file.exists();
        if existed_before && !force {
            anyhow::bail!("主题已存在（force=false）: {}", meta.name);
        }
        // 覆盖已存在主题前备份原文本，供依赖链校验失败时回滚。
        let backup = if existed_before {
            std::fs::read(&file).ok()
        } else {
            None
        };
        std::fs::create_dir_all(&target)?;
        let tmp = file.with_extension("toml.tmp");
        std::fs::write(&tmp, text.as_bytes())?;
        std::fs::rename(&tmp, &file)?;

        // 依赖链校验：写入后按真实主题目录做完整 base 链合并求值，
        // 捕获「base 引用的基础主题不存在」「继承成环」「合并后结构非法」等 validate_text 单文件校验
        // 无法发现的问题。校验失败则回滚（新写入的删除目录；覆盖的恢复原文本）。
        let dirs = self.theme_dirs();
        if let Err(e) = wind_theme::theme::load_typed_dirs(&dirs, &theme_id) {
            match backup {
                Some(bytes) => {
                    let _ = std::fs::write(&file, bytes);
                }
                None => {
                    let _ = std::fs::remove_dir_all(&target);
                }
            }
            anyhow::bail!(
                "主题依赖校验失败：{}（请检查 base 引用的基础主题是否存在）",
                e
            );
        }

        // 推送的就是当前生效主题 → 立刻重解析下发，编辑器里改完即见效。
        // 不做则只落盘，用户得手动切走再切回来（或重启）才看得到自己刚推的改动。
        //
        // 判据是**目录 id 相同**，不比对文件内容：主题可能被 base 继承链间接影响，
        // 且重解析一次远比误判便宜。用户目录优先于安装目录，所以推一个与内置主题
        // 同 id 的用户主题（如 slug=default）同样会改变实际生效外观，这里一并覆盖。
        let current = self.current_theme_name();
        let reloaded = current == theme_id;
        if reloaded {
            // 明暗沿用当前 style（system 时按系统实时判定），与 on_system_theme_changed 同一出口。
            let dark = self.current_theme_is_dark();
            tracing::info!("导入的是当前主题 {}，重新加载以即时生效", theme_id);
            self.push_theme(&theme_id, dark);
        }
        Ok(json!({
            "ok": true,
            "slug": theme_id,
            "display_name": meta.name,
            // 供设置层如实回报给编辑器（此前那一层硬编码 false）。
            "reloaded": reloaded,
        }))
    }

    fn web_theme_list(&self) -> anyhow::Result<Value> {
        // 复用右键菜单的 list_themes_full 顺序，保证与菜单一致 (#5/主题)。
        let dirs = self.theme_search_dirs();
        let out: Vec<Value> = self
            .list_themes_full()
            .into_iter()
            .map(|(id, display, builtin)| {
                let meta = wind_theme::read_meta(&dirs, &id);
                json!({
                    "name": id,
                    "display_name": display,
                    "author": meta.as_ref().map(|m| m.author.clone()).unwrap_or_default(),
                    "version": meta.as_ref().map(|m| m.version.clone()).unwrap_or_default(),
                    "builtin": builtin,
                })
            })
            .collect();
        Ok(json!(out))
    }
}

// 不用 blanket impl：`Arc<Coordinator>` 的方法解析会先命中 T=Arc 的 blanket 候选、
// bound 不满足即报错而不再 deref；具体 impl 才能让 Arc 自动 deref 到 &Coordinator。
impl WebDataRpc for wind_coordinator::Coordinator {}

/// 存储域 → 设置页显示域：真实文本投影为可编辑的转义形态（真换行→`\n`、制表→`\t`、
/// 反斜杠→`\\`）。
///
/// 设置页是**文本编辑界面**，而编辑界面里"看不见的字符"是不可编辑的：一条含真换行的
/// 短语在输入框里只显示成一个断行，用户既分不清那是换行还是别的空白，也无从表达
/// "我要一个字面反斜杠"。投影成转义形态后，所见即所得。
///
/// 与 [`word_item`] 里 `code` 的处理同源——那里也是把存储域的扁平码投影成带空格的
/// 音节码给人看。**存储域与显示域本就该分开**，此处只是把同一原则用到 text 上。
///
/// 命令栏语法条目只投影换行/制表、反斜杠原样——见 [`wind_store::wdict::escape_text_field`]。
/// 设置页是这类条目最主要的书写入口，双重转义在这里最致命：文档写「路径要写 `\\`」，
/// 照做却因为本层先吃掉一个而在 lexer 里变成换行。
fn ui_text(s: &str) -> String {
    wind_store::wdict::escape_text_field(s)
}

/// 疑似笔误 → JSON。`kind` 供 UI 判定，`message` 是给人看的一句话。
fn hint_json(h: &wind_cmdbar::Hint) -> Value {
    use wind_cmdbar::Hint as H;
    match h {
        H::ControlCharInPath(f) => json!({
            "kind": "controlCharInPath",
            "func": f,
            "message": format!("{f} 的路径里出现了换行或制表符，通常是反斜杠只写了一个"),
        }),
    }
}

/// 设置页显示域 → 存储域：[`ui_text`] 的逆。
///
/// **凡是从设置页收 text 的 RPC 都必须先过它**，不只是写入类：`dict.remove`/`update`、
/// `freq.delete`、`temp.promote` 等拿 text 当 **key** 去匹配记录，若拿转义形态去查
/// 真实文本的库，结果是查不到——表现为「删了没反应」，且不报错。
fn store_text(s: &str) -> String {
    wind_store::wdict::unescape_text_field(s)
}

/// UserWordRecord → 前端 UserWordItem。
/// 用户词 → 设置页列表项。
///
/// `code` 输出**带空格的音节码**（`ni hao`），与 `dict.encode` 的出码结果同形，
/// 让用户直观看到拼音词库的音节格式。存储侧 key 仍是扁平的——设置页把这个串原样回传
/// 给 add/update/remove 时，由 `normalize_add_code` / `web_dict_remove` 拆回扁平码。
/// 无边界（旧数据/手输码/五笔码）则不含空格，与改动前一致。
fn word_item(r: wind_store::user_words::UserWordRecord) -> Value {
    let code = wind_store::wdict::join_code_by_boundary(&r.code, r.boundary);
    json!({ "code": code, "text": ui_text(&r.text), "weight": r.weight, "enabled": true })
}

/// 稀疏 diff：返回 `cfg` 相对 `base` 的变化项（仅含改动的叶子/键）；无变化返回 None。
/// 对象逐键递归；数组/标量按整体比较（不同则取 cfg）。用于 schema override 最小化。
/// `getConfig` 附带的只读旁路字段——回传 `saveConfig` 时必须剥掉。
///
/// 它们是「当前生效值」的快照，不是方案配置的一部分。设置页的做法是拿整份 getConfig
/// 结果、改几个字段、原样回传；若不剥，`json_diff` 会认定方案文件缺这个键，于是把整份
/// 快照写进 override——从此该方案的行为被**冻结在打开设置页那一刻**，之后改全局配置对
/// 它再无影响，而用户根本没动过这些项。
///
/// 在服务端剥而不是要求调用方自觉：这是契约边界，任何客户端都该受保护。
pub const READONLY_SIDECAR_FIELDS: &[&str] =
    &["effectiveCodetable", "leadingCodeKeys", "keysOverview"];

/// 按键总览：这个方案下每个绑过的键**当前**干什么、来自哪一层。
///
/// # 为什么由内核组装
///
/// 设置页读得到两层的原始表，却算不出全局那份的**折算结果**：`page_keys = ["minus_equal"]`
/// 这类组名要展开成 `minus` / `equal`，展开规则住在 `KeysConfig::effective_session_actions`。
/// 设置页再写一份组名展开表就是两处慢慢漂移——跨仓契约无编译期约束，本仓已栽过
/// （同 `leadingCodeKeys` 那条：边界上传语义结果，不传待解析的原料）。
///
/// # 为什么只读
///
/// 一个键的当前动作可能来自折算、也可能来自方案层，反写就要决定写哪一层，那就是第二个
/// 真相源，会重蹈 `trigger_keys` 五处并存的覆辙。设置页据此展示并**跳转**到对应编辑处。
/// 判据见 `docs/design/key-resolver-unification.md` §4.3。
///
/// # 动词不翻译
///
/// 给动词原值（`page_prev`）而不是中文名：文案值域已经在设置页的下拉里（两处各一份中文名
/// 必然漂移），而这里给的是**语义结果**——哪个键、什么动作、来自哪层。翻译归 UI。
fn keys_overview(schema_cfg: &Value) -> Vec<Value> {
    let Ok(mut cfg) = wind_config::Config::load(wind_config::Config::data_dir().as_deref()) else {
        // 读不到全局配置就整表不给：只列方案层会让用户以为「全局什么都没配」，
        // 半份数据比没有更误导。
        return Vec::new();
    };
    // 与 `ConfigBundle::build` 一样先 normalize：那里还有存量迁移（旧字段折算进
    // `key_actions` / `session_actions`），跳过它算出来的表与运行时不一致。
    cfg.normalize();

    let schema_table = |key: &str| -> std::collections::BTreeMap<String, String> {
        schema_cfg
            .get(key)
            .and_then(|x| x.as_object())
            .map(|o| {
                o.iter()
                    .map(|(k, v)| (k.clone(), v.as_str().unwrap_or("").to_string()))
                    .collect()
            })
            .unwrap_or_default()
    };

    let mut out = Vec::new();
    push_overview_layer(
        &mut out,
        "lead",
        &cfg.keys.key_actions,
        &schema_table("key_actions"),
    );
    push_overview_layer(
        &mut out,
        "session",
        &cfg.keys.effective_session_actions(),
        &schema_table("session_actions"),
    );
    out
}

/// 一张表的两层仲裁：方案层表了态就用方案的，否则用全局的。
///
/// ★ 方案层的**显式 `none` 也是表态**（＝本方案禁用），不回落全局。这与内核
/// `Coordinator::session_action_for` / `bound_action_with_source` 的处置逐条一致；
/// 若在这里改成「none 视同没配」，总览显示的就与实际行为相反。
fn push_overview_layer(
    out: &mut Vec<Value>,
    table: &str,
    global: &std::collections::BTreeMap<String, String>,
    schema: &std::collections::BTreeMap<String, String>,
) {
    let mut keys: std::collections::BTreeSet<&String> = global.keys().collect();
    keys.extend(schema.keys());
    for k in keys {
        let (action, from) = match schema.get(k) {
            Some(v) => (v.as_str(), "schema"),
            None => (global.get(k).map(String::as_str).unwrap_or(""), "global"),
        };
        out.push(json!({ "key": k, "table": table, "action": action, "from": from }));
    }
}

fn strip_readonly_fields(cfg: &Value) -> Value {
    let Some(o) = cfg.as_object() else {
        return cfg.clone();
    };
    if !READONLY_SIDECAR_FIELDS.iter().any(|k| o.contains_key(*k)) {
        return cfg.clone();
    }
    let mut o = o.clone();
    for k in READONLY_SIDECAR_FIELDS {
        o.remove(*k);
    }
    Value::Object(o)
}

fn json_diff(base: &Value, cfg: &Value) -> Option<Value> {
    match (base, cfg) {
        (Value::Object(b), Value::Object(c)) => {
            let mut out = serde_json::Map::new();
            for (k, cv) in c {
                match b.get(k) {
                    Some(bv) => {
                        if let Some(d) = json_diff(bv, cv) {
                            out.insert(k.clone(), d);
                        }
                    }
                    None => {
                        out.insert(k.clone(), cv.clone());
                    }
                }
            }
            if out.is_empty() {
                None
            } else {
                Some(Value::Object(out))
            }
        }
        _ => {
            if base == cfg {
                None
            } else {
                Some(cfg.clone())
            }
        }
    }
}

/// JSON → toml::Value（写 override 文件）。null 在对象中跳过（TOML 无 null）。
fn json_to_toml(v: &Value) -> toml::Value {
    match v {
        Value::Null => toml::Value::String(String::new()),
        Value::Bool(b) => toml::Value::Boolean(*b),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                toml::Value::Integer(i)
            } else {
                toml::Value::Float(n.as_f64().unwrap_or(0.0))
            }
        }
        Value::String(s) => toml::Value::String(s.clone()),
        // 跳过数组内 null（TOML 无 null），与对象分支语义一致，避免注入空串污染类型。
        Value::Array(a) => toml::Value::Array(
            a.iter()
                .filter(|x| !x.is_null())
                .map(json_to_toml)
                .collect(),
        ),
        Value::Object(o) => {
            let mut t = toml::map::Map::new();
            for (k, val) in o {
                if !val.is_null() {
                    t.insert(k.clone(), json_to_toml(val));
                }
            }
            toml::Value::Table(t)
        }
    }
}

#[cfg(test)]
mod tests {
    //! 数据域 RPC 契约测试：真实 Coordinator + 临时 redb store，断言 web_data_rpc 输出形状
    //! 与 WindInputSetting 的 mock.ts / models.ts 一致。
    use super::*;
    use std::sync::Arc;
    use wind_config::Config;
    use wind_coordinator::Coordinator;
    use wind_store::Store;

    /// 构造一个带临时 store 的无头 Coordinator。
    fn coord(tag: &str) -> Arc<Coordinator> {
        let path = std::env::temp_dir().join(format!("wind_webdata_{tag}.redb"));
        let _ = std::fs::remove_file(&path);
        let store = Arc::new(Store::open(&path).unwrap());
        Coordinator::new_headless_with_store(Config::default(), None, store)
    }

    /// 在 `{sections:[{key,...}]}` 响应里按 key 取某段（无则 Null）。
    fn sec(v: &Value, key: &str) -> Value {
        v.get("sections")
            .and_then(|s| s.as_array())
            .and_then(|a| {
                a.iter()
                    .find(|x| x.get("key").and_then(|k| k.as_str()) == Some(key))
                    .cloned()
            })
            .unwrap_or(Value::Null)
    }

    /// **设置页显示带空格的音节码，存储 key 保持扁平**——两个域在 RPC 边界上的往返契约。
    ///
    /// 用户在设置页看到 `ni hao`（`dict.encode` / 列表回显同形），把它原样提交回来时，
    /// 写入侧必须拆成扁平 key，否则 `niha` 前缀匹配不到这条记录、逐键出候选就废了。
    /// 反过来 remove/search 收到带空格的串也必须拆，不然删不掉、搜不着。
    ///
    /// 顺带确认一条增益：用户打的空格被当作**显式声明的切分**采信，优先于
    /// `normalize_add_code` 的求解链。
    #[test]
    fn dict_spaced_code_display_flat_storage_roundtrip() {
        let c = coord("spaced_roundtrip");
        let p = |code: &str| {
            serde_json::json!({
                "schemaId": "pinyin", "code": code, "text": "你好", "weight": 500
            })
        };

        // 提交带空格的码（模拟用户从「出码」按钮拿到后直接保存）
        c.web_data_rpc("dict.add", &p("ni hao")).unwrap();

        // 存储侧：key 扁平、边界由空格得来（ni|hao → {0,2}）
        let store = c.user_store().expect("有 store");
        let recs = store.get_user_words("pinyin", "nihao").unwrap();
        assert_eq!(recs.len(), 1, "key 必须是扁平的 nihao，不能带空格");
        assert_eq!(recs[0].boundary, 0b101, "用户打的空格即显式切分，须被采信");

        // 显示侧：列表与搜索都回带空格的码
        let items = c
            .web_data_rpc(
                "dict.search",
                &serde_json::json!({ "schemaId": "pinyin", "query": "ni hao" }),
            )
            .unwrap();
        assert_eq!(
            items[0].get("code").and_then(|v| v.as_str()),
            Some("ni hao"),
            "列表回显须与出码同形；搜索词带空格也要能命中（查询侧同样拆）"
        );

        // 删除：带空格的码同样要能删掉
        c.web_data_rpc("dict.remove", &p("ni hao")).unwrap();
        assert!(
            store.get_user_words("pinyin", "nihao").unwrap().is_empty(),
            "remove 收到带空格的码须先拆再删"
        );
    }

    /// 临时词库与用户词库同款：列表显示带空格，remove / promote 收到后各自拆回扁平码。
    /// **三处必须同改**——只改列表就成了「显示得了、删不掉、也晋升不了」。
    #[test]
    fn temp_word_spaced_code_roundtrip() {
        let c = coord("temp_spaced");
        let store = c.user_store().expect("有 store");
        // hao|ya → 起始字节位 {0,3}
        store
            .learn_temp_word("pinyin", "haoya", "好呀", 500, 0b1001)
            .unwrap();
        store
            .learn_temp_word("pinyin", "nihao", "你好", 500, 0b101)
            .unwrap();

        let items = c
            .web_data_rpc("temp.list", &serde_json::json!({ "schemaId": "pinyin" }))
            .unwrap();
        let codes: Vec<&str> = items
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|v| v.get("code").and_then(|c| c.as_str()))
            .collect();
        assert!(
            codes.contains(&"hao ya") && codes.contains(&"ni hao"),
            "临时词库列表须显示带空格的音节码，实际 {codes:?}"
        );

        // 晋升：带空格的 code 要能查到 temp 记录并写进用户词库
        c.web_data_rpc(
            "temp.promote",
            &serde_json::json!({ "schemaId": "pinyin", "code": "ni hao", "text": "你好" }),
        )
        .unwrap();
        assert!(
            !store.get_user_words("pinyin", "nihao").unwrap().is_empty(),
            "promote 收到带空格的码须先拆再晋升"
        );

        // 删除：同理
        c.web_data_rpc(
            "temp.remove",
            &serde_json::json!({ "schemaId": "pinyin", "code": "hao ya", "text": "好呀" }),
        )
        .unwrap();
        assert!(
            store
                .get_temp_words("pinyin", "haoya")
                .unwrap_or_default()
                .is_empty(),
            "remove 收到带空格的码须先拆再删"
        );
    }

    /// 词频列表的编码显示带音节空格 —— 边界靠**反查**，因为词频表自己不存 boundary。
    ///
    /// 三处来源依次问：系统词典 → 用户词表 → 临时词表。这里用后两者（无真实词库时
    /// 系统词典为空，正好把反查降级链走一遍）。查不到的记录原样保持扁平，不得报错。
    ///
    /// 同时锁住 freq.delete：列表给的是带空格的 code，不拆就删不掉。
    #[test]
    fn freq_list_shows_spaced_code_via_boundary_lookup() {
        let c = coord("freq_spaced");
        let store = c.user_store().expect("有 store");
        // 用户词提供边界：ni|hao
        store
            .add_user_word("pinyin", "nihao", "你好", 500, 0b101)
            .unwrap();
        // 临时词提供边界：hao|ya
        store
            .learn_temp_word("pinyin", "haoya", "好呀", 500, 0b1001)
            .unwrap();
        // 无处可查 → 保持扁平（存量简拼记录/已删词条的遗留记录都是这种）
        store.record_freq("pinyin", "nihao", "你好").unwrap();
        store.record_freq("pinyin", "haoya", "好呀").unwrap();
        store.record_freq("pinyin", "wubian", "无边").unwrap();

        let r = c
            .web_data_rpc(
                "freq.listPaged",
                &serde_json::json!({ "schemaId": "pinyin" }),
            )
            .unwrap();
        let items = r.get("items").and_then(|v| v.as_array()).unwrap().clone();
        let code_of = |text: &str| -> String {
            items
                .iter()
                .find(|x| x.get("text").and_then(|t| t.as_str()) == Some(text))
                .and_then(|x| x.get("code").and_then(|c| c.as_str()))
                .unwrap_or_default()
                .to_string()
        };

        assert_eq!(code_of("你好"), "ni hao", "边界应从用户词表反查到");
        assert_eq!(code_of("好呀"), "hao ya", "边界应从临时词表反查到");
        assert_eq!(
            code_of("无边"),
            "wubian",
            "三处都查不到 → 保持扁平码，不得报错"
        );

        // 删除：列表给的是带空格的 code
        c.web_data_rpc(
            "freq.delete",
            &serde_json::json!({ "schemaId": "pinyin", "code": "ni hao", "text": "你好" }),
        )
        .unwrap();
        let r2 = c
            .web_data_rpc(
                "freq.listPaged",
                &serde_json::json!({ "schemaId": "pinyin" }),
            )
            .unwrap();
        let left = r2.get("total").and_then(|v| v.as_u64()).unwrap();
        assert_eq!(left, 2, "带空格的 code 须先拆再删，否则删不掉");
    }

    /// 词频列表的编码搜索同样要能命中中段（与用户词库同款，两处是各自独立的实现）。
    #[test]
    fn freq_search_matches_code_middle_segment() {
        let c = coord("freq_search_middle");
        let store = c.user_store().expect("有 store");
        store.record_freq("pinyin", "haoya", "好呀").unwrap();
        store.record_freq("pinyin", "nihao", "你好").unwrap();

        let hits = |q: &str| -> Vec<String> {
            let r = c
                .web_data_rpc(
                    "freq.listPaged",
                    &serde_json::json!({ "schemaId": "pinyin", "prefix": q }),
                )
                .unwrap();
            r.get("items")
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.get("text").and_then(|t| t.as_str()))
                        .map(String::from)
                        .collect()
                })
                .unwrap_or_default()
        };

        assert!(
            hits("hao").contains(&"好呀".to_string()),
            "前缀命中（原有行为）"
        );
        assert!(
            hits("ya").contains(&"好呀".to_string()),
            "中段命中：haoya 搜 ya 须能找到"
        );
        assert!(
            hits("hao").contains(&"你好".to_string()),
            "nihao 的中段 hao 同样要命中"
        );
        // 词频 key 是扁平码，但用户可能从用户词库列表复制带空格的串来搜
        assert!(
            hits("ni hao").contains(&"你好".to_string()),
            "带空格的搜索词须先拆再匹配"
        );
    }

    /// 编码搜索须能命中**中段**，不能只认前缀。
    ///
    /// redb 前缀扫描只覆盖开头，`haoya` 搜 `ya` 一条也出不来——而搜索框并没有告诉用户
    /// 它只认前缀。词条内容搜索本就在做全量扫描，编码子串搭同一趟车，不增加扫描次数。
    #[test]
    fn dict_search_matches_code_middle_segment() {
        let c = coord("search_middle");
        let add = |code: &str, text: &str| {
            c.web_data_rpc(
                "dict.add",
                &serde_json::json!({
                    "schemaId": "pinyin", "code": code, "text": text, "weight": 500
                }),
            )
            .unwrap();
        };
        add("hao ya", "好呀");
        add("ni hao", "你好");

        let hits = |q: &str| -> Vec<String> {
            let r = c
                .web_data_rpc(
                    "dict.listPaged",
                    &serde_json::json!({ "schemaId": "pinyin", "prefix": q }),
                )
                .unwrap();
            r.get("items")
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.get("text").and_then(|t| t.as_str()))
                        .map(String::from)
                        .collect()
                })
                .unwrap_or_default()
        };

        assert!(
            hits("hao").contains(&"好呀".to_string()),
            "前缀命中（原有行为）"
        );
        assert!(
            hits("ya").contains(&"好呀".to_string()),
            "中段命中：haoya 搜 ya 须能找到"
        );
        assert!(
            hits("hao").contains(&"你好".to_string()),
            "nihao 的中段 hao 同样要命中"
        );
        // 带空格的搜索词照样走中段匹配（先拆再比）
        assert!(
            hits("hao ya").contains(&"好呀".to_string()),
            "带空格的搜索词须先拆再匹配"
        );
    }

    /// 逐行短语文本（`wind:p1`）导入契约：预览落点 → 缺省全部导入 → 重导跳过。
    ///
    /// 命令短语不再受任何额外门控——它本就是短语的主要用途，且不会自行执行。
    #[test]
    fn phrase_text_import_contract() {
        let q = char::from_u32(34).unwrap();
        let nl = char::from_u32(10).unwrap();
        let c = coord("phrasetext");
        let text = format!(
            "wind:p1 我的直通车{nl}kx (＾▽＾){nl}             zd $CC({q}记事本{q}, proc.run({q}notepad.exe{q})){nl}             sh $CC({q}跑{q}, proc.shell({q}echo hi{q})){nl}             编码 缺少合法编码的一行{nl}"
        );

        let prev = c
            .web_data_rpc("phrase.previewImportText", &json!({ "content": text }))
            .unwrap();
        assert_eq!(
            prev.get("title").and_then(|v| v.as_str()),
            Some("我的直通车")
        );
        let entries = prev.get("entries").unwrap().as_array().unwrap();
        assert_eq!(entries.len(), 3, "非法行不进 entries");
        assert_eq!(entries[0].get("line").and_then(|v| v.as_u64()), Some(2));
        assert_eq!(
            entries[0].get("status").and_then(|v| v.as_str()),
            Some("new")
        );

        // 能力分级已移除：预览不再携带这三个字段。
        for e in entries {
            assert!(e.get("capability").is_none(), "不应再有 capability");
            assert!(e.get("highRisk").is_none(), "不应再有 highRisk");
            assert!(e.get("effects").is_none(), "不应再有 effects");
        }

        assert_eq!(
            prev.get("problems").unwrap().as_array().unwrap().len(),
            1,
            "非法编码的行进 problems"
        );
        assert_eq!(
            prev.get("counts")
                .unwrap()
                .get("new")
                .and_then(|v| v.as_u64()),
            Some(3)
        );

        // ★ 缺省全导：含 proc.shell 的那条也直接装进去。
        let out = c
            .web_data_rpc("phrase.importText", &json!({ "content": text }))
            .unwrap();
        assert_eq!(
            out.get("added").and_then(|v| v.as_u64()),
            Some(3),
            "不传 accept 就全部导入，命令短语不再需要额外确认"
        );

        // 再来一次：已存在的原样跳过。
        let again = c
            .web_data_rpc("phrase.importText", &json!({ "content": text }))
            .unwrap();
        assert_eq!(again.get("added").and_then(|v| v.as_u64()), Some(0));
        assert_eq!(
            again.get("skippedExisting").and_then(|v| v.as_u64()),
            Some(3)
        );

        // accept 仍可用于选择性导入。
        let c2 = coord("phrasetextsel");
        let sel = c2
            .web_data_rpc(
                "phrase.importText",
                &json!({ "content": text, "accept": [2] }),
            )
            .unwrap();
        assert_eq!(sel.get("added").and_then(|v| v.as_u64()), Some(1));
        assert_eq!(
            sel.get("skippedUnselected").and_then(|v| v.as_u64()),
            Some(2)
        );

        // position 依次追加。
        let list = c.web_data_rpc("phrase.listUser", &json!({})).unwrap();
        let mut got: Vec<(&str, i64)> = list
            .get("items")
            .unwrap()
            .as_array()
            .unwrap()
            .iter()
            .map(|x| {
                (
                    x.get("code").and_then(|v| v.as_str()).unwrap(),
                    x.get("position").and_then(|v| v.as_i64()).unwrap(),
                )
            })
            .collect();
        got.sort_by_key(|(_, p)| *p);
        let codes: Vec<&str> = got.iter().map(|(c, _)| *c).collect();
        assert_eq!(codes, vec!["kx", "zd", "sh"], "position 按导入顺序递增");
    }

    /// 语法坏掉的条目一律不装——无论有没有被 accept 选中。
    #[test]
    fn phrase_text_rejects_broken_syntax() {
        let c = coord("phrasetextbad");
        let text = "wind:p1\nbad $CC(\"未闭合\nok 好\n";
        let out = c
            .web_data_rpc(
                "phrase.importText",
                &json!({ "content": text, "accept": [2, 3] }),
            )
            .unwrap();
        assert_eq!(out.get("added").and_then(|v| v.as_u64()), Some(1));
        assert_eq!(
            out.get("skippedInvalid").and_then(|v| v.as_u64()),
            Some(1),
            "显式 accept 也不能让语法错误的条目进库"
        );
    }

    /// ★ 分发文本 / 设置页手动新增 / wdict 文件导入，三条路径对同一段文本必须落到
    /// **同一形态**。分发格式没有、也不该有自己的一套反斜杠规则——用户在任一处学到的
    /// 写法要能照搬，这条断言就是那个承诺。
    #[test]
    fn phrase_text_escape_domain_matches_manual_add() {
        let bs = char::from_u32(92).unwrap();
        let q = char::from_u32(34).unwrap();
        let tab = char::from_u32(9).unwrap();
        let nl = char::from_u32(10).unwrap();
        let c = coord("phraseesc");

        // 按约定写法：一个字面反斜杠写两个。目录名刻意取 `notes`——单写时反斜杠
        // 紧跟 n 会变成换行，是最典型的静默损坏。
        let plain = format!("D:{bs}{bs}notes");
        let cmd = format!("$CC({q}x{q}, proc.run({q}D:{bs}{bs}notes{bs}{bs}a.exe{q}))");

        c.web_data_rpc("phrase.add", &json!({ "code": "m1", "text": plain }))
            .unwrap();
        c.web_data_rpc("phrase.add", &json!({ "code": "m2", "text": cmd }))
            .unwrap();
        c.web_data_rpc(
            "phrase.importText",
            &json!({ "content": format!("wind:p1{nl}d1 {plain}{nl}d2 {cmd}{nl}") }),
        )
        .unwrap();
        let hdr = format!(
            "wind_dict:{nl}  version: 1{nl}  sections:{nl}    phrases:{nl}      columns: [code, text, weight, position, enabled]{nl}{nl}--- !phrases{nl}"
        );
        c.web_data_rpc(
            "phrase.import",
            &json!({ "content": format!("{hdr}w1{tab}{plain}{tab}1800{tab}9{tab}1{nl}w2{tab}{cmd}{tab}1800{tab}10{tab}1{nl}") }),
        )
        .unwrap();

        let list = c.web_data_rpc("phrase.listUser", &json!({})).unwrap();
        let text_of = |src: &Value, code: &str| -> String {
            src.get("items")
                .unwrap()
                .as_array()
                .unwrap()
                .iter()
                .find(|it| it.get("code").and_then(|v| v.as_str()) == Some(code))
                .and_then(|it| it.get("text").and_then(|v| v.as_str()))
                .unwrap_or_default()
                .to_string()
        };

        assert_eq!(
            text_of(&list, "m1"),
            text_of(&list, "d1"),
            "普通短语：手动新增与分发导入须同形"
        );
        assert_eq!(
            text_of(&list, "m1"),
            text_of(&list, "w1"),
            "普通短语：手动新增与 wdict 导入须同形"
        );
        assert_eq!(
            text_of(&list, "m2"),
            text_of(&list, "d2"),
            "命令短语：手动新增与分发导入须同形"
        );
        assert_eq!(
            text_of(&list, "m2"),
            text_of(&list, "w2"),
            "命令短语：手动新增与 wdict 导入须同形"
        );

        // 双写确实得到**字面反斜杠**：回显是 escape 后的形态，字面反斜杠会再次双写。
        // 若存进去的是换行（单写的后果），回显只会有一个反斜杠。
        assert_eq!(
            text_of(&list, "d1"),
            plain,
            "双写往返后仍是双写 ⇒ 存的是字面反斜杠"
        );
        assert_eq!(
            text_of(&list, "d1").matches(bs).count(),
            2,
            "期望两个反斜杠，实得 {:?}",
            text_of(&list, "d1")
        );

        // 对照：单写一个反斜杠时它紧跟 n 会变成换行，回显与双写**不同**。
        // 这正是文档要求一律写两个的原因，也保证上面的断言不是恒真。
        c.web_data_rpc(
            "phrase.importText",
            &json!({ "content": format!("wind:p1{nl}s1 D:{bs}notes{nl}") }),
        )
        .unwrap();
        let list2 = c.web_data_rpc("phrase.listUser", &json!({})).unwrap();
        assert_ne!(
            text_of(&list2, "s1"),
            plain,
            "单写与双写必须落到不同内容，否则上面的断言证明不了什么"
        );
    }

    /// 路径里反斜杠写少了 ⇒ 预览里给一句提示，但**不阻止导入**（轻微提示）。
    #[test]
    fn phrase_text_hints_single_backslash_path() {
        let bs = char::from_u32(92).unwrap();
        let q = char::from_u32(34).unwrap();
        let nl = char::from_u32(10).unwrap();
        let c = coord("phrasehint");

        let bad = format!("$CC({q}x{q}, proc.run({q}D:{bs}notes{bs}a.exe{q}))");
        let good = format!("$CC({q}x{q}, proc.run({q}D:{bs}{bs}notes{bs}{bs}a.exe{q}))");
        let text = format!("wind:p1{nl}b1 {bad}{nl}g1 {good}{nl}");

        let prev = c
            .web_data_rpc("phrase.previewImportText", &json!({ "content": text }))
            .unwrap();
        let entries = prev.get("entries").unwrap().as_array().unwrap();
        let hints_of = |i: usize| entries[i].get("hints").unwrap().as_array().unwrap();

        assert_eq!(hints_of(0).len(), 1, "单写路径应有提示");
        assert_eq!(
            hints_of(0)[0].get("kind").and_then(|v| v.as_str()),
            Some("controlCharInPath")
        );
        assert!(hints_of(1).is_empty(), "双写路径不该有提示");

        // 轻微提示：不抦导入。
        let out = c
            .web_data_rpc(
                "phrase.importText",
                &json!({ "content": text, "accept": [2, 3] }),
            )
            .unwrap();
        assert_eq!(
            out.get("added").and_then(|v| v.as_u64()),
            Some(2),
            "带提示的条目照样可以导入"
        );
    }

    /// 没有格式标记 ⇒ 整段拒绝。侦测「不猜」，报错优于误装。
    #[test]
    fn phrase_text_requires_marker() {
        let c = coord("phrasetextmark");
        assert!(
            c.web_data_rpc("phrase.previewImportText", &json!({ "content": "kx 好\n" }))
                .is_err()
        );
    }

    #[test]
    fn dict_export_import_preview_contract() {
        let c = coord("dictio");
        c.web_data_rpc(
            "dict.add",
            &json!({ "schemaId": "wb", "code": "a", "text": "工", "weight": 100 }),
        )
        .unwrap();

        // export → {content} 且是 wdict words 文本
        let exp = c
            .web_data_rpc("dict.export", &json!({ "schemaId": "wb" }))
            .unwrap();
        let content = exp
            .get("content")
            .and_then(|v| v.as_str())
            .expect("content 字符串");
        assert!(content.contains("--- !words"));

        // preview 到空 schema:userWords 段全 willAdd
        let prev = c
            .web_data_rpc(
                "dict.previewImport",
                &json!({ "schemaId": "wb2", "content": content }),
            )
            .unwrap();
        assert_eq!(
            prev.get("format").and_then(|v| v.as_str()),
            Some("winddict")
        );
        let uw = sec(&prev, "userWords");
        assert_eq!(uw.get("willAdd").and_then(|v| v.as_u64()), Some(1));
        assert_eq!(uw.get("willUpdate").and_then(|v| v.as_u64()), Some(0));
        assert_eq!(uw.get("unchanged").and_then(|v| v.as_u64()), Some(0));
        assert!(uw.get("samples").and_then(|v| v.as_array()).is_some());

        // import(缺省 merge)→ sections[userWords]{added,skipped}
        let out = c
            .web_data_rpc(
                "dict.import",
                &json!({ "schemaId": "wb2", "content": content }),
            )
            .unwrap();
        let uw = sec(&out, "userWords");
        assert_eq!(uw.get("added").and_then(|v| v.as_u64()), Some(1));
        assert_eq!(uw.get("skipped").and_then(|v| v.as_u64()), Some(0));

        // 同内容再 import:权重相等 ⇒ 全 unchanged,added=updated=0
        let out2 = c
            .web_data_rpc(
                "dict.import",
                &json!({ "schemaId": "wb2", "content": content }),
            )
            .unwrap();
        let uw2 = sec(&out2, "userWords");
        assert_eq!(uw2.get("added").and_then(|v| v.as_u64()), Some(0));
        assert_eq!(uw2.get("updated").and_then(|v| v.as_u64()), Some(0));
        // preview 同内容 ⇒ unchanged=1,与落盘一致
        let prev2 = c
            .web_data_rpc(
                "dict.previewImport",
                &json!({ "schemaId": "wb2", "content": content }),
            )
            .unwrap();
        assert_eq!(
            sec(&prev2, "userWords")
                .get("unchanged")
                .and_then(|v| v.as_u64()),
            Some(1)
        );

        // replace:先加一条杂词,replace 导入后只剩导入内容
        c.web_data_rpc(
            "dict.add",
            &json!({ "schemaId": "wb2", "code": "x", "text": "另", "weight": 1 }),
        )
        .unwrap();
        let out3 = c
            .web_data_rpc(
                "dict.import",
                &json!({ "schemaId": "wb2", "content": content, "strategy": "replace" }),
            )
            .unwrap();
        assert_eq!(
            sec(&out3, "userWords")
                .get("added")
                .and_then(|v| v.as_u64()),
            Some(1),
            "清空后全部计 added"
        );
        let listed = c
            .web_data_rpc("dict.listPaged", &json!({ "schemaId": "wb2", "limit": 10 }))
            .unwrap();
        assert_eq!(
            listed.get("total").and_then(|v| v.as_u64()),
            Some(1),
            "replace 应清掉 x"
        );
    }

    #[test]
    fn dict_import_rime_and_tsv_auto_detect() {
        let c = coord("dictio_fmt");

        // Rime:默认列 [text, code, weight],拼音码去空格;preview 回报 format + userWords 段
        let rime = "# Rime dictionary\n---\nname: demo\nversion: \"1.0\"\n...\n你好\tni hao\t100\n世界\tshi jie\t50\n";
        let prev = c
            .web_data_rpc(
                "dict.previewImport",
                &json!({ "schemaId": "pinyin", "content": rime }),
            )
            .unwrap();
        assert_eq!(prev.get("format").and_then(|v| v.as_str()), Some("rime"));
        let uw = sec(&prev, "userWords");
        assert_eq!(uw.get("willAdd").and_then(|v| v.as_u64()), Some(2));
        assert_eq!(uw.get("skipped").and_then(|v| v.as_u64()), Some(0));
        let out = c
            .web_data_rpc(
                "dict.import",
                &json!({ "schemaId": "pinyin", "content": rime }),
            )
            .unwrap();
        assert_eq!(
            sec(&out, "userWords").get("added").and_then(|v| v.as_u64()),
            Some(2)
        );
        let listed = c
            .web_data_rpc(
                "dict.listPaged",
                &json!({ "schemaId": "pinyin", "prefix": "nihao", "limit": 10 }),
            )
            .unwrap();
        assert_eq!(
            listed.get("total").and_then(|v| v.as_u64()),
            Some(1),
            "拼音码应去空格入库(ni hao→nihao)"
        );

        // TSV:code\ttext\t[weight];坏行计入 skipped
        let tsv = "a\t工\t10\nbadline\nab\t好\n";
        let prev = c
            .web_data_rpc(
                "dict.previewImport",
                &json!({ "schemaId": "wb", "content": tsv }),
            )
            .unwrap();
        assert_eq!(prev.get("format").and_then(|v| v.as_str()), Some("tsv"));
        let uw = sec(&prev, "userWords");
        assert_eq!(uw.get("willAdd").and_then(|v| v.as_u64()), Some(2));
        assert_eq!(uw.get("skipped").and_then(|v| v.as_u64()), Some(1));
        let out = c
            .web_data_rpc("dict.import", &json!({ "schemaId": "wb", "content": tsv }))
            .unwrap();
        let uw = sec(&out, "userWords");
        assert_eq!(uw.get("added").and_then(|v| v.as_u64()), Some(2));
        assert_eq!(uw.get("skipped").and_then(|v| v.as_u64()), Some(1));

        // 不可识别内容 → 错误
        assert!(
            c.web_data_rpc(
                "dict.previewImport",
                &json!({ "schemaId": "wb", "content": "没有制表符的纯文本\n" }),
            )
            .is_err(),
            "未知格式应报错"
        );
    }

    #[test]
    fn dict_import_rejects_engine_type_mismatch() {
        let c = coord("dict_engine");
        // 手工构造「拼音」来源的 wdict；导入到默认解析为码表的方案 → 应拒绝（防编码错乱）。
        let content = "# x\nwind_dict:\n  version: 1\n  engine_type: pinyin\n  sections:\n    words:\n      columns: [code, text, weight, count]\n\n--- !words\nnihao\t你好\t0\t0\n";
        let r = c.web_data_rpc(
            "dict.import",
            &json!({ "schemaId": "wb", "content": content }),
        );
        assert!(r.is_err(), "拼音来源导入码表方案应被拒绝");
        // previewImport 回报 compatible=false + 来源引擎，供 UI 提前拦。
        let prev = c
            .web_data_rpc(
                "dict.previewImport",
                &json!({ "schemaId": "wb", "content": content }),
            )
            .unwrap();
        assert_eq!(
            prev.get("compatible").and_then(|v| v.as_bool()),
            Some(false)
        );
        assert_eq!(
            prev.get("sourceEngine").and_then(|v| v.as_str()),
            Some("pinyin")
        );
    }

    #[test]
    fn shadow_roundtrip_shape() {
        let c = coord("shadow");
        // pin + delete 两条规则
        c.web_data_rpc(
            "shadow.pin",
            &json!({ "schemaId": "wb", "code": "aaaa", "word": "恭恭敬敬", "candId": "c1", "position": 0 }),
        )
        .unwrap();
        c.web_data_rpc(
            "shadow.delete",
            &json!({ "schemaId": "wb", "code": "bbbb", "word": "某词" }),
        )
        .unwrap();

        let list = c
            .web_data_rpc("shadow.list", &json!({ "schemaId": "wb" }))
            .unwrap();
        let arr = list.as_array().expect("shadow.list 应为数组");
        assert_eq!(arr.len(), 2, "应有 pin/delete 两条");
        // 每条形状对齐 ShadowRuleItem {code, word, candId, type, position?}
        for it in arr {
            assert!(it.get("code").is_some());
            assert!(it.get("word").is_some());
            assert!(it.get("candId").is_some());
            let ty = it["type"].as_str().unwrap();
            assert!(ty == "pin" || ty == "delete");
        }
        let pin = arr.iter().find(|i| i["type"] == "pin").unwrap();
        assert_eq!(pin["candId"], "c1");
        assert_eq!(pin["position"], 0);

        // removeRule 后清空
        c.web_data_rpc(
            "shadow.removeRule",
            &json!({ "schemaId": "wb", "code": "aaaa", "word": "恭恭敬敬" }),
        )
        .unwrap();
        c.web_data_rpc(
            "shadow.removeRule",
            &json!({ "schemaId": "wb", "code": "bbbb", "word": "某词" }),
        )
        .unwrap();
        let list2 = c
            .web_data_rpc("shadow.list", &json!({ "schemaId": "wb" }))
            .unwrap();
        assert_eq!(list2.as_array().unwrap().len(), 0, "removeRule 后应清空");
    }

    #[test]
    fn shadow_add_rule_routes_pin_and_hide() {
        let c = coord("shadow_add_rule");
        // pin：带 position
        c.web_data_rpc(
            "shadow.addRule",
            &json!({ "schemaId": "wb", "code": "aaaa", "word": "恭恭敬敬", "type": "pin", "position": 2 }),
        )
        .unwrap();
        let list = c
            .web_data_rpc("shadow.list", &json!({ "schemaId": "wb" }))
            .unwrap();
        let arr = list.as_array().unwrap();
        // aaaa 应有一条 pin，position=2
        assert!(arr.iter().any(|e| e["code"] == "aaaa"));
        // hide：转为 delete
        c.web_data_rpc(
            "shadow.addRule",
            &json!({ "schemaId": "wb", "code": "bbbb", "word": "某词", "type": "hide" }),
        )
        .unwrap();
        let list2 = c
            .web_data_rpc("shadow.list", &json!({ "schemaId": "wb" }))
            .unwrap();
        assert!(
            list2
                .as_array()
                .unwrap()
                .iter()
                .any(|e| e["code"] == "bbbb")
        );
    }

    #[test]
    fn stats_summary_daily_shape() {
        let c = coord("stats");
        let today = today_str();
        // 采集器记录今日：中文 2（码长 4，首选）。
        c.debug_record_commit("你好", 4, 0, wind_store::stats::CommitSource::Candidate);

        // stats.summary 形状对齐富 StatsSummary（17 字段）。
        let sum = c.web_data_rpc("stats.summary", &json!({})).unwrap();
        for k in [
            "today_chars",
            "today_chinese",
            "today_english",
            "total_chars",
            "active_days",
            "daily_avg",
            "streak_current",
            "streak_max",
            "week_chars",
            "month_chars",
            "max_day_chars",
            "avg_code_len",
            "first_select_rate",
            "today_speed",
            "overall_speed",
            "max_speed",
        ] {
            assert!(sum.get(k).is_some(), "summary 缺 {k}");
        }
        assert_eq!(sum["today_chars"], 2);

        // flush 落库后 stats.daily 形状对齐 DailyStatItem。
        c.stat_collector().unwrap().flush();
        let daily = c
            .web_data_rpc("stats.daily", &json!({ "from": &today, "to": &today }))
            .unwrap();
        let arr = daily.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["d"], json!(today));
        assert_eq!(arr[0]["tc"], 2);
        assert_eq!(arr[0]["cc"], 2);

        // pruneBefore(days) 返回 {pruned}。
        let pr = c
            .web_data_rpc("stats.pruneBefore", &json!({ "days": 0 }))
            .unwrap();
        assert!(pr.get("pruned").and_then(|v| v.as_u64()).is_some());

        // clear 后 summary 归零（含采集器内存）。
        c.web_data_rpc("stats.clear", &json!({})).unwrap();
        let sum2 = c.web_data_rpc("stats.summary", &json!({})).unwrap();
        assert_eq!(sum2["today_chars"], 0);
        assert_eq!(sum2["total_chars"], 0);
    }

    #[test]
    fn freq_list_delete_clear_shape() {
        let c = coord("freq");
        let store = c.user_store().unwrap();
        store.record_freq("py", "de", "的").unwrap();
        store.record_freq("py", "shi", "是").unwrap();

        // freq.listPaged 形状对齐 PagedResult<FreqItem{code,text,count,lastUsed}>
        let r = c
            .web_data_rpc(
                "freq.listPaged",
                &json!({ "schemaId": "py", "limit": 50, "offset": 0 }),
            )
            .unwrap();
        assert_eq!(r["total"], 2);
        let it = &r["items"][0];
        for k in ["code", "text", "count", "lastUsed"] {
            assert!(it.get(k).is_some(), "FreqItem 缺 {k}");
        }
        // delete
        c.web_data_rpc(
            "freq.delete",
            &json!({ "schemaId": "py", "code": "de", "text": "的" }),
        )
        .unwrap();
        let r2 = c
            .web_data_rpc(
                "freq.listPaged",
                &json!({ "schemaId": "py", "limit": 50, "offset": 0 }),
            )
            .unwrap();
        assert_eq!(r2["total"], 1);
        // clear 返回删除数（number）
        let cleared = c
            .web_data_rpc("freq.clear", &json!({ "schemaId": "py" }))
            .unwrap();
        assert_eq!(cleared, json!(1));
    }

    /// **转义形态只活在设置页边界上**：库里存真实文本，RPC 出口投影成 `\n`、
    /// 入口还原回真实文本。
    ///
    /// 出入口必须**成对**。出口投影了而某个入口漏了还原，那个操作就会拿转义形态
    /// 去匹配真实文本的 key —— 查不到、不报错、静默失败，表现为「删了没反应」。
    /// 本测试逐个入口用「列表回什么就拿什么去操作」的方式走一遍，正是为了让漏接
    /// 在这里失败，而不是等用户发现。
    #[test]
    fn phrase_ui_escape_boundary_roundtrips() {
        let c = coord("phrase_esc");
        // 设置页提交转义形态：`\n` 表示换行（用户在输入框里看到并编辑的就是它）
        c.web_data_rpc(
            "phrase.add",
            &json!({ "code": "duo", "text": r"甲\n乙", "position": 0, "weight": 1 }),
        )
        .unwrap();

        // 存储域：真实文本（含真换行），转义形态不入库
        let store = c.user_store().unwrap();
        assert!(
            store
                .list_phrases()
                .unwrap()
                .iter()
                .any(|p| p.code == "duo" && p.text == "甲\n乙"),
            "库里应是真换行；若这里失败说明入口没还原、把字面 \\n 存进去了"
        );

        // 出口：列表回转义形态，用户可继续编辑
        let list = c.web_data_rpc("phrase.list", &json!({})).unwrap();
        assert_eq!(
            list[0]["text"],
            json!(r"甲\n乙"),
            "列表应回转义形态而非真换行——真换行在输入框里没法编辑"
        );

        // 候选侧：短语走 PhraseLayer（比用户词多一层 rebuild + cmdbar/模板分派），
        // 拿到的必须是真实文本。用户词与短语两条路径不同，须各自锁住。
        c.rebuild_phrases();
        let hits = c.debug_phrase_texts("duo");
        assert_eq!(hits.len(), 1, "短语候选应命中一条：{hits:?}");
        assert_eq!(
            hits[0], "甲\n乙",
            "短语候选须是真实文本（含真换行），不是转义形态"
        );

        // 入口 setEnabled：拿列表回的形态去操作，必须命中
        c.web_data_rpc(
            "phrase.setEnabled",
            &json!({ "code": "duo", "text": r"甲\n乙", "enabled": false }),
        )
        .unwrap();
        let list2 = c.web_data_rpc("phrase.list", &json!({})).unwrap();
        assert_eq!(
            list2[0]["enabled"],
            json!(false),
            "setEnabled 未命中 → 该入口漏了 store_text"
        );

        // 入口 remove：同理
        c.web_data_rpc(
            "phrase.remove",
            &json!({ "code": "duo", "text": r"甲\n乙" }),
        )
        .unwrap();
        let after = c.web_data_rpc("phrase.list", &json!({})).unwrap();
        assert!(
            after.as_array().unwrap().is_empty(),
            "remove 未命中 → 该入口漏了 store_text"
        );
    }

    /// **命令栏语法条目在这条边界上原样穿过**：它的反斜杠归 cmdbar lexer 独占。
    ///
    /// 修复前本层与 lexer 各吃一个：用户按文档写 `open("D:\\notes")`，落库剩
    /// `D:\notes`，lexer 再把 `\n` 解成换行 —— 路径静默损坏，只弹一句「命令执行失败」。
    /// 判据是「用户在输入框里写的与库里的逐字相同」，出口回显同样不得改写。
    #[test]
    fn cmdbar_phrase_backslash_survives_ui_boundary() {
        let c = coord("phrase_cmd_esc");
        let src = r#"$CC("[打开临时目录]", open("D:\\notes\\temp"))"#;
        c.web_data_rpc(
            "phrase.add",
            &json!({ "code": "cotmp", "text": src, "position": 0, "weight": 1 }),
        )
        .unwrap();

        let store = c.user_store().unwrap();
        assert!(
            store
                .list_phrases()
                .unwrap()
                .iter()
                .any(|p| p.code == "cotmp" && p.text == src),
            "库里应与用户所写逐字相同；失败即本层又吃掉了一个反斜杠"
        );

        let list = c.web_data_rpc("phrase.list", &json!({})).unwrap();
        assert_eq!(list[0]["text"], json!(src), "出口回显不得再加反斜杠");

        // 入口 remove 拿列表回的形态操作，仍须命中（出入口成对）
        c.web_data_rpc("phrase.remove", &json!({ "code": "cotmp", "text": src }))
            .unwrap();
        assert!(
            c.web_data_rpc("phrase.list", &json!({}))
                .unwrap()
                .as_array()
                .unwrap()
                .is_empty(),
            "remove 未命中 → 出入口分流判据不一致"
        );
    }

    /// 用户词库侧的同一契约（见 [`phrase_ui_escape_boundary_roundtrips`]）。
    #[test]
    fn user_word_ui_escape_boundary_roundtrips() {
        let c = coord("word_esc");
        c.web_data_rpc(
            "dict.add",
            &json!({ "schemaId": "wb", "code": "a", "text": r"甲\n乙", "weight": 100 }),
        )
        .unwrap();

        // 存储域是真实文本
        let store = c.user_store().unwrap();
        let recs = store.get_user_words("wb", "a").unwrap();
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].text, "甲\n乙", "库里应是真换行");

        // 出口投影
        let list = c
            .web_data_rpc("dict.listPaged", &json!({ "schemaId": "wb" }))
            .unwrap();
        let item = &list["items"][0];
        assert_eq!(item["text"], json!(r"甲\n乙"), "列表应回转义形态");

        // 入口：拿列表回的形态删除，必须命中
        c.web_data_rpc(
            "dict.remove",
            &json!({ "schemaId": "wb", "code": "a", "text": r"甲\n乙" }),
        )
        .unwrap();
        assert!(
            store.get_user_words("wb", "a").unwrap().is_empty(),
            "remove 未命中 → 该入口漏了 store_text"
        );
    }

    /// 字面反斜杠必须能表达：用户写 `\\n` 表示"反斜杠加字母 n"，不是换行。
    /// 没有这条，`C:\note` 这类内容就会被存成 `C:` + 换行 + `ote`。
    #[test]
    fn literal_backslash_survives_ui_boundary() {
        let c = coord("backslash_esc");
        c.web_data_rpc(
            "phrase.add",
            &json!({ "code": "p", "text": r"C:\\note", "position": 0, "weight": 1 }),
        )
        .unwrap();
        let store = c.user_store().unwrap();
        assert!(
            store
                .list_phrases()
                .unwrap()
                .iter()
                .any(|p| p.code == "p" && p.text == r"C:\note"),
            "`\\\\n` 应还原为字面反斜杠加 n，而非换行"
        );
        let list = c.web_data_rpc("phrase.list", &json!({})).unwrap();
        assert_eq!(list[0]["text"], json!(r"C:\\note"), "出口须重新转义反斜杠");
    }

    #[test]
    fn phrase_crud_shape() {
        let c = coord("phrase");
        // add → list 形状对齐 PhraseItem{code,text,position,weight,enabled}
        c.web_data_rpc(
            "phrase.add",
            &json!({ "code": "rq", "text": "2026-06-20", "position": 0, "weight": 1 }),
        )
        .unwrap();
        let list = c.web_data_rpc("phrase.list", &json!({})).unwrap();
        let arr = list.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        for k in ["code", "text", "position", "weight", "enabled"] {
            assert!(arr[0].get(k).is_some(), "PhraseItem 缺 {k}");
        }
        assert_eq!(arr[0]["enabled"], json!(true));

        // setEnabled
        c.web_data_rpc(
            "phrase.setEnabled",
            &json!({ "code": "rq", "text": "2026-06-20", "enabled": false }),
        )
        .unwrap();
        let list2 = c.web_data_rpc("phrase.list", &json!({})).unwrap();
        assert_eq!(list2[0]["enabled"], json!(false));

        // update 改 code（键迁移）
        c.web_data_rpc(
            "phrase.update",
            &json!({ "code": "rq", "text": "2026-06-20", "newCode": "date", "weight": 5 }),
        )
        .unwrap();
        let list3 = c.web_data_rpc("phrase.list", &json!({})).unwrap();
        assert_eq!(list3[0]["code"], json!("date"));

        // remove + resetDefault
        c.web_data_rpc(
            "phrase.remove",
            &json!({ "code": "date", "text": "2026-06-20" }),
        )
        .unwrap();
        assert_eq!(
            c.web_data_rpc("phrase.list", &json!({}))
                .unwrap()
                .as_array()
                .unwrap()
                .len(),
            0
        );
        c.web_data_rpc("phrase.resetDefault", &json!({})).unwrap();
    }

    /// 回归：phrase.resetDefault 只删用户短语，系统短语必须保留。
    #[test]
    fn phrase_reset_default_keeps_system() {
        use wind_store::phrases::SystemPhrase;

        let path = std::env::temp_dir().join("wind_webdata_phrase_reset_keeps_system.redb");
        let _ = std::fs::remove_file(&path);
        let store = Arc::new(Store::open(&path).unwrap());

        // 先同步一条系统短语（is_system=true）
        store
            .sync_system_phrases(&[SystemPhrase {
                code: "rq".into(),
                text: "$date".into(),
                weight: 1000,
                position: 0,
            }])
            .unwrap();

        // 构造 coordinator（共享同一个 Arc<Store>）
        let c = Coordinator::new_headless_with_store(Config::default(), None, Arc::clone(&store));

        // 加一条用户短语
        c.web_data_rpc(
            "phrase.add",
            &json!({ "code": "me", "text": "自定义", "position": 0, "weight": 1 }),
        )
        .unwrap();

        // 执行用户"清空"操作
        c.web_data_rpc("phrase.resetDefault", &json!({})).unwrap();

        // 系统短语应保留
        let sys = c.web_data_rpc("phrase.listSystem", &json!({})).unwrap();
        let sys_arr = sys.as_array().expect("listSystem 应返回数组");
        assert_eq!(sys_arr.len(), 1, "系统短语应保留，不应被 resetDefault 删除");
        assert_eq!(sys_arr[0]["code"], json!("rq"));

        // 用户短语应为 0
        let user = c.web_data_rpc("phrase.listUser", &json!({})).unwrap();
        assert_eq!(user["total"], json!(0), "用户短语应被 resetDefault 清空");
    }

    // ───────── quick.*（快捷输入格式表）─────────

    /// headless coordinator + 独立 redb（redb 是单写者，共用文件会让并发测试互相阻塞）。
    /// 格式表走 `FormatTable::load(None)` → 内置出厂表。
    fn quick_coord(tag: &str) -> (Arc<Coordinator>, std::path::PathBuf) {
        let path = std::env::temp_dir().join(format!("wind_webdata_quick_{tag}.redb"));
        let _ = std::fs::remove_file(&path);
        let store = Arc::new(Store::open(&path).unwrap());
        (
            Coordinator::new_headless_with_store(Config::default(), None, store),
            path,
        )
    }

    fn quick_rows(c: &Coordinator) -> Vec<Value> {
        c.web_data_rpc("quick.list", &json!({}))
            .unwrap()
            .as_array()
            .expect("quick.list 应返回数组")
            .clone()
    }

    fn quick_row(rows: &[Value], id: &str) -> Value {
        rows.iter()
            .find(|r| r["id"] == json!(id))
            .unwrap_or_else(|| panic!("列表里找不到 {id}"))
            .clone()
    }

    /// 某类**启用**条目的 id 序（= 候选顺序）。
    fn quick_enabled_ids(c: &Coordinator, kind: &str) -> Vec<String> {
        quick_rows(c)
            .iter()
            .filter(|r| r["kind"] == json!(kind) && r["enabled"] == json!(true))
            .map(|r| r["id"].as_str().unwrap().to_string())
            .collect()
    }

    #[test]
    fn quick_list_covers_every_entry_with_samples() {
        let (c, p) = quick_coord("list");
        let rows = quick_rows(&c);
        // 出厂表全部条目都在（含各类别）。
        //
        // 条数**对照内置表现取**，不写死数字：headless 无 data 目录 ⇒ 格式表回落
        // `FormatTable::builtin()`，两边本就是同一份。写死数字的话，每次往出厂表加
        // 条目这里都会红，而人只会把数字加一——那既没验证「有没有漏」，还平白多一次
        // 无意义的改动。现在验的是真正该验的：**RPC 转换没吞掉也没重复任何条目**。
        let builtin_count = wind_quick_input::FormatTable::builtin().entries().len();
        assert_eq!(
            rows.len(),
            builtin_count,
            "quick.list 应逐条转换出厂表，实际 {} 条对 {} 条",
            rows.len(),
            builtin_count
        );
        for kind in ["date", "month_day", "year_month", "number", "calc"] {
            assert!(
                rows.iter().any(|r| r["kind"] == json!(kind)),
                "缺类别 {kind}"
            );
        }
        // 示例是「用示例输入跑真实候选生成」得到的，故出厂条目必须都有示例——
        // 空示例说明那条渲染不出来，对出厂表而言就是缺陷。
        let cn = quick_row(&rows, "date.cn");
        let sample = cn["sample"].as_str().unwrap();
        assert!(!sample.is_empty(), "date.cn 应有示例");
        assert!(sample.ends_with('日'), "示例应是完整日期: {sample}");
        // 金额示例用 1234.5 渲染，必然带「元」。
        let amt = quick_row(&rows, "number.amount");
        assert!(
            amt["sample"].as_str().unwrap().contains('元'),
            "金额示例: {}",
            amt["sample"]
        );
        // 未调整时的初始状态
        assert_eq!(cn["enabled"], json!(true));
        assert_eq!(cn["adjusted"], json!(false));
        assert_eq!(cn["moveIndex"], json!(0), "date.cn 是 date 组首条");
        assert_eq!(cn["displayPos"], json!(1));
        let _ = std::fs::remove_file(&p);
    }

    /// ★★ 停用后条目**留在列表里**（否则用户再也开不回来），且 `moveIndex` 为 null
    /// ——它不在候选里，移动没有意义。这正是设置页要补的那个缺口。
    #[test]
    fn quick_disabled_entry_stays_listed_and_unmovable() {
        let (c, p) = quick_coord("disable");
        let before = quick_rows(&c).len();
        c.web_data_rpc(
            "quick.setEnabled",
            &json!({ "kind": "date", "id": "date.basic", "enabled": false }),
        )
        .unwrap();
        let rows = quick_rows(&c);
        assert_eq!(rows.len(), before, "管理列表条数不减");
        let row = quick_row(&rows, "date.basic");
        assert_eq!(row["enabled"], json!(false));
        assert_eq!(row["adjusted"], json!(true));
        assert_eq!(row["moveIndex"], Value::Null, "停用项不可移动");
        // 候选那边确实少了一条
        assert!(!quick_enabled_ids(&c, "date").contains(&"date.basic".to_string()));

        // 双向：再开回来，且恢复原位（记录清空 → 回到基表位置）
        c.web_data_rpc(
            "quick.setEnabled",
            &json!({ "kind": "date", "id": "date.basic", "enabled": true }),
        )
        .unwrap();
        let row = quick_row(&quick_rows(&c), "date.basic");
        assert_eq!(row["enabled"], json!(true));
        assert_eq!(row["adjusted"], json!(false), "开回来即无残留规则");
        let _ = std::fs::remove_file(&p);
    }

    /// 移动落到候选顺序上，并被 `moveIndex` 如实反映（UI 的上/下移就是 ±1 这个值）。
    #[test]
    fn quick_move_changes_candidate_order() {
        let (c, p) = quick_coord("move");
        assert_ne!(quick_enabled_ids(&c, "date")[0], "date.lunar");
        c.web_data_rpc(
            "quick.move",
            &json!({ "kind": "date", "id": "date.lunar", "index": 0 }),
        )
        .unwrap();
        assert_eq!(quick_enabled_ids(&c, "date")[0], "date.lunar");
        assert_eq!(
            quick_row(&quick_rows(&c), "date.lunar")["moveIndex"],
            json!(0)
        );
        let _ = std::fs::remove_file(&p);
    }

    /// 单条恢复只清这一条（store 的 `reset_quick_format_entry` 此前无调用点，
    /// 注释写着「单条恢复要等设置页」——就是这个 RPC）。
    #[test]
    fn quick_reset_entry_only_clears_that_entry() {
        let (c, p) = quick_coord("reset_entry");
        c.web_data_rpc(
            "quick.move",
            &json!({ "kind": "date", "id": "date.lunar", "index": 0 }),
        )
        .unwrap();
        c.web_data_rpc(
            "quick.setEnabled",
            &json!({ "kind": "date", "id": "date.basic", "enabled": false }),
        )
        .unwrap();
        c.web_data_rpc(
            "quick.resetEntry",
            &json!({ "kind": "date", "id": "date.lunar" }),
        )
        .unwrap();
        let rows = quick_rows(&c);
        assert_eq!(quick_row(&rows, "date.lunar")["adjusted"], json!(false));
        assert_eq!(
            quick_row(&rows, "date.basic")["enabled"],
            json!(false),
            "不该连累别的条目"
        );
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn quick_reset_kind_clears_whole_kind_only() {
        let (c, p) = quick_coord("reset_kind");
        c.web_data_rpc(
            "quick.move",
            &json!({ "kind": "date", "id": "date.lunar", "index": 0 }),
        )
        .unwrap();
        c.web_data_rpc(
            "quick.setEnabled",
            &json!({ "kind": "number", "id": "number.digits", "enabled": false }),
        )
        .unwrap();
        c.web_data_rpc("quick.resetKind", &json!({ "kind": "date" }))
            .unwrap();
        let rows = quick_rows(&c);
        assert!(
            rows.iter()
                .filter(|r| r["kind"] == json!("date"))
                .all(|r| r["adjusted"] == json!(false)),
            "date 全类恢复"
        );
        assert_eq!(
            quick_row(&rows, "number.digits")["enabled"],
            json!(false),
            "别的类别不动"
        );
        let _ = std::fs::remove_file(&p);
    }

    /// ★★ 端到端往返：导出 → 到一台干净的机器上导入 → 状态必须一致。
    ///
    /// 这是导入导出唯一真正要保证的事，也是最容易在"部分字段没写出去"时静默失守的地方。
    #[test]
    fn quick_export_import_roundtrip_restores_state() {
        let (a, pa) = quick_coord("export_src");
        // 造一组有代表性的改动：移动两条（含 LIFO 关系）+ 停用一条 + 跨类别
        a.web_data_rpc(
            "quick.move",
            &json!({ "kind": "date", "id": "date.iso", "index": 1 }),
        )
        .unwrap();
        a.web_data_rpc(
            "quick.move",
            &json!({ "kind": "date", "id": "date.lunar", "index": 0 }),
        )
        .unwrap();
        a.web_data_rpc(
            "quick.setEnabled",
            &json!({ "kind": "number", "id": "number.digits", "enabled": false }),
        )
        .unwrap();
        let content = a.web_data_rpc("quick.export", &json!({})).unwrap()["content"]
            .as_str()
            .unwrap()
            .to_string();
        let expect_date = quick_enabled_ids(&a, "date");

        let (b, pb) = quick_coord("export_dst");
        let out = b
            .web_data_rpc("quick.import", &json!({ "content": content }))
            .unwrap();
        assert_eq!(out["moved"], json!(2));
        assert_eq!(out["disabled"], json!(1));
        assert_eq!(out["skipped"].as_array().unwrap().len(), 0);

        assert_eq!(
            quick_enabled_ids(&b, "date"),
            expect_date,
            "★ 导入后 date 顺序必须与导出前一致（LIFO 顺序不能在往返中反转）"
        );
        assert_eq!(
            quick_row(&quick_rows(&b), "number.digits")["enabled"],
            json!(false),
            "停用状态也要带过来"
        );
        let _ = std::fs::remove_file(&pa);
        let _ = std::fs::remove_file(&pb);
    }

    /// `strategy = "replace"` 先清空既有调整；缺省（合并）则保留未被文件提到的。
    #[test]
    fn quick_import_replace_clears_existing() {
        let (c, p) = quick_coord("import_replace");
        c.web_data_rpc(
            "quick.setEnabled",
            &json!({ "kind": "calc", "id": "calc.equation", "enabled": false }),
        )
        .unwrap();
        let file = "[[adjust]]\nkind = 'date'\ndisabled = ['date.basic']\n";

        // 合并：两处改动并存
        c.web_data_rpc("quick.import", &json!({ "content": file }))
            .unwrap();
        let rows = quick_rows(&c);
        assert_eq!(quick_row(&rows, "calc.equation")["enabled"], json!(false));
        assert_eq!(quick_row(&rows, "date.basic")["enabled"], json!(false));

        // 替换：文件没提到的 calc 改动被清掉
        c.web_data_rpc(
            "quick.import",
            &json!({ "content": file, "strategy": "replace" }),
        )
        .unwrap();
        let rows = quick_rows(&c);
        assert_eq!(
            quick_row(&rows, "calc.equation")["enabled"],
            json!(true),
            "replace 应先清空"
        );
        assert_eq!(quick_row(&rows, "date.basic")["enabled"], json!(false));
        let _ = std::fs::remove_file(&p);
    }

    /// 坏条目**如实报告**，不静默丢弃：用户看到「导入成功」却少了规则，
    /// 无从判断是文件坏了还是程序吃了。
    #[test]
    fn quick_preview_import_reports_skipped() {
        let (c, p) = quick_coord("preview");
        let file = "\
[[formats]]
id = 'my.one'
kind = 'date'
text = '$YY.$MM'

[[adjust]]
kind = 'weather'
disabled = ['x']

[[adjust]]
kind = 'date'
moved = [{ id = 'date.lunar', position = 0 }]
";
        let pv = c
            .web_data_rpc("quick.previewImport", &json!({ "content": file }))
            .unwrap();
        assert_eq!(pv["moved"], json!(1));
        assert_eq!(pv["formats"], json!(1), "自定义条目如实计数");
        assert_eq!(
            pv["skipped"].as_array().unwrap().len(),
            1,
            "未知类别要报出来"
        );
        assert_eq!(pv["kinds"], json!(["date"]));
        // 预览不得写库
        assert!(
            quick_rows(&c).iter().all(|r| r["adjusted"] == json!(false)),
            "previewImport 必须是只读的"
        );
        let _ = std::fs::remove_file(&p);
    }

    /// 未知类别在 RPC 边界就被挡住，且错误信息带上那个类别名。
    #[test]
    fn quick_edit_rejects_unknown_kind() {
        let (c, p) = quick_coord("bad_kind");
        let e = c
            .web_data_rpc(
                "quick.setEnabled",
                &json!({ "kind": "weather", "id": "x", "enabled": false }),
            )
            .unwrap_err()
            .to_string();
        assert!(e.contains("weather"), "错误应指出是哪个类别: {e}");
        let _ = std::fs::remove_file(&p);
    }

    // ───────── quick.* 自定义条目（P2）─────────

    /// 变量提示覆盖全部五类，且 year_month **不含**农历变量。
    ///
    /// 后一条是真实的分工（农历月与公历月不一一对应），提示里放了农历，用户照着给年月写
    /// `$LMD`，保存时才会被拒——错误发生在他已经写完之后。
    #[test]
    fn quick_vars_covers_all_kinds_without_lunar_in_year_month() {
        let (c, p) = quick_coord("vars");
        let v = c.web_data_rpc("quick.vars", &json!({})).unwrap();
        let arr = v.as_array().unwrap();
        let kinds: Vec<&str> = arr.iter().map(|k| k["kind"].as_str().unwrap()).collect();
        assert_eq!(
            kinds,
            vec!["date", "month_day", "year_month", "number", "calc"],
            "五类齐全且按 ALL 序"
        );
        let names_of = |kind: &str| -> Vec<String> {
            arr.iter().find(|k| k["kind"] == json!(kind)).unwrap()["vars"]
                .as_array()
                .unwrap()
                .iter()
                .map(|x| x["name"].as_str().unwrap().to_string())
                .collect()
        };
        assert!(names_of("date").contains(&"LMD".to_string()));
        assert!(
            !names_of("year_month").contains(&"LMD".to_string()),
            "年月不该提供农历变量"
        );
        assert!(names_of("calc").contains(&"RESULT".to_string()));
        // 每条都得有说明，否则界面上是一列空白
        for k in arr {
            for var in k["vars"].as_array().unwrap() {
                assert!(
                    var["desc"].as_str().is_some_and(|d| !d.is_empty()),
                    "kind={} 的 {} 缺说明",
                    k["kind"],
                    var["name"]
                );
            }
        }
        let _ = std::fs::remove_file(&p);
    }

    /// 新增的条目立刻是一个**完整的**列表行：能列出、有示例、可调序、标着 user。
    ///
    /// 示例列尤其要验：它由「只清 moved/disabled、保留 added」的那份调整渲染，若沿用 P1 的
    /// 空调整，用户条目的示例会恒为空——其它行都好，看着像是模板写错了。
    #[test]
    fn quick_add_yields_a_complete_row() {
        let (c, p) = quick_coord("add");
        let r = c
            .web_data_rpc("quick.add", &json!({ "kind": "date", "text": "$Y/$M/$D" }))
            .unwrap();
        assert_eq!(r["id"], json!("date.u1"), "首条用户条目的 id");

        let rows = quick_rows(&c);
        let row = quick_row(&rows, "date.u1");
        assert_eq!(row["user"], json!(true), "标记为用户条目");
        assert_eq!(row["enabled"], json!(true));
        assert!(row["moveIndex"].is_number(), "启用的条目可调序");
        assert!(
            row["sample"].as_str().is_some_and(|s| s.contains('/')),
            "★ 示例必须渲染出来，实际: {:?}",
            row["sample"]
        );
        // 出厂条目不该被误标
        assert_eq!(quick_row(&rows, "date.cn")["user"], json!(false));

        // 第二条接着排号，且落在本类末尾
        let r2 = c
            .web_data_rpc("quick.add", &json!({ "kind": "date", "text": "$D-$M-$Y" }))
            .unwrap();
        assert_eq!(r2["id"], json!("date.u2"));
        let rows = quick_rows(&c);
        let date_ids: Vec<&str> = rows
            .iter()
            .filter(|r| r["kind"] == json!("date"))
            .map(|r| r["id"].as_str().unwrap())
            .collect();
        assert_eq!(
            &date_ids[date_ids.len() - 2..],
            &["date.u1", "date.u2"],
            "用户条目排在出厂条目之后"
        );
        let _ = std::fs::remove_file(&p);
    }

    /// 校验在**保存前**拒绝并说明原因（与文件加载「剔除 + warn」相反：那边用户看不到日志）。
    #[test]
    fn quick_add_rejects_invalid_template() {
        let (c, p) = quick_coord("add_bad");
        // year_month 没有 $D 系列
        let e = c
            .web_data_rpc(
                "quick.add",
                &json!({ "kind": "year_month", "text": "$Y-$M-$D" }),
            )
            .unwrap_err()
            .to_string();
        assert!(
            e.contains("$D") || e.contains('D'),
            "错误要指出是哪个变量: {e}"
        );

        let e = c
            .web_data_rpc("quick.add", &json!({ "kind": "date", "text": "   " }))
            .unwrap_err()
            .to_string();
        assert!(e.contains("为空"), "纯空白模板要拒绝: {e}");

        assert!(
            quick_rows(&c).iter().all(|r| r["user"] == json!(false)),
            "被拒绝的条目不得落库"
        );
        let _ = std::fs::remove_file(&p);
    }

    /// 模板逐字相同就拒绝，并**指出撞的是哪一条**——两行长得一模一样时用户看不到 id，
    /// 只说「重复了」他还得自己一行行找。
    #[test]
    fn quick_add_rejects_duplicate_template() {
        let (c, p) = quick_coord("add_dup");
        let e = c
            .web_data_rpc(
                "quick.add",
                &json!({ "kind": "date", "text": "$Y年$M月$D日" }),
            )
            .unwrap_err()
            .to_string();
        assert!(e.contains("date.cn"), "要指出撞的是哪条: {e}");

        c.web_data_rpc("quick.add", &json!({ "kind": "date", "text": "$Y/$M/$D" }))
            .unwrap();
        let e = c
            .web_data_rpc("quick.add", &json!({ "kind": "date", "text": "$Y/$M/$D" }))
            .unwrap_err()
            .to_string();
        assert!(e.contains("date.u1"), "也要认出撞的是用户条目: {e}");

        // 同一串模板在别的类别里不算撞车（各类各一张表）
        c.web_data_rpc(
            "quick.add",
            &json!({ "kind": "month_day", "text": "$Y/$M/$D" }),
        )
        .unwrap();
        let _ = std::fs::remove_file(&p);
    }

    /// ★★ 出厂条目的模板不可改、不可删——这是 P2 的核心设计决策，由 store 的数据结构
    /// 兜住（模板只存在 `added` 里）。这条测试是那个决策的守门。
    #[test]
    fn quick_factory_entries_refuse_edit_and_delete() {
        let (c, p) = quick_coord("factory_ro");
        let e = c
            .web_data_rpc(
                "quick.setText",
                &json!({ "kind": "date", "id": "date.cn", "text": "$Y.$M.$D" }),
            )
            .unwrap_err()
            .to_string();
        assert!(e.contains("不可修改"), "要说清为什么: {e}");

        let e = c
            .web_data_rpc("quick.delete", &json!({ "kind": "date", "id": "date.cn" }))
            .unwrap_err()
            .to_string();
        assert!(e.contains("只能停用"), "要指出替代做法: {e}");

        // 模板没被动过
        assert_eq!(
            quick_row(&quick_rows(&c), "date.cn")["text"],
            json!("$Y年$M月$D日")
        );
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn quick_set_text_updates_user_entry_and_its_sample() {
        let (c, p) = quick_coord("set_text");
        // 模板不能与出厂条目逐字相同（`$CNL` 就是出厂的 number.cn_lower），否则被查重拦下
        c.web_data_rpc("quick.add", &json!({ "kind": "number", "text": "约$CNL" }))
            .unwrap();
        c.web_data_rpc(
            "quick.setText",
            &json!({ "kind": "number", "id": "number.u1", "text": "共$THOU元" }),
        )
        .unwrap();
        let row = quick_row(&quick_rows(&c), "number.u1");
        assert_eq!(row["text"], json!("共$THOU元"));
        assert!(
            row["sample"].as_str().is_some_and(|s| s.starts_with('共')),
            "示例要跟着变: {:?}",
            row["sample"]
        );
        let _ = std::fs::remove_file(&p);
    }

    /// 改写时把自己排除掉：只改了个错别字、模板本身没动，不该被判成「与自己重复」。
    #[test]
    fn quick_set_text_to_the_same_value_is_allowed() {
        let (c, p) = quick_coord("set_same");
        c.web_data_rpc("quick.add", &json!({ "kind": "date", "text": "$Y/$M/$D" }))
            .unwrap();
        c.web_data_rpc(
            "quick.setText",
            &json!({ "kind": "date", "id": "date.u1", "text": "$Y/$M/$D" }),
        )
        .expect("模板没变也该允许保存");
        let _ = std::fs::remove_file(&p);
    }

    /// 删除连带清掉它的调序规则，否则重加一条同 id 的条目会被旧规则挪到意外的位置。
    #[test]
    fn quick_delete_also_drops_its_move_rule() {
        let (c, p) = quick_coord("del");
        c.web_data_rpc("quick.add", &json!({ "kind": "date", "text": "$Y/$M/$D" }))
            .unwrap();
        c.web_data_rpc(
            "quick.move",
            &json!({ "kind": "date", "id": "date.u1", "index": 0 }),
        )
        .unwrap();
        assert_eq!(quick_rows(&c)[0]["id"], json!("date.u1"));

        c.web_data_rpc("quick.delete", &json!({ "kind": "date", "id": "date.u1" }))
            .unwrap();
        let rows = quick_rows(&c);
        assert!(
            !rows.iter().any(|r| r["id"] == json!("date.u1")),
            "已从列表消失"
        );
        // 重新加一条：不该继承已删条目的位置，而要落回本类末尾。
        //
        // 这里刻意**不断言 id 是什么**：删掉唯一一条后 id 会复用 `date.u1`，而那是安全的
        // ——删除已连带清掉它的全部规则，没有任何东西还引用这个 id。真正要防的「删中间
        // 一条后撞上仍存在的 id」由 `next_user_id_survives_a_deletion_in_the_middle` 覆盖。
        // 断言 id 不复用会把一个无害的实现细节钉死。
        let r = c
            .web_data_rpc("quick.add", &json!({ "kind": "date", "text": "$Y/$M/$D" }))
            .unwrap();
        let fresh = r["id"].as_str().unwrap().to_string();
        let rows = quick_rows(&c);
        assert_ne!(rows[0]["id"], json!(fresh), "★ 不得继承已删条目的首位");
        let date_ids: Vec<&str> = rows
            .iter()
            .filter(|r| r["kind"] == json!("date"))
            .map(|r| r["id"].as_str().unwrap())
            .collect();
        assert_eq!(*date_ids.last().unwrap(), fresh, "落回本类末尾");
        let _ = std::fs::remove_file(&p);
    }

    /// ★★ 「恢复默认」清调序与停用，**保留用户条目**。
    ///
    /// 端到端守门：store 那层已有同名测试，这里再钉一次 RPC 全链路——右键菜单的
    /// `resetKind` 与设置页共用这条路径，而它一度是 `t.remove(kind)`（会删穿用户条目）。
    #[test]
    fn quick_reset_kind_keeps_user_entries() {
        let (c, p) = quick_coord("reset_keep");
        c.web_data_rpc("quick.add", &json!({ "kind": "date", "text": "$Y/$M/$D" }))
            .unwrap();
        c.web_data_rpc(
            "quick.move",
            &json!({ "kind": "date", "id": "date.u1", "index": 0 }),
        )
        .unwrap();
        c.web_data_rpc(
            "quick.setEnabled",
            &json!({ "kind": "date", "id": "date.cn", "enabled": false }),
        )
        .unwrap();

        c.web_data_rpc("quick.resetKind", &json!({ "kind": "date" }))
            .unwrap();

        let rows = quick_rows(&c);
        let row = quick_row(&rows, "date.u1");
        assert_eq!(row["user"], json!(true), "★ 用户条目必须还在");
        assert_eq!(row["adjusted"], json!(false), "它的调序规则被清掉了");
        assert_eq!(quick_row(&rows, "date.cn")["enabled"], json!(true));
        let date_ids: Vec<&str> = rows
            .iter()
            .filter(|r| r["kind"] == json!("date"))
            .map(|r| r["id"].as_str().unwrap())
            .collect();
        assert_eq!(
            *date_ids.last().unwrap(),
            "date.u1",
            "回到「用户条目在末尾」的初始位置"
        );
        let _ = std::fs::remove_file(&p);
    }

    /// 导出/导入往返带上用户条目本身（不只是调整）。
    #[test]
    fn quick_roundtrip_carries_user_entries() {
        let (a, pa) = quick_coord("rt_user_a");
        a.web_data_rpc("quick.add", &json!({ "kind": "date", "text": "$Y/$M/$D" }))
            .unwrap();
        a.web_data_rpc(
            "quick.add",
            &json!({ "kind": "calc", "text": "结果是$RESULT" }),
        )
        .unwrap();
        a.web_data_rpc(
            "quick.move",
            &json!({ "kind": "date", "id": "date.u1", "index": 0 }),
        )
        .unwrap();
        let content = a.web_data_rpc("quick.export", &json!({})).unwrap()["content"]
            .as_str()
            .unwrap()
            .to_string();

        let (b, pb) = quick_coord("rt_user_b");
        let o = b
            .web_data_rpc("quick.import", &json!({ "content": content }))
            .unwrap();
        assert_eq!(o["formats"], json!(2), "两条自定义条目都导入了");
        assert!(
            o["skipped"].as_array().unwrap().is_empty(),
            "不该有跳过项: {:?}",
            o["skipped"]
        );

        let rows = quick_rows(&b);
        assert_eq!(rows[0]["id"], json!("date.u1"), "调序规则也带过来了");
        assert_eq!(rows[0]["text"], json!("$Y/$M/$D"));
        assert_eq!(quick_row(&rows, "calc.u1")["text"], json!("结果是$RESULT"));
        let _ = std::fs::remove_file(&pa);
        let _ = std::fs::remove_file(&pb);
    }

    /// ★★ 导入时 id 冲突：换新 id 之后，**引用它的 `moved` 规则必须一起改写**。
    ///
    /// 场景是两台机器各有一条 `date.u1`（内容不同）。若只改 `[[formats]]` 侧不改 `moved` 侧，
    /// 那条移动规则会指向本机原有的 `date.u1`——症状是「导入后顺序不对」，两条条目都在、
    /// 都没报错。与漏 `.rev()` 同属导入重放的引用完整性。
    #[test]
    fn quick_import_remaps_conflicting_id_and_its_move_rule() {
        // A 机：date.u1 = $Y/$M/$D，并把它移到首位
        let (a, pa) = quick_coord("remap_a");
        a.web_data_rpc("quick.add", &json!({ "kind": "date", "text": "$Y/$M/$D" }))
            .unwrap();
        a.web_data_rpc(
            "quick.move",
            &json!({ "kind": "date", "id": "date.u1", "index": 0 }),
        )
        .unwrap();
        let content = a.web_data_rpc("quick.export", &json!({})).unwrap()["content"]
            .as_str()
            .unwrap()
            .to_string();

        // B 机：已有一条**内容不同**的 date.u1
        let (b, pb) = quick_coord("remap_b");
        b.web_data_rpc(
            "quick.add",
            &json!({ "kind": "date", "text": "本机原有$Y" }),
        )
        .unwrap();

        let o = b
            .web_data_rpc("quick.import", &json!({ "content": content }))
            .unwrap();
        assert_eq!(o["formats"], json!(1));

        let rows = quick_rows(&b);
        // 本机原有那条内容不变
        assert_eq!(quick_row(&rows, "date.u1")["text"], json!("本机原有$Y"));
        // 导入的那条换了 id
        let fresh = quick_row(&rows, "date.u2");
        assert_eq!(fresh["text"], json!("$Y/$M/$D"), "导入的条目改用 u2");
        // ★ 移动规则跟着改写：排首位的是导入的那条，不是本机原有的
        assert_eq!(
            rows[0]["id"],
            json!("date.u2"),
            "★ moved 必须一起改写，否则规则会指向本机原有的 date.u1"
        );
        let _ = std::fs::remove_file(&pa);
        let _ = std::fs::remove_file(&pb);
    }

    /// 重复导入同一份文件是幂等的：模板相同的条目按「撞车」跳过并如实报告，
    /// 而不是每导一次就多出一条一模一样的候选。
    #[test]
    fn quick_import_twice_does_not_duplicate_entries() {
        let (a, pa) = quick_coord("idem_a");
        a.web_data_rpc("quick.add", &json!({ "kind": "date", "text": "$Y/$M/$D" }))
            .unwrap();
        let content = a.web_data_rpc("quick.export", &json!({})).unwrap()["content"]
            .as_str()
            .unwrap()
            .to_string();
        let (b, pb) = quick_coord("idem_b");
        b.web_data_rpc("quick.import", &json!({ "content": &content }))
            .unwrap();
        let o = b
            .web_data_rpc("quick.import", &json!({ "content": &content }))
            .unwrap();
        assert_eq!(o["formats"], json!(0), "第二次一条都不该写入");
        assert_eq!(
            o["skipped"].as_array().unwrap().len(),
            1,
            "跳过要如实报告: {:?}",
            o["skipped"]
        );
        assert_eq!(
            quick_rows(&b)
                .iter()
                .filter(|r| r["user"] == json!(true))
                .count(),
            1,
            "只有一条用户条目"
        );
        let _ = std::fs::remove_file(&pa);
        let _ = std::fs::remove_file(&pb);
    }

    /// `strategy = "replace"` 连用户条目一起清（语义是「用文件覆盖现状」），
    /// 与面向用户的「恢复默认」刻意相反。
    #[test]
    fn quick_import_replace_clears_user_entries_too() {
        let (a, pa) = quick_coord("repl_user_a");
        a.web_data_rpc("quick.add", &json!({ "kind": "calc", "text": "得$RESULT" }))
            .unwrap();
        let content = a.web_data_rpc("quick.export", &json!({})).unwrap()["content"]
            .as_str()
            .unwrap()
            .to_string();

        let (b, pb) = quick_coord("repl_user_b");
        b.web_data_rpc("quick.add", &json!({ "kind": "date", "text": "本机的$Y" }))
            .unwrap();
        b.web_data_rpc(
            "quick.import",
            &json!({ "content": content, "strategy": "replace" }),
        )
        .unwrap();

        let rows = quick_rows(&b);
        assert!(
            !rows.iter().any(|r| r["text"] == json!("本机的$Y")),
            "replace 应清掉本机原有的用户条目"
        );
        assert_eq!(quick_row(&rows, "calc.u1")["text"], json!("得$RESULT"));
        let _ = std::fs::remove_file(&pa);
        let _ = std::fs::remove_file(&pb);
    }

    /// 用一份 system.phrases.toml 起一个带 data_dir 的 headless coordinator。
    fn coord_with_phrase_toml(tag: &str, toml: &str) -> (Arc<Coordinator>, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("wind_phrase_reread_{tag}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("system.phrases.toml"), toml).unwrap();
        let store = Arc::new(Store::open(dir.join("store.redb")).unwrap());
        let c = Coordinator::new_headless_with_store(Config::default(), Some(&dir), store);
        (c, dir)
    }

    fn system_codes(c: &Coordinator) -> Vec<(String, String)> {
        c.web_data_rpc("phrase.listSystem", &json!({}))
            .unwrap()
            .as_array()
            .expect("listSystem 应返回数组")
            .iter()
            .map(|v| {
                (
                    v["code"].as_str().unwrap().to_string(),
                    v["text"].as_str().unwrap().to_string(),
                )
            })
            .collect()
    }

    /// phrase.resetSystem 应重读 TOML：手工编辑后无需重启服务即可生效。
    #[test]
    fn phrase_reset_system_rereads_toml() {
        let (c, dir) = coord_with_phrase_toml("ok", "[[phrases]]\ncode = 'rq'\ntext = '$date'\n");

        assert_eq!(system_codes(&c), vec![("rq".into(), "$date".into())]);

        // 手工编辑：改文本 + 增一条
        std::fs::write(
            dir.join("system.phrases.toml"),
            "[[phrases]]\ncode = 'rq'\ntext = '$datetime'\n\n[[phrases]]\ncode = 'xx'\ntext = '新增'\n",
        )
        .unwrap();

        c.web_data_rpc("phrase.resetSystem", &json!({})).unwrap();

        let mut got = system_codes(&c);
        got.sort();
        assert_eq!(
            got,
            vec![
                ("rq".to_string(), "$datetime".to_string()),
                ("xx".to_string(), "新增".to_string()),
            ],
            "重读后应取到编辑后的文本并含新增条目"
        );
    }

    /// TOML 语法错误时必须回退到启动缓存，绝不能把库里系统短语清空。
    #[test]
    fn phrase_reset_system_falls_back_on_broken_toml() {
        let (c, dir) =
            coord_with_phrase_toml("broken", "[[phrases]]\ncode = 'rq'\ntext = '$date'\n");

        // 写坏 TOML（未闭合引号）
        std::fs::write(
            dir.join("system.phrases.toml"),
            "[[phrases]]\ncode = 'rq\ntext = ",
        )
        .unwrap();

        c.web_data_rpc("phrase.resetSystem", &json!({})).unwrap();

        assert_eq!(
            system_codes(&c),
            vec![("rq".to_string(), "$date".to_string())],
            "解析失败应沿用启动缓存，不得清空系统短语"
        );
    }

    #[test]
    fn json_diff_sparse() {
        let base = json!({ "a": 1, "t": { "x": 1, "y": 2 }, "same": "v" });
        let cfg = json!({ "a": 9, "t": { "x": 1, "y": 20 }, "same": "v" });
        let d = json_diff(&base, &cfg).unwrap();
        // 仅含变化项：a + t.y
        assert_eq!(d, json!({ "a": 9, "t": { "y": 20 } }));
        // 完全相同 → None
        assert!(json_diff(&base, &base).is_none());
    }

    /// 只读旁路字段不得随 saveConfig 落进 override。
    ///
    /// 失败形态很隐蔽：override 里多一段 `[effectiveCodetable]`，方案照常能用，但该方案
    /// 的所有码表行为从此被冻结在用户打开设置页那一刻——之后改全局配置对它不再有效，
    /// 而用户从没动过那些项。
    #[test]
    fn save_config_strips_readonly_sidecar() {
        let cfg = json!({
            "schema": { "id": "wubi86" },
            "engine": { "type": "codetable", "codetable": { "punct_commit": true } },
            "effectiveCodetable": { "punct_commit": true, "z_key_repeat": false },
        });
        let stripped = strip_readonly_fields(&cfg);
        assert!(
            stripped.get("effectiveCodetable").is_none(),
            "旁路字段应被剥掉，实际 {stripped}"
        );
        // 其余内容原样保留——剥错了会静默丢配置。
        assert_eq!(
            stripped.pointer("/engine/codetable/punct_commit"),
            Some(&json!(true))
        );
        assert_eq!(stripped.pointer("/schema/id"), Some(&json!("wubi86")));

        // 没有旁路字段时原样返回（不因为多一次 clone 就改变结构）。
        let plain = json!({ "engine": { "type": "codetable" } });
        assert_eq!(strip_readonly_fields(&plain), plain);

        // 与 diff 串起来看：剥之后，方案文件没写过的旁路键不会被判成「新增」。
        let base = json!({ "schema": { "id": "wubi86" }, "engine": { "type": "codetable" } });
        let d = json_diff(&base, &stripped).unwrap_or(json!({}));
        assert!(
            d.get("effectiveCodetable").is_none(),
            "diff 里不该出现旁路字段，实际 {d}"
        );

        // ★ 逐个覆盖 READONLY_SIDECAR_FIELDS，而不是只测其中一个：新增旁路字段却忘了
        // 登记，正是这个坑的复发形态，只测 effectiveCodetable 的话照样全绿。
        for f in READONLY_SIDECAR_FIELDS {
            let mut one = json!({ "schema": { "id": "wubi86" } });
            one.as_object_mut()
                .unwrap()
                .insert((*f).to_string(), json!("任意值"));
            let s = strip_readonly_fields(&one);
            assert!(s.get(*f).is_none(), "旁路字段 {f} 未被剥掉，实际 {s}");
            assert_eq!(
                s.pointer("/schema/id"),
                Some(&json!("wubi86")),
                "剥 {f} 时误伤了真实配置"
            );
        }
    }

    /// `[overlay]` 段经 getConfig → 改 → saveConfig 往返，并**立即体现在 overlay 注册表里**。
    ///
    /// 这条锁的是「设置页把一个普通方案变成 overlay 方案（快符）」的整条通路。它之所以
    /// 几乎不需要新代码，正是这次下沉的收益：`[overlay]` 住在方案文件里，于是方案配置
    /// 已有的读（getConfig 三层合并）、写（saveConfig 稀疏 diff 落 override）、生效
    /// （read_schema 的 merge_toml）三条通路全部现成——原先它住在 config.toml 的
    /// StructList 数组里，这三样一样都够不着。
    #[test]
    fn schema_overlay_section_roundtrips_through_save_config() {
        let dir = std::env::temp_dir().join("wind_webdata_overlay_rt");
        let _ = std::fs::remove_dir_all(&dir);
        let schemas = dir.join("schemas");
        std::fs::create_dir_all(&schemas).unwrap();
        // 出厂是个**普通**码表方案：没有 [overlay] 段。
        std::fs::write(
            schemas.join("zz_rt.schema.toml"),
            "[schema]\nid = \"zz_rt\"\nname = \"往返\"\n\
             [engine]\ntype = \"codetable\"\n[engine.codetable]\nmax_code_length = 4\n",
        )
        .unwrap();
        let ov_dir = dir.join("overrides");
        std::fs::create_dir_all(&ov_dir).unwrap();

        let store_path = std::env::temp_dir().join("wind_webdata_overlay_rt.redb");
        let _ = std::fs::remove_file(&store_path);
        let store = std::sync::Arc::new(wind_store::Store::open(&store_path).unwrap());
        let c = Coordinator::new_headless_with_store_override(
            wind_config::Config::default(),
            Some(&dir),
            store,
            Some(ov_dir.clone()),
        );

        // 起点：不是 overlay 方案。
        assert!(
            c.engine_mgr().overlay_index_of("zz_rt").is_none(),
            "出厂无 [overlay] 段，不该在注册表里"
        );
        let mut cfg = c
            .web_data_rpc("schema.getConfig", &json!({ "id": "zz_rt" }))
            .unwrap();
        assert!(
            cfg.get("overlay").is_none_or(|v| v.is_null()),
            "未配置时 overlay 应缺省/为 null，实际 {cfg}"
        );

        // 设置页动作：加上 [overlay] 段并保存。
        cfg.as_object_mut().unwrap().insert(
            "overlay".to_string(),
            json!({ "kind": "special", "show_all_on_enter": true, "candidate_layout": "vertical" }),
        );
        c.web_data_rpc("schema.saveConfig", &json!({ "id": "zz_rt", "cfg": cfg }))
            .unwrap();

        // 落盘的是**稀疏 diff**：只有改动项进 override，方案文件的其余定义仍可透传。
        let written = std::fs::read_to_string(ov_dir.join("zz_rt.toml")).unwrap();
        assert!(
            written.contains("[overlay]"),
            "override 未写入 [overlay]：{written}"
        );

        // 生效：注册表整表随 invalidate 重建，该方案成为 overlay 方案。
        c.engine_mgr().invalidate_schema("zz_rt");
        let idx = c
            .engine_mgr()
            .overlay_index_of("zz_rt")
            .expect("保存后应进 overlay 注册表");
        let e = c.engine_mgr().overlay_modes()[idx as usize].clone();
        assert!(e.spec.show_all_on_enter);
        assert_eq!(e.spec.candidate_layout, wind_config::LayoutIntent::Vertical);
        assert_eq!(e.name, "往返", "override 未提及的字段仍来自方案文件");

        let _ = std::fs::remove_file(&store_path);
    }

    #[test]
    fn schema_get_config_graceful_without_data_dir() {
        // data_dir=None（coord helper）→ 无方案文件 → getConfig 返回 {}，saveConfig 报错（无基础）。
        let c = coord("schema");
        let r = c
            .web_data_rpc("schema.getConfig", &json!({ "id": "pinyin" }))
            .unwrap();
        assert!(r.is_object() && r.as_object().unwrap().is_empty());
        assert!(
            c.web_data_rpc("schema.saveConfig", &json!({ "id": "pinyin", "cfg": {} }))
                .is_err()
        );
    }

    #[test]
    fn stats_summary_rich_fields() {
        use wind_store::stats::CommitSource;
        let c = coord("stats_summary_rich");
        // 今日：中文 2(码长4,首选) + 英文 2(临英,次选)
        c.debug_record_commit("你好", 4, 0, CommitSource::Candidate);
        c.debug_record_commit("ab", 0, 1, CommitSource::TempEnglish);
        let r = c.web_data_rpc("stats.summary", &json!({})).unwrap();
        assert_eq!(r["today_chinese"], 2);
        assert_eq!(r["today_english"], 2);
        assert_eq!(r["today_chars"], 4);
        assert_eq!(r["total_chars"], 4);
        assert_eq!(r["active_days"], 1);
        assert!((r["avg_code_len"].as_f64().unwrap() - 4.0).abs() < 1e-9);
        assert!(
            (r["first_select_rate"].as_f64().unwrap() - 0.5).abs() < 1e-9,
            "首选率=首选1/总选2=0.5"
        );
    }

    #[test]
    fn stats_daily_rich_shape() {
        use wind_store::stats::CommitSource;
        let c = coord("stats_daily_rich");
        c.debug_record_commit("你好", 4, 0, CommitSource::Candidate);
        c.stat_collector().unwrap().flush(); // 落库才能被 daily 区间读到
        let today = today_str();
        let r = c
            .web_data_rpc("stats.daily", &json!({ "from": today, "to": today }))
            .unwrap();
        let arr = r.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        let d = &arr[0];
        assert_eq!(d["d"], today);
        assert_eq!(d["tc"], 2);
        assert_eq!(d["cc"], 2);
        assert_eq!(d["cls"], 4);
        assert_eq!(d["h"].as_array().unwrap().len(), 24);
        assert_eq!(d["cpd"][0], 1);
    }

    /// Task 2：拼音类方案（pinyin_simp / double_pinyin）写入共享 "pinyin" 存储，
    /// 跨方案 id 互读能取到同一份用户词。
    #[test]
    fn pinyin_and_shuangpin_share_userdict() {
        use std::io::Write;
        // 写两个拼音类方案 schema.toml，让 data_schema_id 折叠到 "pinyin"
        let base_dir = std::env::temp_dir().join("wind_coord_share_userdict_test");
        let schemas = base_dir.join("schemas");
        std::fs::create_dir_all(&schemas).unwrap();
        for name in ["pinyin_simp", "double_pinyin"] {
            let mut f = std::fs::File::create(schemas.join(format!("{name}.schema.toml"))).unwrap();
            write!(f, "[engine]\ntype = \"pinyin\"\n").unwrap();
        }

        let db_path = std::env::temp_dir().join("wind_coord_share_userdict.redb");
        let _ = std::fs::remove_file(&db_path);
        let store = Arc::new(Store::open(&db_path).unwrap());
        let c = Coordinator::new_headless_with_store(
            Config::default(),
            Some(base_dir.as_path()),
            Arc::clone(&store),
        );

        // 用拼音方案加词
        c.web_data_rpc(
            "dict.add",
            &json!({ "schemaId": "pinyin_simp", "code": "nihao", "text": "你好", "weight": 5 }),
        )
        .unwrap();

        // 用双拼方案读，应读到同一条（共享 "pinyin" 存储键）
        let list = c
            .web_data_rpc(
                "dict.listPaged",
                &json!({ "schemaId": "double_pinyin", "offset": 0, "limit": 100 }),
            )
            .unwrap();
        let items = list["items"].as_array().unwrap();
        assert!(
            items.iter().any(|it| it["text"] == "你好"),
            "双拼应读到拼音下加的词（data_schema_id 共享）"
        );
    }

    /// `dict.stats` 的候选调整计数必须走 `data_schema_id` 折叠。
    ///
    /// 这里曾是**全仓唯一**一处直传原始方案 id 的 shadow 读取：写端 `candidate_op` 与
    /// `shadow.list` 都折叠到 `"pinyin"`，统计却拿 `double_pinyin` 去查，于是双拼方案的
    /// 规则计数恒显示 0——功能明明生效，设置页却像是一条规则都没有。
    ///
    /// 反向对照（`user_words`）不可省：它**刻意**保持原始 id（走 `write_data_schema_id`
    /// 的按来源分桶，与 shadow 不是同一套归属规则）。没有这一条，本测试无法区分
    /// 「shadow 正确折叠」与「整个函数被改成一律折叠」——后者会悄悄改掉用户词的统计口径。
    #[test]
    fn dict_stats_shadow_count_follows_data_schema_folding() {
        use std::io::Write;
        let base_dir = std::env::temp_dir().join("wind_coord_stats_shadow_fold");
        let schemas = base_dir.join("schemas");
        std::fs::create_dir_all(&schemas).unwrap();
        for name in ["pinyin_simp", "double_pinyin"] {
            let mut f = std::fs::File::create(schemas.join(format!("{name}.schema.toml"))).unwrap();
            write!(f, "[engine]\ntype = \"pinyin\"\n").unwrap();
        }

        let db_path = std::env::temp_dir().join("wind_coord_stats_shadow_fold.redb");
        let _ = std::fs::remove_file(&db_path);
        let store = Arc::new(Store::open(&db_path).unwrap());
        // `dict.stats` 遍历的是**已启用方案**（`config.schema.available`），不是目录扫描结果
        // ——只写 schema.toml 不够，两个方案都得在这份清单里才会出现在统计中。
        let mut cfg = Config::default();
        cfg.schema.available = vec!["pinyin_simp".into(), "double_pinyin".into()];
        cfg.schema.active = "pinyin_simp".into();
        let c =
            Coordinator::new_headless_with_store(cfg, Some(base_dir.as_path()), Arc::clone(&store));

        // 用全拼方案置顶一条（写端折叠 → 落在 "pinyin"）。
        c.web_data_rpc(
            "shadow.pin",
            &json!({ "schemaId": "pinyin_simp", "code": "hao", "word": "好", "position": 0 }),
        )
        .unwrap();

        let stats = c.web_data_rpc("dict.stats", &json!({})).unwrap();
        let rows = stats.as_array().expect("stats 是数组");
        let row_of = |id: &str| -> Value {
            rows.iter()
                .find(|r| r["schemaId"] == id)
                .cloned()
                .unwrap_or(Value::Null)
        };

        assert_eq!(
            row_of("double_pinyin")["shadowRules"],
            1,
            "双拼方案须报出折叠后的规则数（与全拼共享同一条）"
        );
        assert_eq!(row_of("pinyin_simp")["shadowRules"], 1, "全拼方案同样报 1");
    }

    /// 拼音的默认导出段必须含**候选调整**，与设置页子标签同增同减。
    ///
    /// 这条跨仓契约没有编译期约束：设置页给了拼音「候选调整」tab、而导出默认不含该段时，
    /// 用户在设置页看得见规则，导出的文件里却没有，直到还原时才发现丢了数据。
    #[test]
    fn pinyin_default_export_sections_include_shadow() {
        use wind_store::dict_export::DictSection;
        let py = default_dict_sections("pinyin");
        assert!(
            py.contains(&DictSection::Shadow),
            "拼音默认导出段须含候选调整，实际: {py:?}"
        );
        // 反向对照：混输只有候选调整这一段，证明本函数确实在按引擎类型分流，
        // 而不是被改成了「一律返回全部段」。
        assert_eq!(
            default_dict_sections("mixed"),
            vec![DictSection::Shadow],
            "混输仍只导出候选调整"
        );
    }

    /// setDictEnabled 落盘的 override 每库只含 `{id, enabled}`——绝不携带 path/label/base_order
    /// 等结构字段，否则 override 会冻结整份词库定义（方案升级后新增/改动的库透不过来）。
    #[test]
    fn sparse_dict_overrides_carries_only_id_and_enabled() {
        use wind_config::schema::DictSpec;
        let dicts = vec![
            // 有显式启用态 → 落盘
            DictSpec {
                id: "ext1".into(),
                label: "分类词库".into(),
                path: "flypy/11_fl.dict.yaml".into(),
                base_order: 1,
                default_enabled: Some(true),
                enabled: Some(false),
                ..Default::default()
            },
            // 无显式启用态（用户没翻过）→ 不落盘，继承方案的 default_enabled
            DictSpec {
                id: "ext2".into(),
                path: "flypy/21_yj.dict.yaml".into(),
                default_enabled: Some(true),
                enabled: None,
                ..Default::default()
            },
            // 无 id 无法按 id 匹配回方案文件 → 丢弃
            DictSpec {
                id: String::new(),
                path: "flypy/31_fh.dict.yaml".into(),
                enabled: Some(true),
                ..Default::default()
            },
        ];

        let out = Coordinator::sparse_dict_overrides(&dicts);
        let arr = out.as_array().expect("应为数组");
        assert_eq!(arr.len(), 1, "只有 ext1 有显式启用态且带 id");

        let t = arr[0].as_table().unwrap();
        assert_eq!(t.get("id").unwrap().as_str(), Some("ext1"));
        assert_eq!(t.get("enabled").unwrap().as_bool(), Some(false));
        assert_eq!(
            t.len(),
            2,
            "除 id/enabled 外不得携带任何结构字段，实际: {:?}",
            t.keys().collect::<Vec<_>>()
        );
    }

    /// Task 3：schema.list 每个拼音方案应携带 scheme 字段（full/shuangpin），非拼音方案为空串。
    #[test]
    fn schema_list_exposes_scheme() {
        use std::io::Write;
        let base_dir = std::env::temp_dir().join("wind_coord_schema_list_scheme_test");
        let schemas = base_dir.join("schemas");
        std::fs::create_dir_all(&schemas).unwrap();
        // 创建全拼方案
        {
            let mut f = std::fs::File::create(schemas.join("pinyin_full.schema.toml")).unwrap();
            write!(
                f,
                "[engine]\ntype = \"pinyin\"\n[engine.pinyin]\nscheme = \"full\"\n"
            )
            .unwrap();
        }
        // 创建双拼方案
        {
            let mut f =
                std::fs::File::create(schemas.join("double_pinyin_sp.schema.toml")).unwrap();
            write!(
                f,
                "[engine]\ntype = \"pinyin\"\n[engine.pinyin]\nscheme = \"shuangpin\"\n"
            )
            .unwrap();
        }
        let db_path = std::env::temp_dir().join("wind_webdata_schema_list_scheme.redb");
        let _ = std::fs::remove_file(&db_path);
        let store = Arc::new(Store::open(&db_path).unwrap());
        let c = Coordinator::new_headless_with_store(
            Config::default(),
            Some(base_dir.as_path()),
            Arc::clone(&store),
        );

        let list = c.web_data_rpc("schema.list", &json!({})).unwrap();
        let arr = list.as_array().unwrap();

        // 必须有方案
        assert!(!arr.is_empty(), "schema.list 应返回非空数组");
        // 每项都应有 scheme 键
        for item in arr.iter() {
            assert!(
                item.get("scheme").is_some(),
                "每个方案项应有 scheme 字段，缺失于: {item}"
            );
        }
        // 全拼方案 scheme="full"
        let full = arr.iter().find(|s| s["id"] == "pinyin_full");
        assert!(full.is_some(), "应有 pinyin_full 方案");
        assert_eq!(full.unwrap()["scheme"], "full", "全拼方案 scheme 应为 full");
        // 双拼方案 scheme="shuangpin"
        let sp = arr.iter().find(|s| s["id"] == "double_pinyin_sp");
        assert!(sp.is_some(), "应有 double_pinyin_sp 方案");
        assert_eq!(
            sp.unwrap()["scheme"],
            "shuangpin",
            "双拼方案 scheme 应为 shuangpin"
        );
    }

    /// 英文方案是可切换方案：出现在 schema.list 里，且 engineType 自成一档。
    ///
    /// `is_pinyin()` 对 `type = "english"` 走的是「主词库 dict_type 是不是 rime_pinyin」
    /// 那条兜底分支，英文词库是 `type = "english"`，于是会一路落到 `"codetable"`——
    /// 设置页的类型徽章就会把英文标成「码表」。故 `resolve_engine_type` 必须显式分流。
    #[test]
    fn english_schema_listed_with_its_own_engine_type() {
        use std::io::Write;
        let base_dir = std::env::temp_dir().join("wind_coord_schema_list_english_test");
        let schemas = base_dir.join("schemas");
        let _ = std::fs::remove_dir_all(&base_dir);
        std::fs::create_dir_all(&schemas).unwrap();
        {
            let mut f = std::fs::File::create(schemas.join("en_test.schema.toml")).unwrap();
            write!(
                f,
                "[schema]\nid = \"en_test\"\n[engine]\ntype = \"english\"\n"
            )
            .unwrap();
        }
        // 反向对照：显式 hidden 的方案必须仍被挡在列表外。少了这条，上面的断言在
        // 「hidden 过滤整个失效」时也会通过——那才是去掉 english 的 hidden 时最该防的回归。
        {
            let mut f = std::fs::File::create(schemas.join("en_hidden.schema.toml")).unwrap();
            write!(
                f,
                "[schema]\nid = \"en_hidden\"\nhidden = true\n[engine]\ntype = \"english\"\n"
            )
            .unwrap();
        }
        let db_path = std::env::temp_dir().join("wind_webdata_schema_list_english.redb");
        let _ = std::fs::remove_file(&db_path);
        let store = Arc::new(Store::open(&db_path).unwrap());
        let c = Coordinator::new_headless_with_store(
            Config::default(),
            Some(base_dir.as_path()),
            Arc::clone(&store),
        );

        let list = c.web_data_rpc("schema.list", &json!({})).unwrap();
        let arr = list.as_array().unwrap();

        let en = arr.iter().find(|s| s["id"] == "en_test");
        assert!(en.is_some(), "英文方案应出现在 schema.list 中：{arr:?}");
        assert_eq!(
            en.unwrap()["engineType"],
            "english",
            "英文方案的 engineType 应为 english，落成 codetable 会让类型徽章显示为「码表」"
        );
        assert!(
            !arr.iter().any(|s| s["id"] == "en_hidden"),
            "hidden = true 的方案默认不得出现在列表中：{arr:?}"
        );

        // includeHidden = true：隐藏方案出现，且带 hidden 标志供设置页区分该行能配什么。
        let list2 = c
            .web_data_rpc("schema.list", &json!({ "includeHidden": true }))
            .unwrap();
        let arr2 = list2.as_array().unwrap();
        let hid = arr2
            .iter()
            .find(|s| s["id"] == "en_hidden")
            .expect("includeHidden 时隐藏方案应出现");
        assert_eq!(hid["hidden"], true, "隐藏方案应带 hidden = true");
        let vis = arr2.iter().find(|s| s["id"] == "en_test").unwrap();
        assert_eq!(vis["hidden"], false, "非隐藏方案的 hidden 应为 false");
    }

    #[test]
    fn dict_list_paged_sort() {
        let c = coord("dict_sort");
        for (code, text, weight) in [("ab", "B词", 10i32), ("aa", "A词", 30), ("ac", "C词", 5)] {
            c.web_data_rpc(
                "dict.add",
                &json!({ "schemaId": "wb", "code": code, "text": text, "weight": weight }),
            )
            .unwrap();
        }
        // weight asc：5 → 10 → 30
        let r = c
            .web_data_rpc(
                "dict.listPaged",
                &json!({ "schemaId": "wb", "offset": 0, "limit": 10,
                          "sortBy": "weight", "sortOrder": "asc" }),
            )
            .unwrap();
        let items = r["items"].as_array().unwrap();
        assert_eq!(items[0]["weight"], 5, "asc 首项应为最小权重");
        assert_eq!(items[2]["weight"], 30, "asc 末项应为最大权重");
        // weight desc：30 → 10 → 5
        let r2 = c
            .web_data_rpc(
                "dict.listPaged",
                &json!({ "schemaId": "wb", "offset": 0, "limit": 10,
                          "sortBy": "weight", "sortOrder": "desc" }),
            )
            .unwrap();
        assert_eq!(r2["items"][0]["weight"], 30, "desc 首项应为最大权重");
        // 跨页切片：asc offset=1 limit=1 取排序后第 2 条（weight=10）
        let r3 = c
            .web_data_rpc(
                "dict.listPaged",
                &json!({ "schemaId": "wb", "offset": 1, "limit": 1,
                          "sortBy": "weight", "sortOrder": "asc" }),
            )
            .unwrap();
        assert_eq!(r3["total"], 3, "跨页切片 total 不变");
        assert_eq!(r3["items"][0]["weight"], 10, "offset=1 asc 应取 weight=10");
        // 不传 sortBy 行为不变（total 正确）
        let r4 = c
            .web_data_rpc(
                "dict.listPaged",
                &json!({ "schemaId": "wb", "offset": 0, "limit": 10 }),
            )
            .unwrap();
        assert_eq!(r4["total"], 3, "不传 sortBy 应保持原有行为");
    }

    #[test]
    fn freq_list_paged_sort() {
        let c = coord("freq_sort");
        let store = c.user_store().unwrap();
        // de=1次, ta=2次, shi=3次
        store.record_freq("py", "de", "的").unwrap();
        store.record_freq("py", "ta", "他").unwrap();
        store.record_freq("py", "ta", "他").unwrap();
        store.record_freq("py", "shi", "是").unwrap();
        store.record_freq("py", "shi", "是").unwrap();
        store.record_freq("py", "shi", "是").unwrap();
        // count asc：1 → 2 → 3
        let r = c
            .web_data_rpc(
                "freq.listPaged",
                &json!({ "schemaId": "py", "offset": 0, "limit": 10,
                          "sortBy": "count", "sortOrder": "asc" }),
            )
            .unwrap();
        let items = r["items"].as_array().unwrap();
        assert_eq!(items[0]["count"], 1, "asc 首项应为 count=1");
        assert_eq!(items[2]["count"], 3, "asc 末项应为 count=3");
        // count desc：3 → 2 → 1
        let r2 = c
            .web_data_rpc(
                "freq.listPaged",
                &json!({ "schemaId": "py", "offset": 0, "limit": 10,
                          "sortBy": "count", "sortOrder": "desc" }),
            )
            .unwrap();
        assert_eq!(r2["items"][0]["count"], 3, "desc 首项应为 count=3");
        // 跨页切片：asc offset=1 limit=1 取第 2 条（count=2）
        let r3 = c
            .web_data_rpc(
                "freq.listPaged",
                &json!({ "schemaId": "py", "offset": 1, "limit": 1,
                          "sortBy": "count", "sortOrder": "asc" }),
            )
            .unwrap();
        assert_eq!(r3["total"], 3, "跨页切片 total 不变");
        assert_eq!(r3["items"][0]["count"], 2, "offset=1 asc 应取 count=2");
        // 不传 sortBy 行为不变
        let r4 = c
            .web_data_rpc(
                "freq.listPaged",
                &json!({ "schemaId": "py", "offset": 0, "limit": 10 }),
            )
            .unwrap();
        assert_eq!(r4["total"], 3, "不传 sortBy 应保持原有行为");
    }

    #[test]
    fn dict_list_paged_text_query() {
        let c = coord("dict_text_query");
        for (code, text, weight) in [
            ("wghg", "程序", 3i32),
            ("ggkg", "王中", 5),
            ("aaaa", "工", 0),
        ] {
            c.web_data_rpc(
                "dict.add",
                &json!({ "schemaId": "wb", "code": code, "text": text, "weight": weight }),
            )
            .unwrap();
        }
        // 按词条内容搜索：编码 "wghg" 不以 "程" 开头，命中应来自 text 包含匹配。
        let r = c
            .web_data_rpc(
                "dict.listPaged",
                &json!({ "schemaId": "wb", "prefix": "程", "offset": 0, "limit": 10 }),
            )
            .unwrap();
        assert_eq!(r["total"], 1, "词条内容搜索应命中 1 条");
        assert_eq!(r["items"][0]["text"], "程序", "应按 text 内容命中");
        // 按编码前缀搜索仍生效。
        let r2 = c
            .web_data_rpc(
                "dict.listPaged",
                &json!({ "schemaId": "wb", "prefix": "wg", "offset": 0, "limit": 10 }),
            )
            .unwrap();
        assert_eq!(r2["total"], 1, "编码前缀搜索应命中 1 条");
        assert_eq!(r2["items"][0]["code"], "wghg", "应按 code 前缀命中");
    }

    #[test]
    fn freq_list_paged_text_query() {
        let c = coord("freq_text_query");
        let store = c.user_store().unwrap();
        store.record_freq("py", "nihao", "你好").unwrap();
        store.record_freq("py", "women", "我们").unwrap();
        // 按词条内容搜索：编码 "nihao" 不以 "你" 开头，命中应来自 text 包含匹配。
        let r = c
            .web_data_rpc(
                "freq.listPaged",
                &json!({ "schemaId": "py", "prefix": "你", "offset": 0, "limit": 10 }),
            )
            .unwrap();
        assert_eq!(r["total"], 1, "词条内容搜索应命中 1 条");
        assert_eq!(r["items"][0]["text"], "你好", "应按 text 内容命中");
        // 按编码前缀搜索仍生效。
        let r2 = c
            .web_data_rpc(
                "freq.listPaged",
                &json!({ "schemaId": "py", "prefix": "women", "offset": 0, "limit": 10 }),
            )
            .unwrap();
        assert_eq!(r2["total"], 1, "编码前缀搜索应命中 1 条");
        assert_eq!(r2["items"][0]["text"], "我们", "应按 code 前缀命中对应词条");
    }

    #[test]
    fn phrase_list_user_sort() {
        let c = coord("phrase_sort");
        for (code, text, weight) in [("b", "乙", 20i32), ("a", "甲", 50), ("c", "丙", 5)] {
            c.web_data_rpc(
                "phrase.add",
                &json!({ "code": code, "text": text, "position": 0, "weight": weight }),
            )
            .unwrap();
        }
        // weight asc：5 → 20 → 50
        let r = c
            .web_data_rpc(
                "phrase.listUser",
                &json!({ "offset": 0, "limit": 10,
                          "sortBy": "weight", "sortOrder": "asc" }),
            )
            .unwrap();
        let items = r["items"].as_array().unwrap();
        assert_eq!(items[0]["weight"], 5, "asc 首项应为 weight=5");
        assert_eq!(items[2]["weight"], 50, "asc 末项应为 weight=50");
        // weight desc：50 → 20 → 5
        let r2 = c
            .web_data_rpc(
                "phrase.listUser",
                &json!({ "offset": 0, "limit": 10,
                          "sortBy": "weight", "sortOrder": "desc" }),
            )
            .unwrap();
        assert_eq!(r2["items"][0]["weight"], 50, "desc 首项应为 weight=50");
        // 跨页切片：asc offset=1 limit=1 取第 2 条（weight=20）
        let r3 = c
            .web_data_rpc(
                "phrase.listUser",
                &json!({ "offset": 1, "limit": 1,
                          "sortBy": "weight", "sortOrder": "asc" }),
            )
            .unwrap();
        assert_eq!(r3["total"], 3, "跨页切片 total 不变");
        assert_eq!(r3["items"][0]["weight"], 20, "offset=1 asc 应取 weight=20");
        // 不传 sortBy 行为不变
        let r4 = c
            .web_data_rpc("phrase.listUser", &json!({ "offset": 0, "limit": 10 }))
            .unwrap();
        assert_eq!(r4["total"], 3, "不传 sortBy 应保持原有行为");
    }

    #[test]
    fn scheme_package_rpc_contract() {
        let c = coord("schemepkg");
        // exportPackage:不存在的方案 id → 错误
        assert!(
            c.web_data_rpc(
                "scheme.exportPackage",
                &json!({ "id": "zz_no_such_schema", "path": std::env::temp_dir().join("zz_no.zip").to_string_lossy() }),
            )
            .is_err(),
            "不存在的方案应报错"
        );
        // previewImport:不存在的包路径 → 错误
        assert!(
            c.web_data_rpc(
                "scheme.previewImport",
                &json!({ "path": std::env::temp_dir().join("zz_no_such_pkg.zip").to_string_lossy() }),
            )
            .is_err()
        );
        // previewImport:真实构造的包 → 只读预览成功,形状正确
        let t = std::env::temp_dir().join("wind_schemepkg_test");
        let _ = std::fs::remove_dir_all(&t);
        let (user, system) = (t.join("u"), t.join("s"));
        std::fs::create_dir_all(user.join("my")).unwrap();
        std::fs::create_dir_all(&system).unwrap();
        std::fs::write(
            user.join("my.schema.toml"),
            "[schema]\nid=\"my\"\n[[dictionaries]]\npath=\"my/d.yaml\"\n",
        )
        .unwrap();
        std::fs::write(user.join("my/d.yaml"), "d").unwrap();
        let pkg = t.join("my.zip");
        wind_transfer::scheme::export_package(
            "my",
            &user,
            Some(&system),
            None,
            &pkg,
            "1.0.0",
            "windows",
            "t",
        )
        .unwrap();
        let prev = c
            .web_data_rpc(
                "scheme.previewImport",
                &json!({ "path": pkg.to_string_lossy() }),
            )
            .unwrap();
        assert_eq!(
            prev.get("package")
                .and_then(|p| p.get("schema"))
                .and_then(|s| s.get("id"))
                .and_then(|v| v.as_str()),
            Some("my"),
            "v2 预览返回 package 元信息"
        );
        assert!(prev.get("willAdd").and_then(|v| v.as_array()).is_some());
        assert!(prev.get("conflicts").and_then(|v| v.as_array()).is_some());
        let _ = std::fs::remove_dir_all(&t);
        // importPackage:不存在的包路径 → 错误
        assert!(
            c.web_data_rpc(
                "scheme.importPackage",
                &json!({ "path": std::env::temp_dir().join("zz_no_such_pkg.zip").to_string_lossy() }),
            )
            .is_err()
        );
    }

    /// 分发包的说明元信息:包级 title/description 提到响应顶层,片段自带的说明进
    /// `configPatch.info`。两者在本测试里**取不同的值**——取错了源也照绿是这类
    /// "两个来源同名字段"最容易漏的假绿。
    #[test]
    fn scheme_preview_import_separates_package_and_patch_info() {
        let c = coord("schemepkginfo");
        let t = std::env::temp_dir().join("wind_schemepkginfo_test");
        let _ = std::fs::remove_dir_all(&t);
        std::fs::create_dir_all(&t).unwrap();
        let pkg = t.join("info.wpkg");
        {
            let f = std::fs::File::create(&pkg).unwrap();
            let mut w = zip::ZipWriter::new(f);
            let opts = zip::write::SimpleFileOptions::default();
            for (name, body) in [
                (
                    "package.toml",
                    "[package]\nformat_version = 2\ntitle = \"包级标题\"\ndescription = \"包级说明\"\n",
                ),
                (
                    "zz_info_probe.schema.toml",
                    "[schema]\nid=\"zz_info_probe\"\n",
                ),
                (
                    "config_patch.toml",
                    "[package]\ntitle = \"片段级标题\"\ndescription = \"片段级说明\"\n\
                     [ui.candidate]\nper_page = 9\n",
                ),
            ] {
                use std::io::Write;
                w.start_file(name, opts).unwrap();
                w.write_all(body.as_bytes()).unwrap();
            }
            w.finish().unwrap();
        }
        let prev = c
            .web_data_rpc(
                "scheme.previewImport",
                &json!({ "path": pkg.to_string_lossy() }),
            )
            .unwrap();
        assert_eq!(prev["title"], json!("包级标题"), "包级说明提到顶层");
        assert_eq!(prev["description"], json!("包级说明"));
        let cp = prev.get("configPatch").expect("含 config_patch 应附 diff");
        assert_eq!(
            cp["info"]["title"],
            json!("片段级标题"),
            "片段自带的说明进 configPatch.info,不与包级混淆"
        );
        assert_eq!(cp["info"]["description"], json!("片段级说明"));
        // 保留段不产出配置条目:entries 只有真配置键那一条。
        let entries = cp["entries"].as_array().expect("entries 应为数组");
        assert_eq!(entries.len(), 1, "{entries:?}");
        assert_eq!(entries[0]["key"], json!("ui.candidate.per_page"));
        let _ = std::fs::remove_dir_all(&t);
    }

    /// 信封路径的两形:写了说明 → 顶层带出(只写 title 时 description 不出现);
    /// 没写 → 顶层与 configPatch 都没有说明字段。
    #[test]
    fn scheme_preview_import_text_info_present_and_absent() {
        let c = coord("schemetextinfo");
        let with_info = "[package]\nformat_version = 2\nkind = \"schema_text\"\n\
                         title = \"快符方案\"\n\
                         [[files]]\npath = \"zz_kf_info.schema.toml\"\ncontent = \"[schema]\\nid = 'kf'\\n\"\n";
        let prev = c
            .web_data_rpc("scheme.previewImportText", &json!({ "text": with_info }))
            .unwrap();
        assert_eq!(prev["title"], json!("快符方案"));
        assert!(
            prev.get("description").is_none(),
            "没写的字段不输出: {prev}"
        );

        let without_info = "[package]\nformat_version = 2\nkind = \"schema_text\"\n\
                            [[files]]\npath = \"zz_kf_info.schema.toml\"\ncontent = \"[schema]\\nid = 'kf'\\n\"\n\
                            [[files]]\npath = \"config_patch.toml\"\ncontent = \"ui.candidate.per_page = 9\\n\"\n";
        let prev = c
            .web_data_rpc("scheme.previewImportText", &json!({ "text": without_info }))
            .unwrap();
        assert!(prev.get("title").is_none(), "无说明不输出 title: {prev}");
        assert!(prev.get("description").is_none());
        let cp = prev.get("configPatch").expect("含 config_patch 应附 diff");
        assert!(cp.get("info").is_none(), "片段没写说明就没有 info: {cp}");
    }

    /// 文本信封 RPC 契约(只测只读与错误路径:importText 会真写用户 schemas 目录,
    /// 落盘语义已在 wind-transfer::envelope 层覆盖——与 importPackage 同一取舍)。
    #[test]
    fn scheme_text_envelope_rpc_contract() {
        let c = coord("schemetext");
        let envelope = "[package]\nformat_version = 2\nkind = \"schema_text\"\n\
                        [schema]\nid = \"kf\"\nversion = \"1.00.0\"\n\
                        [[files]]\npath = \"zz_kf_probe.schema.toml\"\ncontent = \"[schema]\\nid = 'kf'\\n\"\n\
                        [[files]]\npath = \"config_patch.toml\"\ncontent = \"ui.candidate.per_page = 9\\n\"\n";
        let prev = c
            .web_data_rpc("scheme.previewImportText", &json!({ "text": envelope }))
            .unwrap();
        // 形状与 path 版一致
        assert_eq!(
            prev.get("package")
                .and_then(|p| p.get("schema"))
                .and_then(|s| s.get("id"))
                .and_then(|v| v.as_str()),
            Some("kf")
        );
        assert!(prev.get("willAdd").and_then(|v| v.as_array()).is_some());
        assert!(prev.get("conflicts").and_then(|v| v.as_array()).is_some());
        // 配置片段随预览附带逐键 diff(不落盘、不应用)
        let cp = prev.get("configPatch").expect("含 config_patch 应附 diff");
        assert_eq!(cp["ok"], json!(true));
        let entries = cp["entries"].as_array().expect("entries 应为数组");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0]["key"], json!("ui.candidate.per_page"));
        assert_eq!(entries[0]["next"], json!(9));
        assert!(
            prev.get("willAdd")
                .and_then(|v| v.as_array())
                .map(|a| a.iter().all(|v| v != "config_patch.toml"))
                .unwrap_or(false),
            "config_patch 不进文件清单"
        );

        // 非信封文本 → 错误带 not_schema_text: 前缀(设置端据此回落片段管线),两个方法一致。
        for method in ["scheme.previewImportText", "scheme.importText"] {
            let err = c
                .web_data_rpc(method, &json!({ "text": "ui.candidate.per_page = 9\n" }))
                .unwrap_err()
                .to_string();
            assert!(
                err.contains(wind_transfer::envelope::NOT_SCHEMA_TEXT),
                "{method} 对非信封文本须带回落前缀: {err}"
            );
        }
        // 缺 text 参数 → 错误
        assert!(
            c.web_data_rpc("scheme.previewImportText", &json!({}))
                .is_err()
        );
    }

    #[test]
    fn backup_rpc_contract() {
        let c = coord("backuprpc");
        // 种一条数据,create 到临时路径(coord 的 store 是临时 redb;文件域目录真实但只读不写:
        // create 只读取 config/schemas/themes,不写入它们)
        c.web_data_rpc(
            "dict.add",
            &json!({ "schemaId": "wb", "code": "a", "text": "工", "weight": 100 }),
        )
        .unwrap();
        let out = std::env::temp_dir().join("wind_backup_rpc_test.zip");
        let _ = std::fs::remove_file(&out);
        let r = c
            .web_data_rpc(
                "backup.create",
                &json!({ "path": out.to_string_lossy(), "includeStats": false }),
            )
            .unwrap();
        assert!(r.get("manifest").is_some());
        // inspect
        let ins = c
            .web_data_rpc("backup.inspect", &json!({ "path": out.to_string_lossy() }))
            .unwrap();
        assert_eq!(
            ins.get("manifest")
                .and_then(|m| m.get("kind"))
                .and_then(|v| v.as_str()),
            Some("backup")
        );
        // inspect 不存在的包 → 错误
        assert!(
            c.web_data_rpc(
                "backup.inspect",
                &json!({ "path": std::env::temp_dir().join("zz_no.zip").to_string_lossy() }),
            )
            .is_err()
        );
        // restore 仅数据域 sections(dict):写临时 store,不碰真实用户文件
        c.web_data_rpc("dict.clear", &json!({ "schemaId": "wb" }))
            .unwrap();
        let rr = c
            .web_data_rpc(
                "backup.restore",
                &json!({ "path": out.to_string_lossy(), "sections": ["dict"] }),
            )
            .unwrap();
        assert!(
            rr.get("restored")
                .and_then(|v| v.as_array())
                .map(|a| !a.is_empty())
                .unwrap_or(false)
        );
        let listed = c
            .web_data_rpc("dict.listPaged", &json!({ "schemaId": "wb", "limit": 10 }))
            .unwrap();
        assert_eq!(listed.get("total").and_then(|v| v.as_u64()), Some(1));
        let _ = std::fs::remove_file(&out);
    }
}

#[cfg(test)]
mod phrase_shadowing_tests {
    //! 「用户短语遮蔽系统条目」的可逆性：主键只有 `(code, text)` 一把，遮蔽行归属用户，
    //! 于是**任何删掉该行的操作都会连带删掉那条系统短语**。每条这样的路径都必须补回缺失的
    //! 系统条目，漏一条就是「系统短语莫名少了一条」——正是本特性早期版本的原始 bug。
    use super::*;
    use std::sync::Arc;
    use wind_config::config::Config;
    use wind_coordinator::Coordinator;
    use wind_store::Store;

    const SYS_CODE: &str = "date";
    const SYS_TEXT: &str = "$Y年$M月$D日";

    /// 带真实 `system.phrases.toml` 的无头协调器（`data_dir=None` 时补齐逻辑会整体早退，
    /// 那样测出来的「通过」是假的——它根本没跑到被测代码）。
    fn coord_with_sys_phrase(tag: &str) -> (Arc<Coordinator>, std::path::PathBuf) {
        let base = std::env::temp_dir().join(format!("wind_phrase_shadow_{tag}"));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        std::fs::write(
            base.join("system.phrases.toml"),
            format!(
                "[[phrases]]\ncode = '{SYS_CODE}'\ntext = '{SYS_TEXT}'\nweight = 1000\nposition = 1\n"
            ),
        )
        .unwrap();
        let db = base.join("s.redb");
        let store = Arc::new(Store::open(&db).unwrap());
        let c = Coordinator::new_headless_with_store(Config::default(), Some(&base), store);
        (c, base)
    }

    fn system_phrase_count(c: &Coordinator) -> usize {
        c.user_store().unwrap().list_system_phrases().unwrap().len()
    }

    fn user_phrase_count(c: &Coordinator) -> usize {
        c.user_store()
            .unwrap()
            .list_user_phrases_paged(None, 0, 99)
            .unwrap()
            .1
    }

    /// 前置校验：启动即完成系统短语入库，否则下面每个用例都在空库上跑、结论无意义。
    #[test]
    fn sanity_system_phrase_seeded_on_startup() {
        let (c, base) = coord_with_sys_phrase("seed");
        assert_eq!(system_phrase_count(&c), 1, "启动应把 TOML 系统短语同步入库");
        let _ = std::fs::remove_dir_all(&base);
    }

    /// 删除遮蔽行 → 系统条目露出来（用户预期：删掉我加的那条，就该回到系统默认）。
    #[test]
    fn removing_shadowing_phrase_restores_system_entry() {
        let (c, base) = coord_with_sys_phrase("remove");
        let p = json!({ "code": SYS_CODE, "text": SYS_TEXT, "weight": 5000, "position": 9 });
        c.web_data_rpc("phrase.add", &p).unwrap();
        assert_eq!(user_phrase_count(&c), 1, "遮蔽行归用户");
        assert_eq!(system_phrase_count(&c), 0, "系统条目被遮蔽");

        c.web_data_rpc(
            "phrase.remove",
            &json!({ "code": SYS_CODE, "text": SYS_TEXT }),
        )
        .unwrap();
        assert_eq!(user_phrase_count(&c), 0);
        assert_eq!(
            system_phrase_count(&c),
            1,
            "删掉遮蔽行后系统条目必须回来，而不是两条一起消失"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    /// 「清空用户短语」同理，且**不得**顺带重置用户对系统短语的编辑。
    #[test]
    fn clearing_user_phrases_restores_system_entry_without_resetting_edits() {
        let (c, base) = coord_with_sys_phrase("clear");
        let store = c.user_store().cloned().unwrap();
        // 另加一条纯系统短语并由用户改过权重（模拟在系统短语列表里调过）
        store
            .add_phrase("other", "别的", 0, 1)
            .and_then(|_| {
                store.reclaim_system_phrases(&[wind_store::phrases::SystemPhrase {
                    code: "other".into(),
                    text: "别的".into(),
                    weight: 1,
                    position: 0,
                }])
            })
            .unwrap();
        store
            .update_phrase("other", "别的", None, None, Some(7), Some(4321))
            .unwrap();

        c.web_data_rpc(
            "phrase.add",
            &json!({ "code": SYS_CODE, "text": SYS_TEXT, "weight": 5000, "position": 9 }),
        )
        .unwrap();
        c.web_data_rpc("phrase.resetDefault", &json!({})).unwrap();

        assert_eq!(user_phrase_count(&c), 0, "用户短语已清空");
        assert!(
            store
                .list_system_phrases()
                .unwrap()
                .iter()
                .any(|p| p.code == SYS_CODE),
            "被遮蔽的系统条目须补回"
        );
        let other = store
            .list_system_phrases()
            .unwrap()
            .into_iter()
            .find(|p| p.code == "other")
            .expect("另一条系统短语仍在");
        assert_eq!(
            (other.weight, other.position),
            (4321, 7),
            "清空用户短语不得重置用户对系统短语的编辑（补齐必须只补缺失，不能走 sync）"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    /// 改遮蔽行的编码 → 旧键被 remove，系统条目同样要补回。
    #[test]
    fn rekeying_shadowing_phrase_restores_system_entry() {
        let (c, base) = coord_with_sys_phrase("rekey");
        c.web_data_rpc(
            "phrase.add",
            &json!({ "code": SYS_CODE, "text": SYS_TEXT, "weight": 5000, "position": 0 }),
        )
        .unwrap();
        assert_eq!(system_phrase_count(&c), 0);

        c.web_data_rpc(
            "phrase.update",
            &json!({ "code": SYS_CODE, "text": SYS_TEXT, "newCode": "rq2" }),
        )
        .unwrap();
        assert_eq!(
            system_phrase_count(&c),
            1,
            "改键腾出原 (code,text) 后系统条目须补回"
        );
        assert_eq!(user_phrase_count(&c), 1, "改键后的用户短语仍在");
        let _ = std::fs::remove_dir_all(&base);
    }
}
