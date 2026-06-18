# wind-cmdbar 全面实现计划

> 对照 Go `wind_input/internal/cmdbar/`（~3500 行非测试）完整移植。已核实全部源码。

## 设计决策（综合 Go 现状 + 优化）

- **保留字符串语义的 EvalFunc**：`Fn(&dyn EvalContext, &[String]) -> Result<String>`。Go 全字符串求值已验证可用；IME 命令栏最终都落到 display 文本，引入完整 Value 类型系统收益低、风险高（违背"boring/简单"）。仅在错误处理上升级。
- **错误升级为 thiserror 枚举** `CmdbarError`（ParseError{offset,msg} / UnknownFunc / Arity / NotPure / Service / Runtime）。修 Go 字符串拼接错误的可分类性。
- **Modifiers 用有序 `Vec<(String,ModValue)>`**（非 HashMap）：保留源顺序、debug 可复现，修 Go map 迭代随机化的吐槽点。
- **ObjectLit 解析期直接投影为 `ModValue`**（Str/Num/Bool/Sym），免去 Go 的二次 evalModifierLiteral。
- **统一转义**：lexer 字符串转义与模板/字面量转义共用一份 `decode_escape_byte` 白名单（`\n\r\t\\\"\'\{\}\(\)`，未知保留 `\X`）。

## 阶段

### Stage 1：词法 + 语法 + AST ✅判据：parse 单测全绿
- ast.rs：Phrase(Literal/Template/Command/Array) + Expr(StringLit/Number/Ident/Call/Object/Command) + StringPart + ModValue + Modifiers
- parser/lexer.rs：表达式级 token（ident/number/string含parts/括号/花括号/冒号/点/逗号）；字符串插值嵌套大括号匹配；ASCII ident 起始（避 UTF-8 死循环）
- parser/parser.rs：顶层分派（findTopLevelMarker/$CC/$CC1/$SS、template、literal）；递归下降表达式；options bag；$SS splitArrayArgs + 元素嵌入 $CC；marker 默认值合并

### Stage 2：注册表 + 上下文 + 求值 + 纯函数 ✅判据：eval 单测全绿
- registry.rs：FuncSpec{name,category,min/max_args,pure,deterministic,deprecated,alias_of,desc,example,eval} + Registry + default registry
- context.rs：EvalContext trait + MemoryContext + History 环形缓冲
- eval.rs：evaluate(phrase)->(display,actions)；assert_pure_display；evalExpr；type() 特例→ActionText；expand_array
- funcs：value(code/tail/last/clip/sel/app/title/date/time/now/env) + text(len/upper/lower/trim/sub/replace/regex/split/concat/reverse/t2s/s2t/pinyin/url/html/json/base64/default) + calc(calc/num) + help

### Stage 3：服务 + 动作函数 ✅判据：mock service 单测全绿
- services.rs：Clipboard/KeyInjector/UrlOpener/ProcessRunner/Dict/ImeController/Config/Search traits + Services 束
- action.rs：ActionKind + ResolvedAction（闭包延迟求值）
- funcs/action：open/proc.run/proc.shell/key.{tap,seq,hold,release,type}/clip.{copy,paste}/web.search/dict.add/ime.{toggle,schema,theme,theme_cycle}/setting.{open,web}/config.{get,set,toggle}；register_actions 覆盖 stub

### Stage 4：协调器集成 ✅判据：端到端 $CC 候选→执行
- EvalContext 适配器（coordinator 状态）+ Services 适配器（接现有宿主能力，平台缺失项 stub）
- phrase 解析为 cmdbar AST → 命令候选生成（$CC display + $SS 展开）
- 选中候选 → 执行 ResolvedAction（text 上屏 + effect 副作用）

## 状态
- Stage 1（词法/语法/AST）: ✅ Complete（lexer/parser/ast，含转义/插值/marker/options bag/$SS）
- Stage 2（注册表/上下文/求值/纯函数）: ✅ Complete（value/text/calc/help 全函数 + eval + 纯度检查）
- Stage 3（服务/动作函数）: ✅ Complete（8 service traits + open/proc/key/clip/web/dict/ime/setting/config + type 特例）
- Stage 4（协调器集成）: ✅ 框架完成（display + $CC 动作执行；宿主能力部分接通）
  - 高层 API `wind_cmdbar::phrase`：`is_cmdbar_grammar` / `evaluate_phrase` / `run_actions`（原生测试）。
  - coordinator `phrases.rs` 双路径：cmdbar 语法（`$CC`/`$SS`/`{expr}`）走 cmdbar 求值，否则走旧简单模板。
    `PhraseHit` 携带 `command_src`：template/literal-array 直接显现；`$CC` 命令携源 + is_command。
  - **$CC 动作执行**：候选选中（键盘 commit_selected / 鼠标 mouse_select）拦截 is_command →
    `spawn_command` **独立线程**执行（避开持 state 锁回调自锁方法的重入死锁，对齐 Go 约束）→
    `run_command_candidate` 解析+求值+跑动作；`type()` 文本经 push 管道上屏。
  - 服务装配（handle_cmdbar.rs）：**已接通**
    - ime.toggle(cn-en/fullshape/s2t/toolbar)、ime.schema、ime.theme_cycle、dict.add（Weak<Coordinator> 回调）；
    - proc.run/proc.shell（std::process::Command，跨平台）、open + web.search（系统外壳 cmd start / xdg-open）、
      clip.copy（wind_ui 写剪贴板，cfg(windows)）——这些无需 coordinator 回调，独立 struct。
    - **待补**：key.*（按键注入 SendInput）、clip.paste/get（读剪贴板）、config.*（配置路径访问 + 持久化）、
      setting.open/web（设置应用）、ime.toggle(preedit/candwin/layout)。缺失返回 ServiceUnavailable 优雅降级。
  - 验证局限：coordinator 无法原生测（wind-bridge 预存破）；cmdbar 侧动作派发已原生测（mock 控制器/服务）；
    coordinator 集成仅 windows 目标 check + debug exe 链接通过，**功能需上设备实测**。
- 验证：`cargo test -p wind-cmdbar` 51 passed；clippy 零警告；全工作区 windows check + debug exe 构建通过。
