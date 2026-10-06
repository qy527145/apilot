//! 应用设置：以单条 JSON 存在 `settings_kv` 表里。
//!
//! 配置读多写极少，运行期用 `ArcSwap<AppSettings>` 承载，网关侧每请求无锁读。

use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

use crate::error::AppResult;

const SETTINGS_KEY: &str = "app_settings";

/// 默认监听端口。选 8787 避开常见的 8080/3000 冲突。
pub const DEFAULT_PORT: u16 = 8787;

/// 全局模型替换的模式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelPolicyMode {
    /// 不替换。默认值 —— 不配就完全保持原有行为。
    Off,
    /// 任何入站模型名都换成 `active_model`。
    Always,
    /// 只当请求的模型在 Apilot 里没有任何可用渠道时才替换。
    ///
    /// 适合"平时用某个模型，顺手让别的也能跑"：客户端要的模型配了渠道就用它，
    /// 没配才落到选定的那个。
    Fallback,
    /// 按客户端分别指定（claude-code / codex / ...），没配到的客户端用 `active_model`。
    PerClient,
}

impl ModelPolicyMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Always => "always",
            Self::Fallback => "fallback",
            Self::PerClient => "per_client",
        }
    }
}

impl Default for ModelPolicyMode {
    fn default() -> Self {
        Self::Off
    }
}

/// 全局模型替换策略。
///
/// 目的是让"换个模型"变成一次下拉选择：不必去理解 selector 热切换与规则链，
/// 选中的模型直接套用到所有客户端。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ModelPolicy {
    pub mode: ModelPolicyMode,
    /// `Always` / `Fallback` 时替换成它；`PerClient` 里没配到的客户端也用它。
    pub active_model: Option<String>,
    /// `PerClient` 模式：客户端标识 → 模型名。
    pub per_client: indexmap::IndexMap<String, String>,
}

impl ModelPolicy {
    /// 客户端被分到的模型（`PerClient` 模式用）。没配返回 `None`。
    pub fn for_client(&self, client: &str) -> Option<&str> {
        self.per_client.get(client).map(|s| s.as_str())
    }

    /// 修剪空白、丢掉空条目。
    ///
    /// 界面上「跟随全局」会存成空字符串，那种条目等同于没配 —— 在入口处清理掉，
    /// 免得判定逻辑到处都要 filter 一遍，也免得空串被当成一个真实的模型名发出去。
    pub fn normalized(mut self) -> Self {
        self.active_model = self
            .active_model
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());

        self.per_client = self
            .per_client
            .into_iter()
            .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
            .filter(|(k, v)| !k.is_empty() && !v.is_empty())
            .collect();

        self
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AppSettings {
    /// 网关监听地址。默认仅本机可见。
    pub listen_host: String,
    pub listen_port: u16,
    /// 应用启动时自动拉起网关。
    pub autostart_gateway: bool,

    /// 上游首字节超时：连接建立后多久没收到第一个字节就判失败。
    pub first_byte_timeout_ms: u64,
    /// 流式空闲超时：两个 chunk 之间允许的最大间隔。
    pub idle_timeout_ms: u64,
    /// 非流式请求整体超时。
    pub request_timeout_ms: u64,

    /// 是否捕获并保留请求/响应原文。
    pub capture_enabled: bool,
    /// 捕获记录保留条数上限。
    pub capture_max_entries: u32,

    pub cache_enabled: bool,
    pub cache_ttl_secs: u64,
    pub cache_max_entries: u32,

    /// 全局模型替换。默认关闭。
    pub model_policy: ModelPolicy,
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            listen_host: "127.0.0.1".to_string(),
            listen_port: DEFAULT_PORT,
            autostart_gateway: true,

            first_byte_timeout_ms: 60_000,
            idle_timeout_ms: 300_000,
            request_timeout_ms: 600_000,

            capture_enabled: true,
            capture_max_entries: 500,

            cache_enabled: false, // 默认关：缓存会改变语义，需用户显式开启
            cache_ttl_secs: 3600,
            cache_max_entries: 1000,

            model_policy: ModelPolicy::default(), // 默认关闭
        }
    }
}

impl AppSettings {
    pub fn listen_addr(&self) -> String {
        format!("{}:{}", self.listen_host, self.listen_port)
    }

    /// 把用户输入夹到合法区间。
    ///
    /// 端口 0 是合法的（绑定随机可用端口），但 1024 以下在部分平台需要特权，
    /// 网关只服务本机客户端，没有理由占用特权端口 —— 一律拒绝并回落默认值。
    pub fn normalized(mut self) -> Self {
        if self.listen_port < 1024 {
            self.listen_port = DEFAULT_PORT;
        }
        if self.listen_host.trim().is_empty() {
            self.listen_host = "127.0.0.1".to_string();
        }
        self.first_byte_timeout_ms = self.first_byte_timeout_ms.clamp(1_000, 600_000);
        self.idle_timeout_ms = self.idle_timeout_ms.clamp(1_000, 3_600_000);
        self.request_timeout_ms = self.request_timeout_ms.clamp(1_000, 3_600_000);
        self.cache_max_entries = self.cache_max_entries.clamp(1, 100_000);
        self.capture_max_entries = self.capture_max_entries.clamp(1, 100_000);
        self.model_policy = self.model_policy.normalized();
        self
    }

    /// 从数据库加载；缺失或损坏时回落默认值（不阻塞启动）。
    pub async fn load(pool: &SqlitePool) -> AppResult<Self> {
        let raw: Option<String> =
            sqlx::query_scalar("SELECT value FROM settings_kv WHERE key = ?1")
                .bind(SETTINGS_KEY)
                .fetch_optional(pool)
                .await?;

        Ok(match raw {
            Some(json) => match serde_json::from_str(&json) {
                Ok(s) => s,
                Err(e) => {
                    // 设置损坏不该让应用起不来，记录后走默认值。
                    tracing::warn!("设置解析失败，使用默认值: {e}");
                    Self::default()
                }
            },
            None => Self::default(),
        })
    }

    pub async fn save(&self, pool: &SqlitePool) -> AppResult<()> {
        let json = serde_json::to_string(self)?;
        sqlx::query(
            "INSERT INTO settings_kv (key, value, updated_at) VALUES (?1, ?2, ?3)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
        )
        .bind(SETTINGS_KEY)
        .bind(json)
        .bind(crate::util::now_ms())
        .execute(pool)
        .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn load_returns_defaults_when_absent() {
        let pool = crate::storage::db::open_memory().await.unwrap();
        let s = AppSettings::load(&pool).await.unwrap();
        assert_eq!(s.listen_port, DEFAULT_PORT);
        assert_eq!(s.listen_host, "127.0.0.1");
        assert!(s.autostart_gateway);
    }

    #[tokio::test]
    async fn save_then_load_roundtrips() {
        let pool = crate::storage::db::open_memory().await.unwrap();
        let mut s = AppSettings::default();
        s.listen_port = 9999;
        s.cache_enabled = true;
        s.save(&pool).await.unwrap();

        let loaded = AppSettings::load(&pool).await.unwrap();
        assert_eq!(loaded.listen_port, 9999);
        assert!(loaded.cache_enabled);
    }

    #[tokio::test]
    async fn save_overwrites_existing_row() {
        let pool = crate::storage::db::open_memory().await.unwrap();
        let mut s = AppSettings::default();
        s.save(&pool).await.unwrap();
        s.listen_port = 1234;
        s.save(&pool).await.unwrap();

        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM settings_kv WHERE key = ?1")
            .bind(SETTINGS_KEY)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 1, "重复保存不应产生多行");
        assert_eq!(AppSettings::load(&pool).await.unwrap().listen_port, 1234);
    }

    #[tokio::test]
    async fn corrupt_settings_fall_back_to_defaults() {
        let pool = crate::storage::db::open_memory().await.unwrap();
        sqlx::query("INSERT INTO settings_kv (key, value, updated_at) VALUES (?1, ?2, 0)")
            .bind(SETTINGS_KEY)
            .bind("{ this is not json")
            .execute(&pool)
            .await
            .unwrap();

        let s = AppSettings::load(&pool).await.unwrap();
        assert_eq!(s.listen_port, DEFAULT_PORT);
        assert_eq!(s.model_policy.mode, ModelPolicyMode::Off);
    }

    #[test]
    fn model_policy_normalization_drops_blank_entries() {
        let mut p = ModelPolicy {
            mode: ModelPolicyMode::PerClient,
            active_model: Some("  deepseek-chat  ".into()),
            ..Default::default()
        };
        p.per_client.insert(" codex ".into(), " gpt-5 ".into());
        p.per_client.insert("cursor".into(), "".into());
        p.per_client.insert("".into(), "orphan".into());

        let n = p.normalized();
        assert_eq!(n.active_model.as_deref(), Some("deepseek-chat"));
        assert_eq!(n.per_client.len(), 1, "空值与空键都该被丢掉");
        assert_eq!(n.for_client("codex"), Some("gpt-5"));
    }

    #[test]
    fn blank_active_model_becomes_none() {
        let p = ModelPolicy {
            mode: ModelPolicyMode::Always,
            active_model: Some("   ".into()),
            ..Default::default()
        };
        assert_eq!(p.normalized().active_model, None);
    }
}
