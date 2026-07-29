# 自动配对跨模式跳出：统一到单一配对栈

## 背景

符号自动配对支持用跳出键（`right_symbol` / Tab / Enter）让光标越过已补出的右符号。当前实现下，**能跳出的只有「配对建立时所处的那个模式」**，切换模式后就跳不出去了。

排查后发现问题比「中↔英切换」更大：英文全角下自己打的配对，自己也跳不出去。根因是配对状态有**三处记账**，而跳出判定有**三个互不相认的入口**，各自只看其中一处。

### 三处记账

| 代号 | 位置 | 内容 |
| --- | --- | --- |
| R | `wind-coordinator` `Coordinator::pair_tracker`（`coordinator.rs:661`） | 真正的栈，存左右符号字符 |
| E | `wind_tsf` `CKeyEventSink::_englishPairEngine`（`KeyEventSink.h:293`） | 另一个栈，英文半角配对独有 |
| D | `wind_tsf` `CKeyEventSink::_pairPendingDepth`（`KeyEventSink.h:141`） | R 的镜像计数，`InsertTextWithCursor` 响应 +1、`MoveCursorRight` -1 |

### 三个判定入口

| 入口 | 位置 | 判据 |
| --- | --- | --- |
| C++ 英文分支 | `KeyEventSink.cpp:539` / `:812` | `!isChineseMode && IsEnabled && !IsFullWidth` 且 **E 非空** → 本地 `Pop` + `VK_RIGHT` |
| C++ 中文分支 | `KeyEventSink.cpp:569` / `:1130` | `hasInputSession \|\| isChineseMode` 且 **D > 0** → 吃键转发协调器裁决 |
| Rust 裁决 | `coordinator.rs:4396` | **R 非空** → 返回 `MoveCursorRight` |

### 现状矩阵

| 配对建立于 | 记在 | 中文模式跳出 | 英文半角跳出 | 英文全角跳出 |
| --- | --- | --- | --- | --- |
| 中文标点配对（`coordinator.rs:5038`） | R + D | ✅ | ❌ | ❌ |
| 英文全角配对（`handle_punct.rs:451`） | R + D | ✅ | ❌ | ❌ |
| 英文自定义标点配对（`handle_punct.rs:562`） | R + D | ✅ | ❌ | ❌ |
| 英文半角普通配对（`KeyEventSink.cpp:865`） | **E** | ❌ | ✅ | ❌ |

三个洞的成因各不相同：

- **英文模式跳不出 R 的配对**：C++ 英文分支只看 E；即使改看 D，Rust 裁决也到不了——英文模式在 `coordinator.rs:4268` 就 `return PassThrough` 了，`:4396` 的跳出检查是**死代码**。
- **英文全角跳不出任何配对**：C++ 英文配对块有 `!IsFullWidth` 门槛，全角被整块跳过；随后 `:564` 的中文分支要求 `hasInputSession || isChineseMode`，英文模式无输入会话，也进不来。
- **中文模式跳不出 E 的配对**：中文分支只看 D，而 E 的配对不计入 D。Tab 虽会转发（中文模式无条件转发），但 R 是空的，协调器返回 `PassThrough`。

## 设计目标

1. 任一模式下建立的配对，在任一模式下都能用跳出键跳出。
2. 消除记账处，而不是增加同步机制——三处记账减为一处。
3. 配对状态的生命周期要能扛住「输入被弹框打断」，又不能在很久之后仍然吃掉用户的 Tab。

## 硬性约束（不可协商）

> **DLL 的吃键面不得因本次改动而扩大。** 只有「自动配对开关已开」且「按键产出的字符在生效的配对表内」才吃键；跳出键只在「确实存在未跳出的配对」时才吃。英文模式下多吃一个键就可能让宿主软件的原生功能（Tab 切焦点/补全、Enter 提交）失灵。

这条约束在下面每一处判据里都要能被逐条对上。相关既有铁律：**C++ 吃键集必须 ⊆ Rust 出字集**——吃了却不出字，严格 TSF 宿主（EverEdit 等）会直接丢键。

## 方案：统一到 Rust 单栈

英文半角配对不再由 DLL 本地插入，改为「DLL 精确吃键 → 转发 core → core 出字并记栈」。**这条链路已有两个在产先例**：英文全角配对（`handle_english_full_width`）与英文自定义标点配对（`handle_english_custom_punct`），都已真机验证。本方案是把第三条路径并入同一形态。

改造后：

- **唯一真相源** = R（`pair_tracker`），四条配对建立路径全部入 R。
- **唯一吃键闸门** = D（`_pairPendingDepth`），它已经是 R 的精确镜像。
- E 降级为**查表**：只保留 `SetPairs` / `IsEnabled` / `IsLeft` / `IsRight` / `GetRight` 供吃键判据使用，删除其栈（`Push` / `Pop` / `Peek` / `IsEmpty` / `Clear` 与 `_stack` 成员）。

### 为什么吃键判据不需要新增推送

`push_english_pair_config`（`coordinator.rs:3098`）推送的是 `rt.en_pairs` ——**配置里的原始 ASCII 配对表** + `enabled` 标志。DLL 的 `_MapVkToEnglishPairChar` 把 VK+Shift 映射为 ASCII 后查这张表。也就是说现有吃键判据本就是「开关已开 且 字符在配对表内」，**完全符合硬性约束，一个字都不用改**。

core 侧接手时用同一判据：`punct_char(key_code, shift)` 得到 ASCII，查 `rt.en_pairs`。判据同源，不会漂移。出字仍走 `english_pairs_via_pipeline`（经用户自定义映射的产物表），比 DLL 本地插入更准。

### 跳出闸门

C++ 三个入口合并为一条，不再按 `isChineseMode` / `IsFullWidth` 分岔：

```
D > 0
  && 状态未过期（见「生命周期」）
  && _IsJumpOutKey(vk)
  && !(modifiers & (CTRL|ALT|SHIFT))
      → 吃键，转发协调器裁决
```

`D > 0` 本身就蕴含「开了配对且确实插入过配对且尚未跳出」，因此该闸门天然满足硬性约束——没配对时一个 Tab / Enter 都不会被吃。

Rust 侧把 `coordinator.rs:4396` 的跳出检查**前置到 `:4249` 的英文模式分支之前**，使其在英文模式也可达。守卫维持不变（无编码、无候选、无 Ctrl/Alt/Shift、命中 `jump_out_keys`、R 非空）；栈空则不拦截，落回原有透传路径。

> **实施注意**：前置后这段代码会跑在 CapsLock/全角分支与网址模式激活之前。实施时须逐条确认这些分支不消费 `jump_out_keys` 里的键（尤其网址模式对 Tab / Enter 的处理），否则会形成新的抢键。

### 英文模式跳出窗口：采用宽窗口（已定案）

现状 E 有一条「英文模式下按到任何非配对键就 `Clear()`」的规则（`KeyEventSink.cpp:558-561`），意味着英文配对的跳出窗口极窄：**只有刚打完 `(` 的那一下 Tab 有效，一旦在括号里打了字就跳不出去了**。

统一到 R 之后这条规则自然消失，英文模式获得与中文一致的「括号里打完字仍可跳出」能力。

曾考虑用一个本地布尔标志保住窄窗口以缩小 Tab 干扰面，**已否决**：那会让同一个配对在两个模式下跳出能力不同，工作逻辑不自洽。统一为宽窗口，代价是英文模式下 Tab 在整段未跳出期间会被吃——这个代价由下面的生命周期设计来兜底，而不是靠缩窄窗口。

### 断连兜底（必须做）

现状英文配对是 DLL 本地闭环，且它在 `OnKeyDown` 里的位置（`:802`）**早于** `_SendKeyToService`（`:1208`），所以 IPC 断连时英文配对照常工作。改为转发后，这个优势会丢失，而 IPC 失败路径对非字母键的处理是：

```cpp
// KeyEventSink.cpp:1231-1237
else { *pfEaten = FALSE; ... "ipc_failed_passthrough" }
```

`OnTestKeyDown` 已经吃了这个键，这里吐成 `FALSE` 就是「吃了再吐」翻转——记事本/Chromium 会补发，EverEdit 这类不补发的宿主**直接丢键**（该文件 `:1255` 的注释已记录过同款翻车）。

因此必须补一条兜底：**`_SendKeyToService` 失败且当前键是配对字符时，本地 `InsertText(单个字符)`、`*pfEaten = TRUE`，不配对、不记栈**。断连时降级为「无配对但不丢字」，且因为不记栈，不会留下任何需要清理的状态。

## 配对状态的生命周期

> ### ⚠️ 跨焦点保留已放弃（2026-07-29 真机后决定，本节其余内容仅作决策记录）
>
> 下面设计的「按 reason 细分保留 + owner_token 归属校验」**已撤销**，恢复为**失焦一律清空**。
> 保留下来的只有 TTL（它管的是同一焦点内的陈旧，与跨焦点无关）。
>
> **撤销理由**（真机实测「大部分情况不行」，且不是又一个可修的 bug）：
>
> 1. **两种作用域模型对不齐**：配对状态在 core 是**全局单栈**，在 DLL 是**每个宿主进程各自一份**
>    的 `_pairPendingDepth`。跨应用切换时前者还在、后者已随新进程实例归零。
> 2. **per-app IME 会重建整个上下文**：用户开启「为每个应用配置不同的输入法」后，切换应用
>    可能连输入法都换掉，IME 上下文重建，任何本地状态都无从谈起。
> 3. **最根本的**：焦点离开期间用户做了什么（点走光标、删掉括号、在别处编辑），输入法
>    **完全无法感知**。保留状态本质上是一个猜测——猜对省用户一次按键，猜错在 IDE 里
>    给一次莫名其妙的光标右移。
>
> 实施过程中已修掉 7 个清零点两侧不对称的问题（core 4 + DLL 3），仍未覆盖全部路径。
> **用户判断：一个大部分情况下失效的功能比没有这个功能更糟，容易引起误解。** 认同并采纳。
>
> 曾评估过的替代方案——**按跳出键时直接读光标右侧字符**（`ITfRange::Clone + ShiftEnd(+1) +
> GetText`，代码模式在 `TextService.cpp` 的 `OnEndEdit` 读 prevChar 处现成），可以让整类
> 状态同步问题消失。**用户以「该 API 在普通应用下的生效面不足以支撑一个确定性功能」为由
> 否决**，未实施。若日后重启此方向，先做宿主支持度实测再决定。

配对状态成立的前提是「光标紧贴一个已插入的右符号」。它的失效条件与输入缓冲**不同**：输入缓冲怕的是残留内容跑到新焦点里，配对状态怕的是光标不在原处了。当前实现把两者绑在同一个判据（`reason.clears_input()`）上，导致弹框夺前台时连配对状态一起清掉——而那时光标其实还在括号中间。

### 失焦：按 reason 细分

沿用 `FocusLostReason` 已有的后果矩阵思路，把「是否清配对状态」加为一个独立维度：

| reason | 触发场景 | 清输入缓冲 | **清配对状态** | 理由 |
| --- | --- | --- | --- | --- |
| `Thread` | 整个应用失去前台（弹框、Alt+Tab） | ✓ | **否** | 回来时光标通常仍在原处 |
| `CtxLost` | DocMgr 噪声层（Excel 6ms 掉了又回） | ✗ | **否** | 本就是噪声，现状也不清 |
| `DocChanged` | 同一宿主内换了文档 | ✓ | **是** | 光标必然不在原处 |
| `NoEditCtx` | 换到无可编辑控件的文档（QQ 切会话） | ✓ | **是** | 同上 |

### 归属校验：owner_token

R 是全局单栈，不分宿主。用户在 A 的括号里被打断 → 切到 B 打字 → 切回 A 按 Tab，栈顶可能是 B 压的那层。因此给配对栈记一个 `owner_token`（建立首层配对时的客户端 token），`focus_gained` 时 token 不匹配即清空。

C++ 侧的 D **天然 per-process**（每个宿主进程一个 DLL 实例），不需要同类校验。

> 备选（暂不做）：把 R 改成 `HashMap<token, PairTracker>` 彻底分栈。仅在 owner_token 校验实测不够时再考虑。

### 时效：TTL 120 秒

失焦不清之后，剩下的风险是「同一个输入框，但用户中途用鼠标点过别处、滚过页、删过字」——这些输入法都感知不到。用时效兜底：

- 记录**最后一次按键**的时间戳，距今超过 `state_ttl_secs`（默认 **120 秒**）即视为陈旧，判定时当作空栈并顺手清空。
- 从最后一次按键算起，不是从插入配对算起。**持续输入时不断刷新**，在括号里打多久都不会误过期；只有停手超过阈值才失效。

**TTL 必须在 C++ 侧实现**，因为它是吃键闸门所在：如果只有 Rust 过期而 D 仍 > 0，C++ 会吃下 Tab 转发、Rust 说不跳出、返回 `PassThrough`——又是「吃了再吐」。C++ 用 `GetTickCount64()` 记 `_pairLastActivityTick`，**在所有按键上刷新**（含英文模式的普通字母），闸门判据加上它。

Rust 侧也维护一份 `last_activity`，供 `right_symbol` 跳出路径判定（那条路不经过 C++ 的跳出闸门）。**两侧刷新时机天然不对称**：英文模式下 core 收不到普通字母键，Rust 的时间戳会偏旧。这个不对称的失效方向是安全的——Rust 偏保守地认为已过期，结果是打右符号时正常插入而不是误跳出。宁可不跳出，不要误跳出。

TTL 值由 core 推送给 DLL。**建议新增独立配置键 `CONFIG_KEY_PAIR_STATE_TTL`，不要再扩展 `CONFIG_KEY_JUMP_OUT_KEYS` 的 payload**——那个 payload 已经改过一次格式（前置 `right_symbol` u8，两侧解析偏移 1→2），再叠一层字段容易出现两侧偏移不同步。

### 配置项（内部项，不进 GUI）

两项都做成内部配置：`data/config.toml` 写好默认值与注释，**不同步 wind-setting 仓的 manifest 与 capabilities 快照**，设置界面不暴露。

```toml
[input.auto_pair]
# 失去焦点时是否清空配对状态。false = 保留（应用被弹框夺走前台后切回来仍可跳出）；
# 换文档 / 换到无可编辑控件的场景恒清，不受此项影响。
keep_state_on_focus_lost = true
# 配对状态时效（秒）。距最后一次按键超过该时长即视为陈旧，跳出键不再生效。
# 0 = 不过期。持续输入会不断刷新，不会误过期。
state_ttl_secs = 120
```

「不进 GUI」**不等于**设置仓什么都不用改。实测下来加一个 key 有**五道**守门测试依次拦截，每道都自带修复指引：

| # | 仓 | 测试 | 处理 |
| --- | --- | --- | --- |
| 1 | 主仓 | `registry_covers_every_config_key` | `config_schema.rs` 补一行 |
| 2 | 主仓 | `data_config_toml_covers_registry` | `data/config.toml` 补该键 |
| 3 | 设置仓 | `snapshot_matches_core_generated_capabilities` | 跑 `cargo test regenerate_capabilities_snapshot -- --ignored` |
| 4 | 设置仓 | `rpc::tests::mock_config_matches_core_system_preset` | 跑 `cargo test regenerate_mock_config -- --ignored` |
| 5 | 设置仓 | `uncovered_capability_keys_match_allowlist` | 加进 `capabilities.rs` 的 `UNCOVERED_BY_DESIGN` 并写明理由 |

第 3、4 道的产物（`capabilities.snapshot.json` / `mockdata/config.json`）是从主仓的 `wind-config` + `data` **现算**的，有生成入口，**不要手改**。第 5 道是双向断言（既拦"新增了没接进设置页的键"，也拦"名单里的键已被接进清单或已被删除"），正是内部配置项的正规落点。

## 改动清单

### C++（`wind_tsf`）

| 文件 | 改动 |
| --- | --- |
| `include/KeyEventSink.h` | `PairEngine` 删除 `_stack` 与 `Push/Pop/Peek/IsEmpty/Clear`；新增 `_pairLastActivityTick`、`_pairStateTtlMs` |
| `src/KeyEventSink.cpp:533-555` | `OnTestKeyDown` 英文块：删掉 `!IsFullWidth` 门槛与独立跳出判据；命中配对字符 → 吃键（判据不变） |
| `src/KeyEventSink.cpp:558-562` | 删除「非配对键清栈」整段 |
| `src/KeyEventSink.cpp:564-577` | 跳出闸门提为全模式统一判据（含 TTL） |
| `src/KeyEventSink.cpp:802-880` | `OnKeyDown` 英文块：删除本地插入/跳出，改为落到 `_SendKeyToService` 转发 |
| `src/KeyEventSink.cpp:1120-1140` | 中文跳出转发分支合并进统一闸门 |
| `src/KeyEventSink.cpp:1231-1237` | IPC 失败兜底：配对字符改为本地 `InsertText` 单字符 + `pfEaten=TRUE` |
| `src/KeyEventSink.cpp` 按键入口 | 所有按键刷新 `_pairLastActivityTick` |
| `OnSyncConfig` | 解析新配置键 `CONFIG_KEY_PAIR_STATE_TTL` |

### Rust

| 文件 | 改动 |
| --- | --- |
| `wind-config/src/config.rs` | `AutoPairConfig` 加 `keep_state_on_focus_lost: bool`（默认 true）、`state_ttl_secs: u32`（默认 120），均 `#[serde(default)]` |
| `wind-config/src/config_schema.rs` | 登记上述两键（Bool / Int） |
| `wind-ipc/src/protocol.rs` + `codec.rs` | 新增 `CONFIG_KEY_PAIR_STATE_TTL` 与编码函数 |
| `wind-transform/src/pair_tracker.rs` | `PairTracker` 加 `owner_token`、`last_activity`；`push` 记录，`is_stale(ttl)` 判定 |
| `wind-coordinator/src/coordinator.rs:4249` | 英文模式分支接入英文半角配对接手 |
| `wind-coordinator/src/coordinator.rs:4392-4406` | 跳出检查前置到英文分支之前，加 TTL 判定 |
| `wind-coordinator/src/coordinator.rs:5156` | 失焦清栈条件从 `clears_input()` 改为按 reason 细分 + `keep_state_on_focus_lost` |
| `wind-coordinator/src/coordinator.rs` `handle_focus_gained` | owner_token 不匹配 → 清栈 |
| `wind-coordinator/src/coordinator.rs` 推送段（`:3082` 附近） | 新增 `push_pair_state_ttl_config` |
| `wind-coordinator/src/handle_punct.rs` | 英文半角配对接手；删除 `:519-523` 已不成立的「两半栈」限制说明 |
| `data/config.toml` | 两个新键 + 注释 |

> `handle_english_custom_punct` 里那条已知限制（自定义产物恰为配对左符时无法用跳出键跳出）会被本方案顺带修掉——两半栈合并即消失。

## 测试计划

端到端测试（`crates/wind-coordinator/tests/input_flow.rs`）。**每个用例都要先断言前置条件真的成立**（配对确实入栈、模式确实翻转），否则会静默退化成假绿——本仓已有先例。

跨模式与新能力：

| 用例 | 断言 |
| --- | --- |
| 中文配对 → 切英文 → Tab | `MoveCursorRight` |
| 英文半角配对 → 切中文 → Tab | `MoveCursorRight` |
| 英文半角配对 → 英文模式 Tab | `MoveCursorRight`（回归：现状即支持） |
| 英文半角配对 → 英文模式打字 → Tab | `MoveCursorRight`（B2 新能力，现状会失败） |
| 英文全角配对 → 全角下 Tab | `MoveCursorRight`（新能力） |
| 跨模式嵌套：英文压 `(` → 切中文压 `（` → 连按两次 Tab | 两次都 `MoveCursorRight`，顺序正确 |

吃键面未扩大（硬性约束的回归保护）：

| 用例 | 断言 |
| --- | --- |
| 配对关闭时按 `(` | `PassThrough` |
| 配对开启但该字符不在配对表内 | `PassThrough` |
| 无配对时按 Tab | `PassThrough` |

生命周期：

| 用例 | 断言 |
| --- | --- |
| `Thread` 失焦 → 聚焦同一 token → Tab | `MoveCursorRight`（保留） |
| `CtxLost` 失焦 → Tab | `MoveCursorRight`（保留） |
| `DocChanged` 失焦 → Tab | `PassThrough`（清空） |
| `NoEditCtx` 失焦 → Tab | `PassThrough`（清空） |
| `keep_state_on_focus_lost = false` + `Thread` 失焦 → Tab | `PassThrough`（配置生效） |
| 聚焦到不同 token → Tab | `PassThrough`（归属校验） |
| 超过 TTL → Tab | `PassThrough`；且超时后 `right_symbol` 打右括号应正常插入而非跳出 |
| TTL 内持续按键 → Tab | `MoveCursorRight`（刷新生效，不误过期） |

TTL 相关用例需要可注入的时钟或可配置的极小 TTL（如 1 秒），不要用真实 sleep 拖慢测试。C++ 侧无单测框架，靠构建 + 真机。

## 风险与回滚

| 风险 | 缓解 |
| --- | --- |
| 英文配对由本地闭环改为 IPC 往返，断连时丢键 | 断连兜底（上文），必须真机验证「服务未启动时按 `(`」 |
| B2 宽窗口后英文模式 Tab 干扰宿主 | 真机在 VS Code / IDEA / 浏览器逐一验 Tab 与 Enter；TTL + 归属校验限制陈旧状态的存活面 |
| 两侧 TTL 判据漂移导致「吃了再吐」 | TTL 值单向推送、C++ 为准；Rust 侧偏保守（失效方向安全），并补 desync 场景测试 |
| 跳出检查前置后抢走网址模式等分支的键 | 实施时逐条核对，补对应回归测试 |
| 删除 E 的栈后某处仍在引用 | 编译期即暴露 |

回滚：改动集中在四个 crate 与两个 C++ 文件，按 commit 粒度可独立回退。建议 Rust 与 C++ 分开提交。

## 实施顺序与状态

1. ✅ 配置项与 IPC 键（Rust）
2. ✅ `PairTracker` 加 owner_token / last_activity + 失焦 reason 细分（Rust，含单测）
3. ✅ Rust 跳出检查前置 + 英文半角配对接手（Rust，含端到端测试）
4. ✅ C++ 删 E 的栈、统一闸门、TTL、断连兜底
5. ⬜ **真机验证（未做）**

第 1–3 步落地后 C++ 未改动时行为保持可用：DLL 命中英文配对后直接 `return`、不发 IPC，故 core 的新接手分支收不到键，不会重复插入。**若那个阶段出现重复插入即说明两侧同时处理，须立即排查**（实际未发生）。

### 实施中发现、与原设计不同的地方

- **设置仓有第五道守门** `uncovered_capability_keys_match_allowlist`（原以为四道）。内部配置项必须登记进 `UNCOVERED_BY_DESIGN`，见上文表格。
- **`_jumpOutOnRightSymbol` 保留解析但不再被消费**：右符号跳出统一由协调器裁决（需要比对具体是哪一对，栈在那边）。但它占 payload 首字节，删掉解析会算错后面 VK 列表的偏移，故保留并加注说明。
- **TTL 刷新点定在 `OnTestKeyDown` 的陈旧判定之后**（而非函数入口）。放入口会让每次按键先把自己刷新掉，TTL 永不触发。
- **Rust 侧的 `touch_pair_state()` 放在 `handle_key_event_policed` 末尾**，同理。
- **`OnKeyDown` 少了一条放行分支会丢键（已修）**：英文模式的 custom punct 键有专门的
  `isInputKey = TRUE` 分支才得以转发（`KeyEventSink.cpp:1087`，注释写明「那边吃了、这边不发
  → 键彻底丢失」）。英文配对键原本靠本地处理块提前 `return`，删掉后没有任何分支放行它，
  会被 `OnTestKeyDown` 吃下却无人转发。已补上对称分支。
  **教训：把一条「本地闭环」改成「吃键 + 转发」时，吃键点与转发点必须成对检查——
  本地闭环时根本不存在转发点，删掉闭环就等于同时删掉了出口。**

### 真机第一轮：英文下 Tab/Enter 全部失效（已修）

现象：英文模式按 Tab/Enter 完全没反应；中文打配对符后切英文也跳不出。

**定位过程（三步，全靠日志判据，没有猜测）**：

1. `wind_tsf.log` 里指纹 `cross-mode jumpout` 出现 45 次 → 新 DLL 确实在跑，吃键闸门也命中了。
2. 指纹行内容 `vk=0x09/0x0D depth=1 chinese=0` 反复出现，**depth 始终是 1、从不递减** → core 没有回 `MoveCursorRight`。
3. 同一时段 `wind_input.log` **一条日志都没有** → 键根本没到 core。C++ 吃了却没转发 = 「吃了再吐」。

**根因**：`OnKeyDown` 的跳出放行写在 `if (hasInputSession || isChineseMode)` 块**内部**。英文模式没有输入会话，整块跳过，Tab/Enter 没有任何分支认领，`isInputKey` 保持 FALSE。而 `OnTestKeyDown` 那边已经改成全模式统一闸门——**两侧不对称**。设计文档原本就写了要把这个分支提出来，实施时却只给它加了 TTL 判据、没有移出块外。

**修法**：判据抽成 `isPairJumpOut` 变量在门外算好，门外先放行一次，门内链首复用同一个值（留在链首是必须的，否则会被 `isInputKey = hasInputSession` 覆盖回 FALSE）。

**顺带补上 desync 兜底**：`isPairJumpOut` 吃了键但 core 回 `PassThrough`（它那边栈已被失焦/归属校验清空）时，以 core 为准把本地 depth 归零并 `_ReplayKeyToHost` 重放按键。否则每次 desync 都要丢一个 Tab/Enter。

> **教训**：把「本地闭环」改成「吃键 + 转发」时，吃键点与转发点必须**成对**检查。本地闭环时代根本不存在转发点，删掉闭环等于同时删掉了出口——而编译器不会有任何提示。

### ⚠️ 构建变体：`m1` 与 `dm1` 产物不同名、不同目录

本轮验证一度得出「部署份与构建份 md5 不符」的错误结论，原因是**跑了 `m1` 却去比对 dev 部署目录**：

| 命令 | 变体 | 输出目录 | 文件名 |
| --- | --- | --- | --- |
| `m1` | release | `build/` | `wind_tsf.dll` / `wind_tsf_x86.dll` |
| `dm1` | dev | `build_dev/` | `wind_tsf_dev.dll` / `wind_tsf_x86_dev.dll` |
| `pdm1` | — | 只复制不编译，源为 `build_dev/` | → `C:\Program Files\WindInputDev\` |

用户真机跑的是 **dev 版**，必须 `dm1` → `pdm1`；`m1` 的产物根本不参与 dev 部署。比对 md5 时源目录要跟着变体走，否则会把「比错了文件」误判成「部署失败」。

### 已核对的不变量

- **吃键集 ⊆ 出字集**：C++ `_MapVkToEnglishPairChar` 的 10 个 (vk, shift) 组合与 Rust
  `punct_char` 对同一输入给出**完全相同**的字符（逐条比对：`(` `)` `[` `]` `{` `}` `<` `>` `'` `"`）。
  C++ 映射是 Rust 的真子集，方向安全——只会「没吃但愿意出字」（配对不生效，既有限制），
  不会「吃了却不出字」（丢键）。默认 `english_pairs = ["()", "[]", "{}", "<>"]` 全在映射内。
- **密码框抑制早于配对吃键**：`IsPasswordSuppressActive()` 在 `OnTestKeyDown:297`、
  `OnKeyDown:795`，都在配对判定之前，故密码框里不会吃配对键后再被 core 拒绝。
- **三处判据逐条对齐**：模式、全角、Ctrl/Alt、字符判据在
  `OnTestKeyDown` 吃键 / `OnKeyDown` 放行 / core 出字三处一致。

## 部署与验证判据

C++ 改动的验证极易被假象误导，本仓已栽过多次（部署脚本只复制不编译、退出码 0 但文件被锁定未替换、时间戳新但内容旧）。因此：

1. 构建走 `scripts/dev.ps1 m1`（`pdm1` 只部署不编译，必须先 `m1`）。
2. 在改动分支加一条带独特词的 `WIND_LOG_DEBUG_FMT`（例如 `cross-mode jumpout`）。它一举三得：grep 构建产物 = 编进去了、grep 部署目录 = 换上了、真机日志出现 = 运行时加载了新 DLL。
3. 定案判据用 `md5sum 部署份 构建份` 比对，不要采信脚本的「部署完成」输出。
4. dev 版读 `%APPDATA%\WindInputDev\config.toml`，不是 `%APPDATA%\WindInput\config.toml`。核对生效配置时先分辨清楚。

## 相关

- 跳出键配置（`jump_out_keys` / `right_symbol` 语义）：文档站 `content/docs/advanced/config/input.mdx`
- 用户可见的配对状态说明与已知边界：文档站 `content/docs/input/punctuation.mdx`
