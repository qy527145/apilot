//! 数据库迁移。
//!
//! 手写 DDL 顺序数组，**不用 sqlx 的编译期宏** —— 那样需要 `.sqlx` 离线元数据目录，
//! 每次改 schema 都要额外生成一次。运行时 `sqlx::query` 只需字符串，改完直接生效。

/// v8 的 DDL，单独拎出来是为了让 `db.rs` 能对「版本戳比 schema 还新」的库补跑它
/// （见 `db::ensure_usage_hourly_v8`）。写成常量而不是抄一份，漂移就不会发生。
pub const USAGE_HOURLY_V8_DDL: &str = r#"
CREATE TABLE usage_hourly_v8 (
  bucket_ts             INTEGER NOT NULL,
  client                TEXT NOT NULL,
  provider_tag          TEXT NOT NULL,
  model                 TEXT NOT NULL,
  request_model         TEXT NOT NULL,
  requests              INTEGER NOT NULL DEFAULT 0,
  failed_requests       INTEGER NOT NULL DEFAULT 0,
  input_tokens          INTEGER NOT NULL DEFAULT 0,
  output_tokens         INTEGER NOT NULL DEFAULT 0,
  cache_read_tokens     INTEGER NOT NULL DEFAULT 0,
  cache_creation_tokens INTEGER NOT NULL DEFAULT 0,
  quota                 INTEGER NOT NULL DEFAULT 0,
  cache_hits            INTEGER NOT NULL DEFAULT 0,
  saved_quota           INTEGER NOT NULL DEFAULT 0,
  latency_sum_ms        INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY (bucket_ts, client, provider_tag, model, request_model)
);
INSERT INTO usage_hourly_v8
    (bucket_ts, client, provider_tag, model, request_model, requests, failed_requests,
     input_tokens, output_tokens, cache_read_tokens, cache_creation_tokens, quota,
     cache_hits, saved_quota, latency_sum_ms)
SELECT bucket_ts, client, provider_tag, model, model, requests, failed_requests,
     input_tokens, output_tokens, cache_read_tokens, cache_creation_tokens, quota,
     cache_hits, saved_quota, latency_sum_ms
FROM usage_hourly;
DROP TABLE usage_hourly;
ALTER TABLE usage_hourly_v8 RENAME TO usage_hourly;
CREATE INDEX IF NOT EXISTS idx_hourly_model    ON usage_hourly(model, bucket_ts DESC);
CREATE INDEX IF NOT EXISTS idx_hourly_provider ON usage_hourly(provider_tag, bucket_ts DESC);
CREATE INDEX IF NOT EXISTS idx_hourly_client   ON usage_hourly(client, bucket_ts DESC);
"#;

/// 按顺序执行的迁移。**只追加，不修改已发布的条目**。
///
/// 每条迁移用 `IF NOT EXISTS` 保证幂等；版本号靠 `PRAGMA user_version` 记录已执行到的下标。
pub const MIGRATIONS: &[&str] = &[
    // --- v1: 初始 schema ---
    r#"
-- 渠道 / 服务商
CREATE TABLE IF NOT EXISTS providers (
  id             INTEGER PRIMARY KEY AUTOINCREMENT,
  tag            TEXT NOT NULL UNIQUE,           -- 路由 / selector 引用的稳定标识
  name           TEXT NOT NULL,
  kind           TEXT NOT NULL,                  -- anthropic | openai_chat | openai_responses | gemini
  base_url       TEXT NOT NULL,
  api_key        TEXT,
  auth_style     TEXT NOT NULL DEFAULT 'bearer', -- bearer | x-api-key | x-goog-api-key | none
  wire_api       TEXT,                           -- responses | chat（Codex 侧语义）
  extra_headers  TEXT NOT NULL DEFAULT '{}',     -- JSON
  param_override TEXT,                           -- JSON：请求体字段覆盖
  model_mapping  TEXT NOT NULL DEFAULT '{}',     -- JSON：入站模型名 -> 上游模型名
  weight         INTEGER NOT NULL DEFAULT 1,
  priority       INTEGER NOT NULL DEFAULT 0,
  enabled        INTEGER NOT NULL DEFAULT 1,
  timeout_ms     INTEGER NOT NULL DEFAULT 600000,
  created_at     INTEGER NOT NULL,
  updated_at     INTEGER NOT NULL
);

-- model <-> 渠道映射，等价于 new-api 的 abilities 表
CREATE TABLE IF NOT EXISTS provider_models (
  id             INTEGER PRIMARY KEY AUTOINCREMENT,
  provider_id    INTEGER NOT NULL REFERENCES providers(id) ON DELETE CASCADE,
  model          TEXT NOT NULL,
  upstream_model TEXT,
  client_group   TEXT NOT NULL DEFAULT '*',
  priority       INTEGER NOT NULL DEFAULT 0,
  weight         INTEGER NOT NULL DEFAULT 1,
  enabled        INTEGER NOT NULL DEFAULT 1,
  UNIQUE(provider_id, model, client_group)
);
CREATE INDEX IF NOT EXISTS idx_provider_models_lookup
  ON provider_models(model, client_group, enabled, priority DESC);

-- 路由规则链（顺序求值）
CREATE TABLE IF NOT EXISTS route_rules (
  id          INTEGER PRIMARY KEY AUTOINCREMENT,
  sort_index  INTEGER NOT NULL,
  name        TEXT NOT NULL,
  enabled     INTEGER NOT NULL DEFAULT 1,
  items       TEXT NOT NULL,   -- JSON：RuleItem 树（含 and/or/invert）
  action      TEXT NOT NULL,   -- JSON：RouteAction（终结 / 非终结）
  updated_at  INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_route_rules_order ON route_rules(enabled, sort_index);

-- selector 定义 + 当前选中（热切换的持久化落点）
CREATE TABLE IF NOT EXISTS selectors (
  tag              TEXT PRIMARY KEY,
  name             TEXT NOT NULL,
  mode             TEXT NOT NULL DEFAULT 'selector',  -- selector | urltest
  members          TEXT NOT NULL DEFAULT '[]',        -- JSON: [provider_tag, ...]
  current_provider TEXT,
  tolerance_ms     INTEGER NOT NULL DEFAULT 50,
  updated_at       INTEGER NOT NULL
);

-- 顶层路由配置（单行）
CREATE TABLE IF NOT EXISTS route_config (
  id             INTEGER PRIMARY KEY CHECK (id = 1),
  final_selector TEXT NOT NULL DEFAULT 'default',
  updated_at     INTEGER NOT NULL
);
INSERT OR IGNORE INTO route_config (id, final_selector, updated_at) VALUES (1, 'default', 0);

-- 请求明细日志
CREATE TABLE IF NOT EXISTS request_logs (
  id                    INTEGER PRIMARY KEY AUTOINCREMENT,
  request_id            TEXT NOT NULL,
  ts                    INTEGER NOT NULL,              -- unix ms
  client                TEXT NOT NULL,                 -- claude-code | codex | gemini-cli | unknown
  protocol_in           TEXT NOT NULL,
  protocol_out          TEXT NOT NULL,
  provider_tag          TEXT,
  channel_kind          TEXT,
  model                 TEXT NOT NULL,
  request_model         TEXT NOT NULL,
  is_stream             INTEGER NOT NULL DEFAULT 0,
  status_code           INTEGER NOT NULL DEFAULT 0,
  error_message         TEXT,
  input_tokens          INTEGER NOT NULL DEFAULT 0,
  output_tokens         INTEGER NOT NULL DEFAULT 0,
  cache_read_tokens     INTEGER NOT NULL DEFAULT 0,
  cache_creation_tokens INTEGER NOT NULL DEFAULT 0,
  reasoning_tokens      INTEGER NOT NULL DEFAULT 0,
  usage_source          TEXT NOT NULL DEFAULT 'upstream', -- upstream | local
  quota                 INTEGER NOT NULL DEFAULT 0,
  cost_usd              REAL NOT NULL DEFAULT 0,
  latency_ms            INTEGER NOT NULL DEFAULT 0,
  ttfb_ms               INTEGER,
  cache_hit             INTEGER NOT NULL DEFAULT 0,
  saved_quota           INTEGER NOT NULL DEFAULT 0,
  other                 TEXT NOT NULL DEFAULT '{}'
);
CREATE INDEX IF NOT EXISTS idx_logs_ts       ON request_logs(ts DESC);
CREATE INDEX IF NOT EXISTS idx_logs_model    ON request_logs(model, ts DESC);
CREATE INDEX IF NOT EXISTS idx_logs_provider ON request_logs(provider_tag, ts DESC);
CREATE INDEX IF NOT EXISTS idx_logs_client   ON request_logs(client, ts DESC);
CREATE INDEX IF NOT EXISTS idx_logs_cache    ON request_logs(cache_hit, ts DESC);
CREATE UNIQUE INDEX IF NOT EXISTS uq_logs_request ON request_logs(request_id);

-- 按小时聚合（client × provider × model × hour）
CREATE TABLE IF NOT EXISTS usage_hourly (
  bucket_ts             INTEGER NOT NULL,   -- 取整到小时（unix 秒）
  client                TEXT NOT NULL,
  provider_tag          TEXT NOT NULL,
  model                 TEXT NOT NULL,
  requests              INTEGER NOT NULL DEFAULT 0,
  failed_requests       INTEGER NOT NULL DEFAULT 0,
  input_tokens          INTEGER NOT NULL DEFAULT 0,
  output_tokens         INTEGER NOT NULL DEFAULT 0,
  cache_read_tokens     INTEGER NOT NULL DEFAULT 0,
  cache_creation_tokens INTEGER NOT NULL DEFAULT 0,
  quota                 INTEGER NOT NULL DEFAULT 0,
  cache_hits            INTEGER NOT NULL DEFAULT 0,
  saved_quota           INTEGER NOT NULL DEFAULT 0,
  latency_sum_ms        INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY (bucket_ts, client, provider_tag, model)
);
CREATE INDEX IF NOT EXISTS idx_hourly_model    ON usage_hourly(model, bucket_ts DESC);
CREATE INDEX IF NOT EXISTS idx_hourly_provider ON usage_hourly(provider_tag, bucket_ts DESC);
CREATE INDEX IF NOT EXISTS idx_hourly_client   ON usage_hourly(client, bucket_ts DESC);

-- 模型单价系数
CREATE TABLE IF NOT EXISTS model_pricing (
  model               TEXT PRIMARY KEY,
  model_ratio         REAL NOT NULL DEFAULT 1.0,
  completion_ratio    REAL NOT NULL DEFAULT 1.0,
  cache_ratio         REAL NOT NULL DEFAULT 1.0,
  cache_create_ratio  REAL NOT NULL DEFAULT 1.25,
  group_ratio         REAL NOT NULL DEFAULT 1.0,
  image_ratio         REAL NOT NULL DEFAULT 1.0,
  audio_ratio         REAL NOT NULL DEFAULT 1.0,
  tool_call_surcharge INTEGER NOT NULL DEFAULT 0,
  other_ratios        TEXT NOT NULL DEFAULT '{}',
  currency            TEXT NOT NULL DEFAULT 'USD',
  updated_at          INTEGER NOT NULL
);

-- 响应缓存
CREATE TABLE IF NOT EXISTS response_cache (
  key          TEXT PRIMARY KEY,        -- sha256(canonical request)
  protocol     TEXT NOT NULL,
  model        TEXT NOT NULL,
  provider_tag TEXT,
  created_at   INTEGER NOT NULL,
  expires_at   INTEGER NOT NULL,
  status_code  INTEGER NOT NULL DEFAULT 200,
  body         BLOB NOT NULL,           -- UnifiedResponse 的 JSON 序列化
  usage        TEXT NOT NULL DEFAULT '{}',
  quota        INTEGER NOT NULL DEFAULT 0,   -- 首次生成时花费（命中即节省）
  hits         INTEGER NOT NULL DEFAULT 0,
  last_hit_at  INTEGER,
  size_bytes   INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS idx_cache_expires ON response_cache(expires_at);
CREATE INDEX IF NOT EXISTS idx_cache_lru     ON response_cache(last_hit_at ASC);

-- 负载捕获（请求/响应原文），按需保留
CREATE TABLE IF NOT EXISTS captures (
  request_id   TEXT PRIMARY KEY,
  ts           INTEGER NOT NULL,
  method       TEXT NOT NULL,
  path         TEXT NOT NULL,
  request_headers  TEXT NOT NULL DEFAULT '{}',
  request_body     BLOB,
  response_headers TEXT NOT NULL DEFAULT '{}',
  response_body    BLOB,
  stream_text      TEXT,             -- 流式拼接后的最终文本
  stream_events    INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS idx_captures_ts ON captures(ts DESC);

-- 通用键值设置
CREATE TABLE IF NOT EXISTS settings_kv (
  key        TEXT PRIMARY KEY,
  value      TEXT NOT NULL,
  updated_at INTEGER NOT NULL
);
"#,
    // --- v2: 日志区分入站 / 出站方向；渠道声明支持的协议 ---
    //
    // 动机：原来的日志只记「Apilot 的处理结果」，不记 Apilot 究竟把请求发到了
    // 哪个 URL、用了哪个模型名。上游报 404 时无从判断是 base_url 拼错了、
    // 模型映射改错了，还是协议选错了。这几列就是为那类排查加的。
    r#"
-- 入站请求路径（客户端打给 Apilot 的）与出站目标地址（Apilot 打给上游的）
ALTER TABLE request_logs ADD COLUMN path            TEXT NOT NULL DEFAULT '';
ALTER TABLE request_logs ADD COLUMN upstream_url    TEXT;
ALTER TABLE request_logs ADD COLUMN upstream_model  TEXT;
ALTER TABLE request_logs ADD COLUMN upstream_status INTEGER;

-- 捕获表同样补一份出站方向的原文：与我们返回给客户端的那份成对，
-- 跨协议转换时两者内容不同，必须都留着才能看出转换做了什么。
ALTER TABLE captures ADD COLUMN upstream_url              TEXT;
ALTER TABLE captures ADD COLUMN upstream_headers          TEXT NOT NULL DEFAULT '{}';
ALTER TABLE captures ADD COLUMN upstream_body             BLOB;
ALTER TABLE captures ADD COLUMN upstream_status           INTEGER;
ALTER TABLE captures ADD COLUMN upstream_response_headers TEXT NOT NULL DEFAULT '{}';
ALTER TABLE captures ADD COLUMN upstream_response_body    BLOB;

-- 服务商支持哪些协议、各自的请求路径。
-- 空数组表示"只支持 kind 那一种"，老数据与预设不必回填。
ALTER TABLE providers ADD COLUMN protocols TEXT NOT NULL DEFAULT '[]';
"#,
    // --- v3: 流式响应的结构化内容与原始帧 ---
    //
    // 动机：流式响应此前只留了拼接后的纯文本（stream_text）。那份文本里没有
    // 思考内容、没有工具调用，监控页因此做不出「模型想了什么 / 要调什么工具」。
    // 而这两样恰恰是排查 Agent 行为时最需要看的。
    //
    // response_content 存的是由增量重建出的 UnifiedResponse（IR），不是 SSE 原文 ——
    // 它小得多，且解一次就能拿到全部语义。原始帧另存，用于「原始」视图。
    r#"
ALTER TABLE captures ADD COLUMN response_content    TEXT;
ALTER TABLE captures ADD COLUMN upstream_stream_raw BLOB;
ALTER TABLE captures ADD COLUMN client_stream_raw   BLOB;
-- 原始帧超过上限被截断时置 1。界面必须如实标出来，否则用户会以为拿到的是全部。
ALTER TABLE captures ADD COLUMN stream_raw_truncated INTEGER NOT NULL DEFAULT 0;
"#,
    // --- v4: 以「模型」为单位选渠道 ---
    //
    // 让"同一个模型在多个渠道都有时走哪个"成为模型页可配的东西，而不是只能靠
    // 渠道自身的 priority 与 selector 热切换去推。
    r#"
-- 每个模型的渠道选择策略。没有行 = 完全交给 selector 与路由规则（现有行为）。
CREATE TABLE IF NOT EXISTS model_policies (
  model           TEXT PRIMARY KEY,        -- 客户端可见的模型名
  strategy        TEXT NOT NULL DEFAULT 'priority',  -- priority | latency | weight
  active_provider TEXT,                    -- 手动切换到的渠道 tag；NULL = 按策略自动
  updated_at      INTEGER NOT NULL
);

-- 把 provider_models 的 priority/weight 从"建了但从没被读过"变成真源。
--
-- 关键的一步是**按所属渠道的优先级/权重回填一次**：channels_for_model 从此按
-- 每模型的值排序（COALESCE 到渠道的值），回填之后既有配置的排序结果与改动前
-- 逐条一致。不回填的话，所有已有行的 0/1 会把用户设过的渠道优先级整个抹平 ——
-- 那种改动不会报错，只会让流量悄悄换了渠道。
UPDATE provider_models SET
  priority = COALESCE((SELECT p.priority FROM providers p WHERE p.id = provider_models.provider_id), priority),
  weight   = COALESCE((SELECT p.weight   FROM providers p WHERE p.id = provider_models.provider_id), weight);
"#,
    // --- v5: 渠道级代理 ---
    //
    // 上游服务商经常连不上，得走代理；但本地模型服务又必须直连。所以代理要能
    // 按渠道覆盖，不能只有一个全局开关。存 JSON（与 extra_headers 同形态），
    // NULL / '{}' 都表示"跟随全局" —— 老行不需要回填。
    r#"
ALTER TABLE providers ADD COLUMN proxy TEXT;
"#,
    // --- v6: 流式响应每个事件的时间点 ---
    //
    // 动机：`stream_events` 只有一个总数，`ttfb`/`latency` 只有首字节与总耗时。
    // 于是"这个请求为什么慢"在事后完全查不出来 —— 是上游很久才吐第一个字，
    // 还是中间某一段卡了两秒？实时视图能看见，但流一结束就没了。
    //
    // 存 JSON：平行数组 + 名字表（见 `gateway::stream::StreamTimings`），
    // 两千帧几 KB。老行留 NULL —— 那时本来就没记，界面据此不显示时间轴。
    r#"
ALTER TABLE captures ADD COLUMN stream_timings TEXT;
"#,
    // --- v7: 模型能力 + 价格来源 ---
    //
    // 动机：现在没法回答"这个模型支不支持工具/思考/图片"。协议层虽然完整处理
    // 这些字段，但编码时是**无条件**发出的 —— 不支持就等上游返 4xx，界面上
    // 也无从展示。
    //
    // 能力按 (渠道, 模型) 存，不只按模型名：同一个模型名挂在不同中转/代理后面
    // 行为可能不同，而"这条渠道实际给不给过 tools"才是用户真正要的答案。
    //
    // `verdict` 存三态（supported / unsupported / inconclusive）而不是布尔：
    // 探测"支不支持工具"时模型完全可能只是那一次没调工具，记成"不支持"是撒谎。
    //
    // `source` 区分来源。目录拉来的（catalog）零成本，但只反映模型本身宣称的
    // 能力；实测出来的（probe）反映这条渠道的真实行为。优先级规则在
    // `storage::capabilities` 的 upsert 语句里，不在这里。
    //
    // 价格行的 `source` 同理：NULL = 用户手填（老行全是 NULL，这个语义正好对）。
    // 从目录批量更新时只覆盖 catalog 来源的行，手改过的一律不动 —— 否则用户
    // 精心调完价，下次点一下更新就全白调了。
    r#"
CREATE TABLE IF NOT EXISTS model_capabilities (
  provider_id INTEGER NOT NULL REFERENCES providers(id) ON DELETE CASCADE,
  model       TEXT NOT NULL,
  capability  TEXT NOT NULL,
  verdict     TEXT NOT NULL,
  source      TEXT NOT NULL,
  evidence    TEXT,
  checked_at  INTEGER NOT NULL,
  PRIMARY KEY (provider_id, model, capability)
);
CREATE INDEX IF NOT EXISTS idx_model_capabilities_model ON model_capabilities(model);
ALTER TABLE model_pricing ADD COLUMN source TEXT;
"#,
    // --- v8: 聚合行记住「客户端请求的模型名」 ---
    //
    // 动机：`usage_hourly` 的 `model` 是**生效模型**，统计页「按模型」因此只看得到
    // `deepseek-flash` —— 看不到客户端原本要的 `gpt-6-sol`。而"我发的 gpt-6-sol
    // 怎么按 deepseek-flash 计费"正是用户最想查的一件事。明细表 `request_logs`
    // 记着这条链路，但它会被定期清理（也会被「清空日志」清掉），撑不起长期统计。
    //
    // `request_model` 必须进主键：同一小时同一个生效模型可能由多个请求模型折叠
    // 而来，不进主键的话 `ON CONFLICT DO UPDATE` 会把它们并成一行、先写的那个
    // 名字被后写的顶掉，别名就永久丢了。SQLite 不能改主键，只能重建表。
    //
    // 老行回填 `request_model = model`：那时压根没记这件事，等价于"这一行没有
    // 被改写"。不编造，界面据此不显示别名。
    USAGE_HOURLY_V8_DDL,
];

/// 当前 schema 版本 = 迁移条数。
pub const SCHEMA_VERSION: i64 = MIGRATIONS.len() as i64;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrations_are_non_empty() {
        assert_eq!(SCHEMA_VERSION, 8);
        assert!(!MIGRATIONS[0].trim().is_empty());
    }

    #[test]
    fn statement_splitter_can_handle_our_ddl() {
        // 迁移按 `;` 切分语句，所以每条 DDL 里不能出现字符串内的分号。
        // 这条测试守着"以后加迁移时别踩这个坑"。
        for ddl in MIGRATIONS {
            for stmt in ddl.split(';') {
                let s = stmt.trim();
                if s.is_empty() {
                    continue;
                }
                let quotes = s.matches('"').count() + s.matches('\'').count();
                assert_eq!(
                    quotes % 2,
                    0,
                    "语句里的引号不成对，`;` 可能落在字符串内部: {s}"
                );
            }
        }
    }
}
