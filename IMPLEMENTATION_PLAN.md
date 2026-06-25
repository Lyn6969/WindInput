# 配置项接通修复计划（方案/输入/按键/外观 四 tab 巡检）

策略：低风险优先，逐批验证后分步提交。每批：核实代码(对照 Go) → 改 → 编译+单测 → 部署 debug → 用户实测 → 提交。

本轮纳入大功能：快捷加词+加词界面、候选无效按键 overflow+以词定字。
延后：切换到本输入法热键(注册表)、扩展词库独立 wdb、无界面截图。

## 批次（用户编号映射）

- [x] **批1 外观默认值 (#13)**：后端默认 font_size 18 / follow_theme true / max_chars 16 / layout horizontal / theme default+system；前端对齐 Go(delay 100/pinyin tooltip 开/flip 关)。已提交 1d7f579+4ffbf21。
- [~] **批2 码表接通 (#2)**：标点顶码 `punct_commit` 已做(关闭时**吞键无效**,非透传)；精确匹配 `single_code_input` 推迟(要改 manager.rs)。
- [x] **批3 标点/模式切换**：#4 标点随中英、#12 切英文清 composition。已提交 1db9841。
- [~] **批4 回车/空格/强制竖排**：#5 `enter_behavior`/`space_on_empty_behavior` 已做；#8 force_vertical 已做(后端补字段+进退切换/恢复布局,decimal_places 已生效)。#6 `numpad_behavior` 推后(主流程未处理,需按键入口转换 key_code)。
- [x] **#1 z 键重复**：已做并验证(062a4d4)。
- [x] **#6 numpad 返工 + #7 数字键 overflow**：后端新增 `OverflowConfig{number_key/select_key/select_char_key}`(默认 ignore)。抽 `handle_number_key_select`/`handle_overflow_number_key` 共享入口(对齐 Go handleNumberKey)：命中页内→选词；越界→按 `input.overflow.number_key`(ignore 吞键/commit 顶高亮/commit_and_input 顶高亮+数字)。主键盘 VK_1..9 改用该入口(默认 ignore = 越界吞键，对齐用户「主数字区无效」预期)。numpad follow_main 数字键完整走主键盘逻辑(空缓冲透传/否则选词/越界 overflow，`0`→第10候选)，运算符/小数点顶字后输出；direct 不变。5 个单测(numpad direct/follow_main/passthrough + overflow ignore/commit_and_input)全绿。**待提交+实测**。
  - 注:**select_key / select_char_key(以词定字)overflow 仍待做**——这两个键(`;`/`'`)同时是快捷输入/临拼触发键，越界回落优先级需端口 Go decideBufferedTrigger(候选不足→回落模式激活而非 overflow)，且以词定字本身可能尚未实现，单列下批。
- [~] **批6 临时英文/拼音**：
  - [x] #10 临英 3 项：`shift_behavior`(direct_commit 直接上屏大写不进模式)、`space_as_input`(空格入缓冲仅回车上屏)、`allow_symbols`(可见符号入缓冲累积如 C++)。均对齐 Go。**待提交+实测**。
  - [x] #10 `trigger_keys`(符号键进临英 + 前缀显示)：State 加 `temp_english_prefix`；preedit 拼前缀；空缓冲回车上屏触发键字符；`is_temp_english_trigger` + lifecycle 第7层新分支（优先级：Shift+字母 > 符号触发键 > TempPinyin）。
  - [x] #10 临英 3 项已提交 81b15e7。
  - [ ] #9 临拼 **z 混合仲裁**：曾实现(z_trigger_configured/code_has_prefix/enter_temp_pinyin_from_z + A-Z 臂渐进回退,对齐 Go decideEngineDefaultZFallback)，**用户实测仍不正常已回退**(commit 81b15e7 注释)。待后续与用户**完整重新设计** z 的处理逻辑(zzbd 码表前缀 / z键重复首候选 / 「拼」提示 / 渐进回退临拼的交互)再做。
    - 经验:本地 build_debug/data 在项目根，测试找 wind_input/build_debug/data(差一级)平时 skip；临时符号链接 wind_input/build_debug→../build_debug 可让集成测试真实跑(注意该路径未 gitignore，用后即删)。
    - Go 参考:`handle_temp_pinyin.go getTempPinyinTriggerKey`(首z仲裁)、`decideEngineDefaultZFallback`(续打回退)、`enterTempPinyinFromZBuffer`(residual=buffer[1:]+key)。
- [x] **批7 网址输入 (#11)**：已提交 c71730f。徽标(网址输入/网址)+候选窗保留显示(notify_ui_update+preedit,退出补 notify_ui_hide)+无候选时透明等高占位行(网址/临拼/临英进入与正常候选窗等高)。集成测试 `test_url_mode_enter_and_commit` 真实数据跑通,Windows 实测通过。
  - 注:Go 的 accent_color 高亮(SetModeAccentColor)Rust 暂未套用 url_input.accent_color——徽标已是提示,accent 留作 UI 增强后续。
- [x] **批8 热键分发**：#12 Ctrl+数字/Ctrl+Shift+数字 置顶/删除(含 0=第10)、#12 打开设置默认键+dispatch。已提交 0bffd7b。
- [x] **批9 overflow+以词定字 (#7+#12)**：
  - [x] **select_key 越界 overflow**（次/三选键 `;`/`'`）：`handle_overflow_select_key`，依 `input.overflow.select_key` 三策(ignore/commit/commit_and_input)，镜像 number_key，对齐 Go handleOverflowSelectKey；缓冲分发链把 overflow 延后到模式触发之后(选词<进模式<overflow)。已提交 a79e6bd。
  - [x] **修「有候选时按 ; 进错模式打不了拼音」**：缓冲分发漏 mix 触发(只查残留纯 quick_input)，新增 `commit_and_enter_mix_mode`(顶字+进融合)，有无候选都进同一融合「快捷」。已提交 a79e6bd。
  - [x] **残留清理：移除独立 QuickInput 模式**（ModeKind::QuickInput/handle_quick.rs/State 字段/config.trigger_keys）—— 快捷输入统一为 mix 融合。已提交 f54d4c6。
  - [x] **以词定字 (select_char_keys)**：成对标点键(comma_period/minus_equal/brackets)逐字选当前高亮词。新增 `select_char_vks`(wind-config/hotkey.rs)、`select_char_index`/`handle_select_char`(None=空/词长不足/组候选)/`handle_select_char_with_overflow`(三策同构 select_key)；分发拦截置于 `apply_nav_key` 之前(对齐 Go select_char 优先于翻页)，仅缓冲非空或有候选时拦截(空缓冲放行作普通标点)。默认 `select_char_keys` 空=禁用零回归。词频学习记单字(非整词)。5 集成测试(取第1/2字、默认禁用、overflow ignore/commit/commit_and_input)+全 58 测试绿，Windows 交叉编译过。
- [x] **批10 快捷加词 (#12)**：候选窗快捷加词状态机(对齐 Go handle_addword.go)。
  - State 加字段:add_word_active/add_word_chars(Vec<char>)/add_word_len/add_word_code/add_word_saved_vertical。
  - 入口:add_word 热键(默认 ctrl+=,已注册)→ enter_add_word_mode(从 recent_commits 还原最近字符池,默认词长 2,强制竖排,占位 composition)。在 handle_key_event 热键块特判返回 UpdateComposition(dispatch_hotkey bool 契约只能回 StatusUpdate,故不走它)。
  - 加词模式按键:↑/↓ 调词长[1,len]、Enter 确认写库、Ctrl+Enter 转设置端编辑界面(预填 text/code/schema)、Esc/Backspace 退出、其余消费。在 lock state 后、英文透传前特判 state.add_word_active → handle_add_word_key。
  - 造码:对齐设置端 dict.encode/web_dict_encode——拼音 generate_word_pinyin(无果回退 reverse.gen_pinyin);码表 reverse.wubi_word_code(五笔词组取码,逐字反查组合,**支持新词**)。码空时优雅中止。
  - 写库 store.add_user_word(weight=1200)。dict.changed 广播在 RPC dispatch 层(协调器不持有 EventSink),不发,与现有 web_dict_add 一致。
  - 退出/失焦/模式切换 reset。预览候选直接发 UiCommand::UpdateCandidates 两行(标题行 hint + 词行 code),label=" " 避免被自动编号为序号。
  - 不做:独立 open_add_word_dialog 热键(Go 默认 none)。
  - 用户实测反馈修复:①序号(label 空串被自动编号→改空格)②码表无法计算编码(误用 codetable_reverse_hint 只查已存在词→改 reverse.wubi_word_code)。

## 延后
- 拼音引擎 `punct_commit` 配置(默认开)——待相关引擎配置重构(那份 manager.rs/cache_fp.rs 未提交分支)落定后接入;当前拼音恒顶字上屏
- #2 精确匹配 `single_code_input`(要改 manager.rs,待缓存重构落定)
- #12 切换到本输入法热键(注册表写入 + 兼容性分析)
- #12 无界面截图
