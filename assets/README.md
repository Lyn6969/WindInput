# assets —— 安装包品牌资产

安装包打包时由 `config\app.toml` 的 `[package]` 段引用（`scripts\dev.ps1` 生成变体清单时
也指向本目录）：

| 文件            | 用途                                          | 规格                    |
| --------------- | --------------------------------------------- | ----------------------- |
| `logo.png`      | 安装/卸载向导界面顶部显示的 logo（72×72 圆角）| 144×144，满幅圆角       |
| `installer.ico` | `*-Setup.exe` / `uninstall.exe` 的 PE 图标    | 10 层，16→256           |

## logo.png 为什么是 144×144

安装器 UI 是 `Element::image_bytes(logo).size(72, 72).corner(18.0)`，而 windui 两个后端
都用**双线性**（D2D `D2D1_INTERPOLATION_MODE_LINEAR`、Skia `FilterQuality::Bilinear`），
无 mipmap。双线性只采 2×2，所以源图尺寸偏离显示尺寸越远越糟：放大发虚，缩小超过 2×
则丢样本、细笔画走样。144 = 72×2 在三档主流 DPI 上都落在最优点：

| DPI  | 物理尺寸 | 144 源图的重采样            |
| ---- | -------- | --------------------------- |
| 100% | 72 px    | 精确 2:1 缩小（2×2 平均）   |
| 150% | 108 px   | 1.33× 缩小                  |
| 200% | 144 px   | 1:1，走 blit 不插值         |

初版曾是 64×64（比显示尺寸还小，被放大着画，实测发虚）。**不要图省事换成 256×256** ——
100% DPI 下要 3.56× 点采样缩小，笔画边缘反而变硬、断续。

现版本由 `installer.ico` 的 256×256 层降采样而来（LANCZOS）。重做时必须**先预乘 alpha
再缩放、之后反预乘**：透明区 RGB 通常是黑色，逐通道插值会把黑渗进圆角边缘形成暗边。

两点约束：

- **不要把路径指回 `wind-installer\assets\`。** 那是通用安装器生成器的中性兜底图（W 字标），
  借用会让安装界面和安装程序顶着别人的标识。品牌资产归产品仓自己持有。
- `installer.ico` 与 `wind_tsf\res\wind_input.ico` 目前是同一张图（内容一致）。
  换品牌图时两处都要换 —— 前者是安装程序图标，后者是 TSF DLL 的资源图标，用途独立故不共享文件。

`logo.png` 读不到时 wind-packer 只打印警告并嵌入空 logo，运行期回退到 stub 内置的中性默认图；
`installer.ico` 读不到则是硬错误、打包直接失败。也就是说 **logo 路径失效不会让打包报错**，
只会静默变回 W 字标 —— 改动本目录或引用路径后请实际打一次包确认：

```powershell
# 看安装包里实际嵌入的 logo 字节数（应等于 assets\logo.png 的大小）
wind-packer.exe inspect --file dist\WindInputDev-Setup-<版本>.exe
```

## 已知且刻意不处理：Setup.exe 内残留 W 字标图标组

`editpe` 的 `set_main_icon_file` 是**追加**而非替换，所以打出的 `*-Setup.exe` 里有两组图标：

- `GROUP id=1`（6 层，256→16）—— wind-installer stub 编译期由其 `app.rc` 嵌入的 **W 字标**
- 一个 named 资源组（6 层，256/128/48/32/24/16）—— packer 注入的本产品「风」图标

**实际显示的是「风」**：Windows 资源目录中 named 条目排在 id 条目之前，Shell 取图标走的是
前者（已用 `Icon.ExtractAssociatedIcon` 实测确认）。代价只是约 13 KB 冗余；风险是任何按
`MAKEINTRESOURCE(1)` 硬取图标的代码路径会拿到 W。经评估维持现状，改动需动兄弟仓。
