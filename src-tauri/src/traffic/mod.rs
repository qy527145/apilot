//! 网关运行时流量统计。
//!
//! 这里的数字是**进程内实时值**（当前并发、累计请求数），与数据库里的历史
//! 统计互补：DB 回答"过去一小时花了多少"，这里回答"现在正在跑什么"。

pub mod events;
pub mod stream_events;

use std::collections::VecDeque;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::Arc;

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};


/// 实时流量快照（推送给前端的事件负载）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TrafficEvent {
    /// 自上次推送以来的平均每秒请求数。
    pub rps: f64,
    /// 当前正在处理的请求数。
    pub active: i64,
    /// 最近若干请求的 TTFB 中位数（毫秒）。
    pub ttfb_p50_ms: Option<i64>,
    /// 进程启动以来累计处理的请求数。
    pub total_requests: u64,
}

/// 保留多少个最近的 TTFB 样本用于算中位数。
///
/// 样本太少会抖得厉害，太多则反映不出"现在"的情况。200 个约等于最近几分钟。
const TTFB_WINDOW: usize = 200;

/// 进程内计数器。
#[derive(Debug)]
pub struct TrafficStats {
    active: AtomicI64,
    total_requests: AtomicU64,
    failed_requests: AtomicU64,
    /// 最近的 TTFB 样本，用于算中位数。
    recent_ttfb: Mutex<VecDeque<i64>>,
}

impl Default for TrafficStats {
    fn default() -> Self {
        Self {
            active: AtomicI64::new(0),
            total_requests: AtomicU64::new(0),
            failed_requests: AtomicU64::new(0),
            recent_ttfb: Mutex::new(VecDeque::with_capacity(TTFB_WINDOW)),
        }
    }
}

/// 事件名。前端 `listen` 用这些字符串订阅。
///
/// **改这里就必须同步改前端 `src/lib/events.ts` 的 `ApilotEventMap`** ——
/// 两边都是字面量，对不上不会报错，只会静默收不到。
pub mod event_names {
    pub const GATEWAY: &str = "apilot://gateway";
    pub const TRAFFIC: &str = "apilot://traffic";
    pub const REQUEST: &str = "apilot://request";
    pub const SELECTOR_CHANGED: &str = "apilot://selector-changed";
    pub const CACHE: &str = "apilot://cache";
    /// 请求开始（监控页「进行中」列表的进入点）。
    pub const REQUEST_START: &str = "apilot://request-start";
    /// 请求结束（离开点）。与 `REQUEST` 分开，见 `stream_events::RequestFinished`。
    pub const REQUEST_END: &str = "apilot://request-end";
    /// 流式请求的实时事件批次。
    pub const STREAM: &str = "apilot://stream";
}

impl TrafficStats {
    pub fn new() -> Self {
        Self::default()
    }

    /// 请求开始。返回的守卫在 drop 时自动递减并发数。
    ///
    /// 用 RAII 而不是手工 inc/dec：请求可能从多个分支提前返回（协议错误、
    /// 上游失败、客户端断连），手工递减必然漏掉某条路径，导致并发数只增不减。
    ///
    /// 守卫持有 `Arc` 而不是引用，这样它在请求处理函数里可以跨 `await`
    /// 与 `shell` 的所有权移动共存。
    pub fn begin_request(self: &Arc<Self>) -> ActiveRequest {
        self.active.fetch_add(1, Ordering::Relaxed);
        self.total_requests.fetch_add(1, Ordering::Relaxed);
        ActiveRequest { stats: self.clone() }
    }

    pub fn record_failure(&self) {
        self.failed_requests.fetch_add(1, Ordering::Relaxed);
    }

    pub fn active(&self) -> i64 {
        self.active.load(Ordering::Relaxed)
    }

    pub fn total_requests(&self) -> u64 {
        self.total_requests.load(Ordering::Relaxed)
    }

    pub fn failed_requests(&self) -> u64 {
        self.failed_requests.load(Ordering::Relaxed)
    }

    /// 当前保留的 TTFB 样本数。用于测试窗口是否被封顶。
    pub fn ttfb_sample_count(&self) -> usize {
        self.recent_ttfb.lock().len()
    }

    /// 记录一次 TTFB 样本，用于算中位数。
    pub fn record_ttfb(&self, ms: i64) {
        let mut buf = self.recent_ttfb.lock();
        if buf.len() == TTFB_WINDOW {
            buf.pop_front();
        }
        buf.push_back(ms);
    }

    /// 最近样本的 TTFB 中位数。没有样本时返回 `None`。
    pub fn ttfb_p50(&self) -> Option<i64> {
        let buf = self.recent_ttfb.lock();
        if buf.is_empty() {
            return None;
        }
        // 窗口只有 200 个元素，每次排序的代价可以忽略，换来"永远准确"。
        let mut v: Vec<i64> = buf.iter().copied().collect();
        v.sort_unstable();
        Some(v[v.len() / 2])
    }

    /// 组装给前端的事件负载。`rps` 由调用方按采样间隔算好传入。
    pub fn event(&self, rps: f64) -> TrafficEvent {
        TrafficEvent {
            rps,
            active: self.active(),
            ttfb_p50_ms: self.ttfb_p50(),
            total_requests: self.total_requests(),
        }
    }
}

/// 在途请求的 RAII 守卫。
pub struct ActiveRequest {
    stats: Arc<TrafficStats>,
}

impl Drop for ActiveRequest {
    fn drop(&mut self) {
        // 只减不加；saturating 防止极端情况下变成负数。
        let prev = self.stats.active.fetch_sub(1, Ordering::Relaxed);
        if prev <= 0 {
            self.stats.active.store(0, Ordering::Relaxed);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counters_start_at_zero() {
        let s = TrafficStats::new();
        assert_eq!(s.active(), 0);
        assert_eq!(s.total_requests(), 0);
        assert_eq!(s.failed_requests(), 0);
    }

    #[test]
    fn begin_request_increments_active_and_total() {
        let s = Arc::new(TrafficStats::new());
        let _g = s.begin_request();
        assert_eq!(s.active(), 1);
        assert_eq!(s.total_requests(), 1);
    }

    #[test]
    fn dropping_guard_decrements_active() {
        let s = Arc::new(TrafficStats::new());
        {
            let _g = s.begin_request();
            assert_eq!(s.active(), 1);
        }
        assert_eq!(s.active(), 0);
        assert_eq!(s.total_requests(), 1, "累计数不应回退");
    }

    #[test]
    fn guard_releases_on_early_return_via_drop() {
        // 模拟提前返回的路径：只要离开作用域就会释放
        fn handle(stats: &Arc<TrafficStats>, fail: bool) -> Result<(), &'static str> {
            let _g = stats.begin_request();
            if fail {
                return Err("boom");
            }
            Ok(())
        }

        let s = Arc::new(TrafficStats::new());
        assert!(handle(&s, true).is_err());
        assert!(handle(&s, false).is_ok());
        assert_eq!(s.active(), 0, "两条路径都必须释放并发计数");
        assert_eq!(s.total_requests(), 2);
    }

    #[test]
    fn concurrent_requests_are_counted() {
        let s = Arc::new(TrafficStats::new());
        let g1 = s.begin_request();
        let g2 = s.begin_request();
        let g3 = s.begin_request();
        assert_eq!(s.active(), 3);

        drop(g2);
        assert_eq!(s.active(), 2);
        drop(g1);
        drop(g3);
        assert_eq!(s.active(), 0);
    }

    #[test]
    fn failure_counter_is_monotonic() {
        let s = TrafficStats::new();
        s.record_failure();
        s.record_failure();
        assert_eq!(s.failed_requests(), 2);
    }

    #[test]
    fn snapshot_reflects_current_state() {
        let s = Arc::new(TrafficStats::new());
        let _g = s.begin_request();
        s.record_failure();

        assert_eq!(s.active(), 1);
        assert_eq!(s.total_requests(), 1);
        assert_eq!(s.failed_requests(), 1);
    }

    #[test]
    fn cross_thread_counting_is_consistent() {
        let s = Arc::new(TrafficStats::new());
        let mut handles = Vec::new();

        for _ in 0..8 {
            let s = s.clone();
            handles.push(std::thread::spawn(move || {
                for _ in 0..100 {
                    let _g = s.begin_request();
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }

        assert_eq!(s.total_requests(), 800);
        assert_eq!(s.active(), 0, "并发计数必须归零");
    }

    // --- TTFB 分位 ---

    #[test]
    fn ttfb_p50_is_none_without_samples() {
        assert_eq!(TrafficStats::new().ttfb_p50(), None);
    }

    #[test]
    fn ttfb_p50_takes_the_middle_sample() {
        let s = TrafficStats::new();
        for v in [100, 200, 300, 400, 500] {
            s.record_ttfb(v);
        }
        assert_eq!(s.ttfb_p50(), Some(300));
    }

    #[test]
    fn ttfb_p50_is_robust_to_insertion_order() {
        let s = TrafficStats::new();
        for v in [500, 100, 400, 200, 300] {
            s.record_ttfb(v);
        }
        assert_eq!(s.ttfb_p50(), Some(300));
    }

    #[test]
    fn ttfb_window_keeps_only_recent_samples() {
        let s = TrafficStats::new();
        // 先灌满窗口的旧样本（很慢）
        for _ in 0..TTFB_WINDOW {
            s.record_ttfb(9999);
        }
        // 再来一批新样本（很快）
        for _ in 0..TTFB_WINDOW {
            s.record_ttfb(10);
        }
        assert_eq!(
            s.ttfb_p50(),
            Some(10),
            "旧样本必须被挤出窗口，否则中位数永远反映不了现状"
        );
    }

    #[test]
    fn ttfb_window_is_bounded() {
        let s = TrafficStats::new();
        for i in 0..(TTFB_WINDOW * 3) {
            s.record_ttfb(i as i64);
        }
        assert_eq!(s.ttfb_sample_count(), TTFB_WINDOW);
    }

    #[test]
    fn event_payload_carries_all_frontend_fields() {
        let s = Arc::new(TrafficStats::new());
        let _g = s.begin_request();
        s.record_ttfb(120);

        let ev = s.event(3.5);
        assert_eq!(ev.rps, 3.5);
        assert_eq!(ev.active, 1);
        assert_eq!(ev.total_requests, 1);
        assert_eq!(ev.ttfb_p50_ms, Some(120));
    }

    #[test]
    fn event_serializes_with_expected_keys() {
        let s = TrafficStats::new();
        let v = serde_json::to_value(s.event(0.0)).unwrap();
        for key in ["rps", "active", "ttfb_p50_ms", "total_requests"] {
            assert!(v.get(key).is_some(), "事件负载缺少字段 {key}");
        }
    }
}
