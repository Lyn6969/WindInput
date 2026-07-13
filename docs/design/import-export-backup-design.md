# 导入导出 / 备份还原 统一架构设计

## 背景与目标

Go 版 WindInput 提供三类数据流转能力:方案的导入导出、用户词库的导入导出、整机数据的备份/还原。Rust 版目前这三层基本空白,但底层已具备可复用基座:

- `wind-store` 已有 `wdict.rs` 版本化文本编解码、phrase 的 upsert 合并导入(`export/import_user_phrases_wdict`)、freq 的 `export_records/import_records`。
- `wind-coordinator/webdata.rs` 已有数据域 RPC 转发框架(`web_data_rpc`),并已实现 `phrase.export/import`、`theme.importFromText/Url`。

本设计将三类功能统一为**同一分层栈的不同组合**,避免三套重复实现,并让现有 `phrase`/`theme` 的导入导出自然归位。

### 交付边界

- **本仓(core)**:实现引擎/存储侧的打包解包逻辑,并暴露 `web_data_rpc` 方法契约。
- **wind-setting(独立原生程序仓)**:文件对话框、导入向导、冲突预览界面。本设计给出其所需 RPC 契约,UI 实现不在本仓范围。

### 关键决策(已定)

| 决策点 | 结论 |
|---|---|
| 交付边界 | 核心 + RPC 契约(UI 在 wind-setting) |
| 备份格式 | 单一自描述 `.zip`(含 `manifest.json`) |
| 冲突语义 | 用户可选,默认合并(Merge);另提供 Replace |
| Go 版兼容 | 不兼容旧版(全新格式,不解析 bbolt/旧文件树) |
| 方案包内容 | 方案配置 + 引用资源;**不含**个人用户词/词频 |
| 词库文件格式 | wdict 文本(与 phrase 一致) |
| 备份内用户数据承载 | 逐表文本导出(自描述、跨版本稳健) |

## 核心抽象:三层复用栈

```
┌─ RPC 契约层 (webdata.rs)  scheme.* / dict.* / backup.*   ← wind-setting 调用
├─ 功能编排层  scheme.rs   dict(复用现有) backup.rs
├─ 归档层 (Bundle)         manifest.json + zip 读写         ← 方案包、整机备份共用
├─ 合并引擎 (Merge)        strategy(Merge/Replace) + dry-run 预览  ← 所有导入共用
└─ 编解码层 (Codec)        wdict / jsonl / toml             ← 所有功能共用(上抽自现有 wdict.rs)
```

**设计原则:文本类走 codec 直出,聚合类才套 Bundle。**

- 单方案词库导出是"轻量出口":不进 zip、不带 manifest,直接输出 `.wdict` 文本(与现有 `phrase.export` 同构)。
- 方案包与整机备份才套 Bundle 层(zip + manifest)。

功能与栈的组合关系:

| 功能 | codec | merge | bundle |
|---|---|---|---|
| 用户词库导入导出 | ✅ | ✅(导入) | ❌ |
| 方案包导入导出 | ✅ | ✅(导入) | ✅ |
| 整机备份/还原 | ✅ | ✅(还原) | ✅ |

## Bundle 归档格式

### manifest.json(方案包与备份共用)

```json
{
  "format": "windinput-bundle",
  "kind": "scheme",
  "spec_version": 1,
  "app_version": "x.y.z",
  "platform": "windows",
  "created_at": "2026-07-11T21:00:00+08:00",
  "contents": [
    { "type": "schema", "path": "schema/wubi86.schema.toml", "meta": { "id": "wubi86" } }
  ]
}
```

- `kind`: `"scheme"` | `"backup"`。
- `spec_version`: 归档格式版本(当前 1)。还原时更高版本 → 拒绝并提示升级;更低版本 → 按迁移规则读(首版仅 1)。
- `platform`: 来源平台(`"windows"` | `"darwin"`)。还原到不同平台时,对平台专属项(热键、`app_rules` 路径)由 UI 依据 manifest 提示;核心首版整体导入 + 标注,不做自动转换。
- `contents`: 内容清单,供 `*.inspect`/`*.previewImport` 免解压概览。

### 方案包 `.zip`(kind=scheme)

不含个人用户词/词频。布局:

```
manifest.json
schemas/<id>.schema.toml
schemas/<引用资源,保留 schemas 根相对路径,如 wubi86/xx.dict.yaml>
schemas/shuangpin/<布局>.toml     (若引用自定义双拼布局)
```

- 资源收集:解析 `schema.toml` 中对码表、词典、双拼布局、拆字表、字体等的引用路径,凡指向用户目录(非系统 `data/`)的文件一并纳入;指向系统种子的引用只记路径不打包(导入端若缺失再提示)。zip 内条目保留 schemas 根相对路径(而非按类型重排目录),使 schema.toml 的相对引用免改写、导入即用;系统种子引用与缺失文件以 manifest 的 system_ref/missing 条目记录。

### 整机备份 `.zip`(kind=backup)

逐表文本承载用户数据。布局:

```
manifest.json
config/config.toml
userdata/user_words.wdict
userdata/temp_words.wdict
userdata/phrases.wdict
userdata/freq.jsonl
userdata/shadow.jsonl
userdata/stats.jsonl          (可选,用户勾选)
schemas/<用户方案...>          (复用 schema/ 布局,可多方案)
themes/<用户主题...>
```

- **排除**:`cache/`、`logs/`(可重建/无价值);`state.toml`(本机相关,默认排除,`includeState` 可选包含)。
- 用户方案与主题复用方案包的 `schema/` 布局与主题目录结构。

## 合并引擎(所有导入统一)

- **策略**:
  - `Merge`(默认):保留本地新增项;同 key 以导入为准做 upsert。
  - `Replace`:先清空目标域(该 schema 的词库 / 该表 / 整段配置)再写入。
- **逐项交互式冲突**:由 UI 侧基于 dry-run 结果自行编排;核心首版只提供 `Merge`/`Replace` + 预览。
- **Dry-run 预览**:每个导入方法配套 `previewImport`,返回 `{ willAdd, willUpdate, willConflict, unchanged }` 的计数与样本。
- **去重 key**:沿用各表既有复合 key(`user_words`/`temp`/`freq`/`shadow` 为 `schema\0code\0text`,`phrases` 为 `code\0text`),与 store 现状一致。

## RPC 契约(交付给 wind-setting)

**传输约定**:

- zip 类走**文件路径**(避免大 base64 过 IPC);wind-setting 侧先弹文件对话框拿到路径再调用。
- 文本类(wdict)走 **content 字符串**。

| 域 | 方法 | 入参 | 返回 |
|---|---|---|---|
| 词库 | `dict.export` | `{schemaId}` | `{content}` |
| | `dict.import` | `{schemaId, content, strategy}` | `{added, updated, skipped}` |
| | `dict.previewImport` | `{schemaId, content}` | `{willAdd, willUpdate, willConflict, unchanged, samples}` |
| 方案 | `scheme.exportPackage` | `{id, path}` | `{path}` |
| | `scheme.importPackage` | `{path, strategy}` | `{imported, conflicts}` |
| | `scheme.previewImport` | `{path}` | `{manifest, willConflict, ...}` |
| 备份 | `backup.create` | `{path, includeStats?, includeState?}` | `{path, manifest}` |
| | `backup.inspect` | `{path}` | `{manifest}` |
| | `backup.restore` | `{path, strategy, sections?}` | `{restored, conflicts}` |

- `strategy`: `"merge"`(默认)| `"replace"`。
- `backup.restore` 的 `sections`(可选):限定只还原部分域,如 `["dict", "config"]`,缺省还原全部。
- 现有 `phrase.export/import`、`theme.importFromText/Url` **签名不变**,内部逐步统一到 `wind-transfer` 的 codec/merge。
- 还原/导入落盘后,核心触发 `rebuild_*` 与 `config.changed` / `dict.changed` 事件,通知 TSF/UI 刷新。

## 代码落点

### 新增 crate `wind-transfer`

依赖 `wind-store`、`wind-config`、`zip`、`serde` / `serde_json`。

```
wind-transfer/src/
  lib.rs
  codec/
    wdict.rs      从 wind-store 上抽为通用(phrases / words 共用同一版本化格式)
    jsonl.rs      freq / shadow / stats 的逐行 JSON 编解码
  bundle.rs       manifest 定义 / 校验 + zip 读写(打包、解包、免解压读 manifest)
  merge.rs        Strategy 枚举 + dry-run 预览的通用骨架
  scheme.rs       方案包 export / import + 引用资源收集
  backup.rs       整机 export / restore(组合 codec + merge + bundle)
```

### wind-store 需补的能力

对齐现有 phrase/freq 已有件:

- `export/import_user_words_wdict`(镜像 `export/import_user_phrases_wdict`)。
- `temp_words` / `shadow` / `stats` 的逐表文本导入导出。
- 批量 upsert 带 strategy(`Merge`/`Replace`),供还原/导入统一调用。

`wdict.rs` 由 store 私有编解码上抽到 `wind-transfer/codec`,store 侧改为调用通用 codec(或 store 依赖 wind-transfer 的 codec 子模块;方向在 P1 定,避免循环依赖——倾向把纯 codec 放在不依赖 store 的独立层)。

### wind-coordinator/webdata.rs

新增上述 RPC 方法,注入 `store` 与 `Config` 路径,转发到 `wind-transfer`。

### redb 文件锁

还原走逐表**文本 upsert**(非整文件替换),使用正常写事务即可,无需 `store` 的 `pause/resume` 热替换机制——规避 Windows 文件锁难题。

## 分阶段实施

| 阶段 | 内容 | 依赖 |
|---|---|---|
| **P1 底座** | `wind-transfer` crate + codec 上抽 + manifest/bundle + store 逐表 export/import + merge 引擎 + 单元测试 | — |
| **P2 词库** | `dict.export/import/previewImport` RPC(最独立,复用 phrase 范式) | P1 |
| **P3 方案包** | `scheme.exportPackage/importPackage/previewImport` + 资源收集 + 用户方案目录落地 | P1 |
| **P4 整机备份** | `backup.create/inspect/restore`(组合全部层) | P1–P3 |
| **P5 打磨** | `state`/跨平台项过滤、`stats` 可选、边界与错误提示 | P4 |

P2/P3 相互独立,可并行;P4 收口。

## 测试策略

- **codec**:各格式 round-trip(导出→解析→再导出内容一致);版本号非法拒绝;非法行跳过计数。
- **merge**:Merge upsert 语义、Replace 清空语义、dry-run 计数与实际落盘一致。
- **bundle**:manifest 校验、zip 打包解包 round-trip、`spec_version` 过高拒绝。
- **scheme**:资源收集完整性(引用文件全部入包)、导入后方案可加载并出候选。
- **backup**:整机 export→restore round-trip(空库、已有数据合并、Replace)、`sections` 局部还原、排除项确实未入包。
- **契约**:`webdata.rs` 集成测试(真实 Coordinator + 临时 redb),断言各 RPC 输出形状,对齐现有 `web_data_rpc` 契约测试风格。

## 非目标(YAGNI)

- 不解析/迁移 Go 版旧文件(bbolt 备份、旧目录树)。
- 首版不做逐项交互式冲突解决的核心态机(交由 UI 基于 dry-run 编排)。
- 首版不做跨平台配置项的自动转换(仅标注来源平台并提示)。
- 不做云同步 / 增量备份(整机备份为全量快照)。
