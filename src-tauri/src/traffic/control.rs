//! 进行中请求的手动控制：中断 / 中断并重试。
//!
//! 上游把 `timeout_ms` 配到十分钟时，一条卡住的流可以干等十分钟 —— 客户端那头
//! 连接还挂着，用户除了去客户端里按 Esc 别无他法。这个对象就是让界面上那个
//! 按钮能把话传进正在跑的请求里。
//!
//! 它挂在 [`super::InflightEntry`] 上而不是另建一张表：**寿命必须与「进行中」
//! 底账完全一致**。另建一张的话，收尾的四条路（`Recorder::finish` /
//! `Recorder::Drop` / `StreamEmitter::retire` / 重启后的新请求）每一条都得记得
//! 摘一次，漏一条就是永远留着的一个小白点。
//!
//! 信号只走一个 `AtomicU8` + `Notify` 的组合，不用 channel：
//! - 命令层是同步写入，不需要为发一个信号建一个异步发送端；
//! - 消费端（候选循环、流的转发循环）本来就是 `select!` 里等一个 future，
//!   `Notify` 的 `notified()` 正好是那个 future。

use std::sync::atomic::{AtomicU8, Ordering};

use parking_lot::Mutex;
use tokio::sync::Notify;

/// 用户点了哪个按钮。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestSignal {
    /// 中断并重试：放弃当前这次上游请求，重新走一遍管线。
    Retry,
    /// 只中断：结束这条响应，不重试。
    Cancel,
}

const IDLE: u8 = 0;
const RETRY: u8 = 1;
const CANCEL: u8 = 2;

fn encode(sig: RequestSignal) -> u8 {
    match sig {
        RequestSignal::Retry => RETRY,
        RequestSignal::Cancel => CANCEL,
    }
}

fn decode(raw: u8) -> Option<RequestSignal> {
    match raw {
        RETRY => Some(RequestSignal::Retry),
        CANCEL => Some(RequestSignal::Cancel),
        _ => None,
    }
}

/// 一条正在跑的请求的控制句柄。
#[derive(Debug, Default)]
pub struct RequestControl {
    /// 待处理的信号。`IDLE` 表示没有。
    ///
    /// 用原子量而不是 `Notify` 自带的许可来记状态：`Notify` 只回答"有没有被
    /// 叫醒过"，回答不了"叫醒我的那个信号是什么"（重试与中断的处置完全不同）。
    signal: AtomicU8,
    notify: Notify,
    /// 这条请求已经试过的渠道，按试的先后。
    ///
    /// 重试要**换一条路**才有意义 —— 打回同一个卡住的渠道等于什么都没做。
    /// 记在这里而不是让命令层去查：只有出站的那一刻才知道真正在跑哪个候选。
    /// 是**一串**而不是"当前那个"：只记住最后一个的话，两条渠道会来回弹
    ///（A 卡 → 走 B → B 卡 → 又回 A），第一轮卡过的那个又回来了。
    attempted: Mutex<Vec<String>>,
}

impl RequestControl {
    pub fn new() -> Self {
        Self::default()
    }

    /// 下发一个信号。命令层调用，非异步。
    ///
    /// 用 `notify_one` 而不是 `notify_waiters`：前者在没有等待者时会**存下一个
    /// 许可**，于是"先点按钮、后进入 `select!`"这一瞬也不会丢（`notify_waiters`
    /// 会丢，表现为点了没反应，直到上游下一次吐字节才被看见）。
    pub fn request(&self, signal: RequestSignal) {
        self.signal.store(encode(signal), Ordering::SeqCst);
        self.notify.notify_one();
    }

    /// 等到有信号为止。**不消费**信号 —— 消费是 [`Self::take`] 的事，
    /// 好让调用方在同一处把"原来是重试还是中断"一次问清楚。
    pub async fn wait(&self) {
        loop {
            // 先建 future 再看状态：顺序反过来的话，检查之后、注册之前到达的
            // 通知会落进空档。这里靠 `notify_one` 存许可兜住，两道保险。
            let notified = self.notify.notified();
            if self.signal.load(Ordering::SeqCst) != IDLE {
                return;
            }
            notified.await;
        }
    }

    /// 取走信号。取走之后同一个信号不会再触发一次 —— 少了这一步，换到下一个
    /// 候选的第一次 `select!` 就会立刻又命中"重试"，一路把候选表刷穿。
    pub fn take(&self) -> Option<RequestSignal> {
        decode(self.signal.swap(IDLE, Ordering::SeqCst))
    }

    /// 记下正在出站的渠道，供重试时轮换。
    ///
    /// 同一个渠道可能出现多次（先失败过一次、后来又被选上），去重就行：
    /// 调用方要的是"哪些试过了"，不是"试过几次"。
    pub fn note_provider(&self, tag: &str) {
        let mut attempted = self.attempted.lock();
        if !attempted.iter().any(|t| t == tag) {
            attempted.push(tag.to_string());
        }
    }

    /// 这条请求试过的渠道 tag。
    pub fn attempted(&self) -> Vec<String> {
        self.attempted.lock().clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn a_signal_sent_before_waiting_is_not_lost() {
        // 命令层与网关是两条任务：按钮先到、`select!` 后进入是常态。
        let c = RequestControl::new();
        c.request(RequestSignal::Retry);
        tokio::time::timeout(Duration::from_millis(100), c.wait())
            .await
            .expect("信号已经下发了，wait 必须立刻返回");
        assert_eq!(c.take(), Some(RequestSignal::Retry));
    }

    #[tokio::test]
    async fn waiting_first_is_woken_by_a_later_signal() {
        let c = std::sync::Arc::new(RequestControl::new());
        let waiter = {
            let c = c.clone();
            tokio::spawn(async move { c.wait().await })
        };
        // 让等待者先真正挂上去。
        tokio::time::sleep(Duration::from_millis(20)).await;
        c.request(RequestSignal::Cancel);
        tokio::time::timeout(Duration::from_millis(100), waiter)
            .await
            .expect("信号到达后必须唤醒")
            .unwrap();
        assert_eq!(c.take(), Some(RequestSignal::Cancel));
    }

    #[test]
    fn taking_twice_yields_the_signal_once() {
        let c = RequestControl::new();
        c.request(RequestSignal::Retry);
        assert_eq!(c.take(), Some(RequestSignal::Retry));
        // 不消费的话，换到下一个候选的第一轮就会立刻又命中"重试"。
        assert_eq!(c.take(), None);
    }

    #[test]
    fn attempted_providers_accumulate_without_duplicates() {
        let c = RequestControl::new();
        assert!(c.attempted().is_empty());
        c.note_provider("deepseek");
        c.note_provider("openai");
        // 同一个渠道被再次选中不该让列表变长 —— 列表的用途是"别再走这些"。
        c.note_provider("deepseek");
        assert_eq!(c.attempted(), vec!["deepseek".to_string(), "openai".to_string()]);
    }
}