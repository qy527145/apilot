//! 数字解析：把模型回答变成长数字序列。
//!
//! 必须与 lm-detector 的 `shared/fingerprint-core.js::parseNumbers` 逐字节一致，
//! 否则同一个回答在 TypeScript 与 Rust 里会得到不同的指纹。两个容易写错的点：
//!
//! 1. **只认 ASCII 数字**。JS 的 `\d` 是 `[0-9]`，Rust 的 `\d` 默认是 Unicode
//!    十进制数字（`٠١٢` 之类也会命中）；这里显式判断 `is_ascii_digit`。
//! 2. **字母分隔符切段**。回答里常带「Here are 300 numbers: ...」之类的前言；
//!    前言与正文之间的字母把数字切成两个 run，最后只取最长的那个。

use once_cell::sync::Lazy;
use regex::Regex;

pub const VALUE_MIN: i32 = 1;
pub const VALUE_MAX: i32 = 355;
pub const DIMENSION: usize = (VALUE_MAX - VALUE_MIN + 1) as usize;

/// JS `\p{L}`：Unicode 字母。用它判断分隔符，而不是 `is_alphabetic` ——
/// 后者还包含部分组合标记，严格说不是同一个集合。
static LETTER: Lazy<Regex> = Lazy::new(|| Regex::new(r"\p{L}").expect("字母正则是常量"));

/// 诊断用的宽松数字扫描（对应 JS 的 `-?\d+`），不参与指纹。
static SIGNED_DIGITS: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"-?[0-9]+").expect("数字正则是常量"));

/// 解析一段回答，返回最长的一串 1..=355 的整数。
///
/// 行为完全对齐 JS：
/// - 连续数字是一个 run；
/// - run 之间的分隔符里只要出现字母，就结束当前 run；
/// - 超出 1..=355 的值丢弃，但**不**结束 run；
/// - 最后取最长的 run（长度相同取先出现的，`reduce` 的语义）。
pub fn parse_numbers(text: &str) -> Vec<i32> {
    let bytes = text.as_bytes();
    let mut runs: Vec<Vec<i32>> = Vec::new();
    let mut current: Vec<i32> = Vec::new();
    let mut previous_end = 0usize;

    let mut index = 0usize;
    while index < bytes.len() {
        if !bytes[index].is_ascii_digit() {
            index += 1;
            continue;
        }
        let start = index;
        while index < bytes.len() && bytes[index].is_ascii_digit() {
            index += 1;
        }
        let end = index;

        // 分隔符是「上一段数字结束」到「这一段数字开始」之间的原文。
        let separator = &text[previous_end..start];
        if !current.is_empty() && LETTER.is_match(separator) {
            runs.push(std::mem::take(&mut current));
        }

        if let Some(value) = small_value(&bytes[start..end]) {
            current.push(value);
        }
        previous_end = end;
    }
    if !current.is_empty() {
        runs.push(current);
    }

    runs.into_iter()
        .fold(Vec::new(), |best, run| if run.len() > best.len() { run } else { best })
}

/// 只关心 1..=355；超过 356 就提前饱和，避免超长数字串把整数撑爆。
fn small_value(digits: &[u8]) -> Option<i32> {
    let mut value: i32 = 0;
    for &byte in digits {
        value = value.saturating_mul(10).saturating_add((byte - b'0') as i32);
        if value > VALUE_MAX {
            return None;
        }
    }
    if value >= VALUE_MIN {
        Some(value)
    } else {
        None
    }
}

/// 355 个桶的计数。输入已经过 `parse_numbers` 过滤，越界值只是防御性丢弃。
pub fn count_numbers(numbers: &[i32]) -> Vec<f64> {
    let mut counts = vec![0.0; DIMENSION];
    for &number in numbers {
        if (VALUE_MIN..=VALUE_MAX).contains(&number) {
            counts[(number - VALUE_MIN) as usize] += 1.0;
        }
    }
    counts
}

/// 诊断用：回答里所有带符号的整数（含越界），对应 JS 的 `-?\d+`。
pub fn raw_numbers(text: &str) -> Vec<i64> {
    SIGNED_DIGITS
        .find_iter(text)
        .map(|match_| match_.as_str().parse::<i64>().unwrap_or(i64::MAX))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Deserialize)]
    struct Case {
        text: String,
        numbers: Vec<i32>,
    }

    #[derive(Deserialize)]
    struct Fixture {
        cases: Vec<Case>,
    }

    fn fixture() -> Fixture {
        serde_json::from_str(include_str!("fixtures/parse_cases.json")).unwrap()
    }

    #[test]
    fn matches_the_type_script_parser_on_all_fixture_cases() {
        for case in fixture().cases {
            assert_eq!(
                parse_numbers(&case.text),
                case.numbers,
                "输入: {:?}",
                case.text
            );
        }
    }

    #[test]
    fn letters_split_runs_but_punctuation_does_not() {
        // 前言里的 300 与正文被字母隔开，只取正文这一长串。
        assert_eq!(parse_numbers("Here are 300 numbers: 1 2 3 4"), vec![1, 2, 3, 4]);
        // 逗号和空格不切段。
        assert_eq!(parse_numbers("1,2, 3"), vec![1, 2, 3]);
    }

    #[test]
    fn out_of_range_values_are_dropped_without_splitting_the_run() {
        assert_eq!(parse_numbers("1 0 356 2"), vec![1, 2]);
    }

    #[test]
    fn only_ascii_digits_are_numbers() {
        // 全角数字不是 JS `\d`，不能被当成数字。
        assert_eq!(parse_numbers("１２３ 42"), vec![42]);
    }

    #[test]
    fn empty_text_yields_no_numbers() {
        assert!(parse_numbers("").is_empty());
    }

    #[test]
    fn count_numbers_indexes_from_one() {
        let counts = count_numbers(&[1, 1, 355]);
        assert_eq!(counts[0], 2.0);
        assert_eq!(counts[354], 1.0);
        assert_eq!(counts.iter().sum::<f64>(), 3.0);
    }
}
