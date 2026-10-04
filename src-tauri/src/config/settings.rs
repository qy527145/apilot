//! 应用设置：以单条 JSON 存在 `settings_kv` 表里。
//!
//! 配置读多写极少，运行期用 `ArcSwap<AppSettings>` 承载，网关侧每请求无锁读。

use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

use crate::error::AppResult;

const SETTINGS_KEY: &str = "app_settings";

/// 默认监听端口。选 8787 避开常见的 8080/3000 冲突。
pub const DEFAULT_PORT: u16 = 8787;

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
    }
}
