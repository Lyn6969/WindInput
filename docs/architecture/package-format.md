# 分发包与配置片段格式规格

状态：规格草案（2026-08 讨论定稿）。文中「现状」= 已实现行为；「新增」= 已定方向、未实施。
本文件是跨仓契约的权威来源，涉及四个仓库：

| 仓库 | 角色 |
|---|---|
| WindInput（主仓） | 格式实现（`wind-transfer/src/scheme.rs`）、片段校验与应用（wind-config / wind-webdata RPC） |
| wind-setting | 导入 UI、内容侦测分派、`windinput://` 协议与 `.wpkg` 关联 |
| WindInputTools | web 打包 / 导出工具（生成端，须严格按本规格产出） |
| WindInputDocs | cookbook 分发（片段代码块 + 一键导入链接） |

跨仓契约无编译期约束，改动本规格必须同步四仓（参照 `datadir.conf` 的教训：单向契约、静默漂移）。

## 1. 术语与格式家族

- **配置片段（fragment）**：一段 TOML 文本，内容为 config.toml 的键值子集。一等格式，可独立分发（剪贴板 / `.toml` 文件 / 文档站代码块）。
- **方案包（scheme package）**：zip 容器，内容为 schemas 根相对路径的方案文件与其引用资源。现状即 v2 格式。
- **配置包（config package）**：方案包的超集——额外携带 `config_patch.toml`（即一份配置片段）与可选 extras 资源。
- **分发包**：方案包与配置包的统称。后缀 `.wpkg`（新增，用于文件关联）或 `.zip`，导入端永远两者都接受。
- **备份包**：`kind = "backup"` 的自描述 zip（清单 `manifest.toml`，有 `spec_version` 门禁）。**不在本规格范围**，见 `docs/design/import-export-backup-design.md`。个人数据（用户词库/词频/临时词）只进备份包，永不进分发包。

## 2. 配置片段（一等格式）

### 2.1 定义

- 合法的 TOML 文本，键为 config.toml 的点分路径展开形式（表结构或点分键均可，语义等价）。
- 键域 = `wind-config/src/config_schema.rs` 的 `REGISTRY` 登记键 ∪ 「合法但不登记」显式白名单（注释模板等刻意不进 REGISTRY 的 `Option<T>` 键；名单落在 wind-config，须配守门测试防腐烂，与 `ABSENT_FROM_DATA_CONFIG` 同族待遇）。

### 2.2 应用语义（新增）

- 逐键走 `set_user_value` 通路合并进用户层 config.toml：REGISTRY 类型/值域校验、等默认即删（prune）、类型迁移全部自动生效。**禁止整文件覆盖**（备份还原的 config 域整文件覆盖是备份专属语义，分发格式不得复用）。
- **原子性**：预览阶段逐键报告（键 / 当前值 / 新值 / 错误原因）；应用阶段任何一键校验失败则**整片段拒绝**，不做半应用。
- 应用必须经过与 `config.setItems` 相同的代码路径，保证运行时镜像回灌（改了即生效，不许出现「重启后才生效」）。

### 2.3 RPC 契约（新增，草案）

- `config.previewPatch { text }` → `{ entries: [{ key, current, next, error? }], ok }`
- `config.applyPatch { text }` → 成功 / 整体拒绝（错误列表）。内部复用 previewPatch 的校验结果。

## 3. 分发包容器

### 3.1 现状（方案包 v2）

- zip，条目名 = schemas 根相对路径（零层级，导入零改写落到 `%APPDATA%\WindInput\schemas\`）。
- `package.toml`（`PACKAGE_META_NAME`）可选；缺失时按「根目录存在 `*.schema.toml`」识别。现有字段：
  - `[package]` app_version / platform / created_at
  - `[schema]` id / version
  - `[refs]` system / missing
- 导出恒为自包含（系统目录资源读源打包）。
- 导入校验：逐条目穿越守卫 `bundle::validate_entry_rel`；含 `manifest.toml`/`manifest.json` → 报「是备份包」；根无 `*.schema.toml` → 报「不是有效的方案包」。写入 = 全部读入内存后逐文件 tmp + rename。

### 3.2 新增：`format_version` 门禁

- `[package]` 增加 `format_version: u32`。当前规格版本 = **2**。
- 门禁规则（落点 `list_payload_entries`，仿 `Manifest::validate`）：
  - `package.toml` 缺失 → 视为 legacy（format_version = 1），按现状宽容导入；
  - `format_version >` 实现支持的版本 → **硬拒绝**并提示升级应用。
- 原则：**宽容只给过去，不给未来**。`read_package_meta` 现状「解析失败静默回落默认值」的行为对 legacy 包保留，但含 `format_version` 字段而解析失败的包应报错而非静默回落。

### 3.3 新增：配置包布局

```
<id>-<version>.wpkg  (zip)
├─ package.toml              # 携带新能力时必须；format_version = 2
│    [contents]              # 声明包内内容类型，导入端照单校验，未声明的顶层目录拒绝
├─ *.schema.toml + 词库/拆字/字体/双拼布局    # 零层级，语义与 v2 相同
├─ config_patch.toml         # 可选；一份配置片段，语义完全按 §2
└─ <extras 顶层目录>/         # 保留位；类型集合未定（首批候选：注释库），暂不实现
```

- `config_patch.toml` 的校验与应用 = §2 的片段管线，包不引入任何额外语义。
- extras 目录解包必须走同一个 `validate_entry_rel` 守卫，不得另写守卫。

## 4. 内容侦测规则（统一导入分派）

导入入口收到内容后按序判定（规则顺序即优先级）：

1. zip 且含 `manifest.toml` / `manifest.json` → 备份包 → 转备份还原流程（或提示去备份页）。
2. zip 且根含 `*.schema.toml` → 分发包（方案包/配置包）→ `scheme.previewImport` 流程；若同时含 `config_patch.toml`，预览必须附带 §2.3 的逐键 diff。
3. zip 且仅含 `config_patch.toml`（无方案文件）→ 纯配置包 → 片段流程。
4. 非 zip 的合法 TOML 文本（文件或剪贴板）→ 配置片段流程。
5. 均不匹配 → 明确报错，不猜。

生成端（WindInputTools / 手工打包）必须保证产物落进规则 2 或 3，否则视为无效包。

## 5. 安全要求

- **穿越守卫**：所有 zip 条目一律过 `validate_entry_rel`（components 白名单式；拦 `..`、绝对路径、盘符相对 `C:foo`、UNC、段内 `:`）。
- **限额**（新增，数值待定）：下载体积上限、解压后总量与条目数上限、下载超时。
- **确认对话框最低内容**（不可减配，任何入口一致）：来源（域名或文件路径）、方案 id 与版本、文件数与体积、与现有文件的冲突数、`config_patch` 将改动的键（当前值 → 新值）逐键列出。
- **协议来源策略**：`windinput://` 的 `url` 参数限官方域白名单 + https（现状已限 https）。本地文件 / 剪贴板不限来源——能力越强的内容走越窄的门（片段仅能改登记键 → 最开放；协议一键到达 → 最紧）。

## 6. 版本与兼容

- 已发布旧客户端遇到含 `config_patch.toml` 的包：将其当普通文件落进 schemas 目录（无害但配置不生效）。**无法补救**，分发侧（文档站 / web 工具）必须标注最低应用版本。
- 导出端默认文件名切换到 `.wpkg` 的时机：在 `.wpkg` 文件关联随版本发布**之后**，避免「导出的文件双击没反应」窗口期。
- 备份包保持 `.zip`，不参与 `.wpkg` 关联（防误双击触发整机还原确认）。
