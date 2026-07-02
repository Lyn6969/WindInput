//! 短语系统：静态/动态短语模板展开 + 命令栏（cmdbar）双路径。
//!
//! 与 Go 版本 `wind_input/internal/dict/phrase.go` + `internal/cmdbar` 对齐。
//! 加载 `system.phrases.toml`，输入码命中短语 code 时生成候选。
//!
//! **双路径**（对齐 Go design §7.2）：
//! - 短语 text 使用命令栏语法（含 `$CC(`/`$SS(`/`$AA(` marker 或顶层 `{expr}` 插值）→ 经
//!   `wind-cmdbar` 解析求值（`{date()}`/`{calc(code)}`/`{upper(code)}`/`$SS` 字符串组/`$AA` 字符组等）。
//! - 否则 → 旧的简单模板变量展开（$Y/$M/$MM/$D/$DD/$HH/$mm/$ss/$WC/$YC/$MC/$DC/$ts/$tsms）。
//!
//! 命令栏 display 侧只用纯函数（无需宿主服务）；`$CC` 的副作用动作需平台服务（按键/剪贴板/
//! 进程注入），Rust 端平台层尚缺，故当前仅显现 display 候选，动作执行待平台服务补齐。

use chrono::{DateTime, Datelike, Local, Timelike};
use std::collections::HashMap;
use std::path::Path;
use tracing::warn;
use wind_cmdbar::{
    Phrase, PhraseEval, Services, default_registry, evaluate, evaluate_phrase, is_cmdbar_grammar,
    parse,
};

/// 一条短语（同 code 下按 weight 降序、position 升序排列）
#[derive(Debug, Clone)]
pub struct PhraseEntry {
    pub text: String,
    pub weight: i32,
    pub position: i32,
}

/// 一条短语命中：展开后的候选文本 + 权重 + 可选命令源 / 前缀导航目标。
/// - `command_src` 非空 → 这是 `$CC` 命令短语（选中时执行动作而非上屏 text），
///   其值为待重新求值/执行的命令源（如 `$CC("切简繁", ime.toggle("s2t"))`）。
/// - `nav_code` 非空 → 这是**前缀导航候选**（敲 `zz`/`co` 列出的 `zzbd`/`coen` 等），
///   `text` 为组名/命令显示名，`comment` 为码后缀（如 `bd`）。选中时补全输入到
///   `nav_code` 完整码并重查展开（见 coordinator commit_selected 的 is_group 臂）。
#[derive(Debug, Clone, PartialEq)]
pub struct PhraseHit {
    pub text: String,
    pub weight: i32,
    pub command_src: Option<String>,
    pub nav_code: Option<String>,
    pub comment: String,
}

impl PhraseHit {
    fn plain(text: String, weight: i32) -> Self {
        Self {
            text,
            weight,
            command_src: None,
            nav_code: None,
            comment: String::new(),
        }
    }

    /// 前缀导航——**组**候选（`$SS`/`$AA`）：`code` 为补全目标完整码，选中后补全展开。
    fn nav(text: String, weight: i32, code: String, comment: String) -> Self {
        Self {
            text,
            weight,
            command_src: None,
            nav_code: Some(code),
            comment,
        }
    }

    /// 前缀导航——**命令**候选（`$CC`）：选中后**直接执行** `src`（不二级展开），
    /// `code` 为完整码（执行时作输入上下文），`comment` 为码后缀。
    fn command_nav(text: String, weight: i32, src: String, code: String, comment: String) -> Self {
        Self {
            text,
            weight,
            command_src: Some(src),
            nav_code: Some(code),
            comment,
        }
    }
}

/// TOML 系统短语原始条目（platform 过滤后），供上层同步入库。
#[derive(Debug, Clone)]
pub struct SystemPhraseEntry {
    pub code: String,
    pub text: String,
    pub weight: i32,
    pub position: i32,
}

/// 短语层：code → 多条短语
#[derive(Debug, Default)]
pub struct PhraseLayer {
    map: HashMap<String, Vec<PhraseEntry>>,
}

#[derive(serde::Deserialize)]
struct PhrasesFile {
    #[serde(default)]
    phrases: Vec<RawPhrase>,
}

#[derive(serde::Deserialize)]
struct RawPhrase {
    code: String,
    text: String,
    #[serde(default)]
    weight: Option<i32>,
    #[serde(default)]
    position: Option<i32>,
    #[serde(default)]
    platform: Option<String>,
}

impl PhraseLayer {
    /// 从 system.phrases.toml 加载（文件缺失/解析失败 → 空层）
    pub fn load(path: &Path) -> Self {
        let content = match std::fs::read_to_string(path) {
            Ok(c) => c,
            Err(_) => return Self::default(),
        };
        let parsed: PhrasesFile = match toml::from_str(&content) {
            Ok(p) => p,
            Err(e) => {
                warn!("Parse phrases failed {}: {}", path.display(), e);
                return Self::default();
            }
        };
        let mut map: HashMap<String, Vec<PhraseEntry>> = HashMap::new();
        for r in parsed.phrases {
            // 平台过滤：空/"all"/"windows" 接受
            if let Some(p) = &r.platform {
                let p = p.to_lowercase();
                if !p.is_empty() && p != "all" && p != "windows" {
                    continue;
                }
            }
            map.entry(r.code).or_default().push(PhraseEntry {
                text: r.text,
                weight: r.weight.unwrap_or(1000),
                position: r.position.unwrap_or(0),
            });
        }
        for v in map.values_mut() {
            v.sort_by(|a, b| b.weight.cmp(&a.weight).then(a.position.cmp(&b.position)));
        }
        Self { map }
    }

    /// 解析 system.phrases.toml 为原始条目（platform 过滤，默认 weight=1000/position=0）。
    /// 供 coordinator 同步进 store；文件缺失/解析失败 → 空。
    pub fn parse_system_entries(path: &std::path::Path) -> Vec<SystemPhraseEntry> {
        let content = match std::fs::read_to_string(path) {
            Ok(c) => c,
            Err(_) => return Vec::new(),
        };
        let parsed: PhrasesFile = match toml::from_str(&content) {
            Ok(p) => p,
            Err(e) => {
                warn!("Parse phrases failed {}: {}", path.display(), e);
                return Vec::new();
            }
        };
        let mut out = Vec::new();
        for r in parsed.phrases {
            if let Some(p) = &r.platform {
                let p = p.to_lowercase();
                if !p.is_empty() && p != "all" && p != "windows" {
                    continue;
                }
            }
            out.push(SystemPhraseEntry {
                code: r.code,
                text: r.text,
                weight: r.weight.unwrap_or(1000),
                position: r.position.unwrap_or(0),
            });
        }
        out
    }

    /// 从 (code,text,weight,position) 记录构建短语层（调用方只传 enabled 项）。
    pub fn from_records(records: impl IntoIterator<Item = (String, String, i32, i32)>) -> Self {
        let mut map: HashMap<String, Vec<PhraseEntry>> = HashMap::new();
        for (code, text, weight, position) in records {
            map.entry(code).or_default().push(PhraseEntry {
                text,
                weight,
                position,
            });
        }
        for v in map.values_mut() {
            v.sort_by(|a, b| b.weight.cmp(&a.weight).then(a.position.cmp(&b.position)));
        }
        Self { map }
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// 查 code 对应的展开短语；跳过含不支持变量的项。
    /// `last` 为上屏历史快照（index 0 = 最近），供命令栏 display 侧的 `last(n)` 使用
    /// （如 `coll` 的 `$CC(last(), ...)` 候选需显示上一次上屏内容）。
    /// `clip` 为剪贴板读取回调（宿主注入，避免本 crate 依赖平台 UI 层）；供命令栏 display
    /// 侧的 `clip(n)` 使用（如 `coad` 的 `剪贴板加词:{clip()}` 候选标签）。测试传空闭包。
    pub fn lookup(
        &self,
        code: &str,
        last: &[String],
        clip: &dyn Fn(i64) -> String,
    ) -> Vec<PhraseHit> {
        self.lookup_at(code, Local::now(), last, clip)
    }

    /// 同 lookup，但显式传入时间（便于测试）。
    pub fn lookup_at(
        &self,
        code: &str,
        now: DateTime<Local>,
        last: &[String],
        clip: &dyn Fn(i64) -> String,
    ) -> Vec<PhraseHit> {
        let entries = match self.map.get(code) {
            Some(e) => e,
            None => return Vec::new(),
        };
        let mut out = Vec::new();
        for e in entries {
            if is_cmdbar_grammar(&e.text) {
                // 命令栏路径：display 求值（纯函数 + last/clip 上下文，无副作用服务）。
                let ctx = PhraseCtx {
                    input: code.to_string(),
                    now,
                    last,
                    clip,
                };
                match evaluate_phrase(&e.text, &ctx, default_registry()) {
                    // 无动作（literal/template，如 {date()}）→ 显示即上屏文本。
                    Ok(PhraseEval::Single { display, actions }) if actions.is_empty() => {
                        out.push(PhraseHit::plain(display, e.weight))
                    }
                    // $CC 命令短语（有动作）：携带命令源，选中时由 coordinator 执行动作。
                    Ok(PhraseEval::Single { display, .. }) => out.push(PhraseHit {
                        text: display,
                        weight: e.weight,
                        command_src: Some(e.text.clone()),
                        nav_code: None,
                        comment: String::new(),
                    }),
                    Ok(PhraseEval::Array(arr)) => {
                        for el in arr.elements {
                            // 仅显现无动作的字面元素（符号等）；带动作的嵌入 $CC 需元素级源，后续补。
                            if el.actions.is_empty() {
                                out.push(PhraseHit::plain(el.display, e.weight));
                            }
                        }
                    }
                    Err(err) => warn!("cmdbar phrase eval failed ({:?}): {}", e.text, err),
                }
            } else if let Some(text) = expand_template(&e.text, &now) {
                out.push(PhraseHit::plain(text, e.weight));
            }
        }
        out
    }

    /// 前缀导航：敲 `code`（长度 ≥ `min_len`）时，列出所有**码以 `code` 开头但更长**的
    /// marker 短语（`$CC`/`$SS`/`$AA`，未显式 `{prefix: false}`），每条出一个导航候选——
    /// `text` 为组名/命令显示名，`comment` 为码后缀。选中后由 coordinator 补全到完整码再展开。
    /// 数据驱动：新增短语零配置自动列出（对齐 Go SearchCommand 情况 3）。
    ///
    /// 不含精确码本身（走 [`Self::lookup_at`]），不列普通字面/模板短语（无 marker，维持
    /// 精确匹配语义，对齐 Go SearchPrefix 对 `$X` 模板的处理）。
    pub fn lookup_prefix(&self, code: &str, last: &[String], min_len: usize) -> Vec<PhraseHit> {
        self.lookup_prefix_at(code, Local::now(), last, min_len)
    }

    /// 同 [`Self::lookup_prefix`]，显式传时间便于测试。
    pub fn lookup_prefix_at(
        &self,
        code: &str,
        now: DateTime<Local>,
        last: &[String],
        min_len: usize,
    ) -> Vec<PhraseHit> {
        if code.is_empty() || code.len() < min_len {
            return Vec::new();
        }
        let reg = default_registry();
        // 廉价上下文：clip/sel/app/title 返回空，**避免列举时读整个剪贴板**（如 coad 的
        // display `剪贴板加词:{clip()}` 会读全部剪贴板内容 → 内存暴涨）；last 仍取真实
        // 快照（仅 Vec 索引，廉价，coll/cozd 等可正常显示）。真正执行命令时才用完整上下文。
        let ctx = NavCtx {
            input: code.to_string(),
            now,
            last,
        };
        let mut out = Vec::new();
        for (full_code, entries) in &self.map {
            // 只列更长的码（精确码本身走 lookup）；码均为 ASCII，字节长即字符长。
            if full_code.len() <= code.len() || !full_code.starts_with(code) {
                continue;
            }
            let suffix = full_code[code.len()..].to_string();
            for e in entries {
                let phrase = match parse(&e.text) {
                    Ok(p) => p,
                    Err(_) => continue,
                };
                // prefix 语义：除非显式 `{prefix: false}` 否则都列。
                // Literal/Template → 普通命中（command_src=None, nav_code=None）；
                // $SS/$AA → **组** nav（选中补全到码再展开成员，二级选择）；
                // $CC → **命令** nav（选中**直接执行**，不二级展开），display 经廉价上下文求值。
                match &phrase {
                    Phrase::Literal(_) | Phrase::Template(_) => {
                        let display = match evaluate(&phrase, &ctx, reg) {
                            Ok(ev) => ev.display,
                            Err(_) => continue,
                        };
                        out.push(PhraseHit::plain(display, e.weight));
                    }
                    Phrase::Array(ap) => {
                        if ap.modifiers.get_bool("prefix") == Some(false) {
                            continue;
                        }
                        out.push(PhraseHit::nav(
                            ap.name.clone(),
                            e.weight,
                            full_code.clone(),
                            suffix.clone(),
                        ));
                    }
                    Phrase::Command(cp) => {
                        if cp.modifiers.get_bool("prefix") == Some(false) {
                            continue;
                        }
                        let display = match evaluate(&phrase, &ctx, reg) {
                            Ok(ev) => ev.display,
                            Err(_) => continue,
                        };
                        out.push(PhraseHit::command_nav(
                            display,
                            e.weight,
                            e.text.clone(),
                            full_code.clone(),
                            suffix.clone(),
                        ));
                    }
                }
            }
        }
        // 权重降序，同权重按完整码字母序——导航候选顺序稳定可预测。
        out.sort_by(|a, b| {
            b.weight.cmp(&a.weight).then_with(|| {
                a.nav_code
                    .as_deref()
                    .unwrap_or("")
                    .cmp(b.nav_code.as_deref().unwrap_or(""))
            })
        });
        out
    }
}

/// 命令栏 display 侧的 [`wind_cmdbar::EvalContext`] 适配器（短语候选生成用）。
/// 提供 input/now/env + 上屏历史 last + 剪贴板 clip（供 `coll`/`coad` 等命令的 display
/// 标签显示 `last()`/`clip()`）；sel/app/title 与副作用服务侧留空（生成阶段不跑动作）。
struct PhraseCtx<'a> {
    input: String,
    now: DateTime<Local>,
    /// 上屏历史快照（index 0 = 最近）。
    last: &'a [String],
    /// 剪贴板读取回调（宿主注入；本 crate 不依赖平台 UI 层）。
    clip: &'a dyn Fn(i64) -> String,
}

impl wind_cmdbar::EvalContext for PhraseCtx<'_> {
    fn input(&self) -> String {
        self.input.clone()
    }
    fn last(&self, n: i64) -> String {
        if n < 1 {
            return String::new();
        }
        self.last.get((n - 1) as usize).cloned().unwrap_or_default()
    }
    fn clip(&self, n: i64) -> String {
        (self.clip)(n)
    }
    fn sel(&self) -> String {
        String::new()
    }
    fn app(&self) -> String {
        String::new()
    }
    fn title(&self) -> String {
        String::new()
    }
    fn env(&self, name: &str) -> String {
        std::env::var(name).unwrap_or_default()
    }
    fn now(&self) -> DateTime<Local> {
        self.now
    }
    fn services(&self) -> Option<&Services> {
        None
    }
}

/// 前缀导航列举用的**廉价**求值上下文：`clip`/`sel`/`app`/`title` 一律返回空，
/// 避免列举多条命令时各自读整个剪贴板/前台窗口等昂贵副作用（内存暴涨根因）。
/// `last` 仍取真实快照（仅 Vec 索引，廉价），`now`/`env` 廉价照常。命令真正执行时
/// 由 coordinator 用完整 CmdbarCtx（含真实剪贴板）求值。
struct NavCtx<'a> {
    input: String,
    now: DateTime<Local>,
    last: &'a [String],
}

impl wind_cmdbar::EvalContext for NavCtx<'_> {
    fn input(&self) -> String {
        self.input.clone()
    }
    fn last(&self, n: i64) -> String {
        if n < 1 {
            return String::new();
        }
        self.last.get((n - 1) as usize).cloned().unwrap_or_default()
    }
    fn clip(&self, _n: i64) -> String {
        String::new() // 列举阶段不读剪贴板
    }
    fn sel(&self) -> String {
        String::new()
    }
    fn app(&self) -> String {
        String::new()
    }
    fn title(&self) -> String {
        String::new()
    }
    fn env(&self, name: &str) -> String {
        std::env::var(name).unwrap_or_default()
    }
    fn now(&self) -> DateTime<Local> {
        self.now
    }
    fn services(&self) -> Option<&Services> {
        None
    }
}

/// 展开模板字符串；遇到不支持的变量返回 None（该短语项被跳过）。
/// 支持 `$name`、`${name}`，`$$` 转义为字面 `$`。
pub fn expand_template(text: &str, now: &DateTime<Local>) -> Option<String> {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < text.len() {
        if bytes[i] == b'$' {
            // $$ → 字面 $
            if i + 1 < text.len() && bytes[i + 1] == b'$' {
                out.push('$');
                i += 2;
                continue;
            }
            // ${name} 或 $name
            let (name, next) = if i + 1 < text.len() && bytes[i + 1] == b'{' {
                let rel = text[i + 2..].find('}')?;
                let close = i + 2 + rel;
                (&text[i + 2..close], close + 1)
            } else {
                let start = i + 1;
                let mut j = start;
                while j < text.len() && bytes[j].is_ascii_alphabetic() {
                    j += 1;
                }
                if j == start {
                    // 孤立的 $，原样输出
                    out.push('$');
                    i += 1;
                    continue;
                }
                (&text[start..j], j)
            };
            let val = expand_var(name, now)?;
            out.push_str(&val);
            i = next;
        } else {
            // 拷贝一个 UTF-8 字符
            let len = utf8_len(bytes[i]);
            out.push_str(&text[i..i + len]);
            i += len;
        }
    }
    Some(out)
}

fn utf8_len(lead: u8) -> usize {
    if lead < 0x80 {
        1
    } else if lead < 0xE0 {
        2
    } else if lead < 0xF0 {
        3
    } else {
        4
    }
}

const CN_DIGITS: [&str; 10] = ["〇", "一", "二", "三", "四", "五", "六", "七", "八", "九"];
const WEEKDAY_CN: [&str; 7] = ["日", "一", "二", "三", "四", "五", "六"];

/// 展开单个变量；不支持的返回 None。
fn expand_var(name: &str, now: &DateTime<Local>) -> Option<String> {
    Some(match name {
        "Y" => now.year().to_string(),
        "M" => now.month().to_string(),
        "MM" => format!("{:02}", now.month()),
        "D" => now.day().to_string(),
        "DD" => format!("{:02}", now.day()),
        "HH" => format!("{:02}", now.hour()),
        "mm" => format!("{:02}", now.minute()),
        "ss" => format!("{:02}", now.second()),
        "WC" => WEEKDAY_CN[now.weekday().num_days_from_sunday() as usize].to_string(),
        "YC" => year_chinese(now.year()),
        "MC" => small_int_chinese(now.month()),
        "DC" => small_int_chinese(now.day()),
        "ts" => now.timestamp().to_string(),
        "tsms" => now.timestamp_millis().to_string(),
        _ => return None,
    })
}

/// 年份逐位中文："2026" → "二〇二六"
fn year_chinese(year: i32) -> String {
    year.to_string()
        .bytes()
        .filter(|b| b.is_ascii_digit())
        .map(|b| CN_DIGITS[(b - b'0') as usize])
        .collect()
}

/// 1~99 的中文读法（月/日用）：6→六，12→十二，25→二十五，20→二十
fn small_int_chinese(n: u32) -> String {
    if n < 10 {
        return CN_DIGITS[n as usize].to_string();
    }
    if n < 20 {
        return format!(
            "十{}",
            if n.is_multiple_of(10) {
                ""
            } else {
                CN_DIGITS[(n % 10) as usize]
            }
        );
    }
    let tens = n / 10;
    let ones = n % 10;
    format!(
        "{}十{}",
        CN_DIGITS[tens as usize],
        if ones == 0 {
            ""
        } else {
            CN_DIGITS[ones as usize]
        }
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn fixed() -> DateTime<Local> {
        // 2026-06-14 09:05:07 周日
        Local.with_ymd_and_hms(2026, 6, 14, 9, 5, 7).unwrap()
    }

    /// 测试用空剪贴板读取回调。
    fn no_clip() -> impl Fn(i64) -> String {
        |_| String::new()
    }

    #[test]
    fn test_expand_date() {
        let now = fixed();
        assert_eq!(
            expand_template("$Y年$M月$D日", &now).unwrap(),
            "2026年6月14日"
        );
        assert_eq!(expand_template("$Y-$MM-$DD", &now).unwrap(), "2026-06-14");
    }

    #[test]
    fn test_expand_time_and_week() {
        let now = fixed();
        assert_eq!(expand_template("$HH:$mm:$ss", &now).unwrap(), "09:05:07");
        assert_eq!(expand_template("星期$WC", &now).unwrap(), "星期日");
    }

    #[test]
    fn test_expand_chinese() {
        let now = fixed();
        assert_eq!(
            expand_template("${YC}年${MC}月${DC}日", &now).unwrap(),
            "二〇二六年六月十四日"
        );
    }

    #[test]
    fn test_escape_and_unsupported() {
        let now = fixed();
        assert_eq!(expand_template("$$5", &now).unwrap(), "$5");
        // 含不支持的模板变量 → None（注意 $AA( 是 cmdbar 字符组 marker，走 cmdbar 路径，
        // 不经此简单模板展开；这里用一个永不存在的变量名验证未知变量降级）。
        assert!(expand_template("$QQ", &now).is_none());
    }

    #[test]
    fn test_cmdbar_dual_path() {
        // 命令栏语法（含 {expr}）走 cmdbar；其余走简单模板。
        let mut map: HashMap<String, Vec<PhraseEntry>> = HashMap::new();
        map.insert(
            "rq".into(),
            vec![PhraseEntry {
                text: r#"{date("YYYY-MM-DD")}"#.into(),
                weight: 1000,
                position: 0,
            }],
        );
        map.insert(
            "js".into(),
            vec![PhraseEntry {
                text: "{calc(\"1+2*3\")}".into(),
                weight: 900,
                position: 0,
            }],
        );
        map.insert(
            "old".into(),
            vec![PhraseEntry {
                text: "$Y-$MM-$DD".into(),
                weight: 800,
                position: 0,
            }],
        );
        // $CC 命令短语（有动作）：暂不显现（待动作执行通路），避免误上屏 display 标签。
        map.insert(
            "cmd".into(),
            vec![PhraseEntry {
                text: r#"$CC("切简繁", ime.toggle("s2t"))"#.into(),
                weight: 700,
                position: 0,
            }],
        );
        let layer = PhraseLayer { map };
        let now = fixed();
        assert_eq!(
            layer.lookup_at("rq", now, &[], &no_clip()),
            vec![PhraseHit::plain("2026-06-14".into(), 1000)]
        );
        assert_eq!(
            layer.lookup_at("js", now, &[], &no_clip()),
            vec![PhraseHit::plain("7".into(), 900)]
        );
        // 旧简单模板路径仍工作
        assert_eq!(
            layer.lookup_at("old", now, &[], &no_clip()),
            vec![PhraseHit::plain("2026-06-14".into(), 800)]
        );
        // 命令短语：display 为标签，携带命令源（选中时执行动作）。
        let cmd = layer.lookup_at("cmd", now, &[], &no_clip());
        assert_eq!(cmd.len(), 1);
        assert_eq!(cmd[0].text, "切简繁");
        assert_eq!(
            cmd[0].command_src.as_deref(),
            Some(r#"$CC("切简繁", ime.toggle("s2t"))"#)
        );
    }

    #[test]
    fn test_cmdbar_array_phrase_expands() {
        let mut map: HashMap<String, Vec<PhraseEntry>> = HashMap::new();
        map.insert(
            "fh".into(),
            vec![PhraseEntry {
                text: r#"$SS("符号", "（）", "【】")"#.into(),
                weight: 500,
                position: 0,
            }],
        );
        let layer = PhraseLayer { map };
        let got = layer.lookup_at("fh", fixed(), &[], &no_clip());
        assert_eq!(
            got,
            vec![
                PhraseHit::plain("（）".into(), 500),
                PhraseHit::plain("【】".into(), 500)
            ]
        );
    }

    #[test]
    fn test_cmdbar_aa_char_group_expands() {
        // $AA 字符组：逐字符炸开为多个上屏候选（镜像发货 system.phrases.toml 的符号组）。
        let mut map: HashMap<String, Vec<PhraseEntry>> = HashMap::new();
        map.insert(
            "zzsz".into(),
            vec![PhraseEntry {
                text: r#"$AA("数字", "①②③")"#.into(),
                weight: 500,
                position: 0,
            }],
        );
        let layer = PhraseLayer { map };
        let got = layer.lookup_at("zzsz", fixed(), &[], &no_clip());
        assert_eq!(
            got,
            vec![
                PhraseHit::plain("①".into(), 500),
                PhraseHit::plain("②".into(), 500),
                PhraseHit::plain("③".into(), 500),
            ]
        );
    }

    #[test]
    fn test_prefix_nav_lists_matching_groups() {
        // 敲 zz → 列出 zzbd/zzsz 字符组导航候选（组名 + 码后缀），不含无关码。
        let mut map: HashMap<String, Vec<PhraseEntry>> = HashMap::new();
        map.insert(
            "zzbd".into(),
            vec![PhraseEntry {
                text: r#"$AA("标点", "、。")"#.into(),
                weight: 500,
                position: 0,
            }],
        );
        map.insert(
            "zzsz".into(),
            vec![PhraseEntry {
                text: r#"$AA("数字", "①②")"#.into(),
                weight: 500,
                position: 0,
            }],
        );
        map.insert(
            "xx".into(),
            vec![PhraseEntry {
                text: "无关".into(),
                weight: 500,
                position: 0,
            }],
        );
        let layer = PhraseLayer { map };
        let got = layer.lookup_prefix_at("zz", fixed(), &[], 2);
        assert_eq!(got.len(), 2);
        // 同权重按完整码字母序 zzbd < zzsz。
        assert_eq!(got[0].text, "标点");
        assert_eq!(got[0].comment, "bd");
        assert_eq!(got[0].nav_code.as_deref(), Some("zzbd"));
        assert_eq!(got[1].text, "数字");
        assert_eq!(got[1].comment, "sz");
        assert_eq!(got[1].nav_code.as_deref(), Some("zzsz"));
    }

    #[test]
    fn test_prefix_nav_min_len_gate_and_exact_excluded() {
        let mut map: HashMap<String, Vec<PhraseEntry>> = HashMap::new();
        map.insert(
            "zzbd".into(),
            vec![PhraseEntry {
                text: r#"$AA("标点", "、。")"#.into(),
                weight: 500,
                position: 0,
            }],
        );
        let layer = PhraseLayer { map };
        // 前缀长度 < min_len → 不触发。
        assert!(layer.lookup_prefix_at("z", fixed(), &[], 2).is_empty());
        // 精确码本身（== 完整码）不作为导航候选返回（只列更长的码）。
        assert!(layer.lookup_prefix_at("zzbd", fixed(), &[], 2).is_empty());
        // 真前缀 → 1 个导航候选。
        assert_eq!(layer.lookup_prefix_at("zz", fixed(), &[], 2).len(), 1);
    }

    #[test]
    fn test_prefix_nav_command_default_on_prefix_false_off() {
        let mut map: HashMap<String, Vec<PhraseEntry>> = HashMap::new();
        map.insert(
            "cobd".into(),
            vec![PhraseEntry {
                text: r#"$CC("百度", open("https://baidu.com"))"#.into(),
                weight: 500,
                position: 0,
            }],
        );
        map.insert(
            "coex".into(),
            vec![PhraseEntry {
                text: r#"$CC("退出", type("x"), {prefix: false})"#.into(),
                weight: 500,
                position: 0,
            }],
        );
        let layer = PhraseLayer { map };
        let got = layer.lookup_prefix_at("co", fixed(), &[], 2);
        // $CC 默认列出（百度），显式 {prefix: false} 退出列举（退出）。
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].text, "百度");
        assert_eq!(got[0].comment, "bd");
        assert_eq!(got[0].nav_code.as_deref(), Some("cobd"));
        // 命令 nav：携命令源（选中**直接执行**，非二级展开）。
        assert!(got[0].command_src.is_some());
    }

    #[test]
    fn test_prefix_nav_command_display_skips_clipboard_read() {
        // 命令 display 含 {clip()}（如 coad）：列举用廉价上下文，clip() 返回空——
        // 不读整个剪贴板（内存安全），只显示静态部分。
        let mut map: HashMap<String, Vec<PhraseEntry>> = HashMap::new();
        map.insert(
            "coad".into(),
            vec![PhraseEntry {
                text: r#"$CC("剪贴板加词:{clip()}", type("x"))"#.into(),
                weight: 500,
                position: 0,
            }],
        );
        let layer = PhraseLayer { map };
        let got = layer.lookup_prefix_at("co", fixed(), &[], 2);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].text, "剪贴板加词:");
        assert!(got[0].command_src.is_some());
    }

    #[test]
    fn test_prefix_nav_includes_literal_template() {
        // 静态/旧模板短语（无 marker）现在参与前缀列举，以字面文本出现在候选中。
        let mut map: HashMap<String, Vec<PhraseEntry>> = HashMap::new();
        map.insert(
            "rq".into(),
            vec![PhraseEntry {
                text: "$Y-$MM-$DD".into(),
                weight: 500,
                position: 0,
            }],
        );
        let layer = PhraseLayer { map };
        let got = layer.lookup_prefix_at("r", fixed(), &[], 1);
        // $Y-$MM-$DD 不含 cmdbar 语法，parse 返回 Literal，直接以原文出现。
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].text, "$Y-$MM-$DD");
        assert!(got[0].command_src.is_none());
        assert!(got[0].nav_code.is_none());
    }

    #[test]
    fn test_cmdbar_command_display_uses_last() {
        // coll = $CC(last(), type(last()))：候选 display 应显示上一次上屏内容，并携命令源。
        let mut map: HashMap<String, Vec<PhraseEntry>> = HashMap::new();
        map.insert(
            "coll".into(),
            vec![PhraseEntry {
                text: "$CC(last(), type(last()))".into(),
                weight: 2000,
                position: 0,
            }],
        );
        let layer = PhraseLayer { map };
        let recent = vec!["上次内容".to_string()];
        let got = layer.lookup_at("coll", fixed(), &recent, &no_clip());
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].text, "上次内容"); // display = last() 显示上次上屏
        assert!(got[0].command_src.is_some()); // 仍是命令（选中执行 type(last())）
    }

    #[test]
    fn from_records_builds_lookup() {
        let layer = PhraseLayer::from_records([
            ("bj".to_string(), "北京".to_string(), 1000, 0),
            ("bj".to_string(), "北京市".to_string(), 500, 1),
        ]);
        let hits = layer.lookup("bj", &[], &|_| String::new());
        // 两条同码，按 weight 降序
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].text, "北京");
        assert_eq!(hits[1].text, "北京市");
    }

    #[test]
    fn test_small_int_chinese() {
        assert_eq!(small_int_chinese(6), "六");
        assert_eq!(small_int_chinese(10), "十");
        assert_eq!(small_int_chinese(12), "十二");
        assert_eq!(small_int_chinese(20), "二十");
        assert_eq!(small_int_chinese(25), "二十五");
        assert_eq!(small_int_chinese(31), "三十一");
    }

    #[test]
    fn lookup_prefix_lists_static_phrases() {
        // 静态字面短语（Literal）应出现在前缀结果中，command_src=None，nav_code=None。
        let mut map: HashMap<String, Vec<PhraseEntry>> = HashMap::new();
        // 静态短语：码 yx，文本 user@example.com
        map.insert(
            "yx".into(),
            vec![PhraseEntry {
                text: "user@example.com".into(),
                weight: 800,
                position: 0,
            }],
        );
        // $CC 命令短语：码 yxbd，保证命令短语仍正常工作
        map.insert(
            "yxbd".into(),
            vec![PhraseEntry {
                text: r#"$CC("百度", open("https://baidu.com"))"#.into(),
                weight: 500,
                position: 0,
            }],
        );
        let layer = PhraseLayer { map };
        let got = layer.lookup_prefix_at("y", fixed(), &[], 1);
        // 静态短语应出现
        let static_hit = got.iter().find(|h| h.text == "user@example.com");
        assert!(static_hit.is_some(), "静态短语应出现在前缀结果中");
        let sh = static_hit.unwrap();
        assert!(sh.command_src.is_none(), "静态短语 command_src 应为 None");
        assert!(sh.nav_code.is_none(), "静态短语 nav_code 应为 None");
        // $CC 命令短语仍正常工作
        let cmd_hit = got.iter().find(|h| h.text == "百度");
        assert!(cmd_hit.is_some(), "$CC 命令短语应仍出现在前缀结果中");
        assert!(
            cmd_hit.unwrap().command_src.is_some(),
            "$CC 命令短语 command_src 应非 None"
        );
    }
}
