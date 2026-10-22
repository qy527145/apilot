//! 接管时使用的共享常量。
//!
//! 具体的"哪些键归我们所有"直接写在各客户端的 `plan_apply` 里 ——
//! 那里能一眼看到"清掉什么、写回什么"的完整因果，比在另一个文件里
//! 维护一份键名清单更不容易出错。

/// 旧版接管写的本地占位密钥（所有客户端同一个值）。
///
/// 现在接管写的是 [`client_token`]；留着它是为了认出老配置 —— 网关识别客户端时
/// 遇到它要回落到 UA / 路径判断，网关启动时会把它换成按客户端区分的令牌。
pub const LOCAL_PLACEHOLDER_KEY: &str = "apilot-local";

/// 本地令牌的前缀：`apilot-<客户端名>` 会被网关识别成那个客户端。
pub const CLIENT_TOKEN_PREFIX: &str = "apilot-";

/// 接管时写进客户端配置的本地令牌。
///
/// 客户端会把它当作 API Key 发给我们（真正的上游密钥由渠道配置提供，入站鉴权头
/// 在转发前会被剔除）。按客户端区分的用意是**识别**：令牌是客户端每个请求都必带的，
/// 比 UA 可靠，也不要求客户端支持自定义请求头。它只是标签，不是鉴权 —— 谁都能伪造。
pub fn client_token(client: &str) -> String {
    format!("{CLIENT_TOKEN_PREFIX}{client}")
}

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
    fn client_token_carries_the_client_name_after_the_prefix() {
        assert_eq!(client_token("codex"), "apilot-codex");
        // 旧占位密钥也落在这个前缀下，识别时必须先把它单独挑出来，否则会被认成客户端 "local"。
        assert!(LOCAL_PLACEHOLDER_KEY.starts_with(CLIENT_TOKEN_PREFIX));
    }

    #[test]
    fn codex_provider_name_is_a_stable_slug() {
        // TOML 的裸键名限制；带空格或点号会让生成的配置无法解析
        assert!(CODEX_PROVIDER_NAME
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
    }
}
