//! 应用设置：以单条 JSON 存在 `settings_kv` 表里。
//!
//! 配置读多写极少，运行期用 `ArcSwap<AppSettings>` 承载，网关侧每请求无锁读。

use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

use crate::error::AppResult;

const SETTINGS_KEY: &str = "app_settings";

/// 默认监听端口。选 8787 避开常见的 8080/3000 冲突。
pub const DEFAULT_PORT: u16 = 8787;

/// 模型替换的模式。
///
/// 全局与客户端两级用的是同一套模式，区别只在客户端那边多一个「跟随全局」
/// （见 [`ClientMode`]）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelPolicyMode {
    /// 不改写，用客户端请求的那个。默认值 —— 不配就完全保持原有行为。
    #[default]
    // 老存档写的是 "off"。认不出来不是小事：`AppSettings` 是整份 JSON 反序列化，
    // 一个认不出的枚举串会让 `load()` 落到 `Self::default()`，把用户的端口、超时、
    // 缓存设置一起冲掉。
    #[serde(alias = "off")]
    Passthrough,
    /// 任何入站模型名都换成选定的模型。
    Always,
    /// 只当请求的模型在 Apilot 里没有任何可用渠道时才替换。
    ///
    /// 适合"平时用某个模型，顺手让别的也能跑"：客户端要的模型配了渠道就用它，
    /// 没配才落到选定的那个。
    Fallback,
    /// 自定义规则：映射表或 JS 脚本，二选一，见 [`CustomRules`]。
    Custom,
    /// 旧版的「按客户端分别指定」。
    ///
    /// 只为读得懂老存档而存在 —— `normalized()` 会立刻把它折算成上面四种之一，
    /// 判定逻辑永远见不到它。
    #[serde(rename = "per_client")]
    PerClientLegacy,
    /// 认不出的取值（手改 JSON、装了更新版本又降级）。`normalized()` 把它当
    /// 「不改写」处理，同样是 `#[serde(default)]` 救不了的那种错误。
    #[serde(other)]
    Unknown,
}

/// 客户端的模式：与全局那四种一样，多一个「跟随全局」。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClientMode {
    /// 跟随全局。默认，也等同于这条客户端配置不存在。
    #[default]
    Inherit,
    Passthrough,
    Always,
    Fallback,
    Custom,
    /// 同 [`ModelPolicyMode::Unknown`]。
    #[serde(other)]
    Unknown,
}

impl ClientMode {
    /// 折算成生效模式；`None` = 跟随全局。
    pub fn to_policy_mode(self) -> Option<ModelPolicyMode> {
        match self {
            Self::Inherit | Self::Unknown => None,
            Self::Passthrough => Some(ModelPolicyMode::Passthrough),
            Self::Always => Some(ModelPolicyMode::Always),
            Self::Fallback => Some(ModelPolicyMode::Fallback),
            Self::Custom => Some(ModelPolicyMode::Custom),
        }
    }
}

/// 自定义规则的两种写法。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CustomForm {
    /// 映射表：自上而下，首个命中生效。默认。
    #[default]
    Table,
    /// 高级：JavaScript。
    Script,
    #[serde(other)]
    Unknown,
}

/// 映射表一行的匹配方式。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MatchKind {
    /// 前缀。
    #[default]
    Prefix,
    /// 通配：`*` 任意长度、`?` 单个字符。
    Glob,
    /// 正则。
    Regex,
    /// 完全相同。
    Exact,
    /// 无条件命中 —— 就是兜底那一行，不需要表达式。
    Any,
    /// 认不出的取值。有它才不会让一个拼错的字符串把**整份**设置打回默认值；
    /// `normalized()` 会把这行丢掉。
    #[serde(other)]
    Unknown,
}

/// 映射表的一行。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MappingRow {
    /// 可选：只对这个客户端生效；空 = 任何客户端都适用。
    pub client: Option<String>,
    pub match_kind: MatchKind,
    /// 表达式；`Any` 时忽略。
    pub pattern: Option<String>,
    /// 命中后换成它。`None` = 「保持原样」：在此停下，不改写。
    pub target: Option<String>,
}

impl MappingRow {
    fn normalized(mut self) -> Self {
        self.client = trimmed(&self.client);
        self.pattern = trimmed(&self.pattern);
        self.target = trimmed(&self.target);
        self
    }
}

/// 自定义规则的两套写法。
///
/// 两套**同时保存**：在界面上切换「映射表 / JavaScript」不该丢掉另一套，
/// 用户来回试的时候尤其明显。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CustomRules {
    pub form: CustomForm,
    pub table: Vec<MappingRow>,
    /// JS 源码，须定义一个 `resolve(ctx)`。见 `routing/model_script.rs`。
    pub script: Option<String>,
}

impl CustomRules {
    /// 两套都空 —— 这条规则什么也不做。
    pub fn is_empty(&self) -> bool {
        self.table.is_empty() && self.script.is_none()
    }

    fn normalized(mut self) -> Self {
        self.script = trimmed(&self.script);
        if self.form == CustomForm::Unknown {
            self.form = CustomForm::Table;
        }

        self.table = self
            .table
            .into_iter()
            .map(MappingRow::normalized)
            .filter(|r| match r.match_kind {
                MatchKind::Unknown => false,
                // 兜底行不需要表达式；其余写法里空表达式等同于没写。
                MatchKind::Any => true,
                _ => r.pattern.is_some(),
            })
            // 编译不过的正则也在这里丢掉：判定逻辑因此不用再处理"编译失败"
            // 这条分支。前端会即时校验并拦在保存之前，这只是兜底。
            .filter(|r| {
                r.match_kind != MatchKind::Regex
                    || r.pattern.as_deref().is_some_and(is_valid_regex)
            })
            .collect();

        self
    }
}

/// 单个客户端的覆盖配置。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ClientRule {
    pub mode: ClientMode,
    /// `Always` / `Fallback` 用的模型；留空则沿用全局的 `active_model`。
    pub model: Option<String>,
    /// `Custom` 用的规则。
    pub custom: CustomRules,
}

/// 客户端覆盖在存档里的两种形态。
///
/// `untagged` 是为了读得懂老存档：那时 `per_client` 的值直接就是模型名。
/// 少了它，老设置会因为类型不匹配而整份回落默认值。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ClientOverride {
    /// 旧格式 `{"codex": "gpt-5"}` —— 等价于「无条件换成 gpt-5」。
    Legacy(String),
    Rule(ClientRule),
}

impl ClientOverride {
    fn normalized(self) -> Self {
        match self {
            Self::Legacy(model) => Self::Rule(ClientRule {
                mode: ClientMode::Always,
                model: trimmed(&Some(model)),
                custom: CustomRules::default(),
            }),
            Self::Rule(mut rule) => {
                // 跟随全局时把其余字段一并清空：留着既不会生效，又会让下面
                // 那个「整条都是跟随全局」的判据失灵，条目就永远删不掉了。
                if rule.mode == ClientMode::Inherit || rule.mode == ClientMode::Unknown {
                    return Self::Rule(ClientRule::default());
                }
                rule.model = trimmed(&rule.model);
                rule.custom = rule.custom.normalized();
                Self::Rule(rule)
            }
        }
    }

    /// 整条都是「跟随全局」——留着没有任何作用，只是判定逻辑的噪音。
    fn is_inherit(&self) -> bool {
        match self {
            Self::Legacy(_) => false,
            Self::Rule(r) => {
                r.mode == ClientMode::Inherit && r.model.is_none() && r.custom.is_empty()
            }
        }
    }
}

/// 两级配置拼出来的、这一次请求真正生效的那条规则。
#[derive(Debug, Clone, Copy)]
pub struct ResolvedRule<'a> {
    pub mode: ModelPolicyMode,
    pub model: Option<&'a str>,
    pub custom: &'a CustomRules,
}

/// 模型替换策略。
///
/// 两级：全局一份，客户端一份。客户端那份只覆盖它写了的字段，其余沿用全局 ——
/// 所以「给 Codex 单独换个模型」不必把全局的模型也改一遍。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ModelPolicy {
    pub mode: ModelPolicyMode,
    /// `Always` / `Fallback` 的目标模型；客户端规则没写模型时也用它。
    pub active_model: Option<String>,
    /// 客户端级覆盖：客户端标识 → 覆盖配置。
    ///
    /// 字段名沿用旧版的 `per_client`（值类型换成了 [`ClientOverride`]），
    /// 这样新老存档都能读。
    pub per_client: indexmap::IndexMap<String, ClientOverride>,
    /// 全局 `Custom` 用的规则。
    pub custom: CustomRules,
}

impl ModelPolicy {
    /// 该客户端真正生效的那条规则。
    pub fn effective(&self, client: &str) -> ResolvedRule<'_> {
        let global = ResolvedRule {
            mode: self.mode,
            model: self.active_model.as_deref(),
            custom: &self.custom,
        };

        // 没配过的客户端就是跟随全局。
        let Some(entry) = self.per_client.get(client) else {
            return global;
        };

        match entry {
            ClientOverride::Legacy(model) => ResolvedRule {
                mode: ModelPolicyMode::Always,
                model: Some(model.as_str()),
                ..global
            },
            ClientOverride::Rule(rule) => {
                let Some(mode) = rule.mode.to_policy_mode() else {
                    return global; // 跟随全局
                };
                match mode {
                    // 自定义规则整套由客户端自己带，不跟全局混。
                    ModelPolicyMode::Custom => ResolvedRule {
                        mode,
                        model: global.model,
                        custom: &rule.custom,
                    },
                    // 客户端只改了模式、没写模型时沿用全局那个 —— 与旧版
                    // 「没配到的客户端用 active_model」是同一个意思。
                    _ => ResolvedRule {
                        mode,
                        model: rule.model.as_deref().or(global.model),
                        custom: &rule.custom,
                    },
                }
            }
        }
    }

    /// 修剪空白、丢掉空条目，并把旧存档折算成新格式。
    pub fn normalized(mut self) -> Self {
        // 旧版的「按客户端分别指定」折算成等价的「全局 + 客户端覆盖」：
        //   全局 = 有模型就无条件替换，没有就不改写；
        //   每个客户端条目 = 无条件替换成它自己那个模型。
        // 折算后与旧的 resolve 逐条等价：配过的客户端拿自己的，没配的拿全局的。
        if self.mode == ModelPolicyMode::PerClientLegacy {
            self.mode = if trimmed(&self.active_model).is_some() {
                ModelPolicyMode::Always
            } else {
                ModelPolicyMode::Passthrough
            };
        }
        if self.mode == ModelPolicyMode::Unknown {
            self.mode = ModelPolicyMode::Passthrough;
        }

        self.active_model = trimmed(&self.active_model);
        self.custom = self.custom.normalized();

        self.per_client = self
            .per_client
            .into_iter()
            .map(|(k, v)| (k.trim().to_string(), v.normalized()))
            .filter(|(k, v)| !k.is_empty() && !v.is_inherit())
            .collect();

        self
    }
}

/// 修剪空白；空白串等同于没配。
///
/// 界面上「留空 = 跟随全局」「保持原样」都会存成空串，那种值必须在入口处清掉 ——
/// 否则空串会被当成一个真实的模型名发往上游。
fn trimmed(v: &Option<String>) -> Option<String> {
    v.as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn is_valid_regex(pattern: &str) -> bool {
    regex::Regex::new(pattern).is_ok()
}

/// 全局出站代理怎么选。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProxyMode {
    /// 强制直连：机器上配着 `HTTPS_PROXY` 也不走。
    Direct,
    /// 跟随环境变量（`HTTPS_PROXY` / `ALL_PROXY` / `HTTP_PROXY`，大小写都看）。
    /// 默认值 —— 与引入本功能之前的行为逐字等价。
    #[default]
    System,
    /// 用 `url` 指定的那个地址。
    Manual,
    /// 认不出的取值。理由同 `ModelPolicyMode::Unknown`：不能让一个拼错的串
    /// 把整份设置打回默认值，那会连带丢掉用户配的监听端口、超时、模型策略。
    #[serde(other)]
    Unknown,
}

/// 全局代理设置。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ProxySettings {
    pub mode: ProxyMode,
    /// `mode == Manual` 时的地址，支持 `http:// https:// socks5:// socks5h://`。
    pub url: Option<String>,
    /// 忽略上游 TLS 证书校验（等价于 `curl -k`）。
    ///
    /// 存在的理由只有一个：抓包工具（mitmproxy / Fiddler / Charles）要解密 HTTPS 就得
    /// 用自签的 CA 重新签一遍，而那个 CA 默认不在系统信任库里。装了 CA 是正路，
    /// 但装 CA 有门槛、有时也改不动（无管理员权限、公司管控的机器），这个开关是兜底。
    ///
    /// 代价必须说清楚：证书链与主机名都不再校验，中间人无法再被发现。所以它是
    /// **opt-in 且默认关**，开启时 `build_with` 会打一条 warn，界面上也要挂警示。
    ///
    /// 作用范围是**所有出站连接**，与走不走代理无关 —— 开关说的是"不校验对端证书"，
    /// 内网自签名证书的模型服务同样适用。绑在代理模式上会造出一个隐性状态：
    /// 拨了开关却因为代理没启用而静默地仍然严格校验。
    pub insecure_tls: bool,
}

impl ProxySettings {
    pub fn normalized(mut self) -> Self {
        if self.mode == ProxyMode::Unknown {
            self.mode = ProxyMode::System;
        }
        self.url = trimmed(&self.url);
        // 选了「自定义」却没填地址，等价于直连 —— 在 resolve 里体现，
        // 这里只把模式收回去，免得界面上显示成"自定义代理"却什么都没配。
        if self.mode == ProxyMode::Manual && self.url.is_none() {
            self.mode = ProxyMode::Direct;
        }
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

    /// 接管客户端时，把「当前配置的模型」也写进客户端配置。
    ///
    /// **为什么需要这个开关。** Codex 用不用 Responses Lite 只由模型元数据决定，而
    /// GPT 系名字内置就是 Lite（工具塞进 `input[].additional_tools`）—— 那个形状对
    /// 不少上游是**静默失效**的：收下请求、返回 200、工具一个不认，模型只能把调用
    /// 写成 DSML 正文。换成一个它不认识的名字，元数据就退回兜底那份（经典工具集）。
    ///
    /// **为什么默认关。** 它会覆盖用户在客户端里选的模型名；而按模型名配的路由规则、
    /// 每模型渠道选择都会跟着变。对"模型策略已经把模型定死"的人（`Always` / `Fallback`
    /// 且指定了 `active_model`）它是无副作用的，但那是用户自己该判断的事。
    #[serde(default)]
    pub inject_client_model: bool,

    /// 全局出站代理。默认跟随环境变量 —— 与引入本功能之前逐字等价。
    pub proxy: ProxySettings,
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
            inject_client_model: false,            // 默认不碰客户端选的模型
            proxy: ProxySettings::default(),      // 默认跟随环境变量
        }
    }
}

impl AppSettings {
    /// 接管 `client` 时要写进它配置的模型名；不该写时是 `None`。
    ///
    /// 三个条件缺一不可：开关开着、这个客户端的生效规则**指定了**模型、且那个模式
    /// 真的会用它。`Custom` 和 `Passthrough` 都没有一个「Apilot 指定的模型名」可写 ——
    /// 前者的模型要跑规则才知道，后者压根不改写，硬写一个名字反而会改变选路。
    pub fn client_model(&self, client: &str) -> Option<&str> {
        if !self.inject_client_model {
            return None;
        }
        let rule = self.model_policy.effective(client);
        match rule.mode {
            ModelPolicyMode::Always | ModelPolicyMode::Fallback => rule.model,
            _ => None,
        }
    }
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
        self.proxy = self.proxy.normalized();
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
            Some(json) => match serde_json::from_str::<Self>(&json) {
                Ok(mut s) => {
                    // 老存档里的旧模式要**在这里**就折算掉：判定逻辑不该为了兼容
                    // 永远带着一条分支，而且不折算的话，用户在下次「保存」之前的
                    // 行为跟之后不一致。
                    s.model_policy = s.model_policy.normalized();
                    s
                }
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
        assert_eq!(s.model_policy.mode, ModelPolicyMode::Passthrough);
    }

    #[test]
    fn legacy_off_mode_value_still_deserializes() {
        // 老存档写的是 "off"。读不出来不是小事：整份设置会回落默认值，
        // 用户的端口、超时、缓存设置一起没了。
        let p: ModelPolicy = serde_json::from_str(r#"{"mode":"off"}"#).unwrap();
        assert_eq!(p.mode, ModelPolicyMode::Passthrough);

        let json = serde_json::to_string(&p.normalized()).unwrap();
        assert!(json.contains("\"passthrough\""), "再存回去该写新名字: {json}");
    }

    #[tokio::test]
    async fn legacy_settings_do_not_reset_unrelated_fields() {
        // 这条是整份设置被冲掉的回归闸门：老存档里同时有旧模式与普通字段。
        let pool = crate::storage::db::open_memory().await.unwrap();
        sqlx::query("INSERT INTO settings_kv (key, value, updated_at) VALUES (?1, ?2, 0)")
            .bind(SETTINGS_KEY)
            .bind(
                r#"{"listen_port":9999,"listen_host":"0.0.0.0",
                    "model_policy":{"mode":"off","per_client":{"codex":"gpt-5"}}}"#,
            )
            .execute(&pool)
            .await
            .unwrap();

        let s = AppSettings::load(&pool).await.unwrap();
        assert_eq!(s.listen_port, 9999, "一个旧模式串不该把端口一起打回默认值");
        assert_eq!(s.listen_host, "0.0.0.0");
        assert_eq!(s.model_policy.mode, ModelPolicyMode::Passthrough);
    }

    #[test]
    fn legacy_per_client_mode_migrates_to_a_client_rule() {
        let old = r#"{"mode":"per_client","active_model":"deepseek-chat",
                      "per_client":{"codex":"gpt-5"}}"#;
        let p: ModelPolicy = serde_json::from_str(old).unwrap();
        let n = p.normalized();

        assert_eq!(n.mode, ModelPolicyMode::Always);
        assert_eq!(n.active_model.as_deref(), Some("deepseek-chat"));

        // 没配过的客户端拿全局那个 —— 与旧的「没配到的客户端用 active_model」一致。
        let other = n.effective("cursor");
        assert_eq!(other.mode, ModelPolicyMode::Always);
        assert_eq!(other.model, Some("deepseek-chat"));

        // 配过的客户端拿自己的。
        let codex = n.effective("codex");
        assert_eq!(codex.mode, ModelPolicyMode::Always);
        assert_eq!(codex.model, Some("gpt-5"));
    }

    #[test]
    fn legacy_per_client_without_a_global_model_becomes_passthrough() {
        let old = r#"{"mode":"per_client","per_client":{"codex":"gpt-5"}}"#;
        let n = serde_json::from_str::<ModelPolicy>(old).unwrap().normalized();

        assert_eq!(n.mode, ModelPolicyMode::Passthrough);
        assert_eq!(n.effective("cursor").mode, ModelPolicyMode::Passthrough);
        assert_eq!(n.effective("codex").model, Some("gpt-5"));
    }

    #[test]
    fn legacy_client_string_value_is_read_as_unconditional_override() {
        // 老格式里 per_client 的值直接就是模型名。untagged 少了它，
        // 这份设置会因为类型不匹配而整份回落默认值。
        let old = r#"{"mode":"passthrough","per_client":{"codex":"gpt-5"}}"#;
        let n = serde_json::from_str::<ModelPolicy>(old).unwrap().normalized();

        let codex = n.effective("codex");
        assert_eq!(codex.mode, ModelPolicyMode::Always);
        assert_eq!(codex.model, Some("gpt-5"));
    }

    #[test]
    fn unknown_match_kind_is_dropped_instead_of_resetting_settings() {
        // 手改 JSON 拼错一个匹配方式，只该让那一行失效。
        let old = r#"{"listen_port":9999,"model_policy":{"mode":"custom",
                      "custom":{"form":"table","table":[
                        {"match_kind":"nope","pattern":"a","target":"b"},
                        {"match_kind":"exact","pattern":"a","target":"b"}]}}}"#;
        let s: AppSettings = serde_json::from_str(old).unwrap();
        assert_eq!(s.listen_port, 9999);

        let n = s.model_policy.normalized();
        assert_eq!(n.custom.table.len(), 1);
        assert_eq!(n.custom.table[0].match_kind, MatchKind::Exact);
    }

    #[test]
    fn custom_rows_with_blank_patterns_are_dropped() {
        let p = ModelPolicy {
            mode: ModelPolicyMode::Custom,
            custom: CustomRules {
                table: vec![
                    row(MatchKind::Prefix, Some("   "), Some("x")),
                    row(MatchKind::Exact, Some(" gpt-5 "), Some("x")),
                    // 兜底行不需要表达式，留着。
                    row(MatchKind::Any, None, Some("x")),
                ],
                ..Default::default()
            },
            ..Default::default()
        };

        let table = p.normalized().custom.table;
        assert_eq!(table.len(), 2, "空表达式的行该被丢掉");
        assert_eq!(table[0].match_kind, MatchKind::Exact);
        assert_eq!(table[0].pattern.as_deref(), Some("gpt-5"));
        assert_eq!(table[1].match_kind, MatchKind::Any);
    }

    #[test]
    fn custom_rows_with_a_broken_regex_are_dropped() {
        let p = ModelPolicy {
            mode: ModelPolicyMode::Custom,
            custom: CustomRules {
                table: vec![
                    row(MatchKind::Regex, Some("("), Some("x")),
                    row(MatchKind::Regex, Some("^gpt-\\d$"), Some("x")),
                ],
                ..Default::default()
            },
            ..Default::default()
        };

        let table = p.normalized().custom.table;
        assert_eq!(table.len(), 1, "编译不过的正则该在入口处就丢掉");
        assert_eq!(table[0].pattern.as_deref(), Some("^gpt-\\d$"));
    }

    #[test]
    fn an_all_inherit_client_entry_is_dropped() {
        let mut p = ModelPolicy::default();
        p.per_client
            .insert("codex".into(), ClientOverride::Rule(ClientRule::default()));
        // 模式是跟随全局、却留着个模型：那也是跟随全局，模型一并清掉。
        p.per_client.insert(
            "cursor".into(),
            ClientOverride::Rule(ClientRule {
                mode: ClientMode::Inherit,
                model: Some("gpt-5".into()),
                ..Default::default()
            }),
        );

        assert!(p.normalized().per_client.is_empty());
    }

    #[test]
    fn a_client_custom_rule_with_an_empty_table_is_kept() {
        // 明确设成自定义规则、表却是空的：它是有意义的（这个客户端永不改写），
        // 不能当成"跟随全局"丢掉。
        let mut p = ModelPolicy::default();
        p.per_client.insert(
            "codex".into(),
            ClientOverride::Rule(ClientRule {
                mode: ClientMode::Custom,
                ..Default::default()
            }),
        );

        let n = p.normalized();
        assert_eq!(n.per_client.len(), 1);
        assert_eq!(n.effective("codex").mode, ModelPolicyMode::Custom);
    }

    #[test]
    fn switching_form_keeps_both_sub_forms() {
        // 界面上切「映射表 / JavaScript」不该丢掉另一套。
        let rules = CustomRules {
            form: CustomForm::Script,
            table: vec![row(MatchKind::Any, None, Some("m"))],
            script: Some("function resolve(){ return null }".into()),
        };

        let n = rules.normalized();
        assert_eq!(n.form, CustomForm::Script);
        assert_eq!(n.table.len(), 1, "切成脚本不该把映射表丢掉");
        assert!(n.script.is_some());
    }

    #[test]
    fn inject_client_model_needs_the_switch_and_a_named_model() {
        // 开关关着时一个字都不写 —— 这是默认，用户选的模型名不该被我们改掉。
        let settings = AppSettings {
            inject_client_model: false,
            model_policy: ModelPolicy {
                mode: ModelPolicyMode::Fallback,
                active_model: Some("deepseek-chat".into()),
                ..Default::default()
            },
            ..Default::default()
        };
        assert_eq!(settings.client_model("codex"), None);

        // 打开开关、且策略真的指定了模型 —— 这时才有一个"Apilot 配置的模型"可写。
        let settings = AppSettings {
            inject_client_model: true,
            ..settings
        };
        assert_eq!(settings.client_model("codex"), Some("deepseek-chat"));
    }

    #[test]
    fn passthrough_mode_has_no_model_to_inject() {
        // Passthrough 的意思就是"不改写客户端要的模型"，所以没有一个名字能写进
        // 客户端配置；硬写一个反而会改变选路。
        let settings = AppSettings {
            inject_client_model: true,
            model_policy: ModelPolicy {
                mode: ModelPolicyMode::Passthrough,
                active_model: Some("deepseek-chat".into()),
                ..Default::default()
            },
            ..Default::default()
        };
        assert_eq!(settings.client_model("codex"), None);
    }

    #[test]
    fn injected_model_follows_each_clients_own_rule() {
        // 模型策略允许给单个客户端单独指定模型；写进客户端配置的必须是**它自己**
        // 生效的那个，不然客户端报的模型名和网关实际用的会对不上。
        let mut policy = ModelPolicy {
            mode: ModelPolicyMode::Fallback,
            active_model: Some("deepseek-chat".into()),
            ..Default::default()
        };
        policy.per_client.insert(
            "codex".into(),
            ClientOverride::Legacy("deepseek-reasoner".into()),
        );
        let settings = AppSettings {
            inject_client_model: true,
            model_policy: policy,
            ..Default::default()
        };

        assert_eq!(settings.client_model("codex"), Some("deepseek-reasoner"));
        assert_eq!(settings.client_model("claude-code"), Some("deepseek-chat"));
    }

    #[test]
    fn client_rule_without_its_own_model_uses_the_global_one() {
        let mut p = ModelPolicy {
            mode: ModelPolicyMode::Passthrough,
            active_model: Some("deepseek-chat".into()),
            ..Default::default()
        };
        p.per_client.insert(
            "codex".into(),
            ClientOverride::Rule(ClientRule {
                mode: ClientMode::Always,
                model: None,
                ..Default::default()
            }),
        );

        let n = p.normalized();
        assert_eq!(n.effective("claude-code").mode, ModelPolicyMode::Passthrough);
        let codex = n.effective("codex");
        assert_eq!(codex.mode, ModelPolicyMode::Always);
        assert_eq!(codex.model, Some("deepseek-chat"), "只改了模式的客户端沿用全局模型");
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

    fn row(kind: MatchKind, pattern: Option<&str>, target: Option<&str>) -> MappingRow {
        MappingRow {
            match_kind: kind,
            pattern: pattern.map(String::from),
            target: target.map(String::from),
            ..Default::default()
        }
    }

    // -----------------------------------------------------------------------
    // 代理设置
    // -----------------------------------------------------------------------

    #[test]
    fn an_old_settings_file_without_a_proxy_block_still_loads() {
        // 老存档里没有 proxy 字段，必须回落成"跟随环境变量"而不是整份设置打回默认 ——
        // 后者会连带丢掉用户配的监听端口、超时、模型策略。
        let old = r#"{"listen_port": 9999, "cache_enabled": true}"#;
        let s: AppSettings = serde_json::from_str(old).unwrap();
        assert_eq!(s.listen_port, 9999);
        assert_eq!(s.proxy.mode, ProxyMode::System);
        assert_eq!(s.proxy.url, None);
    }

    #[test]
    fn a_misspelled_proxy_mode_falls_back_to_system_not_to_a_reset() {
        // `#[serde(other)]` 的意义：认不出的枚举串只影响它自己那一项。
        let raw = r#"{"listen_port": 9999, "proxy": {"mode": "soocks5"}}"#;
        let s: AppSettings = serde_json::from_str(raw).unwrap();
        assert_eq!(s.listen_port, 9999);
        assert_eq!(s.proxy.mode, ProxyMode::Unknown);
        assert_eq!(s.normalized().proxy.mode, ProxyMode::System);
    }

    #[test]
    fn a_blank_proxy_url_is_cleared() {
        let s = AppSettings {
            proxy: ProxySettings {
                mode: ProxyMode::Manual,
                url: Some("   ".into()),
                insecure_tls: false,
            },
            ..Default::default()
        };
        assert_eq!(s.normalized().proxy.url, None);
    }

    #[test]
    fn manual_mode_without_a_url_becomes_direct() {
        // 界面上"选了自定义却没填地址"很常见；留着 Manual 会显示成配了代理，
        // 实际却不生效，不如直接折成直连。
        let s = ProxySettings {
            mode: ProxyMode::Manual,
            url: None,
            insecure_tls: false,
        };
        assert_eq!(s.normalized().mode, ProxyMode::Direct);
    }

    #[test]
    fn a_real_proxy_url_survives_normalization() {
        let s = ProxySettings {
            mode: ProxyMode::Manual,
            url: Some("  socks5://127.0.0.1:1080  ".into()),
            insecure_tls: false,
        };
        let n = s.normalized();
        assert_eq!(n.mode, ProxyMode::Manual);
        assert_eq!(n.url.as_deref(), Some("socks5://127.0.0.1:1080"));
    }

    #[test]
    fn an_old_proxy_block_without_the_tls_switch_still_loads() {
        // 这一行是升级前真实落库的样子。缺字段必须回落成"严格校验"，
        // 而且**不能**因为缺字段就把 mode/url 一起打回默认 —— 那等于用户
        // 升级一次，代理配置就没了。
        let raw = r#"{"mode":"manual","url":"http://127.0.0.1:8080"}"#;
        let parsed: ProxySettings = serde_json::from_str(raw).expect("老配置必须能读进来");
        assert_eq!(parsed.mode, ProxyMode::Manual);
        assert_eq!(parsed.url.as_deref(), Some("http://127.0.0.1:8080"));
        assert!(!parsed.insecure_tls, "缺字段时默认必须是严格校验");
    }

    #[test]
    fn the_tls_switch_survives_a_round_trip() {
        let s = ProxySettings {
            mode: ProxyMode::Manual,
            url: Some("http://127.0.0.1:8080".into()),
            insecure_tls: true,
        };
        let parsed: ProxySettings =
            serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        assert!(parsed.insecure_tls);
    }

    #[test]
    fn normalization_never_silently_flips_the_tls_switch_back_on() {
        // 规范化只该动 mode/url。悄悄把开关关掉会让用户以为"抓包又坏了"，
        // 而他明明没碰过这个开关。
        let s = ProxySettings {
            mode: ProxyMode::Manual,
            url: None, // 会被折成 Direct
            insecure_tls: true,
        };
        assert!(s.normalized().insecure_tls);
    }
}
