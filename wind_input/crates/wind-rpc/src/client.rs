//! 最小同步 RPC 客户端：连 core 的控制通道（named pipe / unix socket），
//! 发一条 JSON-RPC 请求、读一条响应。供 `wind_input config` CLI 触发运行中 core 热重载。
//!
//! 连不上（core 未运行）即返回 Err，调用方应回退到离线直写配置文件。

use std::io::{Read, Write};

use serde_json::Value;
use wind_ipc::rpc::{PROTOCOL_VERSION, Request, Response, encode_message};

/// 向运行中的 core 发一条请求并取结果。连接失败或 core 返回 error 均为 Err。
pub fn call(suffix: &str, method: &str, params: Value) -> anyhow::Result<Value> {
    let endpoint = crate::server::ctrl_endpoint(suffix);
    let req = Request {
        version: PROTOCOL_VERSION,
        id: 1,
        method: method.to_string(),
        params,
    };
    let frame = encode_message(&req)?;

    #[cfg(unix)]
    {
        use std::os::unix::net::UnixStream;
        let mut stream = UnixStream::connect(&endpoint)?;
        stream.write_all(&frame)?;
        return finish(read_response(&mut stream)?);
    }
    #[cfg(windows)]
    {
        use std::fs::OpenOptions;
        let mut stream = OpenOptions::new().read(true).write(true).open(&endpoint)?;
        stream.write_all(&frame)?;
        finish(read_response(&mut stream)?)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = frame;
        anyhow::bail!("RPC 客户端不支持本平台（endpoint={endpoint}）")
    }
}

#[cfg(any(unix, windows))]
fn read_response<S: Read>(stream: &mut S) -> anyhow::Result<Response> {
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf)?;
    let len = u32::from_be_bytes(len_buf) as usize;
    if len > wind_ipc::rpc::MAX_MESSAGE_SIZE {
        anyhow::bail!("响应过大: {len} 字节");
    }
    let mut payload = vec![0u8; len];
    stream.read_exact(&mut payload)?;
    Ok(serde_json::from_slice(&payload)?)
}

#[cfg(any(unix, windows))]
fn finish(resp: Response) -> anyhow::Result<Value> {
    if let Some(err) = resp.error {
        anyhow::bail!("{err}");
    }
    Ok(resp.result.unwrap_or(Value::Null))
}
