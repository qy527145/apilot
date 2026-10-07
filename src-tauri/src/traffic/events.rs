//! 向前端推送事件。
//!
//! 网关在后台线程里跑，Tauri 的 `emit` 是发送到所有窗口的广播。流量类事件
//! **必须节流**：逐 chunk 推送会把 IPC 通道打满，前端反而卡死。

use std::sync::atomic::{AtomicI64, Ordering};
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use tauri::{AppHandle, Emitter};

use super::event_names;

/// 流量事件的节流间隔。
pub const TRAFFIC_THROTTLE: Duration = Duration::from_millis(250);

/// 节流器：同一类事件在窗口期内只发一次。
pub struct Throttle {
    last: Mutex<Option<Instant>>,
    interval: Duration,
}

impl Throttle {
    pub fn new(interval: Duration) -> Self {
        Self {
            last: Mutex::new(None),
            interval,
        }
    }

    /// 是否应当放行这次推送。
    pub fn allow(&self) -> bool {
        let mut last = self.last.lock();
        let now = Instant::now();
        match *last {
            Some(t) if now.duration_since(t) < self.interval => false,
            _ => {
                *last = Some(now);
                true
            }
        }
    }
}

/// 网关侧的事件发送器。
pub struct EventBus {
    app: AppHandle,
    traffic_throttle: Throttle,
    /// 由于节流而被跳过的请求通知数，凑够一批后合并推送一次。
    pending_requests: AtomicI64,
}

impl EventBus {
    pub fn new(app: AppHandle) -> Self {
        Self {
            app,
            traffic_throttle: Throttle::new(TRAFFIC_THROTTLE),
            pending_requests: AtomicI64::new(0),
        }
    }

    /// 推送流量快照。受节流限制。
    pub fn traffic<T: serde::Serialize + Clone>(&self, payload: &T) {
        if self.traffic_throttle.allow() {
            let _ = self.app.emit(event_names::TRAFFIC, payload);
        }
    }

    /// 推送一条新请求。受节流限制，避免高频请求打满 IPC。
    pub fn request<T: serde::Serialize + Clone>(&self, payload: &T) {
        self.pending_requests.fetch_add(1, Ordering::Relaxed);
        if self.traffic_throttle.allow() {
            self.pending_requests.store(0, Ordering::Relaxed);
            let _ = self.app.emit(event_names::REQUEST, payload);
        }
    }

    /// 推送网关状态。低频，不节流。
    pub fn gateway<T: serde::Serialize + Clone>(&self, payload: &T) {
        let _ = self.app.emit(event_names::GATEWAY, payload);
    }

    /// 推送 selector 切换。低频且用户可感知，不节流。
    pub fn selector_changed<T: serde::Serialize + Clone>(&self, payload: &T) {
        let _ = self.app.emit(event_names::SELECTOR_CHANGED, payload);
    }

    /// 推送缓存统计。低频，不节流。
    pub fn cache<T: serde::Serialize + Clone>(&self, payload: &T) {
        let _ = self.app.emit(event_names::CACHE, payload);
    }

    /// 推送「请求开始」。低频（每请求一次），不节流 ——
    /// 这是监控页「进行中」列表的进入点，漏一条就会留下一个永远不消失的幽灵。
    pub fn request_started<T: serde::Serialize + Clone>(&self, payload: &T) {
        let _ = self.app.emit(event_names::REQUEST_START, payload);
    }

    /// 推送「请求结束」。同样不节流。
    ///
    /// 不能复用 [`EventBus::request`]：那个带 250ms 节流且会合并负载，
    /// 用它当结束信号会让一部分请求永远停在「进行中」。
    pub fn request_finished<T: serde::Serialize + Clone>(&self, payload: &T) {
        let _ = self.app.emit(event_names::REQUEST_END, payload);
    }
}

impl crate::traffic::stream_events::StreamEventSink for EventBus {
    /// 流事件已经由 `StreamBatcher` 攒过批了，这里直接发，**不要再节流** ——
    /// 节流会丢帧，而流事件丢一帧就是内容缺一块。
    fn emit(&self, event: &crate::traffic::stream_events::StreamEvent) {
        let _ = self.app.emit(event_names::STREAM, event);
    }
}
