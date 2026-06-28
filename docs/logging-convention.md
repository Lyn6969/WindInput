# 日志规范（Logging Convention）

WindInput 是高频事件系统：每次按键、每条 IPC 命令、每次坐标更新都可能触发代码路径。
默认日志级别为 `info`（见 `apps/service/src/main.rs::init_logger`），因此 **`info` 必须低频、对运维有意义**，否则单次输入会刷出几十行日志、淹没日志文件。

## 分级标准

| 级别 | 用途 | 触发频率 | 示例 |
|------|------|----------|------|
| `error!` | 进程级、不可恢复的失败 | 极低 | 服务启动失败、单例冲突、RPC 初始化失败 |
| `warn!`  | 可恢复异常 / 降级 / 协议错误 / 配置回退 | 低，每条都代表"有点不对但能继续" | 解析 header 失败、词库缺失回退、写响应失败 |
| `info!`  | 进程生命周期里程碑 | **低频** | 启动/就绪/重启、切方案、词库加载完成、配置热重载、切主题 |
| `debug!` | 开发诊断（连接 / 会话级） | 可高频（默认不输出） | 客户端连接/断开、按键处理细节、坐标更新、菜单命令 |
| `trace!` | 逐命令 / 字节级 / 协议细节 | 最高频 | 逐 IPC 命令收发（`Received command` / `Sending response`）、payload 内容、协议字段（仍禁止隐私明文） |

## 硬性约束

1. **`info` 及以上禁止出现在每次按键 / 每条 IPC 命令 / 每次坐标更新 / 每次连接读写的路径上。**
   这类逐事件日志一律用 `debug!`（需要时通过 `RUST_LOG=debug` 或配置 `[debug] log_level` 打开）。
2. **隐私红线**：`info` 及以上禁止包含用户输入内容、词库词条、组合串（preedit）明文。
3. **不打预期分支**：如消息模式 `ReadFile` 返回 `ERROR_MORE_DATA` 但已读到完整 header，是协议预期行为，**不记日志**。
4. **不打冗余日志**：一次操作只在一个有信息量的点记录，避免 "Sending response" + "Response sent" 这类成对重复。

## 配置与开启方式

- 默认级别：`info`。
- 临时调高：环境变量 `RUST_LOG=debug`（最优先）。
- 持久配置：配置文件 `[debug] log_level = "debug"`（见 `DebugConfig`）。
- 可按模块过滤：`RUST_LOG=wind_bridge=debug,info`（只让 bridge 输出 debug，其余 info）。
- 日志落盘：`%LOCALAPPDATA%\WindInput[Dev]\logs\wind_input.log`，按 `log_max_size_mb` / `log_max_files` 滚动。

## 已知合规说明

- `wind-engine` 词库加载日志（"Loading dictionary" / "Dictionary loaded: N entries" / 合并缓存写入）属于**启动 / 切方案时一次性**的里程碑，保留在 `info`。其中逐子文件的 "Merging N entries from ..." 细节属诊断性质，可在噪音敏感时降为 `debug`。
- `wind-coordinator` 的按键处理路径（`handle_candidate` / `handle_key` 等）已统一使用 `debug!`，`info!` 仅保留重启、切方案、切主题等低频里程碑。
