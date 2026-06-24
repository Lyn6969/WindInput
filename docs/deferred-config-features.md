# 待恢复的配置功能（Go 旧版有、Rust 暂未接线）

Stage 1（config_schema 注册表）从 `data/config.toml` 删除了 33 个**孤立键**——它们在 `Config` 结构体里无对应字段，被 serde 静默丢弃，写了不生效。其中部分是 **Go 旧版已有、后续计划在 Rust 重新实现**的功能。此处登记，便于日后正经恢复。

恢复某项的正确三步：① 在 `wind-config/src/config.rs` 给对应结构体加字段（含 `#[serde(default)]` 与 Default）；② 在 `wind-config/src/config_schema.rs` 的 `REGISTRY` 登记键+类型（注册表测试会强制要求）；③ 在 `data/config.toml` 写回系统预置值。

> 注：纯样式类（"跟随主题"）与主题层重复的键，不应简单恢复到 config，详见各项备注。

## 1. 热键（hotkeys）
| 键 | 含义 | 备注 |
|---|---|---|
| `hotkeys.take_screenshot` | 截图（OCR/取词）热键 | Go 有截图功能；Rust「无界面截图」在计划中延后 |
| `hotkeys.open_add_word_dialog` | 独立打开加词对话框热键 | Go 默认 none；当前 Ctrl+Enter 在加词模式内转设置端，未做独立热键 |
| `hotkeys.enter_temp_pinyin` | 进入临时拼音的全局热键 | 临拼当前靠触发键进入，无独立热键 |

## 2. 输入（input）
| 键 | 含义 | 备注 |
|---|---|---|
| `input.auto_pair.blacklist` | 自动配对的应用黑名单 | 指定应用内不自动补全配对符号 |
| `input.temp_pinyin.z_include_on_commit` | z 临拼上屏时是否包含引导字符 z | 与 #9「z 混合仲裁」一同重新设计 |
| `input.temp_pinyin.accent_color` | 临拼模式强调色 | UI 增强，整体「模式强调色」一并做 |
| `input.url_input.accent_color` | 网址模式强调色 | 同上 |

## 3. 候选/字体（ui）
| 键 | 含义 | 备注 |
|---|---|---|
| `ui.candidate.mode_accent_border` | 候选窗模式强调边框 | UI 增强 |
| `ui.font.gdi_weight` / `gdi_scale` | GDI 渲染字重 / 缩放 | 仅 GDI 渲染路径用；当前默认 directwrite |
| `ui.font.menu_weight` / `menu_size` | 右键菜单字重 / 字号 | 菜单样式 |
| `ui.theme.editor_auto_start` | 主题编辑器随启动自启 | 配合 Web 主题编辑服务（计划中延后项） |

### 候选窗与主题层重复的键（**不要直接恢复**）
`ui.candidate.always_show_pager(_follow_theme)` / `show_page_number(_follow_theme)` / `vertical_max_width(_follow_theme)` —— 这些概念真正的真相源在 **theme.yaml（wind-theme）**。config 层若要提供覆盖，应统一走已有的 `pager_bar_display` / `page_number_display` 字符串覆盖机制，**不要**恢复 `bool + *_follow_theme` 的双写设计。`vertical_max_width` 若需 config 覆盖，新增一个明确的覆盖字段即可。

## 4. 状态提示（ui.status_indicator）
| 键 | 含义 | 备注 |
|---|---|---|
| `show_mode` / `show_punct` / `show_full_width` | 分项开关：中英 / 标点 / 全半角切换是否各自弹气泡 | **行为类**，值得恢复（加结构体字段即可） |
| `font_size` / `opacity` / `background_color` / `text_color` / `border_radius` | 气泡样式 | **样式类**，当前跟随主题（theme.views.status）；若要独立样式需先决定样式归属（theme vs config），勿简单恢复 |

## 5. 统计（features.stats）
| 键 | 含义 | 备注 |
|---|---|---|
| `features.stats.retain_days` | 统计数据保留天数（自动清理旧数据） | 已有 stats.prune 逻辑，接上 config 即可 |

## 6. 快捷输入字母 provider（features.quick_input.alpha_providers）
| 键 | 含义 | 备注 |
|---|---|---|
| `alpha_providers.pinyin` | 字母输入走拼音候选 | 现快捷输入由 mix「快捷」融合接管 |
| `alpha_providers.rare_char` / `rare_char_id` | 生僻字 provider 开关 / 引用方案 id | |
| `alpha_providers.english` | 英文 provider 开关 | |
| `features.quick_input.accent_color` | 快捷输入模式强调色 | UI 增强 |

> 注：快捷输入已重构为 `features.mix_modes` 融合的成员。恢复这些 provider 开关时，应考虑是否并入 mix_mode 的成员配置体系（见 SETTINGS_REVAMP_PLAN.md 的「实例集合 schema」一节），而非恢复独立 `alpha_providers` 表。
