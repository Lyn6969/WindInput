//! 端到端集成测试：用真实 Coordinator(headless) 作 handler，经 UDS 跑 KeyEvent 往返。
#![cfg(unix)]

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use wind_bridge::endpoint;
use wind_bridge::server::{BridgeConfig, BridgeServer};
use wind_config::Config;
use wind_ipc::protocol::{CMD_KEY_EVENT, IpcHeader};

#[test]
fn key_event_roundtrip_through_coordinator() {
    // 使用进程 pid 隔离临时目录，避免并行测试冲突
    let rt = std::env::temp_dir().join(format!("wind_e2e_{}", std::process::id()));
    std::fs::create_dir_all(&rt).expect("create runtime dir");
    // safety: 单线程测试，env 设置在 server 启动前完成
    unsafe { std::env::set_var("WIND_INPUT_RUNTIME_DIR", &rt) };

    // headless coordinator 作为 MessageHandler（无 UI、无持久化）
    let coordinator = wind_coordinator::Coordinator::new_headless(Config::default(), None);
    let handler: Arc<dyn wind_bridge::handler::MessageHandler> = coordinator;

    // 构造 BridgeServer
    let bridge = BridgeServer::new(
        BridgeConfig {
            suffix: String::new(),
            request_timeout_ms: 1000,
        },
        handler,
    );

    // 在后台线程启动 UDS 服务器（start() 内部 spawn 线程后立即返回）
    let rt2 = tokio::runtime::Runtime::new().unwrap();
    rt2.block_on(async { bridge.start().await.unwrap() });

    // 等待 socket 文件出现（最多 2 秒）
    let path = endpoint::request_socket_path("");
    let mut stream = None;
    for _ in 0..200 {
        match UnixStream::connect(&path) {
            Ok(s) => {
                stream = Some(s);
                break;
            }
            Err(_) => std::thread::sleep(std::time::Duration::from_millis(10)),
        }
    }
    let mut stream = stream.expect("connect bridge.sock: server did not start in time");

    // 发一个字母键 'a'（VK 0x41）按下事件
    // KeyPayload: key_code(4) + scan_code(4) + modifiers(4) + event_type(1) + toggles(1) + event_seq(2) + prev_char(2) = 18 bytes
    let mut payload = [0u8; 18];
    payload[0..4].copy_from_slice(&0x41u32.to_le_bytes()); // key_code = VK 'a'
    // 其余字段默认 0（event_type=0 即 key_down，modifiers=0 无修饰键）

    let header = IpcHeader::new(CMD_KEY_EVENT, 18).to_bytes();
    stream.write_all(&header).unwrap();
    stream.write_all(&payload).unwrap();

    // 读回 8 字节响应 header
    let mut resp = [0u8; 8];
    stream
        .read_exact(&mut resp)
        .expect("read response header from coordinator");
    let rh = IpcHeader::from_bytes(&resp);

    // coordinator 对 'a'（中文模式默认 on）的响应应为合法命令（UpdateComposition/Consumed 等）
    // 只断言读到了完整 8 字节 header 且 command 非 0
    // IpcHeader 是 #[repr(C, packed)]，不能直接引用字段，需先拷贝到局部变量
    let cmd = rh.command;
    assert_ne!(cmd, 0, "response command should be non-zero, got: {:?}", rh);

    // 清理临时目录
    let _ = std::fs::remove_dir_all(&rt);
}
