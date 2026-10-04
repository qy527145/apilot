//! 额度单位与换算。
//!
//! 全程用整数 `i64` 记额度，不用浮点 —— 浮点累加会产生可见的漂移，
//! 而对账时「少收/多收了一分钱」是要被追问的。倍率可以是浮点，
//! 但只在**单次结算**时相乘、立刻取整，误差不会累积。
//!
//! 单位沿用 new-api 的约定：`1 USD = 500_000 quota`，即 1 quota ≈ $2e-6。

/// 1 美元对应的额度。
pub const QUOTA_PER_UNIT: i64 = 500_000;

/// 额度上限。取 i32 上限，保证累加时不会溢出 SQLite 的 INTEGER。
pub const MAX_QUOTA: i64 = i32::MAX as i64;

/// 额度 → 美元。
pub fn quota_to_usd(quota: i64) -> f64 {
    quota as f64 / QUOTA_PER_UNIT as f64
}

/// 把浮点额度安全地转成整数，并夹到合法区间。
pub fn clamp_quota(value: f64) -> i64 {
    if !value.is_finite() {
        // NaN / inf 说明倍率配置有严重问题，记 0 而不是崩溃。
        // 调用方会看到这一笔没有计费，便于从日志里发现配置错误。
        return 0;
    }
    (value.round() as i64).clamp(0, MAX_QUOTA)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quota_and_usd_roundtrip() {
        assert_eq!(quota_to_usd(QUOTA_PER_UNIT), 1.0);
        assert_eq!(quota_to_usd(250_000), 0.5);
    }

    #[test]
    fn clamp_handles_pathological_values() {
        assert_eq!(clamp_quota(10.4), 10);
        assert_eq!(clamp_quota(10.6), 11);
        assert_eq!(clamp_quota(-5.0), 0, "负额度没有意义，夹到 0");
        assert_eq!(clamp_quota(f64::NAN), 0);
        assert_eq!(clamp_quota(f64::INFINITY), 0);
        assert_eq!(clamp_quota(1e30), MAX_QUOTA);
    }

    #[test]
    fn display_precision_is_reasonable() {
        // 前端展示用 4 位小数，$0.0001 应当能表示出来
        assert!(quota_to_usd(50) > 0.00009);
        assert!(quota_to_usd(50) < 0.00011);
    }
}
