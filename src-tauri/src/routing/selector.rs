//! Selector：运行时热切换选中的渠道，不需要重启。
//!
//! 核心是「读侧无锁、写侧原子替换」：
//! 每次请求只做一次 `ArcSwap` 读 + 一次 `DashMap` 查表，拿到的 `Arc<dyn Outbound>`
//! 在请求生命周期内是稳定的 —— 即使此刻前端切换了渠道，在途请求仍走旧渠道跑完，
//! 新请求立刻走新渠道。

use std::sync::Arc;

use arc_swap::{ArcSwap, ArcSwapOption};
use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

use crate::error::{AppError, AppResult};
use crate::upstream::outbound::Outbound;
use crate::upstream::registry::ProviderRegistry;

/// selector 的选路模式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SelectorMode {
    /// 手动切换：由用户或 API 调用决定当前选中项。
    Selector,
    /// 自动测速：后台探测各成员延迟，自动选最快的。
    Urltest,
}

impl SelectorMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Selector => "selector",
            Self::Urltest => "urltest",
        }
    }

    pub fn parse(s: &str) -> Self {
        match s {
            "urltest" => Self::Urltest,
            _ => Self::Selector,
        }
    }
}

/// 切换发生时广播给订阅者的事件。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SelectionEvent {
    pub selector: String,
    pub provider_tag: String,
    pub reason: String,
}

/// 一个 selector 实例。
pub struct Selector {
    tag: String,
    registry: Arc<ProviderRegistry>,
    members: ArcSwap<Vec<String>>,
    current: ArcSwapOption<String>,
    mode: ArcSwap<SelectorMode>,
    tolerance_ms: ArcSwap<i64>,
    events: broadcast::Sender<SelectionEvent>,
}

impl Selector {
    pub fn new(
        tag: impl Into<String>,
        registry: Arc<ProviderRegistry>,
        members: Vec<String>,
        current: Option<String>,
        mode: SelectorMode,
        tolerance_ms: i64,
    ) -> Self {
        // 容量 64：订阅者是 UI 事件转发，落后就丢，不该反压请求路径。
        let (events, _) = broadcast::channel(64);
        Self {
            tag: tag.into(),
            registry,
            members: ArcSwap::from_pointee(members),
            current: ArcSwapOption::from(current.map(Arc::new)),
            mode: ArcSwap::from_pointee(mode),
            tolerance_ms: ArcSwap::from_pointee(tolerance_ms),
            events,
        }
    }

    pub fn tag(&self) -> &str {
        &self.tag
    }

    pub fn mode(&self) -> SelectorMode {
        **self.mode.load()
    }

    pub fn tolerance_ms(&self) -> i64 {
        **self.tolerance_ms.load()
    }

    pub fn members(&self) -> Vec<String> {
        self.members.load().as_ref().clone()
    }

    pub fn is_member(&self, tag: &str) -> bool {
        self.members.load().iter().any(|m| m == tag)
    }

    /// 当前选中的渠道 tag。
    pub fn selected_tag(&self) -> Option<String> {
        self.current.load().as_ref().map(|t| t.as_ref().clone())
    }

    /// 当前选中的出站。
    ///
    /// 返回 `None` 的两种情况：尚未选择，或选中的 tag 已从注册表里消失
    /// （渠道被删除）。后者由调用方决定是否回落。
    pub fn selected(&self) -> Option<Arc<dyn Outbound>> {
        self.selected_tag().and_then(|t| self.registry.get(&t))
    }

    /// 切换选中项。
    ///
    /// 只接受成员列表内的 tag —— 否则一个 selectors 表里的手误就能把流量
    /// 导到任意渠道去。持久化由 [`SelectorManager`] 负责。
    pub fn set(&self, provider_tag: &str, reason: &str) -> AppResult<()> {
        if !self.is_member(provider_tag) {
            return Err(AppError::msg(format!(
                "渠道「{provider_tag}」不是 selector「{}」的成员",
                self.tag
            )));
        }

        let previous = self.selected_tag();
        if previous.as_deref() == Some(provider_tag) {
            return Ok(());
        }

        self.current.store(Some(Arc::new(provider_tag.to_string())));

        // 广播失败只意味着当前没有订阅者，不是错误。
        let _ = self.events.send(SelectionEvent {
            selector: self.tag.clone(),
            provider_tag: provider_tag.to_string(),
            reason: reason.to_string(),
        });

        tracing::info!(
            selector = %self.tag,
            from = ?previous,
            to = provider_tag,
            reason,
            "selector 已切换"
        );
        Ok(())
    }

    /// 更新成员列表。若当前选中项已不在新成员里，自动回落到第一个成员。
    pub fn set_members(&self, members: Vec<String>) {
        self.members.store(Arc::new(members.clone()));

        let still_valid = self
            .selected_tag()
            .map(|t| members.contains(&t))
            .unwrap_or(false);

        if !still_valid {
            match members.first() {
                Some(first) => {
                    self.current.store(Some(Arc::new(first.clone())));
                    let _ = self.events.send(SelectionEvent {
                        selector: self.tag.clone(),
                        provider_tag: first.clone(),
                        reason: "成员变更后自动回落".into(),
                    });
                }
                None => self.current.store(None),
            }
        }
    }

    pub fn set_mode(&self, mode: SelectorMode) {
        self.mode.store(Arc::new(mode));
    }

    pub fn set_tolerance_ms(&self, ms: i64) {
        self.tolerance_ms.store(Arc::new(ms));
    }

    /// 订阅切换事件。
    pub fn subscribe(&self) -> broadcast::Receiver<SelectionEvent> {
        self.events.subscribe()
    }

    /// 启动时的选择恢复。
    ///
    /// 优先级：持久化的选择 → 第一个**实际存在**的成员 → 不选。
    /// 跳过不存在的成员很重要：渠道可能已被删除，硬选会导致所有请求失败。
    pub fn restore(&self, persisted: Option<String>) {
        if let Some(tag) = persisted {
            if self.is_member(&tag) && self.registry.contains(&tag) {
                self.current.store(Some(Arc::new(tag)));
                return;
            }
            tracing::warn!(
                selector = %self.tag,
                tag = %tag,
                "持久化的选中渠道已不存在，改为自动回落"
            );
        }

        let fallback = self
            .members()
            .into_iter()
            .find(|m| self.registry.contains(m));

        match fallback {
            Some(tag) => self.current.store(Some(Arc::new(tag))),
            None => self.current.store(None),
        }
    }
}

/// 所有 selector 的集合，负责实例化与持久化。
pub struct SelectorManager {
    by_tag: DashMap<String, Arc<Selector>>,
    registry: Arc<ProviderRegistry>,
}

impl SelectorManager {
    pub fn new(registry: Arc<ProviderRegistry>) -> Self {
        Self {
            by_tag: DashMap::new(),
            registry,
        }
    }

    pub fn get(&self, tag: &str) -> Option<Arc<Selector>> {
        self.by_tag.get(tag).map(|e| e.value().clone())
    }

    pub fn insert(&self, selector: Arc<Selector>) {
        self.by_tag.insert(selector.tag().to_string(), selector);
    }

    pub fn remove(&self, tag: &str) -> Option<Arc<Selector>> {
        self.by_tag.remove(tag).map(|(_, v)| v)
    }

    pub fn tags(&self) -> Vec<String> {
        let mut v: Vec<String> = self.by_tag.iter().map(|e| e.key().clone()).collect();
        v.sort();
        v
    }

    pub fn all(&self) -> Vec<Arc<Selector>> {
        self.by_tag.iter().map(|e| e.value().clone()).collect()
    }

    pub fn len(&self) -> usize {
        self.by_tag.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_tag.is_empty()
    }

    /// 解析请求应使用的出站。
    ///
    /// 这是请求路径上的热点函数：`selector` 不存在时直接回落到注册表的默认渠道，
    /// 保证「配置没写完」不会让整个网关不可用。
    pub fn resolve(&self, selector_tag: &str) -> Option<Arc<dyn Outbound>> {
        match self.get(selector_tag) {
            Some(s) => s.selected().or_else(|| self.registry.default()),
            None => self.registry.default(),
        }
    }

    pub fn clear(&self) {
        self.by_tag.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::models::{AuthStyle, Provider, ProviderKind};
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
            protocols: Vec::new(),
            extra_headers: IndexMap::new(),
            param_override: None,
            model_mapping: IndexMap::new(),
            weight: 1,
            priority: 0,
            enabled: true,
            timeout_ms: 600_000,
            proxy: Default::default(),
            created_at: 0,
            updated_at: 0,
        }
    }

    fn setup(tags: &[&str]) -> (Arc<ProviderRegistry>, Arc<Selector>) {
        let registry = Arc::new(ProviderRegistry::new(reqwest::Client::new()));
        registry.reload(&tags.iter().map(|t| provider(t)).collect::<Vec<_>>());
        let sel = Arc::new(Selector::new(
            "default",
            registry.clone(),
            tags.iter().map(|t| t.to_string()).collect(),
            None,
            SelectorMode::Selector,
            50,
        ));
        (registry, sel)
    }

    #[test]
    fn set_switches_selected_channel() {
        let (_r, s) = setup(&["a", "b"]);
        s.set("a", "test").unwrap();
        assert_eq!(s.selected_tag().as_deref(), Some("a"));
        assert_eq!(s.selected().unwrap().tag(), "a");

        s.set("b", "test").unwrap();
        assert_eq!(s.selected_tag().as_deref(), Some("b"));
    }

    #[test]
    fn set_rejects_non_member() {
        let (r, s) = setup(&["a"]);
        r.reload(&[provider("a"), provider("outsider")]);
        let err = s.set("outsider", "test").unwrap_err();
        assert!(err.to_string().contains("不是 selector"), "实际: {err}");
    }

    #[test]
    fn set_to_same_value_is_a_noop() {
        let (_r, s) = setup(&["a"]);
        s.set("a", "first").unwrap();
        let mut rx = s.subscribe();

        s.set("a", "again").unwrap();
        assert!(
            rx.try_recv().is_err(),
            "值未变化时不应广播事件，否则 UI 会抖动"
        );
    }

    #[test]
    fn switch_broadcasts_event() {
        let (_r, s) = setup(&["a", "b"]);
        let mut rx = s.subscribe();
        s.set("b", "用户手动切换").unwrap();

        let ev = rx.try_recv().unwrap();
        assert_eq!(ev.selector, "default");
        assert_eq!(ev.provider_tag, "b");
        assert_eq!(ev.reason, "用户手动切换");
    }

    #[test]
    fn selected_is_none_when_channel_was_deleted() {
        let (r, s) = setup(&["a"]);
        s.set("a", "test").unwrap();
        assert!(s.selected().is_some());

        // 渠道被删掉，但 selector 还指着它
        r.reload(&[]);
        assert!(s.selected().is_none(), "应安全返回 None 而不是 panic");
    }

    #[test]
    fn set_members_falls_back_when_current_removed() {
        let (_r, s) = setup(&["a", "b"]);
        s.set("a", "test").unwrap();
        s.set_members(vec!["b".into()]);
        assert_eq!(s.selected_tag().as_deref(), Some("b"));
    }

    #[test]
    fn set_members_keeps_current_when_still_present() {
        let (_r, s) = setup(&["a", "b"]);
        s.set("b", "test").unwrap();
        s.set_members(vec!["b".into(), "c".into()]);
        assert_eq!(s.selected_tag().as_deref(), Some("b"), "仍在成员里就不该变");
    }

    #[test]
    fn set_members_to_empty_clears_selection() {
        let (_r, s) = setup(&["a"]);
        s.set("a", "test").unwrap();
        s.set_members(vec![]);
        assert!(s.selected_tag().is_none());
    }

    #[test]
    fn restore_prefers_persisted_choice() {
        let (_r, s) = setup(&["a", "b"]);
        s.restore(Some("b".into()));
        assert_eq!(s.selected_tag().as_deref(), Some("b"));
    }

    #[test]
    fn restore_skips_persisted_tag_that_no_longer_exists() {
        let (r, s) = setup(&["a"]);
        // b 在成员列表里，但注册表里没有
        s.set_members(vec!["b".into(), "a".into()]);
        r.reload(&[provider("a")]);

        s.restore(Some("b".into()));
        assert_eq!(
            s.selected_tag().as_deref(),
            Some("a"),
            "不存在的渠道不能被选中，否则请求全挂"
        );
    }

    #[test]
    fn restore_falls_back_to_first_available_member() {
        let (r, s) = setup(&["gone", "live"]);
        r.reload(&[provider("live")]);
        s.restore(None);
        assert_eq!(s.selected_tag().as_deref(), Some("live"));
    }

    #[test]
    fn restore_leaves_nothing_selected_when_no_member_available() {
        let (r, s) = setup(&["gone"]);
        r.reload(&[]);
        s.restore(None);
        assert!(s.selected_tag().is_none());
    }

    #[test]
    fn mode_and_tolerance_are_updatable() {
        let (_r, s) = setup(&["a"]);
        assert_eq!(s.mode(), SelectorMode::Selector);

        s.set_mode(SelectorMode::Urltest);
        s.set_tolerance_ms(200);
        assert_eq!(s.mode(), SelectorMode::Urltest);
        assert_eq!(s.tolerance_ms(), 200);
    }

    #[test]
    fn changed_selector_member_sees_new_channel_without_restart() {
        // 这条对应验收标准：切到 B 之后，新请求立刻走 B。
        let (r, s) = setup(&["a", "b"]);
        s.set("a", "test").unwrap();
        assert_eq!(s.selected().unwrap().tag(), "a");

        // 中途新增渠道并切换
        r.reload(&[provider("a"), provider("b"), provider("c")]);
        s.set_members(vec!["a".into(), "b".into(), "c".into()]);
        s.set("c", "test").unwrap();
        assert_eq!(s.selected().unwrap().tag(), "c");
    }

    // --- SelectorManager ---

    #[test]
    fn manager_resolves_through_selector() {
        let (r, s) = setup(&["a", "b"]);
        let m = SelectorManager::new(r);
        s.set("b", "test").unwrap();
        m.insert(s);

        let out = m.resolve("default").unwrap();
        assert_eq!(out.tag(), "b");
    }

    #[test]
    fn manager_falls_back_to_registry_default_for_unknown_selector() {
        let registry = Arc::new(ProviderRegistry::new(reqwest::Client::new()));
        registry.reload(&[provider("fallback")]);
        registry.set_default(Some("fallback".into()));

        let m = SelectorManager::new(registry);
        let out = m.resolve("does-not-exist").unwrap();
        assert_eq!(out.tag(), "fallback", "配置没写完时不该让网关整体不可用");
    }

    #[test]
    fn manager_falls_back_when_selector_has_no_members() {
        // 首次启动时 default selector 是空的（用户还没加渠道），
        // 加完渠道后应当能直接跑通常路，而不是一路 503。
        let registry = Arc::new(ProviderRegistry::new(reqwest::Client::new()));
        registry.reload(&[provider("only-one")]);
        registry.set_default(Some("only-one".into()));

        let m = SelectorManager::new(registry.clone());
        m.insert(Arc::new(Selector::new(
            "default",
            registry,
            vec![], // 空成员表
            None,
            SelectorMode::Selector,
            50,
        )));

        let out = m.resolve("default").unwrap();
        assert_eq!(out.tag(), "only-one");
    }

    #[test]
    fn manager_resolve_is_none_when_nothing_configured() {
        let registry = Arc::new(ProviderRegistry::new(reqwest::Client::new()));
        let m = SelectorManager::new(registry);
        assert!(m.resolve("default").is_none());
    }

    #[test]
    fn manager_remove_drops_selector() {
        let (r, s) = setup(&["a"]);
        let m = SelectorManager::new(r);
        m.insert(s);
        assert_eq!(m.len(), 1);
        assert!(m.remove("default").is_some());
        assert!(m.get("default").is_none());
    }

    #[test]
    fn manager_tags_are_sorted() {
        let registry = Arc::new(ProviderRegistry::new(reqwest::Client::new()));
        registry.reload(&[provider("a")]);
        let m = SelectorManager::new(registry.clone());
        for t in ["zeta", "alpha"] {
            m.insert(Arc::new(Selector::new(
                t,
                registry.clone(),
                vec!["a".into()],
                None,
                SelectorMode::Selector,
                50,
            )));
        }
        assert_eq!(m.tags(), vec!["alpha", "zeta"]);
    }
}
