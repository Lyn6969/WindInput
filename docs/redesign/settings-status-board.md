# 设置系统 · 跨会话协调看板（settings-status-board）

> 活文档 / 多会话共享看板。最后更新：2026-06-19（重建）。

## ⚠️ 事故记录（必读）
2026-06-19 `main` 发生过 **rebase 历史重写**（reflog 实证），导致:
- 早前"阶段1 manifest 字段对齐"3 个提交被丢弃(对象已不存在)；本看板曾被删除(已重建)。
- 当时"菜单/数据桥/域名改名"工作一度只在工作区裸奔，已于 `5a7fb78` 抢救固化。
**教训:改动尽快用显式路径提交;rebase/force 前先同步各会话,别动别人未提交文件。**

## 现状一句话
HTTP 传输/安全/引导/配置读写 + **数据桥(schema/dict 真实接入)** 已通;manifest 已**重写为 6 Tab + 真实配置 key**。
其余数据命名空间(temp/freq/shadow/stats/phrase/theme)仍是合法空壳,待深化。

## 已完成
| 项 | 提交 | 说明 |
|---|---|---|
| 菜单「设置」→ 开网页配置 | `5a7fb78` | settings_url provider + OpenPath 开浏览器 |
| 数据桥 | `5a7fb78` | CoreStatus::data_rpc → Coordinator::web_data_rpc(webdata.rs);schema.*/dict.* 真实,其余空壳 |
| 域名 → setting.windinput.com | `5a7fb78`+docs | local.rs/security.rs + 文档 |
| manifest 重写(6 Tab + 真实 key) | 本次 | 含 `ui.candidate.preedit_display` 三值;保留另一会话 `[features.*]` 段 |

## 任务认领看板
> ⬜待认领 🟡进行中 ✅完成 🔴阻塞
| ID | 任务 | 状态 | 认领 | 说明/验收 |
|---|---|---|---|---|
| W1 | manifest 重写(6Tab+真实key) | ✅ | sess-A@06-19 | webapi 契约测试通过 |
| W2 | data_rpc 桥 + schema/dict | ✅ | sess-A@06-19 | 真实 engine/store |
| W3 | 深化 temp.* / freq.* / shadow.* | ⬜ | | wind-store 已有 API |
| W4 | 深化 stats.*(wind-store stats.rs) | ⬜ | | |
| W5 | 深化 phrase.*(wind-store phrases.rs) | ⬜ | | |
| W6 | 深化 theme.preview/import；dict.encode/genPinyin | ⬜ | | |
| W7 | schema.getConfig/saveConfig(方案YAML) | ⬜ | | 对话框 |
| W8 | 保存/重载/恢复默认 移到**导航栏**(替浮现栏) | ⬜ | | useConfig 已有 dirty/save/reset;补 getDefaults/reload |
| W9 | 前端 Tab 全面 manifest 驱动(方案A) | ✅ | sess-A@06-19 | WindInputSetting 1571c0c;通用/输入/外观/高级按组渲染,删硬编码 schema 使用,build 通过。二级对话框/分级 UI 仍可后续细化 |
| W10 | 热重载实装(apply_config) | ⬜ | | 见 hotreload-draft.patch |

## 待确认（manifest 中标 "# TODO opts"）
- `input.filter_mode` 选项(暂 smart/general/gb18030)、`input.enter_behavior`、`input.space_on_empty_behavior`、
  `input.pinyin_separator`、`schema.primary_*`(应为方案下拉) 的确切枚举值。
- 候选字体大小("font_size")在真实配置中疑由**主题**承载,非 `ui.font`,待定位。

## 变更日志
| 日期 | 会话 | 变更 |
|---|---|---|
| 2026-06-19 | sess-A@06-19 | 事故后重建看板;固化菜单/数据桥/改名(5a7fb78);manifest 重写 6Tab+真实key+嵌入编码三值 |
