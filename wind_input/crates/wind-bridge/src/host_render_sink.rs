//! host-render 推帧抽象：forwarder 经此把 push 帧 fanout 给客户端，解耦 + 可测。
pub trait HostRenderSink: Send + Sync {
    fn push_frame(&self, frame: &[u8]);
}
