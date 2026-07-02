# WindInput 开源方案

> 目标：开源本仓库（核心服务 wind_input + wind_tsf + wind_macos），配套的
> wind-setting / wind-portable / wind-installer 暂不开源。
> 其他人可以阅读、学习、构建核心部分，但无法用本仓库直接打包出完整发行版。
> 许可证沿用原 Go 项目的 MIT。
>
> 命名决策：本仓库后续直接替换原仓库，即更名/迁移为 `huanfeng/WindInput`；
> 原 Go 版仓库另存为 `huanfeng/WindInput-Go`。对外产品名不变（清风输入法），
> 不使用 "Plus" 品牌。

## 一、现状盘点（结论）

| 项 | 现状 | 结论 |
|---|---|---|
| 仓库边界 | 本仓库只含 `wind_input/`（18 crate）、`wind_tsf/`、`wind_macos/`、`data/`、`scripts/`、`docs/`；三个闭源组件一直是 `../wind-setting` 等**兄弟独立仓库** | 无需拆分，边界天然成立 |
| 构建脚本 | `dev.sh` / `dev.ps1` 对闭源仓库是「不存在则跳过」；`pack-installer.sh` 依赖 `../wind-installer`，缺失时报错退出 | 开源用户可构建核心，打不出安装包，符合预期 |
| CI / Release | `.github/workflows/` 已有完整 CI（ubuntu 交叉编译，`dev.sh ci` + TSF make）与 Release（tag 触发，经 `WIND_REPOS_TOKEN` PAT 检出私有兄弟仓库打完整安装包） | 开源后照常工作；secret 不会暴露给 fork PR |
| git 历史 | `.claude/` `.superpowers/` `.remember/` `scripts/deploy.local`（真实配置）从未入库，只有 `.example` 入库 | **历史干净，可直接转 public，无需重建仓库或改写历史** |
| LICENSE | 已存在，MIT，`Copyright (c) 2026 WindInput Contributors`，与原项目一致 | 不用动 |
| 远程仓库 | `github.com/huanfeng/WindInput`，当前 PRIVATE | 补文件 → 审计 → 更名为 WindInput → 转 public |

## 二、许可证与第三方数据

### 源代码

- 全仓库源代码：**MIT**（沿用现有 LICENSE 文件）。
- 可选：`wind_input/Cargo.toml` workspace 增加 `license = "MIT"`、`repository` 元数据（若未来发 crates.io 则必须）。

### 词库与数据（NOTICE.md 已声明）

| 资源 | 许可证 | 在本仓库中的形态 | 处理 |
|---|---|---|---|
| 极点五笔码表 (rime-wubi86-jidian) | Apache-2.0 | **已提交入库**（`data/schemas/wubi86/*.dict.yaml`） | NOTICE 声明来源与许可证；Apache-2.0 允许再分发 |
| 白霜拼音 (rime-frost) | **GPL-3.0** | 不入库，构建期下载到 `.cache/` | 维持「数据不入库」设计——GPL 数据与 MIT 代码隔离的关键；NOTICE 声明 |
| pinyin-data | MIT | 构建期下载/生成 | NOTICE 声明 |
| OpenCC 简繁数据 | Apache-2.0 | 构建期下载 | NOTICE 声明（较原项目 NOTICE **新增**） |
| 腾讯词频 (经 rime-frost tencent.dict.yaml) | — | 构建期下载 | NOTICE 声明 |
| `wubi86_chaizi.txt` / `HeiTiZiGen.ttf` | 来源不详，无授权信息 | 已入库 | 沿用原项目做法：NOTICE 如实声明并附权利人联系渠道；若收到权利主张再移除/替换 |

### 关于「无法打包完整作品」的边界认知

MIT 许可证**不能**从法律上阻止他人基于核心代码自行实现设置程序、
便携启动器和安装器后打包成品。本方案实现的是工程层面的效果：
本仓库不提供这些组件的源码，官方成品仅通过 Releases 分发二进制。
fork + 自研配套是 MIT 允许的，需有此预期。

### CLA 的价值

原项目 CLA 第 6 条「许可证变更」授权维护者未来调整许可证。
本项目存在闭源配套组件，未来可能需要把开源代码用于闭源发行版，
CLA 尤其值得保留，继续用 CLA Assistant 机器人流程。

## 三、工作清单

### 已完成（`worktree-opensource-prep` 分支）

- [x] 根 `README.md`：精简版，产品名直接为清风输入法 (WindInput)，
  声明开源边界（配套组件未开源，成品从 Releases 获取），尾部链接 WindInput-Go
- [x] `NOTICE.md`：按「已入库 / 构建期下载」两类重写，新增 OpenCC 与 Squirrel 参考
- [x] `CONTRIBUTING.md`：开发环境改为 Rust 工具链（dev.ps1 / dev.sh / mac），
  沿用 alpha PR 政策、CLA 流程与 Conventional Commits
- [x] `CLA.md`：自原项目逐字沿用
- [x] `.github/ISSUE_TEMPLATE/`：Bug 模板操作系统下拉**新增 macOS**、
  新增安装方式（安装包/便携版/pkg/源码）与三平台日志路径；功能建议模板新增「涉及平台」
- [x] `.github/PULL_REQUEST_TEMPLATE.md`：Rust 化（`dev.ps1 ci`、cargo fmt、AGENTS.md 同步）
- [x] `.github/workflows/ci.yml`：在既有 ubuntu 交叉编译 CI 上**新增 macOS 门控
  编译校验**（paths-filter 命中 wind_macos/wind_input/scripts/mac 时 `swift build` + `cargo check`）
- [x] `.github/workflows/cla.yml`：CLA Assistant（需配置 `CLA_TOKEN` secret）
- [x] `.github/release.yml`：Release Notes 自动分类配置
- [x] 根目录内部计划文档归档至 `docs/archive/`

### 转 public 前待办

1. **最终敏感信息扫描**：gitleaks（或 trufflehog）全历史扫描兜底（初查已干净）。
2. **仓库 Secrets**：确认 `WIND_REPOS_TOKEN`（Release 用）与 `CLA_TOKEN`
   （CLA Assistant 用）已配置。
3. **仓库更名**：旧名 → WindInput（GitHub 自动重定向旧 URL）；
   原 Go 仓库先更名为 WindInput-Go 腾出名字。产物命名中的 "WindInput"
   （release.yml、pack-installer.sh、config/app.toml 等）随后统一改为 WindInput。
4. **GitHub 仓库设置**：description、topics（ime / input-method / tsf / rust /
   wubi / pinyin）、启用 Discussions（issue 模板 config.yml 已链接）、
   branch protection（main 需 PR + CLA 检查）。
5. **转 public**；原 WindInput-Go 仓库 README 加指引说明 Rust 版所在。

### 开源后逐步完善

6. 维护面向用户的 `CHANGELOG.md` 发布说明。
7. Cargo workspace 补 `license` / `repository` 元数据（可选）。

## 四、文档站（WindInputDocs）

独立仓库 `WindInputDocs`（VitePress），当前内容面向 Go 版。规划（暂不动手）：

- 文档站保持独立仓库不变，便于社区贡献与主仓库解耦。
- Rust 版发布后需重写的内容：下载/安装页（新产物名与 macOS pkg）、
  配置说明（config.toml 键位已迁移）、快捷键/方案文档、FAQ。
- 主仓库更名为 WindInput 后，文档站中指向主仓库的链接虽有 GitHub 自动重定向，
  仍建议统一更新；在线文档地址与徽章同理。
- 本仓库 README 暂不链接文档站，待其内容替换为 Rust 版后再加入。

## 五、风险与决策记录

- **GPL 词库进发行包**：安装包/便携版内含 rime-frost 生成的词库数据，
  数据部分适用 GPL-3.0 条款（NOTICE 指明来源与许可证）。数据文件与 MIT
  代码是加载关系而非代码链接，原 Go 项目已按此模式发布，维持现状。
- **来源不详资源**（拆字库、字根字体）：以透明声明换取可用性，
  与原项目一致；备选方案是改为「构建期可选下载」但改动成本高，暂不做。
- **wind-rpc / webdata 协议随核心开源**：设置端 JSON-RPC 协议公开，
  第三方可自写设置界面。这属于「可学习」的范畴，接受。
- **Release workflow 引用私有仓库**：public 仓库中 workflow 文件可见（他人可知
  存在私有配套仓库），但 `WIND_REPOS_TOKEN` secret 不会暴露，fork 的 PR 无法读取。
- **AGENTS.md / docs 设计文档**：无敏感信息，公开保留。
