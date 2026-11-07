//! 零散工具函数。

use std::time::{SystemTime, UNIX_EPOCH};

/// 当前 unix 毫秒时间戳。
pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// 当前 unix 秒时间戳。
pub fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// 把 unix 秒向下取整到小时，用作 `usage_hourly.bucket_ts`。
pub fn hour_bucket(ts_secs: i64) -> i64 {
    ts_secs - ts_secs.rem_euclid(3600)
}

/// 把可能含敏感信息的 key 打码，用于日志与前端展示。
pub fn mask_secret(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= 8 {
        return "*".repeat(chars.len());
    }
    let head: String = chars[..4].iter().collect();
    let tail: String = chars[chars.len() - 4..].iter().collect();
    format!("{head}…{tail}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn now_ms_is_plausible() {
        // 2020-01-01 之后、2100 之前
        let t = now_ms();
        assert!(t > 1_577_836_800_000 && t < 4_102_444_800_000);
    }

    #[test]
    fn hour_bucket_truncates_down() {
        assert_eq!(hour_bucket(0), 0);
        assert_eq!(hour_bucket(3599), 0);
        assert_eq!(hour_bucket(3600), 3600);
        assert_eq!(hour_bucket(3601), 3600);
        // 负值也要向下取整，而不是向零截断
        assert_eq!(hour_bucket(-1), -3600);
    }

    #[test]
    fn mask_secret_hides_middle() {
        assert_eq!(mask_secret("short"), "*****");
        assert_eq!(mask_secret("sk-1234567890abcdef"), "sk-1…cdef");
        assert_eq!(mask_secret(""), "");
    }
}
