# 第三方资源声明

清风输入法 (WindInput) 使用了以下第三方资源，在此表示感谢并声明其许可证信息。

## 词库与数据资源

### 已包含在本仓库中的资源

#### 五笔86拆字数据库 (wubi86_chaizi.txt)

- **用途**: 五笔字根拆字数据，用于悬停提示中显示候选字的拆字信息
- **文件**: `data/schemas/wubi86/wubi86_chaizi.txt`
- **来源**: 来自五笔输入法资源网盘，原始来源及作者不详
- **许可证**: 未附带任何版权声明或许可证信息。如您是该资源的权利人且认为
  本项目的使用不当，请通过 Issue 联系我们，我们将及时处理

#### 黑体字根字体 (HeiTiZiGen.ttf)

- **用途**: 渲染拆字提示中 PUA 私用区的五笔字根字符
- **文件**: `data/schemas/wubi86/HeiTiZiGen.ttf`
- **来源**: 来自五笔输入法资源网盘，原始来源及作者不详
- **许可证**: 未附带任何版权声明或许可证信息。处理方式同上


### 构建时下载的资源（不包含在本仓库中）

以下资源在构建过程中由构建脚本从原始仓库下载（缓存于 `.cache/`，已被
gitignore），用于生成词库数据文件，其各自适用原项目的许可证条款。

#### 极点五笔 for Rime (rime-wubi86-jidian)

- **用途**: 五笔 86 版码表数据源
- **仓库**: https://github.com/KyleBing/rime-wubi86-jidian
- **许可证**: Apache-2.0
- **使用的文件**: `wubi86_jidian.dict.yaml`（主码表）、
  `wubi86_jidian_extra.dict.yaml`（扩展词库）、
  `wubi86_jidian_extra_district.dict.yaml`（行政区域词库）
- **加工方式**: 由 `wind-tools/gen_dict` 处理后写入构建产物 `data/schemas/wubi86/`：
  主码表按 unigram 词频重新赋权排序、单字提权，并按简码级别分层；扩展词库按字符
  类型拆分为 extra / emoji / english / symbols 四个文件；行政区域词库原样透传
  （仅清理头部的 librime `sort:` 键）。条目文本本身未作增删改

#### 白霜拼音 (rime-frost)

- **用途**: 拼音词库数据源（单字词库、基础词库、扩展词库、英文词库），
  用于生成拼音 unigram 语言模型
- **仓库**: https://github.com/gaboolic/rime-frost
- **许可证**: GPL-3.0
- **使用的文件**: `rime_frost.dict.yaml`、`cn_dicts/`（8105 / 41448 / base /
  ext / others / corrections / tencent）、`en_dicts/`（en / en_ext）

#### pinyin-data

- **用途**: 汉字现代普通话读音数据，用于生成拼音映射与悬停提示中的拼音显示
- **仓库**: https://github.com/mozillazg/pinyin-data
- **许可证**: MIT

#### OpenCC

- **用途**: 简繁转换词典数据
- **仓库**: https://github.com/BYVoid/OpenCC
- **许可证**: Apache-2.0

#### Unicode CLDR / Unicode emoji 数据（研究用，未纳入发行版）

- **用途**: emoji 中文名称，供 `wind-tools/gen_emoji_names` 研究「按语义用五笔
  编码检索 emoji」（输入 `khgf` 出 ⚽）。**当前功能未启用**，其产物既不入库
  也不进发行版，仅在本机 `.cache/` 下用于评估
- **仓库/来源**:
  - https://github.com/unicode-org/cldr — `common/annotations/zh.xml`、
    `common/annotationsDerived/zh.xml`
  - https://unicode.org/Public/emoji/latest/emoji-test.txt
- **许可证**: **Unicode-3.0**（Unicode License V3）。允许自由使用、修改、再分发
  与商业使用，仅要求保留版权与许可声明。`Copyright © 1991-2025 Unicode, Inc.`，
  完整条款见 https://www.unicode.org/license.txt。若将来启用该功能，其产物可
  直接入库——本许可证无 copyleft 传染性

#### 腾讯词向量

- **用途**: 词频数据参考（经由 rime-frost 的 `tencent.dict.yaml`），
  用于 unigram 语言模型的词频权重
- **来源**: 腾讯 AI Lab 中文词向量数据集

## 技术参考

以下项目/文档作为实现参考，本项目未复制其代码：

### Windows TSF 官方文档

- **来源**: https://learn.microsoft.com/en-us/windows/win32/tsf/text-services-framework
- **用途**: TSF 框架接口实现参考

### Windows Classic Samples

- **仓库**: https://github.com/microsoft/Windows-classic-samples
- **许可证**: MIT
- **用途**: TSF 输入法示例代码参考

### 鼠须管 (Squirrel)

- **仓库**: https://github.com/rime/squirrel
- **许可证**: GPL-3.0
- **用途**: macOS IMKit 输入法架构参考

## 许可证兼容性说明

本项目源代码采用 [MIT 许可证](LICENSE)。

词库数据文件来源于上述第三方项目，其各自适用原项目的许可证条款。
GPL-3.0 许可的词库数据（rime-frost）不包含在本仓库中，而是在构建过程中
作为外部数据依赖从原始仓库下载；发行版中包含由其生成的词库数据文件，
该部分数据适用 GPL-3.0 条款。

Apache-2.0（极点五笔码表）与 Unicode-3.0（Unicode CLDR，当前仅研究用）
均为宽松许可证，允许修改后再分发，其加工产物可直接包含在本仓库中，
无 copyleft 传染性。

## 本项目自身的名称与标识

本项目的 logo（`pic/logo.png`）与应用图标为原创图形作品，不在 MIT 许可证的
授权范围内。关于项目名称「清风输入法」「WindInput」的使用，
详见 [BRANDING.md](BRANDING.md)。
