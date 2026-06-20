# 实现计划：悬停提示(ui.tooltip) + 候选窗微调(ui.candidate)

补齐 Go 配置差距两块。follow_theme 约定：值留空/0 = 跟随主题（不照搬 Go 显式布尔）。

## Stage 1: wind-reverse — 字根加载 + tooltip_for 参数化
**Goal**: `tooltip_for` 按 `TooltipOptions` 门控 code/pinyin/chaizi，支持 heteronyms/max_readings；补存字根列。
**Tests**: 各 provider 开关组合、heteronyms=false 取首音、max_readings 截断、chaizi 显示 `字根 [编码]`。
**Status**: Complete

## Stage 2: config + manifest — 配置面
**Goal**: `ui.tooltip`{delay, code.enabled, pinyin.{enabled,heteronyms,max_readings}, chaizi.enabled, debug.enabled} +
`ui.candidate`{font_size, max_chars, index_labels, flip_when_above} 的 serde 结构 + 默认值 + manifest 条目。
**Tests**: 默认值正确、TOML 解析、留空回退语义。
**Status**: Complete

## Stage 3: coordinator — tooltip 组装 + 候选文本
**Goal**: 从 config 构 TooltipOptions 调 tooltip_for；debug.enabled 时追加候选元信息(source/weight/code)；
CandidateItem 构建处按 max_chars 截断 text、按 index_labels 覆盖序号。
**Tests**: max_chars 截断纯函数、index_labels 取标签纯函数。
**Status**: Complete

## Stage 4: wind-ui — 字号覆盖 / 上方翻转 / delay（Windows 目标）
**Goal**: 新 UiCommand 下发 font_size(0=主题)/flip_when_above；tooltip delay 映射 hover 防抖。
**Success**: x86_64-pc-windows-gnu 编译通过；设备实测候选字号/上方翻转/提示延迟。
**Status**: Partial
- ✅ font_size 覆盖（render base_fs 用 override>0 否则主题）+ UiCommand::SetCandidateFontSize
- ✅ tooltip delay（CandidateMouse.engage_delay_ms 取代硬编码 60ms）+ UiCommand::SetTooltipDelay
- ⏸ flip_when_above 未做：需在定位(clamp_to_work_area 决定 above/below)后逆序候选渲染，
  且保持 hit_rect/选中索引映射不变——属 render 流程重排，盲改风险高，待设备核验再实施。
  配置/manifest 已就位（设置该项当前 UI 不响应）。
