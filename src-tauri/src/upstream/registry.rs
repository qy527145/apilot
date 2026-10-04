//! 渠道注册表：按 tag 索引所有可用出站，并记录默认兜底。

use std::sync::Arc;

use arc_swap::ArcSwapOption;
use dashmap::DashMap;

use super::channel::Channel;
use super::outbound::Outbound;
use crate::storage::models::Provider;

/// 全部渠道的热可换注册表。
///
/// 读侧（每请求一次）走 `DashMap` 分片读与 `ArcSwap` 无锁读；
/// 写侧（前端改配置后）整体替换，在途请求持有的旧 `Arc` 继续有效。
pub struct ProviderRegistry {
    by_tag: DashMap<String, Arc<dyn Outbound>>,
    default_tag: ArcSwapOption<String>,
    client: reqwest::Client,
}

impl ProviderRegistry {
    pub fn new(client: reqwest::Client) -> Self {
        Self {
            by_tag: DashMap::new(),
            default_tag: ArcSwapOption::empty(),
            client,
        }
    }

    pub fn client(&self) -> &reqwest::Client {
        &self.client
    }

    pub fn insert(&self, outbound: Arc<dyn Outbound>) {
        self.by_tag.insert(outbound.tag().to_string(), outbound);
    }

    /// 按 tag 取出渠道。返回的是 `Arc`，调用方拿到后可安全 await（无锁守卫残留）。
    pub fn get(&self, tag: &str) -> Option<Arc<dyn Outbound>> {
        self.by_tag.get(tag).map(|e| e.value().clone())
    }

    pub fn contains(&self, tag: &str) -> bool {
        self.by_tag.contains_key(tag)
    }

    pub fn len(&self) -> usize {
        self.by_tag.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_tag.is_empty()
    }

    /// 所有渠道的 tag，按字典序排好便于前端稳定展示。
    pub fn tags(&self) -> Vec<String> {
        let mut v: Vec<String> = self.by_tag.iter().map(|e| e.key().clone()).collect();
        v.sort();
        v
    }

    /// 全量替换。旧渠道的 `Arc` 若仍被在途请求持有，会自然存活到请求结束。
    pub fn reload(&self, providers: &[Provider]) {
        self.by_tag.clear();
        for p in providers {
            let ch = Channel::new(p.clone(), self.client.clone());
            self.insert(Arc::new(ch));
        }
    }

    pub fn set_default(&self, tag: Option<String>) {
        self.default_tag.store(tag.map(Arc::new));
    }

    pub fn default_tag(&self) -> Option<String> {
        self.default_tag.load().as_ref().map(|t| t.as_ref().clone())
    }

    /// 默认渠道。tag 未设置或指向不存在的渠道时返回 `None`。
    pub fn default(&self) -> Option<Arc<dyn Outbound>> {
        self.default_tag() .and_then(|t| self.get(&t))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::models::{AuthStyle, ProviderKind};
    use indexmap::IndexMap;

    fn provider(tag: &str) -> Provider {
        Provider {
            id: 1,
            tag: tag.into(),
            name: tag.into(),
            kind: ProviderKind::Anthropic,
            base_url: "https://x.example.com".into(),
            api_key: Some("k".into()),
            auth_style: AuthStyle::XApiKey,
            extra_headers: IndexMap::new(),
            param_override: None,
            model_mapping: IndexMap::new(),
            weight: 1,
            priority: 0,
            enabled: true,
            timeout_ms: 600_000,
            created_at: 0,
            updated_at: 0,
        }
    }

    fn registry() -> ProviderRegistry {
        ProviderRegistry::new(reqwest::Client::new())
    }

    #[test]
    fn reload_replaces_all_channels() {
        let r = registry();
        r.reload(&[provider("a"), provider("b")]);
        assert_eq!(r.len(), 2);
        assert!(r.contains("a"));

        r.reload(&[provider("c")]);
        assert_eq!(r.len(), 1);
        assert!(!r.contains("a"), "旧渠道应被清掉");
        assert!(r.contains("c"));
    }

    #[test]
    fn get_returns_none_for_unknown_tag() {
        let r = registry();
        r.reload(&[provider("a")]);
        assert!(r.get("nope").is_none());
    }

    #[test]
    fn default_resolves_to_tagged_channel() {
        let r = registry();
        r.reload(&[provider("a"), provider("b")]);
        r.set_default(Some("b".into()));
        assert_eq!(r.default().unwrap().tag(), "b");
    }

    #[test]
    fn default_is_none_when_tag_points_nowhere() {
        // 渠道被删掉但 default 还指着它：应安全返回 None，而不是 panic
        let r = registry();
        r.reload(&[provider("a")]);
        r.set_default(Some("gone".into()));
        assert!(r.default().is_none());
    }

    #[test]
    fn default_is_none_when_unset() {
        let r = registry();
        r.reload(&[provider("a")]);
        assert!(r.default().is_none());
    }

    #[test]
    fn tags_are_sorted_for_stable_ui() {
        let r = registry();
        r.reload(&[provider("zeta"), provider("alpha"), provider("mu")]);
        assert_eq!(r.tags(), vec!["alpha", "mu", "zeta"]);
    }

    #[test]
    fn channel_exposes_its_wire_protocol() {
        let r = registry();
        r.reload(&[provider("a")]);
        assert_eq!(
            r.get("a").unwrap().wire(),
            crate::protocol::dto::Protocol::AnthropicMessages
        );
    }
}
