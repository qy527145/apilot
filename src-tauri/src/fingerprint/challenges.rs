//! 固定挑战库与无放回抽样。
//!
//! 36 道挑战来自 lm-detector 的 `data/enrollment-suite.json`，参考库就是用它们
//! 采集的。用自己的话改写提示词当然也能让模型写数字，但输入分布会偏离参考库，
//! 所以这里原样沿用。

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Challenge {
    #[serde(default)]
    pub condition: String,
    #[serde(default)]
    pub system: String,
    #[serde(default)]
    pub user_prefix: String,
    #[serde(default)]
    pub transport: String,
    pub challenge_id: String,
    pub expected_count: u32,
    pub prompt: String,
}

/// 内置挑战库。每次调用都解析 152KB JSON 有点浪费，由引擎在加载时解析一次。
pub fn embedded() -> Result<Vec<Challenge>, serde_json::Error> {
    serde_json::from_str(include_str!("../../assets/fingerprint/challenges.json"))
}

/// 从固定库里无放回抽 `count` 条。
///
/// 用 LCG 而不是引入 `rand`：这里只要求"每轮不同"，不要求密码学随机；
/// `seed` 由命令层用当前时间/UUID 提供，测试里传固定值即可复现。
pub fn pick_challenges(challenges: &[Challenge], count: usize, seed: u64) -> Vec<Challenge> {
    let mut indices: Vec<usize> = (0..challenges.len()).collect();
    let mut state = seed | 1;
    for index in (1..indices.len()).rev() {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let swap = (state as usize) % (index + 1);
        indices.swap(index, swap);
    }
    indices
        .into_iter()
        .take(count.min(challenges.len()))
        .map(|index| challenges[index].clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_embedded_suite_has_36_distinct_challenges() {
        let challenges = embedded().unwrap();
        assert_eq!(challenges.len(), 36);
        let unique: std::collections::HashSet<&str> = challenges
            .iter()
            .map(|challenge| challenge.challenge_id.as_str())
            .collect();
        assert_eq!(unique.len(), 36);
        assert!(challenges
            .iter()
            .all(|challenge| challenge.expected_count >= 218 && !challenge.prompt.is_empty()));
    }

    #[test]
    fn picks_are_distinct_and_reproducible() {
        let challenges = embedded().unwrap();
        let first = pick_challenges(&challenges, 3, 42);
        let second = pick_challenges(&challenges, 3, 42);
        assert_eq!(
            first
                .iter()
                .map(|challenge| &challenge.challenge_id)
                .collect::<Vec<_>>(),
            second
                .iter()
                .map(|challenge| &challenge.challenge_id)
                .collect::<Vec<_>>()
        );
        let ids: std::collections::HashSet<&str> = first
            .iter()
            .map(|challenge| challenge.challenge_id.as_str())
            .collect();
        assert_eq!(ids.len(), 3);
    }
}
