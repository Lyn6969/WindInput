# 重设计：core 内嵌 HTTP 控制/配置 API（wind-webapi）

> **架构已变更（2026-06-19）**：原"控制 RPC 服务端（命名管道 + JSON-RPC 帧 + 新建 wind-control crate）"方案**已弃用**。改为 **core(`apps/service`) 内嵌 HTTP 服务**，GUI 与远程 Web 共用同一套 HTTP API，不再有独立的 RPC 控制协议。本文档已更新为**实现态**。
>

## 1. 为什么是 HTTP 而非 RPC（决策演进）


- **体积**：体积敏感的是注入每个应用进程的 **TSF DLL**，HTTP 不进 DLL；它进的是单一常驻的 `apps/service` 进程，单份开销。
- **service 已依赖 tokio**（`main.rs` 即 `new_multi_thread()`），加 `axum` 只是 HTTP 层增量，不引入新运行时。
- **简化**：GUI 与远程 Web 共用**一套 HTTP API**，省掉一套 RPC 协议 + 一个中间桥进程；契约单一（统一 manifest + HTTP）。
- **安全可控**：成熟库（axum）+ 严格中间件 + **Web 授权按需**（见 §4）把跨站攻击面压到可接受。

代价：HTTP 服务在数据权威进程上（攻击面上移），靠中间件 + 按需授权缓解。`apps/rpc` 空壳 bin 维持废弃。

## 2. 现状（已实现，2026-06-19）

新建 crate **`wind-webapi`**，由 `apps/service` 内嵌启动。`cargo test -p wind-webapi` 9/9 通过；真实浏览器 e2e（web↔core）验证 manifest 渲染 + config 读写 + 特性门控全绿。

```
crates/wind-webapi/
├── Cargo.toml          deps: axum, tokio, serde_json, toml, anyhow, tracing, uuid, wind-ipc, wind-config
└── src/
    ├── lib.rs          serve() + build_router() + CoreStatus trait + 9 个契约测试
    ├── session.rs      WebState：清单缓存 / 端口 / 按需 Web token（短时效）/ control{suffix}.json
    ├── manifest.rs     加载 data/settings/manifest.toml + 注入运行时 app/engine/variant
    ├── rpc.rs          /api/rpc 分发：system.* / config.*
    ├── security.rs     /api/* 与 /local/* 两套中间件（token/Origin/CORS/PNA / 拒浏览器）
    └── local.rs        /local/* 处理（GUI 专用：info / web-config open|close）
    examples/dev_server.rs  无 core 的联调用 stub server（WIND_DEV=1 打印带 token 的 URL）
```

## 3. 架构与端点

- 内嵌在 `apps/service`：`Coordinator` 就绪后 `runtime.spawn(wind_webapi::serve(status, variant))`。**仅监听 loopback** `127.0.0.1:<随机端口>`，端口写入 `%LOCALAPPDATA%/WindInput/control{suffix}.json` 供 GUI 发现。
- **`CoreStatus` trait 解耦**：wind-webapi **不依赖 wind-coordinator/wind-ui**，运行时状态（`is_chinese_mode`/`active_schema_id`/`apply_config`）由宿主经 trait 注入（service 用 `WebStatus` 适配 `Coordinator`）。因此 wind-webapi 可在任意平台独立编译/联调。
- 端点两层：

| 端点 | 用途 |
|------|------|
| `POST /api/rpc` | Web 数据端点（JSON-RPC 形态 `{version,id,method,params}` → `{id,result,error}`）；system.* / config.* |
| `GET /local/info` | GUI 专用：版本/变体/连接态/端口 |
| `POST /local/web-config/open` | GUI 触发：按需签发短时效 token + 放行 Web，返回 `config.windinput.com/?port=&token=` |
| `POST /local/web-config/close` | 撤销 token，收回 Web 访问 |

## 4. 连接与安全模型

- **协议字段统一为 `version`**（`wind-ipc/rpc.rs` 的 `Request` 已去除 `#[serde(rename="v")]`；双边一致，避免 v 歧义）。
- **`/api/*`**：默认拒绝；用户点"打开网页配置"→ core 签发短时效 token + 临时放行 `https://config.windinput.com` Origin（开发期放行 `http://localhost:*`/`127.0.0.1:*`）。逐请求校验 `X-WindInput-Token` + Origin 白名单 + CORS（含 `Access-Control-Allow-Private-Network: true` 处理 Chrome PNA 预检）。
- **`/local/*`**：带 `Origin`（即浏览器跨源）一律 403，仅放行本机非浏览器客户端（GUI 的 ureq/同类）。
- "按需"= 浏览器暴露窗口按需（token + Origin），而非端口物理开关——loopback 端口常开给本地 GUI，Web 通道按需授权、可即时撤销。

## 5. 统一声明式设置清单（manifest）

真相源 `data/settings/manifest.toml`：`[meta]version` + `[[groups]]` + `[[items]]`(key/type/label/hint/default/since/options/min/max/step/enabled_when) + `[features]`(模块+子特性两层可用性)。core 解析后注入 `app/engine/variant`，经 `system.manifest` 返回 `{manifest,app,engine,variant,groups,items,features}`。web 据此渲染设置项并用 `features` 显示/隐藏/禁用模块。详见 web 端 `fromManifest.ts`/`useCapabilities.ts`。

## 6. 方法实现状态（按 contract.ts 10 命名空间）

| 命名空间 | 状态 |
|----------|------|
| `system` | status/info/manifest/fonts/notifyReload **已实现**（fonts MVP 返回空表、notifyReload 占位） |
| `config` | get/getDefaults/setItems/reload **已实现**；setItems 经 `Config::set_user_value` 落用户层 + 调 `CoreStatus::apply_config` 返回真实 `needsRestart` |
| `schema`/`dict`/`freq`/`temp`/`shadow`/`phrase`/`stats`/`theme` | **未实现**（返回 `unknown method`）；web 端靠 `features` 门控 + mock 兜底，属后续里程碑 |

`config`+`system` 已 100% 形状对齐（曾修 `system.info` 字段错配 app/engine→version/platform/dataDir/running）。

## 7. 待补（后续里程碑）

- 其余命名空间接真后端：`Store` 的 user_words/freq/shadow/temp 已实现可直接接线；`phrases.rs`/`stats.rs` 仍空壳需落地；schema 需 SchemaInfo 聚合 + 覆盖层。
- **config 热重载**：草稿已设计（`Coordinator` 的 `config`→`RwLock<Arc<ConfigBundle>>` + `install_config`/`reload_user_config`，轻字段即时生效、引擎结构性变更标 `needsRestart`），待 `wind-ui` 在非 Windows 可编译后接入并验证。
- **事件推送**：`GET /api/events`(SSE) 尚未实现（web 端有订阅，当前 CORS 失败属预期）。

## 8. 验证

`cargo test -p wind-webapi`（9 个契约测试：形状 + 安全行为）；web 端 `pnpm test`（zod 契约，mock + 真实 core 双层）；真实浏览器 e2e（chrome-headless-shell + Playwright，`WindInputSetting/web-e2e.mjs`）。
