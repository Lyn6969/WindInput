# 重设计：控制 RPC 服务端（apps/rpc → 内嵌 service）

>

## 1. 现状（已核实，2026-06-18）

| 部件 | 现状 |
|------|------|
| `apps/rpc/src/main.rs` | 仅 `println!` 占位，独立 bin `wind_rpc`，Cargo 依赖 tokio/wind-store/wind-dict/wind-config |
| `apps/rpc/src/services/{config,dict,phrase,shadow,stats,system}.rs` | 各 ~22 字节注释空壳；`mod.rs` 仅重导出 |
| `wind-ipc/rpc.rs` | **协议已就绪**：`Request{v,id,method,params}` / `Response{id,result,error}` / `EventMessage{event,data}`；`encode_message`(4B BE 长度+JSON) / `decode_message_header`；`MAX_MESSAGE_SIZE=16MB` |
| `wind-bridge`（server.rs/push.rs） | **同步线程**传输框架：`CreateNamedPipe`(`PIPE_UNLIMITED_INSTANCES`) + 每连接一线程 `handle_client` 循环；经 `MessageHandler` trait 解耦。管道名 `\\.\pipe\wind_input{suffix}` / `..._push`。**无 `wind_input_ctrl` 控制管道** |
| `apps/service/src/main.rs` | 启动序列：单例 mutex → tokio rt → PushServer.start → BridgeServer.start → `Coordinator::new(push_server)` → `deferred.set_ready` → `restart_rx.recv()` 阻塞 |
| `Coordinator` | 字段 `config: Config` / `engine_mgr: EngineManager` / `store: Option<Arc<Store>>` **全私有**；公开仅 `active_schema_id` / `is_chinese_mode` / `debug_*`。**无 RPC 读写访问器** |
| `Config`（config.rs） | 有 `load` / `user_config_dir`；**无 `save`** |
| `Store` CRUD | `user_words`/`freq`/`shadow`/`temp_words` **已实现**（如 `add_user_word`/`get_user_words`/`search_user_words_prefix`/`remove_user_word`/`update_user_word_weight`）；`phrases`(21行)/`stats`(19行)/`migration`(3行) **空壳**；`Store` 自身仅 `open/version/pause/resume/path` |
| `EngineManager` | `available_schemas`(→`&[String]` 仅 id) / `active_schema_id` / `switch_schema` / `cycle_schema` / `convert`·`convert_with`(编码) / `current_engine_type` / `freq_settings` 已有；**缺 SchemaInfo 元信息聚合** |
| 设置前端事件推送 | **不存在**（仅有给 DLL 的二进制 `PushServer`） |

## 2. 架构决策

### D1 内嵌 service 进程（非独立）
redb 基于文件锁，**不支持多进程并发**；`Coordinator` 在 service 进程内独占 `Arc<Store>` + `Config` + `EngineManager`。控制 RPC 必须与它**同进程**直接持有这些，否则要再加一层进程间中转，徒增延迟与一致性问题。

- **`apps/rpc` 独立 bin 废弃**（或降为开发期 REPL 客户端）。
- 控制服务端逻辑提为新库 crate **`wind-control`**（dispatcher + services + ctrl 传输），由 `apps/service` 依赖并在 `main` 启动。
- `tokio` 不必引入控制路径——复用 wind-bridge 的同步线程模型。

### D2 新增控制管道 `wind_input_ctrl`
与现有 `wind_input`(bridge)/`wind_input_push` 并列，新增 `\\.\pipe\wind_input_ctrl{suffix}`（Unix：`$XDG_RUNTIME_DIR/wind_input_ctrl{suffix}.sock`）。**走 JSON-RPC 帧**（`rpc.rs` 的 4B 长度+JSON），与 bridge 的二进制 `IpcHeader` 协议分离——职责清晰，互不干扰。

### D3 鉴权靠 OS ACL

## 3. 进程内布局

```
apps/service/src/main.rs 启动序列（在 Coordinator 就绪后插入）
  ...
  let coordinator = Coordinator::new(push_server.clone());   // 持 store/config/engine
  deferred.set_ready(coordinator.clone());
  // ▼ 新增
  let ctrl = wind_control::ControlServer::new(ControlCtx {
      coordinator: coordinator.clone(),   // 写操作经它的锁，避免与按键处理竞态
      events: EventHub::new(),            // 给设置前端的事件广播
      suffix: PIPE_SUFFIX,
  });
  ctrl.start();                            // 同步线程监听 wind_input_ctrl
  ...
  restart_rx.recv()                        // 主线程仍阻塞于此
```

```
crates/wind-control/ (新库)
└── src/
    ├── lib.rs          ControlServer / ControlCtx / 启动
    ├── transport.rs    ctrl 管道监听(仿 wind-bridge server.rs，但 rpc.rs 帧) + 每连接线程
    ├── dispatch.rs     method:str → handler 路由表; Request→Response; 错误归一
    ├── events.rs       EventHub: 广播 EventMessage 给所有活跃 ctrl 连接
    ├── access.rs       数据访问层(见 §5): Coordinator 上的 RPC facade
    └── services/       config/schema/dict/freq/temp/shadow/phrase/stats/theme/system
```

## 4. 传输与分发

- **传输**（`transport.rs`）：复用 wind-bridge 的"`CreateNamedPipe`(`PIPE_UNLIMITED_INSTANCES`) + `ConnectNamedPipe` + 每连接一线程"骨架。每连接循环：读 4B 长度 → 读 JSON → `decode` 为 `Request` → `dispatch` → 写 `Response`。同一连接上服务端可异步写 `EventMessage`（见 §6），故写出加锁串行化。
- **分发**（`dispatch.rs`）：`method` 形如 `"config.get"`，按 `"<ns>.<op>"` 路由到 `services::<ns>::<op>(ctx, params) -> Result<Value, RpcError>`。统一包 `Response::success/error`。`v`/`id` 原样回带。
- **错误模型**：`Response.error` 为字符串（协议所限）。约定 `"<code>: <msg>"` 前缀（如 `not_found:`、`invalid_params:`、`backend:`），前端按前缀分类。

## 5. 数据访问层（关键：handler 委托谁）

handler **不直接碰私有字段**。在 `Coordinator` 上新增一组 RPC facade（`access.rs` 调用），统一在 `Coordinator` 的锁下操作，杜绝与按键处理的写竞态：

```rust
impl Coordinator {
    // 读
    pub fn rpc_config_snapshot(&self) -> Config;                     // 克隆当前 config
    pub fn rpc_schemas(&self) -> Vec<SchemaInfo>;                    // 聚合 engine_mgr + schema 文件元信息
    pub fn rpc_store(&self) -> Option<Arc<Store>>;                   // 词库/词频/shadow/temp 直读
    // 写（内部加锁 + 落盘 + 应用 + 发事件）
    pub fn rpc_apply_config(&self, items: &[ConfigSetItem]) -> anyhow::Result<SaveConfigResult>;
    pub fn rpc_set_active_schema(&self, id: &str) -> anyhow::Result<()>;
}
```

- **读 store**：`dict/freq/temp/shadow` 直接用已实现的 `Store` 方法（§1 表）。`dict.listPaged` 的分页在 handler 层基于 `search_user_words_prefix` 结果切片，或给 store 补带 `limit/offset` 的查询。
- **写配置**：`rpc_apply_config` = 深合并 `items`（段隔离，对齐前端 `lib/configDiff` 语义）→ `Config::save`（**待补**，§7）→ `apply`/`reload` 到 `engine_mgr` 与状态机 → `EventHub` 发 `config` 事件 → 必要时 `PushServer` 推 mode/重载给 DLL。返回 `{ needsRestart }`。

## 6. 事件推送（新增）

`EventHub`（`events.rs`）：维护活跃 ctrl 连接的发送端列表（仿 `push.rs` 的 clients 模式）。`Coordinator` 写操作完成后 `events.emit(EventMessage{event, data})`，各连接写 4B 长度+JSON。客户端区分：**有 `id` 字段 = `Response`；有 `event` 字段 = 事件**。

| 事件 | 触发 | data |
|------|------|------|
| `config-event` | config.setItems / schema 变更 | `{type, schemaId?, action}` |
| `dict-event` | dict/temp/freq/shadow/phrase 变更 | `{type, schemaId?, action}` |
| `stats-event` | 统计更新 | `{type}` |
| `system-event` | 重载/方案切换 | `{type, action}` |


## 7. 配置写入与热重载链路（待补核心）

```
config.setItems(items)
  → 深合并到 user 层 Config (段隔离)
  → Config::save()                 ← 新增: 写 user_config_dir()/config.toml, 仅用户层差异
  → Coordinator::apply_config()    ← 新增: 重建/刷新 engine_mgr + 状态机 (热生效, 无需重启)
  → EventHub.emit("config-event")
  → 需要时 PushServer 推 ModePush / 触发 DLL 重载
  → 返回 { needsRestart: 取决于改了哪些段 }
```

- `Config::save`：序列化为 TOML，写回 `user_config_dir()/config.toml`；只写用户层（系统预置层不动），保持三层合并语义。
- `apply_config`：参考 `handle_config.rs`（当前无 `pub fn`，热重载入口待补）。区分"可热生效"(候选窗/标点/热键) 与"需重启"(引擎结构性变更) 的字段集。

## 8. 方法契约与实现状态映射

按 `contract.ts` 的 10 命名空间，标注每组的落地路径与缺口：

| 命名空间 | 方法 | 委托 | 状态 |
|----------|------|------|------|
| `config` | get/getDefaults/setItems/reload | `rpc_config_snapshot` / `Config::default` / `rpc_apply_config` | 读✅；**`Config::save`+`apply_config` 待补** |
| `schema` | list/active/setActive/getConfig/saveConfig/resetConfig/setDictEnabled/references/delete | `engine_mgr` + schema 文件层 | active/switch✅；**SchemaInfo 聚合、覆盖层 save/reset、references 待补** |
| `dict` | listPaged/search/add/update/remove/clear/stats/encode/genPinyin | `Store` user_words + `engine_mgr.convert` | CRUD✅；**listPaged 分页、stats、encode/genPinyin 接线待补** |
| `freq` | listPaged/delete/clear | `Store` freq | ✅（分页接线） |
| `temp` | list/promote/promoteAll/remove/clear | `Store` temp_words + 晋升逻辑 | CRUD✅；**promote 晋升到 user 层待接** |
| `shadow` | list/pin/delete/removeRule | `Store` shadow | ✅ |
| `phrase` | list/add/update/remove/setEnabled/resetDefault | `Store` phrases | **phrases.rs 空壳，全待实现** |
| `stats` | summary/daily/clear/pruneBefore | `Store` stats | **stats.rs 空壳 + 统计收集器待实现** |


## 9. 落地路线

| 阶段 | 内容 | 验收 |
|------|------|------|
| **R0** | 新建 `wind-control` crate；`transport.rs`(ctrl 管道+rpc 帧)+`dispatch.rs`；内嵌 service 启动；`system.status` | 设置端 `system.status` 通 |
| **R1** | `Config::save` + `Coordinator::rpc_apply_config`/`apply_config`；`config.*` 全通 + `config-event` | 改配置落盘+热生效+事件 |
| **R2** | `schema.*`(SchemaInfo 聚合 + 覆盖层)；`dict/freq/temp/shadow.*`（已实现 store 接线 + 分页） | 词库/方案页接真后端 |
| **R3** | 落地 `phrases.rs` + `stats.rs`(收集器) → `phrase.*`/`stats.*` | 短语/统计页接通 |
| **R4** | `theme.*`；事件全量；并发与重连压测 | 取代前端 mock 全量联调 |

