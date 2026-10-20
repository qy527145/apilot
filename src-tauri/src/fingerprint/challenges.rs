//! 固定挑战库与无放回抽样。
//!
//! 36 道挑战来自 lm-detector 的 `data/enrollment-suite.json`，参考库就是用它们
//! 采集的。用自己的话改写提示词当然也能让模型写数字，但输入分布会偏离参考库，
//! 所以这里原样沿用。

use std::collections::HashSet;

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

/// 用户可手动指定挑战；`None` 表示随机抽 3 条（默认）。
///
/// 手动模式必须正好 3 条、互不重复且都存在于当前挑战库。保留用户给的顺序，
/// 这样结果里的三条回答与界面上的选择顺序一致。
pub fn select_challenges(
    challenges: &[Challenge],
    requested: Option<&[String]>,
    seed: u64,
) -> Result<Vec<Challenge>, String> {
    let Some(ids) = requested else {
        return Ok(pick_challenges(challenges, 3, seed));
    };
    if ids.len() != 3 {
        return Err("手动选择挑战时必须正好选择 3 条".into());
    }
    let mut seen = HashSet::new();
    let mut selected = Vec::with_capacity(3);
    for id in ids {
        if !seen.insert(id.as_str()) {
            return Err("手动选择的挑战不能重复".into());
        }
        let challenge = challenges
            .iter()
            .find(|challenge| &challenge.challenge_id == id)
            .ok_or_else(|| format!("找不到挑战 {id}"))?;
        selected.push(challenge.clone());
    }
    Ok(selected)
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

    #[test]
    fn manual_selection_requires_three_distinct_known_challenges() {
        let challenges = embedded().unwrap();
        let ids: Vec<String> = challenges
            .iter()
            .take(3)
            .map(|challenge| challenge.challenge_id.clone())
            .collect();
        let selected = select_challenges(&challenges, Some(&ids), 0).unwrap();
        assert_eq!(
            selected
                .iter()
                .map(|challenge| &challenge.challenge_id)
                .collect::<Vec<_>>(),
            ids.iter().collect::<Vec<_>>()
        );

        assert!(select_challenges(&challenges, Some(&ids[..2]), 0).is_err());
        let duplicated = vec![ids[0].clone(), ids[0].clone(), ids[2].clone()];
        assert!(select_challenges(&challenges, Some(&duplicated), 0).is_err());
        let unknown = vec![ids[0].clone(), ids[1].clone(), "does-not-exist".into()];
        assert!(select_challenges(&challenges, Some(&unknown), 0).is_err());
        assert_eq!(select_challenges(&challenges, None, 7).unwrap().len(), 3);
    }
}
