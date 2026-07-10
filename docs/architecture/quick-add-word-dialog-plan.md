# 快捷加词界面 实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: 用 superpowers:subagent-driven-development（推荐）或 superpowers:executing-plans 逐任务实现。步骤用 checkbox（`- [ ]`）跟踪。

**Goal:** 让 `Ctrl+=→Ctrl+Enter` 与新增的 `Ctrl+Shift+=` 都能拉起一个独立的加词小窗（复用 wind-setting 词库加词对话框），支持 `--text/--code/--schema` 预填。

**Architecture:** core 侧新增 `open_add_word_dialog` 热键（默认 `ctrl+shift+equal`、仅中文），触发 `open_add_word_from_history`（不进加词模式、取最近输入预填），与已有 `Ctrl+Enter` 路径共用页面构造 → `open_settings("add-word …")`。wind-setting 侧解析 `--page add-word` 参数，走独立单例 app_id 起紧凑小窗，复用 `DictManagerState` 的加词对话框/出码/提交逻辑，关窗即退进程。

**Tech Stack:** Rust；core = wind-config + wind-coordinator（TSF/IPC 输入法核心）；wind-setting = windui（自研原生 GUI）；JSON-RPC over named pipe。

## Global Constraints

- **两个 git 仓库**：core 改动在 `D:\Develop\workspace\windinput\WindInput`（分支 `feat/quick-add-word`，已建）；设置改动在**独立仓** `D:\Develop\workspace\windinput\wind-setting`（需先 `git checkout -b feat/quick-add-word`）。两仓各自提交。
- 直开热键默认值（verbatim）：`open_add_word_dialog = "ctrl+shift+equal"`。
- 直开热键策略：**仅中文模式**（`HOTKEY_POLICY_CHINESE_ONLY`），与 `add_word` 一致。
- 加词默认权重（verbatim）：`1200`。
- 加词小窗单例 app_id（verbatim）：`format!("wind_setting_addword{}", crate::mode::pipe_suffix())`。
- 加词小窗**不**调用 `LoadedState::fetch()`；只做最小 RPC（`schema.list` 一次 + 出码/提交按需）。
- 暂不做「连续添加」；编码**不**自动重算（仅「出码」按钮手动触发）。
- 提交信息不带 Co-Authored-By 与 AI trailer（Constraint/Confidence 等），用中文 conventional commit。
- 每仓提交前跑 `cargo fmt`；core 侧注意 CRLF（本仓 `.rs` 用 LF，git 会提示替换，正常）。

---

## 仓库 A：core（WindInput，分支 feat/quick-add-word）

### Task 1: 配置字段 open_add_word_dialog

**Files:**
- Modify: `wind_input/crates/wind-config/src/config.rs`（Hotkeys/KeysConfig 结构体 895-932、默认值 934-977、Default impl 978-995）
- Modify: `data/config.toml`（`[hotkeys]` 段，约 160-163 行）

**Interfaces:**
- Produces: `KeysConfig.open_add_word_dialog: String`，默认函数 `default_open_add_word_dialog() -> String`。

- [ ] **Step 1: 加字段**（config.rs，紧跟 `add_word` 字段 906-907 之后）

```rust
    #[serde(default = "default_add_word")]
    pub add_word: String,
    #[serde(default = "default_open_add_word_dialog")]
    pub open_add_word_dialog: String,
```

- [ ] **Step 2: 加默认函数**（config.rs，紧跟 `default_add_word` 950-952 之后）

```rust
fn default_add_word() -> String {
    "ctrl+equal".to_string()
}
fn default_open_add_word_dialog() -> String {
    "ctrl+shift+equal".to_string()
}
```

- [ ] **Step 3: 补 Default impl**（config.rs，`Default for KeysConfig` 里 `add_word:` 那行之后，988 附近）

```rust
            add_word: default_add_word(),
            open_add_word_dialog: default_open_add_word_dialog(),
```

- [ ] **Step 4: 补 config.toml**（`add_word = "ctrl+equal"` 之后）

```toml
add_word = "ctrl+equal"
open_add_word_dialog = "ctrl+shift+equal"
```

- [ ] **Step 5: 编译验证**

Run: `cd wind_input && cargo build -p wind-config`
Expected: 编译通过（无 “missing field open_add_word_dialog” 错误，说明 Default 已补齐）。

- [ ] **Step 6: 提交**

```bash
cd D:/Develop/workspace/windinput/WindInput
git add wind_input/crates/wind-config/src/config.rs data/config.toml
git commit -m "feat(addword): 新增 open_add_word_dialog 热键配置（默认 ctrl+shift+equal）"
```

---

### Task 2: 热键编译注册（chinese-only 组）

**Files:**
- Modify: `wind_input/crates/wind-config/src/hotkey.rs`（CHINESE_ONLY key_down 组，约 119-131）

**Interfaces:**
- Consumes: `KeysConfig.open_add_word_dialog`（Task 1）。
- Produces: action 串 `"open_add_word_dialog"` 注册进 key_down（`HOTKEY_POLICY_CHINESE_ONLY`）。

- [ ] **Step 1: 写失败测试**（hotkey.rs 末尾 `#[cfg(test)] mod tests` 内；若无则新建）

```rust
    #[test]
    fn open_add_word_dialog_registered_chinese_only() {
        let mut cfg = Config::default();
        cfg.keys.open_add_word_dialog = "ctrl+shift+equal".to_string();
        let compiled = Compiler::new(cfg).compile();
        // action 串应出现在 key_down 组
        assert!(
            compiled.key_down.iter().any(|e| e.action == "open_add_word_dialog"),
            "open_add_word_dialog 应注册进 key_down"
        );
    }
```

> 注：`HotkeyEntry` 字段名以本文件实际为准（见 48-56 行 `pub struct HotkeyEntry`）；若 action 字段名不是 `action`，按实际改断言。

- [ ] **Step 2: 跑测试确认失败**

Run: `cd wind_input && cargo test -p wind-config open_add_word_dialog_registered -- --nocapture`
Expected: FAIL（action 未注册）。

- [ ] **Step 3: 注册进 chinese-only 组**（hotkey.rs，`("add_word", &h.add_word),` 那行之后，约 122）

```rust
        for (action, value) in [
            ("add_word", &h.add_word),
            ("open_add_word_dialog", &h.open_add_word_dialog),
        ] {
```

> 若该处循环结构与此不同（例如单元素数组），按同样模式追加一项 `("open_add_word_dialog", &h.open_add_word_dialog)`，保证走 `HOTKEY_POLICY_CHINESE_ONLY` 分支（与 add_word 同一循环体）。

- [ ] **Step 4: 跑测试确认通过**

Run: `cd wind_input && cargo test -p wind-config open_add_word_dialog_registered`
Expected: PASS。

- [ ] **Step 5: 提交**

```bash
git add wind_input/crates/wind-config/src/hotkey.rs
git commit -m "feat(addword): open_add_word_dialog 编译进 chinese-only 热键组"
```

---

### Task 3: handle_addword 抽公共构造 + open_add_word_from_history

**Files:**
- Modify: `wind_input/crates/wind-coordinator/src/handle_addword.rs`（`open_add_word_dialog` 285-314；测试 mod 412-619）

**Interfaces:**
- Produces:
  - `fn build_add_word_page(word: &str, code: &str, schema: &str) -> String`（纯函数）
  - `Coordinator::open_add_word_dialog_with(&self, word, code, schema) -> KeyAction`
  - `Coordinator::open_add_word_from_history(&self, state: &mut State) -> KeyAction`
- Consumes（既有）：`add_word_recent_chars`、`add_word_current_word`、`calc_add_word_code`、`reset_exclusive_modes`、`reset_pinyin_composition`、`notify_ui_hide`、`engine_mgr.active_schema_id`、常量 `ADD_WORD_MIN_LEN/DEFAULT_LEN/MAX_LEN`。

- [ ] **Step 1: 写失败测试**（handle_addword.rs 测试 mod 内）

```rust
    #[test]
    fn build_page_omits_empty_fields() {
        use super::build_add_word_page;
        assert_eq!(build_add_word_page("你好", "nihao", "pinyin"),
            "add-word --text=你好 --code=nihao --schema=pinyin");
        assert_eq!(build_add_word_page("", "", ""), "add-word");
        assert_eq!(build_add_word_page("你好", "", "wubi"),
            "add-word --text=你好 --schema=wubi");
    }

    #[test]
    fn from_history_does_not_enter_add_word_mode() {
        let c = coord("fromhist");
        push_commits(&c, &["你", "好"]);
        let mut st = c.state.lock().unwrap();
        c.open_add_word_from_history(&mut st);
        // 直开路径不得进入加词模式、不得改候选窗布局占位
        assert!(!st.add_word_active, "直开加词界面不应进入加词模式");
        assert!(st.add_word_chars.is_empty(), "不应填充加词字符池");
    }
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd wind_input && cargo test -p wind-coordinator build_page_omits_empty_fields from_history_does_not`
Expected: FAIL（`build_add_word_page` / `open_add_word_from_history` 未定义）。

- [ ] **Step 3: 加纯函数 + 公共构造**（handle_addword.rs，`impl Coordinator` 块外顶层加纯函数；`open_add_word_dialog` 前加公共构造）

```rust
/// 构造设置端加词界面参数串（对齐 Go openAddWordDialogWith）：非空字段才附带。
fn build_add_word_page(word: &str, code: &str, schema: &str) -> String {
    let mut page = String::from("add-word");
    if !word.is_empty() {
        page.push_str(" --text=");
        page.push_str(word);
    }
    if !code.is_empty() {
        page.push_str(" --code=");
        page.push_str(code);
    }
    if !schema.is_empty() {
        page.push_str(" --schema=");
        page.push_str(schema);
    }
    page
}
```

`impl Coordinator` 内新增公共构造：

```rust
    /// 拉起设置端加词界面（预填 word/code/schema）。两条入口共用。
    pub(crate) fn open_add_word_dialog_with(
        &self,
        word: &str,
        code: &str,
        schema: &str,
    ) -> KeyAction {
        self.open_settings(Some(&build_add_word_page(word, code, schema)));
        KeyAction::ClearComposition
    }
```

- [ ] **Step 4: 重构 open_add_word_dialog 走公共构造**（替换 285-314 现有实现体末段）

```rust
    /// Ctrl+Enter：从加词模式转到设置端加词界面，预填当前 词/编码/方案。
    pub(crate) fn open_add_word_dialog(&self, state: &mut State) -> KeyAction {
        let (word, code) = if state.add_word_len >= ADD_WORD_MIN_LEN
            && state.add_word_chars.len() >= ADD_WORD_MIN_LEN
        {
            (self.add_word_current_word(state), state.add_word_code.clone())
        } else {
            (String::new(), String::new())
        };
        let schema = self.engine_mgr.active_schema_id();
        self.exit_add_word_mode(state);
        self.open_add_word_dialog_with(&word, &code, &schema)
    }
```

- [ ] **Step 5: 新增 open_add_word_from_history**（紧随 open_add_word_dialog 之后）

```rust
    /// Ctrl+Shift+=：不进加词模式，直接取最近输入预填并拉起加词界面
    /// （对齐 Go openAddWordDialogFromHistory）。
    pub(crate) fn open_add_word_from_history(&self, state: &mut State) -> KeyAction {
        // 清理未上屏输入/候选/独占残留，避免残留 composition
        self.reset_exclusive_modes(state);
        self.reset_pinyin_composition(state);
        self.notify_ui_hide();

        let chars = self.add_word_recent_chars(ADD_WORD_MAX_LEN);
        let (word, code) = if chars.len() >= ADD_WORD_MIN_LEN {
            let len = ADD_WORD_DEFAULT_LEN.min(chars.len());
            let word: String = chars[chars.len() - len..].iter().collect();
            let code = self.calc_add_word_code(&word);
            (word, code)
        } else {
            (String::new(), String::new())
        };
        let schema = self.engine_mgr.active_schema_id();
        self.open_add_word_dialog_with(&word, &code, &schema)
    }
```

- [ ] **Step 6: 跑测试确认通过**

Run: `cd wind_input && cargo test -p wind-coordinator build_page_omits_empty_fields from_history_does_not`
Expected: PASS。

- [ ] **Step 7: 回归 + 提交**

```bash
cd wind_input && cargo test -p wind-coordinator handle_addword 2>&1 | tail -5
cd D:/Develop/workspace/windinput/WindInput
git add wind_input/crates/wind-coordinator/src/handle_addword.rs
git commit -m "feat(addword): 抽 build_add_word_page 公共构造 + 新增 open_add_word_from_history"
```

---

### Task 4: 热键分派

**Files:**
- Modify: `wind_input/crates/wind-coordinator/src/coordinator.rs`（key_down 匹配处，`if action == "add_word"` 分支 3239-3246）

**Interfaces:**
- Consumes: `open_add_word_from_history`（Task 3）、action 串 `"open_add_word_dialog"`（Task 2）。

- [ ] **Step 1: 加分派分支**（coordinator.rs，把现有 `if action == "add_word" {…}` 扩成含 else-if）

```rust
            if action == "add_word" {
                let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
                if state.chinese_mode {
                    return self.enter_add_word_mode(&mut state);
                }
            } else if action == "open_add_word_dialog" {
                let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
                if state.chinese_mode {
                    return self.open_add_word_from_history(&mut state);
                }
            } else if self.dispatch_hotkey(&action) {
                return KeyAction::StatusUpdate(self.build_status());
            }
```

- [ ] **Step 2: 编译**

Run: `cd wind_input && cargo build -p wind-coordinator`
Expected: 编译通过。

- [ ] **Step 3: 全量测试回归**

Run: `cd wind_input && cargo test 2>&1 | tail -15`
Expected: 与改动前一致（既有失败台账不新增；本功能相关测试 PASS）。

- [ ] **Step 4: 提交**

```bash
git add wind_input/crates/wind-coordinator/src/coordinator.rs
git commit -m "feat(addword): Ctrl+Shift+= 分派到 open_add_word_from_history（仅中文）"
```

---

## 仓库 B：wind-setting（独立仓，需先建分支 feat/quick-add-word）

> 起点：`cd D:/Develop/workspace/windinput/wind-setting && git checkout -b feat/quick-add-word`

### Task 5: cli.rs 解析 --page add-word 参数

**Files:**
- Modify: `src/cli.rs`（新增 `AddWordParams` + `parse_add_word`；测试 mod 44-80）

**Interfaces:**
- Produces: `pub struct AddWordParams { pub text: String, pub code: String, pub schema: String }`；`pub fn parse_add_word(argv: &[String]) -> Option<AddWordParams>`。

- [ ] **Step 1: 写失败测试**（cli.rs 测试 mod 内）

```rust
    #[test]
    fn parse_add_word_forms() {
        // --page add-word + 各参数（= 与空格两式）
        let p = parse_add_word(&a(&["exe", "--page", "add-word", "--text=你好", "--code=nihao", "--schema=pinyin"])).unwrap();
        assert_eq!(p.text, "你好");
        assert_eq!(p.code, "nihao");
        assert_eq!(p.schema, "pinyin");
        // 空格式
        let p2 = parse_add_word(&a(&["exe", "--page=add-word", "--text", "好", "--schema", "wubi"])).unwrap();
        assert_eq!(p2.text, "好");
        assert_eq!(p2.code, "");
        assert_eq!(p2.schema, "wubi");
        // 仅 --page add-word 无预填
        let p3 = parse_add_word(&a(&["exe", "--page", "add-word"])).unwrap();
        assert_eq!(p3.text, "");
        // 非 add-word → None
        assert!(parse_add_word(&a(&["exe", "--page", "input"])).is_none());
    }
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd wind-setting && cargo test parse_add_word_forms`
Expected: FAIL（未定义）。

- [ ] **Step 3: 实现**（cli.rs 顶层，`parse_protocol` 之后）

```rust
/// 加词界面参数（`--page add-word` 触发）。
#[derive(Debug, Default, Clone, PartialEq)]
pub struct AddWordParams {
    pub text: String,
    pub code: String,
    pub schema: String,
}

/// 检测 argv 是否请求加词界面并解析 `--text/--code/--schema`（`=` 与空格两式）。
/// 触发条件：含 `--page add-word` / `--page=add-word` / `--add-word`。
pub fn parse_add_word(argv: &[String]) -> Option<AddWordParams> {
    let mut requested = false;
    let mut it = argv.iter().peekable();
    while let Some(a) = it.peek() {
        let a = a.as_str();
        if a == "--add-word" || a == "--page=add-word" {
            requested = true;
        } else if a == "--page" {
            // 向前看下一个是否 add-word
            let mut clone = it.clone();
            clone.next();
            if clone.peek().map(|s| s.as_str()) == Some("add-word") {
                requested = true;
            }
        }
        it.next();
    }
    if !requested {
        return None;
    }
    let mut p = AddWordParams::default();
    let mut it = argv.iter();
    while let Some(a) = it.next() {
        if let Some(v) = a.strip_prefix("--text=") {
            p.text = v.to_string();
        } else if a == "--text" {
            if let Some(v) = it.next() {
                p.text = v.to_string();
            }
        } else if let Some(v) = a.strip_prefix("--code=") {
            p.code = v.to_string();
        } else if a == "--code" {
            if let Some(v) = it.next() {
                p.code = v.to_string();
            }
        } else if let Some(v) = a.strip_prefix("--schema=") {
            p.schema = v.to_string();
        } else if a == "--schema" {
            if let Some(v) = it.next() {
                p.schema = v.to_string();
            }
        }
    }
    Some(p)
}
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cd wind-setting && cargo test parse_add_word_forms`
Expected: PASS。

- [ ] **Step 5: 提交**

```bash
cd D:/Develop/workspace/windinput/wind-setting
git add src/cli.rs
git commit -m "feat(addword): cli 解析 --page add-word 及 --text/--code/--schema"
```

---

### Task 6: DictManagerState::open_add_word_prefilled

**Files:**
- Modify: `src/pages/dict/state.rs`（新增方法；测试 mod 1055+）

**Interfaces:**
- Produces: `DictManagerState::open_add_word_prefilled(&self, text: &str, code: &str, schema: &str)`。
- Consumes（既有）：`folded_domains`、`tabs_for_domain`、`domain`/`sub_tab` 信号、`edit_code/edit_text/edit_weight/edit_title/edit_l_code/edit_l_text/edit_visible`、`set_schemas_direct`（测试用，见 1066）、`WordCategory::UserDict`、`LogicalTab`。

- [ ] **Step 1: 写失败测试**（state.rs 测试 mod；参照既有 `schema`/`schema_s` 辅助 1058-1063 与 `set_schemas_direct` 1066）

```rust
    #[test]
    fn prefill_selects_userdict_domain_for_pinyin() {
        let st = DictManagerState::new();
        st.set_schemas_direct(&[
            schema("wubi", "五笔", "codetable"),
            schema_s("quanpin", "全拼", "pinyin", "full"),
        ]);
        st.open_add_word_prefilled("你好", "nihao", "quanpin");
        // 命中拼音域（domain>=1），子标签落在用户词库
        assert_eq!(st.current_category(), super::spec::WordCategory::UserDict);
        assert_eq!(st.edit_text.get(), "你好");
        assert_eq!(st.edit_code.get(), "nihao");
        assert_eq!(st.edit_weight.get(), "1200");
        assert!(st.edit_visible.get());
        assert_eq!(st.current_schema_id().as_deref(), Some("quanpin"));
    }

    #[test]
    fn prefill_matches_pinyin_alias() {
        let st = DictManagerState::new();
        st.set_schemas_direct(&[schema_s("quanpin", "全拼", "pinyin", "full")]);
        // core 传来的双拼 id 归一到拼音域代表
        st.open_add_word_prefilled("好", "hao", "shuangpin_ziranma");
        assert_eq!(st.current_category(), super::spec::WordCategory::UserDict);
    }
```

> 若 `set_schemas_direct` 非公开，测试内改用现有测试可见的注入辅助（见 1066 附近实现），保持"不触发 rpc"。

- [ ] **Step 2: 跑测试确认失败**

Run: `cd wind-setting && cargo test prefill_selects_userdict prefill_matches_pinyin`
Expected: FAIL（未定义）。

- [ ] **Step 3: 实现**（state.rs，`open_add` 714 之后）

```rust
    /// 预填加词对话框（供加词小窗/快捷键使用）：定位 schema 所属域 → 用户词库子标签，
    /// 预填 词/编码/权重(1200)，`editing_orig=None`（新增），打开对话框。
    /// schema 命中规则：先精确匹配 rep_schema_id；未命中且为拼音变体时落拼音域；
    /// 命中域若无「用户词库」子标签（如混输仅候选调整），回退到首个含用户词库的域。
    pub fn open_add_word_prefilled(&self, text: &str, code: &str, schema: &str) {
        use super::spec::WordCategory;
        let domains = self.folded_domains();

        // 1) 找目标域索引（0-based over folded_domains；对外 domain=idx+1）
        let mut target: Option<usize> = domains.iter().position(|d| d.rep_schema_id == schema);
        // 拼音变体（如双拼 id）→ 落拼音域
        if target.is_none() && !schema.is_empty() {
            target = domains.iter().position(|d| d.kind == "pinyin");
        }

        // 2) 该域是否含用户词库子标签；无则回退首个含用户词库的域
        let has_userdict = |idx: usize| -> Option<usize> {
            // 临时把 domain 指向 idx 以复用 tabs_for_domain（读取用信号，读后恢复）
            let prev = self.domain.get();
            self.domain.set(idx + 1);
            let pos = self
                .tabs_for_domain()
                .iter()
                .position(|t| t.category() == WordCategory::UserDict);
            self.domain.set(prev);
            pos
        };

        let (domain_idx, sub) = match target.and_then(|i| has_userdict(i).map(|s| (i, s))) {
            Some(v) => v,
            None => {
                // 回退：首个含用户词库的域
                let mut fallback = None;
                for i in 0..domains.len() {
                    if let Some(s) = has_userdict(i) {
                        fallback = Some((i, s));
                        break;
                    }
                }
                match fallback {
                    Some(v) => v,
                    None => (0, 0), // 无任何方案域：退化，仍打开对话框（提交会因方案缺失报错）
                }
            }
        };

        self.domain.set(domain_idx + 1);
        self.sub_tab.set(sub);

        *self.editing_orig.borrow_mut() = None;
        self.edit_code.set(code.to_string());
        self.edit_text.set(text.to_string());
        self.edit_weight.set("1200".to_string());
        self.edit_title.set("添加用户词库".to_string());
        self.edit_l_code.set("编码".to_string());
        self.edit_l_text.set("词条".to_string());
        self.edit_visible.set(true);
    }
```

> 注：`editing_orig` 为私有字段，本方法在同 `impl` 内可访问。`LogicalTab::category()` 已有（`current_tab().category()` 用过）。

- [ ] **Step 4: 跑测试确认通过**

Run: `cd wind-setting && cargo test prefill_selects_userdict prefill_matches_pinyin`
Expected: PASS。

- [ ] **Step 5: 提交**

```bash
git add src/pages/dict/state.rs
git commit -m "feat(addword): DictManagerState::open_add_word_prefilled 复用词库加词逻辑"
```

---

### Task 7: 关窗哨兵 widget（edit_visible→false 即退窗）

**Files:**
- Modify: `src/pages/dict/reactive.rs`（仿 `ToastSink`/`toast_sink` 新增 `close_on_hidden`）

**Interfaces:**
- Produces: `pub fn close_on_hidden(visible: Signal<bool>) -> Element`——当 `visible` 由 true 翻 false 时，在 `on_update` 里 `ctx.request_close()`。

- [ ] **Step 1: 实现**（reactive.rs，`toast_sink` 之后）

```rust
/// 关窗哨兵：监听 `visible` 版本变化，当其变为 false 时请求关闭窗口。
/// 用于加词小窗——复用的对话框取消/确定成功都会把 edit_visible 置 false。
struct CloseOnHidden {
    visible: Signal<bool>,
    last: u64,
}

impl Widget for CloseOnHidden {
    fn on_update(&mut self, ctx: &mut EventCtx) {
        let cur = self.visible.version();
        if cur != self.last {
            self.last = cur;
            if !self.visible.get() {
                ctx.request_close();
            }
        }
    }
}

/// 构造不可见的关窗哨兵节点。
pub fn close_on_hidden(visible: Signal<bool>) -> Element {
    let last = visible.version();
    let w = CloseOnHidden { visible, last };
    Element::leaf().widget(w).reactive().width(0).height(0)
}
```

- [ ] **Step 2: 编译验证**

Run: `cd wind-setting && cargo build`
Expected: 编译通过（`CloseOnHidden` 引用的 `Widget`/`EventCtx` 已在文件顶部 `use windui::core::{EventCtx, Widget};` 导入）。

- [ ] **Step 3: 提交**

```bash
git add src/pages/dict/reactive.rs
git commit -m "feat(addword): 新增 close_on_hidden 关窗哨兵 widget"
```

---

### Task 8: 加词小窗（main.rs 分支 + 独立单例）

**Files:**
- Modify: `src/state.rs`（`load_schemas` 695 提为 `pub(crate)`）
- Create: `src/add_word_window.rs`（紧凑窗构建）
- Modify: `src/main.rs`（早分支 + 声明 mod）

**Interfaces:**
- Consumes: `cli::parse_add_word`/`AddWordParams`（Task 5）、`DictManagerState::open_add_word_prefilled`（Task 6）、`pages::dict::reactive::close_on_hidden`（Task 7）、`pages::dict::dialogs::build_dict_dialogs`、`pages::dict::reactive::toast_sink`、`state::load_schemas`、`DictManagerState::set_schemas`、`mode::pipe_suffix`、`theme_def::theme_for`。
- Produces: `add_word_window::run(params: cli::AddWordParams)`。

- [ ] **Step 1: 把 load_schemas 提为 pub(crate)**（state.rs:695-696）

```rust
/// 加载方案列表（schema.list）。
pub(crate) fn load_schemas() -> Vec<SchemaInfo> {
```

- [ ] **Step 2: 新建 add_word_window.rs**

```rust
//! 快捷加词小窗 — 独立单例、紧凑窗、复用词库加词对话框，关窗即退进程。
//!
//! 与主设置窗用不同 app_id（`wind_setting_addword{suffix}`），互不劫持；自身单例，
//! 连按只复用同一小窗重新预填。只做最小 RPC（schema.list 一次 + 出码/提交按需），
//! 不走整页的 LoadedState::fetch。

use std::rc::Rc;

use windui::prelude::*;

use crate::cli::AddWordParams;
use crate::pages::dict::dialogs::build_dict_dialogs;
use crate::pages::dict::reactive::{close_on_hidden, toast_sink};
use crate::pages::dict::state::DictManagerState;
use crate::state::load_schemas;
use crate::theme_def::theme_for;

/// 用参数预填一个 DictManagerState（加载方案 + 定位 + 预填 + 打开对话框）。
fn prepare(params: &AddWordParams) -> DictManagerState {
    let mgr = DictManagerState::new();
    mgr.set_schemas(&load_schemas());
    mgr.open_add_word_prefilled(&params.text, &params.code, &params.schema);
    mgr
}

/// 运行加词小窗；此函数不返回（App::run 进入消息循环，关窗即退进程）。
pub fn run(params: AddWordParams) {
    let dark = std::env::args().any(|a| a == "--dark");
    let mgr = Rc::new(prepare(&params));

    // 内容 = 复用的词库对话框集合（含加/改词对话框，edit_visible 已置 true）
    //        + toast 反馈汇 + 关窗哨兵（edit_visible→false 即 request_close）。
    let root = Element::stack()
        .fill()
        .bg_role(Role::Bg)
        .child(build_dict_dialogs(&mgr))
        .child(toast_sink(mgr.feedback))
        .child(close_on_hidden(mgr.edit_visible));

    let app_id = format!("wind_setting_addword{}", crate::mode::pipe_suffix());
    // 二次启动：重新解析参数并预填，复用同一小窗（不新起进程）。
    let si_mgr = mgr.clone();
    App::new("添加用户词库", 460, 300)
        .single_instance(app_id, move |argv| {
            if let Some(p) = crate::cli::parse_add_word(&argv) {
                si_mgr.set_schemas(&load_schemas());
                si_mgr.open_add_word_prefilled(&p.text, &p.code, &p.schema);
            }
        })
        .frameless()
        .centered()
        .theme(theme_for(dark))
        .bg(theme_for(dark).palette.bg)
        .content(root)
        .run();
}
```

> 注：`DictManagerState` 是 `Clone`（Signal 为 Copy），闭包捕获用 `Rc` 或直接 clone 均可；此处用 `Rc` 以与二次启动闭包共享同一实例。若 windui `App` 无 `.theme(..)` 后再 `.bg(..)` 的顺序要求，参照 `main.rs` 现有链式顺序调整。

- [ ] **Step 3: main.rs 早分支 + 声明 mod**（main.rs）

`mod` 声明处（8-30 行区）加：

```rust
mod add_word_window;
```

`fn main()` 里，`mode::init(&args)` + `logger::init()` 之后、构建常规 App 之前，插入：

```rust
    // 快捷加词小窗：独立单例、紧凑窗，复用词库加词对话框；不落入常规设置界面路径。
    if let Some(params) = cli::parse_add_word(&args) {
        add_word_window::run(params);
        return;
    }
```

> 放在 `protocol::self_heal_registration()` 之后即可（保持协议注册自愈）；务必在 `App::new("清风输入法设置", …)` 常规分支**之前** `return`。

- [ ] **Step 4: 编译**

Run: `cd wind-setting && cargo build`
Expected: 编译通过。

- [ ] **Step 5: 出图冒烟（无需 core）**

Run: `cd wind-setting && $env:WIND_RPC_MOCK=1; cargo run -- --page add-word --text=你好 --code=nihao --schema=pinyin --screenshot "$env:TEMP/addword.png"`
Expected: 生成截图，显示「添加用户词库」对话框、编码框 `nihao`、词条 `你好`、权重 `1200`、「出码/取消/确定」按钮。
（`--screenshot` 由 `screenshot_from_args` 支持——注意本分支未挂 `.screenshot_from_args()`；若需出图，Step 2 的 `App` 链在 `.content` 前补 `.screenshot_from_args()`。）

- [ ] **Step 6: 提交**

```bash
git add src/state.rs src/add_word_window.rs src/main.rs
git commit -m "feat(addword): 独立加词小窗（独立单例 + 复用词库对话框 + 关窗即退）"
```

---

### Task 9: 端到端联调与真机验证清单

**Files:** 无代码改动（除按需修补）。

- [ ] **Step 1: 两仓编译**

Run:
```
cd D:/Develop/workspace/windinput/WindInput/wind_input && cargo build
cd D:/Develop/workspace/windinput/wind-setting && cargo build
```
Expected: 均通过。

- [ ] **Step 2: 构建并部署到真机运行环境**（按本仓既有 build/部署脚本；参见 `scripts/`）。

- [ ] **Step 3: 真机手测清单**（逐项确认）
  - `Ctrl+=` 进加词模式候选窗；`Ctrl+Enter` 拉起加词小窗，词/编码/方案预填正确。
  - `Ctrl+Shift+=` 直接拉起加词小窗（不经候选窗、不改布局），预填最近输入。
  - 设置窗已开时按 `Ctrl+Shift+=`：**不**劫持设置窗切页，独立弹小窗。
  - 连按 `Ctrl+Shift+=`：只复用同一小窗、重新预填，不堆多个进程/窗口。
  - 加词小窗内：改词条 → 点「出码」重算编码；点「确定」写库；小窗关闭退出。
  - 点「取消」小窗关闭退出。
  - 英文模式下 `Ctrl+Shift+=` 不触发。
  - 加词后到设置词库页确认用户词已入库（权重 1200）。

- [ ] **Step 4: 记录结果**：把真机结果补进 `docs/architecture/quick-add-word-dialog-design.md` 的验证清单（或提交信息），未过项开 issue。

---

## Self-Review

**Spec coverage：**
- 设计 Part A-1（配置字段）→ Task 1；A-2（编译注册）→ Task 2；A-3（公共构造 + from_history）→ Task 3；A-4（分派）→ Task 4。
- Part B-1（cli 解析）→ Task 5；B-3（复用词库逻辑）→ Task 6 + Task 8（prepare/复用对话框）；B-2（独立小窗）→ Task 8；B-4（单例隔离）→ Task 8（独立 app_id）；B-5（最小 IPC）→ Task 8（load_schemas 复用、跳过 LoadedState::fetch）；关窗即退 → Task 7 + Task 8。
- 测试策略：core → Task 2/3 单测；wind-setting → Task 5/6 单测 + Task 8 出图冒烟 + Task 9 真机。
- 覆盖完整，无遗漏需求。

**Placeholder scan：** 无 TBD/TODO；每个代码步骤含完整代码。仅 Task 2/6 有"若字段名/辅助与实际不符则按实际调整"的实现期提示（因目标文件个别私有符号名以运行期为准），非占位。

**Type consistency：**
- `build_add_word_page(word, code, schema) -> String`（Task 3）→ Task 3 内 `open_add_word_dialog_with` 调用一致。
- `AddWordParams{text,code,schema}`（Task 5）→ Task 8 `parse_add_word`/`open_add_word_prefilled(text,code,schema)` 参数序一致。
- `close_on_hidden(Signal<bool>)`（Task 7）→ Task 8 传 `mgr.edit_visible` 一致。
- `open_add_word_prefilled(&self, text, code, schema)`（Task 6）→ Task 8 `prepare` 调用一致。
- action 串 `"open_add_word_dialog"`（Task 2）↔ 分派判断（Task 4）一致；配置字段 `open_add_word_dialog`（Task 1）↔ 编译读取（Task 2）一致。

**已知风险（实现期留意，非占位）：**
- 混输方案：`--schema=` 命中的域若仅有「候选调整」无「用户词库」，`open_add_word_prefilled` 回退首个含用户词库的域（Task 6 已含回退）；真机需验证混输下预填落点是否符合预期（Task 9 补测）。
- Task 8 出图依赖 `.screenshot_from_args()`——如需 Step 5 冒烟，按注记在 App 链补挂。
