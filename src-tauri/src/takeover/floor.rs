//! 接管时使用的共享常量。
//!
//! 具体的"哪些键归我们所有"直接写在各客户端的 `plan_apply` 里 ——
//! 那里能一眼看到"清掉什么、写回什么"的完整因果，比在另一个文件里
//! 维护一份键名清单更不容易出错。

/// 接管时使用的本地占位密钥。
///
/// 客户端会把它当作 API Key 发给我们；真正的上游密钥由渠道配置提供，
/// 所以这里只需要一个非空值，让客户端别自作主张去读环境变量。
pub const LOCAL_PLACEHOLDER_KEY: &str = "apilot-local";

/// Codex `config.toml` 里我们写入的 provider 名。
///
/// 会被 selector / 文档 / 用户配置引用，改动等于破坏兼容，因此固定。
pub const CODEX_PROVIDER_NAME: &str = "apilot";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placeholder_key_is_not_empty() {
        // 空值会让客户端回退到读环境变量，从而绕过网关
        assert!(!LOCAL_PLACEHOLDER_KEY.is_empty());
    }

    #[test]
    fn codex_provider_name_is_a_stable_slug() {
        // TOML 的裸键名限制；带空格或点号会让生成的配置无法解析
        assert!(CODEX_PROVIDER_NAME
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
    }
}
