//! 预扣 → 结算 → 退款会话。
//!
//! Apilot 是本地网关，没有用户钱包，所以这里不做真实扣减。它的价值有两个：
//!
//! 1. **可观测性**：请求开始时用本地估算值预扣，结束时用上游真实 usage 结算，
//!    两者的差额就是「估算偏差」。把偏差记进日志，用户能看到估算靠不靠谱。
//! 2. **为额度管控留位**：将来若要加"每月预算上限"，就是在这里对预扣值做判断。
//!
//! `settle` / `refund` 都幂等：流式请求可能因客户端断连而走多次收尾路径，
//! 重复结算会把额度算两遍。

use super::quota::clamp_quota;

#[derive(Debug, Clone)]
pub struct BillingSession {
    /// 请求开始时按本地估算预扣的额度。
    estimated_quota: i64,
    /// 上游报告的实际额度（结算后填充）。
    actual_quota: Option<i64>,
    settled: bool,
}

impl BillingSession {
    pub fn new(estimated_quota: i64) -> Self {
        Self {
            estimated_quota: clamp_quota(estimated_quota as f64),
            actual_quota: None,
            settled: false,
        }
    }

    /// 不做预估、直接以结算为准的会话。非流式请求常用这种。
    pub fn deferred() -> Self {
        Self::new(0)
    }

    /// 用实际额度结算，返回 `实际 - 预估` 的差额（正数表示该补，负数表示该退）。
    ///
    /// 幂等：重复调用返回 0，不会重复调整。
    pub fn settle(&mut self, actual_quota: i64) -> i64 {
        if self.settled {
            return 0;
        }
        self.settled = true;
        let actual = clamp_quota(actual_quota as f64);
        self.actual_quota = Some(actual);
        actual.saturating_sub(self.estimated_quota)
    }

    /// 请求失败时退回全部预扣，返回退回的额度。幂等。
    pub fn refund(&mut self) -> i64 {
        if self.settled {
            return 0;
        }
        self.settled = true;
        self.actual_quota = Some(0);
        self.estimated_quota
    }

    /// 估算偏差（实际 - 预估）。未结算时为 `None`。
    ///
    /// 正值表示低估（实际比预估贵），负值表示高估。
    pub fn estimate_drift(&self) -> Option<i64> {
        self.actual_quota
            .map(|a| a.saturating_sub(self.estimated_quota))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settle_returns_positive_delta_when_underestimated() {
        let mut s = BillingSession::new(100);
        assert_eq!(s.settle(150), 50);
        assert_eq!(s.actual_quota, Some(150));
        assert!(s.settled);
    }

    #[test]
    fn settle_returns_negative_delta_when_overestimated() {
        let mut s = BillingSession::new(100);
        assert_eq!(s.settle(60), -40);
    }

    #[test]
    fn settle_is_idempotent() {
        let mut s = BillingSession::new(100);
        assert_eq!(s.settle(150), 50);
        assert_eq!(s.settle(150), 0, "重复结算必须返回 0");
        assert_eq!(s.settle(999), 0, "结算后不接受新值");
        assert_eq!(s.actual_quota, Some(150));
    }

    #[test]
    fn refund_returns_the_precharge_and_is_idempotent() {
        let mut s = BillingSession::new(250);
        assert_eq!(s.refund(), 250);
        assert_eq!(s.refund(), 0);
        assert_eq!(s.actual_quota, Some(0));
    }

    #[test]
    fn refund_after_settle_does_nothing() {
        // 流式请求可能先结算再走异常收尾，不能把已结算的额度又退一遍
        let mut s = BillingSession::new(100);
        s.settle(120);
        assert_eq!(s.refund(), 0);
        assert_eq!(s.actual_quota, Some(120), "结算结果不该被退款覆盖");
    }

    #[test]
    fn deferred_session_has_zero_estimate() {
        let s = BillingSession::deferred();
        assert_eq!(s.estimated_quota, 0);
        assert!(!s.settled);
    }

    #[test]
    fn estimate_drift_reports_direction() {
        let mut under = BillingSession::new(100);
        under.settle(180);
        assert_eq!(under.estimate_drift(), Some(80), "低估");

        let mut over = BillingSession::new(100);
        over.settle(30);
        assert_eq!(over.estimate_drift(), Some(-70), "高估");
    }

    #[test]
    fn drift_is_none_before_settlement() {
        assert_eq!(BillingSession::new(100).estimate_drift(), None);
    }

    #[test]
    fn negative_estimate_is_clamped_to_zero() {
        let s = BillingSession::new(-50);
        assert_eq!(s.estimated_quota, 0);
    }

    #[test]
    fn settle_clamps_negative_actual() {
        let mut s = BillingSession::new(100);
        s.settle(-10);
        assert_eq!(s.actual_quota, Some(0));
    }
}
