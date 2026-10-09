//! 监控页「自定义 JS 表达式」筛选的求值器。
//!
//! 与 `routing::model_script` 是**两套东西**，别混：那个改模型名、住在网关
//! 请求热路径上；这个只回答「这一条日志要不要显示」，住在监控页的查询命令里。
//! 共用的是同一套引擎约束（rquickjs、线程本地、超时 + 内存上限、异常必须取走）。
//!
//! 表达式为什么在后端而不是前端求值：日志是**分页**的，而请求/响应原文存在
//! 另一张 `captures` 表里。前端只拿得到当前这一页，拿不到全量的报文 ——
//! 于是「筛出所有请求体里带 tools 的请求」在前端根本无从做起。所以匹配必须
//! 发生在分页**之前**，只能在能同时看到 logs 与 captures 的地方做。
//!
//! 前端仍保留一份同语义的求值（`src/lib/logExpr.ts`），但只用于「进行中」的
//! 请求 —— 那些还没落库、没有 captures，也就没有分页问题。
//!
//! # 报文为什么是「按需取用」的
//!
//! 这个筛选器最容易踩的坑是**把报文塞进 JS**。一条真实请求的 body 动辄几 MB
//! （客户端把整个会话、附件都塞进去；本机库里实测最大 1.7MB），而只看
//! `ctx.status`、`ctx.latencyMs` 的表达式根本用不着它。第一版一律先解析再塞进
//! 8MB 的 JS 堆，在真实流量上就是必然的 `out of memory`：单条解析要 100ms 量级
//! （本机实测 1.7MB 的 body 约 105ms），5000 条的扫描窗口更是根本跑不完。
//!
//! 于是把内置对象拆成两半：
//!
//! - **元数据**（模型、状态、耗时、token…）很小，照旧一次性给；
//! - **报文**（四个方向的 body）在元数据里只留一个哨兵字符串，JS 侧把带哨兵的
//!   属性换成 getter，**只有表达式真的读到它**才回调 Rust 去解析（`Payload`）。
//!
//! 调用方据此走两趟：第一趟不带报文，`Outcome::dirty` 如实报出哪些行读过报文
//! —— 没读过的行结果**已经定音**了；第二趟只给那些行挂上真报文重算。于是
//! 「只看状态码」的表达式一条报文都不解析，而「按 body 筛」的表达式结果依然正确。
//! 换 getter 而不是把 `body` 塞成函数，是为了让**任何**读法都会留下痕迹
//! （`ctx.request.body`、`ctx.request["body"]`、`JSON.stringify(ctx)` 都算）。

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::{Duration, Instant};

use rquickjs::{Context, Ctx, Function, Runtime};

/// 单行的求值上限。
///
/// 用户脚本只该做几个字符串比较，撞上它的多半是死循环 —— 那要**如实报错**，
/// 因为该改的是用户自己写的表达式。留 1 秒而不是 50ms：解析一条真实报文本身
/// 就要几十上百毫秒（见模块注释），单行预算卡太紧会把「报文大」误报成「死循环」。
const ROW_TIMEOUT: Duration = Duration::from_millis(1000);

/// 整批求值的总预算。
///
/// 单行超时挡不住「一行很快、但有一万行」的组合 —— 那会把 tokio 的工作线程
/// 占住几十秒，监控页整个卡死。
///
/// 与单行超时不同，**撞上总预算不是错误**：它不是用户脚本写错了，而是我们给的
/// 扫描窗口到底了。于是停下并如实标 `Outcome::truncated`，让调用方告诉用户
/// 「只扫到这里」—— 报一句「检查死循环」反而会把用户引到完全错误的方向。
const TOTAL_BUDGET: Duration = Duration::from_millis(2000);

/// 一次求值里允许真正解析进 JS 的报文总量。
///
/// 单独一个字节预算的原因：解析一条 1.7MB 的报文要 100ms 量级，只按**条数**
/// 限制挡不住「400 条 × 1.7MB」这种组合。这个预算只被**真的读到报文**的表达式
/// 消耗 —— 只看 status/latency 的表达式一条报文都不碰，于是不受它约束，
/// 该有的 5000 行窗口一行不少。
const PAYLOAD_BUDGET: usize = 64 * 1024 * 1024;

/// 脚本可用的堆上限，挡 `"x".repeat(1e9)` 这类写法。
///
/// 它必须**放得下最坏的单条报文**：按需取用之后，任意时刻堆里只有一行的报文，
/// 但那一行可能就有几 MB，解析成对象图还要再翻几倍。第一版按「几个字符串比较」
/// 定的 8MB，遇上真实流量（本机库里最大 1.7MB 的请求体）就成了必然的
/// out of memory —— 这个数就是从那次的教训里来的。
const MEMORY_LIMIT: usize = 64 * 1024 * 1024;

/// 元数据里给报文占位的哨兵。前缀带 NUL 是为了不可能与用户数据撞上。
const LAZY_SENTINEL: &str = "\u{0}apilot-lazy:";

/// 同一个哨兵在 JS 源码里的写法。两处必须一致，有测试守着。
const LAZY_SENTINEL_JS: &str = "\\u0000apilot-lazy:";

/// 单行超时时给用户看的话。
///
/// QuickJS 那边抛的是一句 `interrupted`（见 `js_poll_interrupts`），
/// 对用户毫无指向 —— 他要的是「去检查死循环」。
const TIMEOUT_HINT: &str = "表达式执行超时：请检查脚本里是否有死循环或过大的循环";

/// 内存上限撞上时给用户看的话。
///
/// QuickJS 自己只会抛一句 `out of memory`（`Error::Allocation` 那边是更含糊的
/// 一句），对用户毫无指向 —— 他需要知道的是「哪件事太大了」。
const OUT_OF_MEMORY_HINT: &str =
    "表达式执行时内存不足：单条报文过大，或表达式自己造了过大的字符串 / 数组";

/// 报文某个位的占位符 —— 元数据里存它，JS 侧靠它认出「这里要按需去取」。
pub fn lazy_slot(slot: &str) -> String {
    format!("{LAZY_SENTINEL}{slot}")
}

/// 一行里「按需取用」的报文，全部保持**原始字节** —— 构造它只搬字节，不解析。
///
/// 这是整个设计的关键：解析（也就是那条 100ms）推迟到表达式真的读到它的时候，
/// 由 `Engine::install_payload_bridge` 装进 JS 的 `__apilot_get` 现取现解。
#[derive(Default)]
pub struct Payload {
    /// 请求方法。它小得可以放进元数据，但那样第一趟就得 JOIN `captures` ——
    /// 而 captures 的行里躺着几 MB 的报文，只为读一个小字段也要把它们翻出来。
    /// 实测那一次 JOIN 让「只看状态码」的表达式从 15ms 涨到 800ms。
    pub request_method: Option<String>,
    /// 四个方向的 headers，存的是**库里那份 JSON 文本**（由 `save_capture` 写入）。
    pub request_headers: Option<String>,
    pub response_headers: Option<String>,
    pub upstream_request_headers: Option<String>,
    pub upstream_response_headers: Option<String>,
    pub request_body: Option<Vec<u8>>,
    pub response_body: Option<Vec<u8>>,
    /// 流式响应没有完整 body，只有拼接后的文本，与 `response_body` 同一槽位。
    pub response_stream_text: Option<String>,
    pub upstream_request_body: Option<Vec<u8>>,
    pub upstream_response_body: Option<Vec<u8>>,
}

impl Payload {
    /// 取某个位对应的 JSON 文本；`None` 表示这段报文没被捕获。
    ///
    /// 解不出 JSON 的报文按字符串给 —— 被截断的、非 JSON 的报文恰恰可能是
    /// 用户想筛的东西，退化成字符串比变成 `null` 有用得多。
    fn json_text(&self, slot: &str) -> Option<String> {
        let value = match slot {
            "request.method" => match &self.request_method {
                Some(m) => serde_json::Value::String(m.clone()),
                None => serde_json::Value::Null,
            },
            // headers 在库里就是 JSON 文本，但仍过一遍解析：解不出来时退化成
            // 字符串，别让一个坏行把整个表达式求值炸掉（与 body 同一口径）。
            "request.headers" => text_to_json(self.request_headers.as_deref()),
            "response.headers" => text_to_json(self.response_headers.as_deref()),
            "upstreamRequest.headers" => text_to_json(self.upstream_request_headers.as_deref()),
            "upstreamResponse.headers" => text_to_json(self.upstream_response_headers.as_deref()),
            "request.body" => bytes_to_json(self.request_body.as_deref()),
            "response.body" => {
                let body = bytes_to_json(self.response_body.as_deref());
                if body.is_null() {
                    match &self.response_stream_text {
                        Some(t) => serde_json::Value::String(t.clone()),
                        None => serde_json::Value::Null,
                    }
                } else {
                    body
                }
            }
            "upstreamRequest.body" => bytes_to_json(self.upstream_request_body.as_deref()),
            "upstreamResponse.body" => bytes_to_json(self.upstream_response_body.as_deref()),
            _ => serde_json::Value::Null,
        };
        (!value.is_null()).then(|| value.to_string())
    }
}

/// 库里的 JSON 文本 → 表达式里的值。口径与 `bytes_to_json` 一致。
fn text_to_json(text: Option<&str>) -> serde_json::Value {
    bytes_to_json(text.map(str::as_bytes))
}

/// 报文 → 表达式里的值：能当 JSON 解就解成对象，否则按字符串给。
fn bytes_to_json(bytes: Option<&[u8]>) -> serde_json::Value {
    match bytes {
        None => serde_json::Value::Null,
        Some(b) => serde_json::from_slice(b).unwrap_or_else(|_| {
            serde_json::Value::String(String::from_utf8_lossy(b).into_owned())
        }),
    }
}

/// 一行待求值的日志：小字段走 `meta`（JSON 文本），大报文走 `payload`（按需）。
pub struct Row {
    pub meta: String,
    pub payload: Payload,
}

/// 一次求值的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    /// 与**求值过**的行一一对应。长度可能小于行数 —— 见 `truncated`。
    pub mask: Vec<bool>,
    /// 探测趟里「需要真报文」的行：读过报文（读到的是 `null`），或干脆因为
    /// 报文缺席抛了错（`ctx.request.body.tools` 这种没写可选链的写法）。
    /// 它们的 `mask` 一律是 `false` 占位，**结果不可信**，调用方必须拿真报文
    /// 重算 —— 把这一趟的结果当结论就是撒谎。终趟不再有 dirty（空）。
    pub dirty: Vec<usize>,
    /// 后面的行**没有被求值**。不能当成「不命中」—— 调用方要如实告诉用户。
    pub truncated: bool,
}

impl Outcome {
    /// 不筛（表达式为空 / 引擎起不来）时的结果：全部命中。
    fn all_true(count: usize) -> Self {
        Self { mask: vec![true; count], dirty: Vec::new(), truncated: false }
    }
}

/// 扫描预算。默认值就是上面那几个常量；测试传小预算，才能不真的耗掉两秒。
#[derive(Clone, Copy)]
struct Budgets {
    row: Duration,
    total: Duration,
    payload_bytes: usize,
}

impl Default for Budgets {
    fn default() -> Self {
        Self { row: ROW_TIMEOUT, total: TOTAL_BUDGET, payload_bytes: PAYLOAD_BUDGET }
    }
}

/// 当前行的可读状态。`__apilot_get` 从这里取报文，并往两个计数器上记账。
#[derive(Default)]
struct State {
    /// 正在求值的那一行的报文。
    payload: Option<Payload>,
    /// 本行已经被取走多少字节。
    row_read_bytes: usize,
    /// 整批累计取走多少字节 —— 字节预算盯的是它。
    total_read_bytes: usize,
}

thread_local! {
    /// 每线程一份引擎，惰性创建。不用 `static Mutex<Engine>`：`Runtime`/`Context`
    /// 不是 `Send`/`Sync`（同 `model_script` 的理由），塞进 static 编译不过。
    static ENGINE: RefCell<Option<Engine>> = const { RefCell::new(None) };
}

struct Engine {
    ctx: Context,
    /// interrupt handler 读的截止时刻。注册一次、每次求值前改写，比反复挂 handler 便宜。
    deadline: Rc<Cell<Option<Instant>>>,
    /// 与 JS 里的 `__apilot_get` 共享。只能用 `Rc<RefCell<_>>` 而不是借用：
    /// 那个闭包必须活得和 context 一样久，而引擎住在 thread_local 里。
    state: Rc<RefCell<State>>,
}

/// JS 侧的前置代码：把带哨兵的属性换成惰性 getter。
///
/// 用 `Object.defineProperty` 而不是把 `body` 直接换成函数：属性读法有几种
/// （`.`、`["body"]`、`JSON.stringify`），换 getter 能让它们**全都**经过这里，
/// 「表达式到底读没读报文」就不会因为写法不同而漏判。
///
/// 缓存按**哨兵值**而不是属性名：四个方向都有 `body`，按名字缓存会串味。
const JS_PRELUDE: &str = r#"
const __sentinel = "__APILOT_SENTINEL__";
const __cache = new Map();
const __hydrate = (o) => {
  if (o === null || typeof o !== "object") return o;
  for (const k of Object.keys(o)) {
    const v = o[k];
    if (typeof v === "string" && v.charCodeAt(0) === 0) {
      Object.defineProperty(o, k, {
        enumerable: true,
        configurable: true,
        get() {
          if (!__cache.has(v)) {
            __cache.set(v, JSON.parse(__apilot_get(v.slice(__sentinel.length))));
          }
          return __cache.get(v);
        },
      });
    } else {
      __hydrate(v);
    }
  }
  return o;
};
"#;

/// 把用户表达式包成 `(__json) => boolean`。返回的两种写法都试，谁先编译用谁。
///
/// 试两种是因为用户自然会先写 `ctx.status === 500`（表达式），认真起来又会写
/// `if (ctx.latencyMs > 1000) return true`（语句体）。只支持一种，另一种就会
/// 报一个和真实意图毫无关系的语法错。
///
/// 把 ctx 当 JSON 字符串传进来再 `JSON.parse`，而不是逐个字段往 JS 上挂：
/// 内置对象是嵌套的（request/response/upstreamRequest/upstreamResponse），
/// 手写转换要写一堆递归，而 `JSON.parse` 一行就是全部。
fn candidates(source: &str) -> [String; 2] {
    let prelude = JS_PRELUDE.replace("__APILOT_SENTINEL__", LAZY_SENTINEL_JS);
    [
        // 表达式形式：`ctx.model.includes("sonnet")`
        format!(
            "((__json) => {{ {prelude} const ctx = __hydrate(JSON.parse(__json)); \
             return !!({source}); }})"
        ),
        // 语句体形式：`if (ctx.status >= 400) return true; return false;`
        //
        // 先包一层 IIFE 再取真假值：用户 `return` 出来的可能是字符串或数字，
        // 而 Rust 侧按 `bool` 取结果 —— 不在这里归一，`ctx.model` 这种完全
        // 自然的写法会撞上「string 转不成 bool」。
        format!(
            "((__json) => {{ {prelude} const ctx = __hydrate(JSON.parse(__json)); \
             const __r = (() => {{ {source} }})(); return !!__r; }})"
        ),
    ]
}

/// 从挂起的异常里挖出用户看得懂的错因，**并把它取走**。
///
/// QuickJS 的 `Error::to_string()` 只给「Exception generated by QuickJS」这种
/// 壳，真正有用的 message 挂在异常对象上。用户写 `throw new Error("boom")` 时
/// 要看的是 boom；语法错也一样，`unexpected token` 远比那句壳有用。
///
/// 取走挂起异常是必须的：不清掉的话，下一次求值会立刻在同一份异常上翻车。
fn exception_message(ctx: &rquickjs::Ctx<'_>, err: &rquickjs::Error) -> String {
    let caught = ctx.catch();
    if let Some(obj) = caught.as_object() {
        if let Ok(m) = obj.get::<_, String>("message") {
            return m;
        }
    }
    // `throw "boom"` 抛的是裸字符串，没有 message 可取。
    if let Some(s) = caught.as_string().and_then(|s| s.to_string().ok()) {
        return s;
    }
    err.to_string()
}

/// 是不是撞了内存上限。
///
/// QuickJS 的内存上限是它自己在 C 里抛的 InternalError，rquickjs 只把它当普通
/// 异常交出来，所以只能认 message；`Error::Allocation` 是 rquickjs 侧自己的那条路。
fn is_out_of_memory(err: &rquickjs::Error, message: &str) -> bool {
    matches!(err, rquickjs::Error::Allocation) || message.contains("out of memory")
}

/// 是不是单行超时。
///
/// 时间上限也是 QuickJS 在 C 里抛的 ——「interrupted」是它写的固定 message，
/// 只能靠它认（那一刻的异常不可捕获，用户的 try/catch 拦不住）。
fn is_timeout(err: &rquickjs::Error, message: &str) -> bool {
    matches!(err, rquickjs::Error::Exception) && message == "interrupted"
}

impl Engine {
    fn new() -> rquickjs::Result<Self> {
        let rt = Runtime::new()?;
        rt.set_memory_limit(MEMORY_LIMIT);

        let deadline: Rc<Cell<Option<Instant>>> = Rc::new(Cell::new(None));
        let watch = deadline.clone();
        rt.set_interrupt_handler(Some(Box::new(move || {
            watch.get().is_some_and(|at| Instant::now() >= at)
        })));

        let ctx = Context::full(&rt)?;
        Ok(Self { ctx, deadline, state: Rc::new(RefCell::new(State::default())) })
    }

    /// 把 `__apilot_get` 挂到 JS 全局上：JS 侧的惰性 getter 靠它回调 Rust 取报文。
    ///
    /// 每次求值装一次。**不能**改到 `Engine::new` 里装：`Function::new` 要借
    /// `Ctx`，而 `Ctx` 只活在 `with` 闭包内，装出来的函数带不出闭包。
    fn install_payload_bridge(&self, ctx: Ctx<'_>) -> Result<(), String> {
        let state = Rc::clone(&self.state);
        let bridge = Function::new(ctx.clone(), move |slot: String| -> rquickjs::Result<String> {
            let mut st = state.borrow_mut();
            let text = st
                .payload
                .as_ref()
                .and_then(|p| p.json_text(&slot))
                .unwrap_or_else(|| "null".to_string());
            // 记账：这条报文有多长，预算就消耗多少 —— 预算是按**真读了多少**算的。
            st.row_read_bytes += text.len();
            Ok(text)
        })
        .map_err(|e| e.to_string())?;
        ctx.globals()
            .set("__apilot_get", bridge)
            .map_err(|e| e.to_string())
    }

    /// 编译并逐行求值，全程留在同一个 `ctx.with` 里。
    ///
    /// 不能把编译好的 `Function` 带出闭包：它的生命周期绑在 `Ctx` 上，跨出去
    /// 编译不过。`model_script` 也是把整段求值放在闭包里，这里照做 —— 代价是
    /// 「行」必须由 `row` 回调现造，不能在闭包外先造好再传进来。
    ///
    /// `probe` 见 `probe()`：那一趟里缺报文引起的抛错不算失败。
    fn run_all(
        &self,
        source: &str,
        count: usize,
        budgets: Budgets,
        probe: bool,
        mut row: impl FnMut(usize) -> Row,
    ) -> Result<Outcome, String> {
        self.ctx.with(|ctx| {
            let f = Self::compile(&ctx, source)?;
            self.install_payload_bridge(ctx.clone())?;

            let started = Instant::now();
            let mut mask = Vec::with_capacity(count);
            let mut dirty = Vec::new();

            for i in 0..count {
                let over_bytes = self.state.borrow().total_read_bytes > budgets.payload_bytes;
                if started.elapsed() > budgets.total || over_bytes {
                    return Ok(Outcome { mask, dirty, truncated: true });
                }

                let Row { meta, payload } = row(i);
                {
                    let mut st = self.state.borrow_mut();
                    st.total_read_bytes += st.row_read_bytes;
                    st.row_read_bytes = 0;
                    st.payload = Some(payload);
                }

                self.deadline.set(Some(Instant::now() + budgets.row));
                let r: rquickjs::Result<bool> = f.call((meta,));
                self.deadline.set(None);

                // 用完就扔。引擎住在 thread_local 里、活到进程结束，把这一行的报文
                // 留在那儿就是白占几 MB（撞上超大报文更甚），而下一次求值只会覆写它。
                // 放在这里而不是函数末尾：后面几个 return 分支都会漏掉清理。
                self.state.borrow_mut().payload = None;

                match r {
                    Ok(hit) => {
                        if self.state.borrow().row_read_bytes > 0 {
                            dirty.push(i);
                        }
                        mask.push(hit);
                    }
                    Err(e) => {
                        // 先取走挂起异常（`exception_message` 内部会 `catch`）：
                        // 不清掉的话，下一行会立刻在同一份异常上翻车。
                        let msg = exception_message(&ctx, &e);
                        if is_out_of_memory(&e, &msg) {
                            return Err(OUT_OF_MEMORY_HINT.into());
                        }
                        if is_timeout(&e, &msg) {
                            return Err(TIMEOUT_HINT.into());
                        }
                        // 探测趟里报文是缺席的：`ctx.request.body.tools` 这种没写
                        // 可选链的表达式会**抛错**。这不是用户的错，是这一趟本来
                        // 就没带报文 —— 标成 dirty 让调用方拿真报文重算。
                        // 表达式真有问题的话，终趟会照样把它抛出来。
                        if probe {
                            dirty.push(i);
                            mask.push(false);
                            continue;
                        }
                        return Err(msg);
                    }
                }
            }
            Ok(Outcome { mask, dirty, truncated: false })
        })
    }

    /// 只编译、不求值。给编辑框做行内报错用。
    fn compile_only(&self, source: &str) -> Result<(), String> {
        self.ctx.with(|ctx| Self::compile(&ctx, source).map(|_| ()))
    }

    /// 编译用户表达式。两种包装都试，谁先成功用谁。
    ///
    /// 返回的 `Function` 绑在 JS 上下文的 `'js` 上，**不是** `ctx` 那个借用 ——
    /// 写成 `Function<'_>` 编译器会要求两个生命周期同一个，那就没法只在
    /// `with` 闭包里借一下 `ctx` 了。
    fn compile<'js>(ctx: &rquickjs::Ctx<'js>, source: &str) -> Result<Function<'js>, String> {
        let mut last_err = String::new();
        for cand in candidates(source) {
            match ctx.eval::<Function, _>(cand) {
                Ok(f) => return Ok(f),
                Err(e) => {
                    last_err = exception_message(ctx, &e);
                }
            }
        }
        Err(last_err)
    }
}

/// 检查表达式能否编译，`Err` 是给编辑框做行内报错用的错因。
pub fn validate(source: &str) -> Result<(), String> {
    if source.trim().is_empty() {
        return Ok(());
    }
    with_engine(|e| e.compile_only(source), || Ok(()))
}

/// 对 `count` 行求值，返回每行是否命中。`row` 按需现造每一行 —— 见 `Engine::run_all`。
///
/// 这是**带真报文的终趟**。`Err` 只有两种：表达式编译不过（语法错），或某一行
/// **自己**跑飞了（死循环、读崩）—— 两种都是用户要改的问题，都该原样告诉他，
/// 而不是静默返回空结果（那看起来就像「没有匹配的请求」，会把用户引到完全
/// 错误的方向）。注意「扫到一半停下」**不是**错误，它在 `Outcome::truncated` 里。
pub fn filter(
    source: &str,
    count: usize,
    row: impl FnMut(usize) -> Row,
) -> Result<Outcome, String> {
    run(source, count, Budgets::default(), false, row)
}

/// **探测趟**：行里不带真报文，只回答「哪些行需要它」。
///
/// 与 `filter` 只差一处：缺报文引起的读失败（读到 `null`，或没写可选链时直接
/// 抛错）**不算错**，而是进 `dirty` 交给调用方拿真报文重算。真正的失败
/// （超时、内存、语法错）照旧报出来 —— 那些不是「缺报文」能解释的。
pub fn probe(
    source: &str,
    count: usize,
    row: impl FnMut(usize) -> Row,
) -> Result<Outcome, String> {
    run(source, count, Budgets::default(), true, row)
}

fn run(
    source: &str,
    count: usize,
    budgets: Budgets,
    probe: bool,
    row: impl FnMut(usize) -> Row,
) -> Result<Outcome, String> {
    if source.trim().is_empty() {
        return Ok(Outcome::all_true(count));
    }

    with_engine(
        |engine| engine.run_all(source, count, budgets, probe, row),
        || Ok(Outcome::all_true(count)),
    )
}

/// 和 `model_script` 同款的线程本地引擎取用。
///
/// 引擎起不来（QuickJS 分配失败）时**不报错**：筛选条件失效等于「不筛」，
/// 比整页查询失败温和得多。日志里留一条 warn 就够了。
fn with_engine<T>(
    f: impl FnOnce(&Engine) -> Result<T, String>,
    on_init_failure: impl FnOnce() -> Result<T, String>,
) -> Result<T, String> {
    ENGINE.with(|slot| {
        let mut slot = slot.borrow_mut();
        if slot.is_none() {
            match Engine::new() {
                Ok(engine) => *slot = Some(engine),
                Err(e) => {
                    tracing::warn!(error = %e, "筛选表达式引擎初始化失败，自定义筛选不生效");
                    return on_init_failure();
                }
            }
        }
        f(slot.as_ref().expect("上面刚填过"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 一小片报文字节，用来验「读到了 / 没读到」。
    fn body(json: &str) -> Payload {
        Payload { request_body: Some(json.as_bytes().to_vec()), ..Default::default() }
    }

    /// 与 storage 里 `expr_meta` 同形状的元数据：凡是从捕获来的字段都是哨兵，
    /// 小字段是真的。那边加了槽位这边就要跟着加 —— 形状对不上时测试会以
    /// 「读到 undefined」这种最不直观的方式失败，所以这里刻意照抄它的结构。
    fn meta(status: i64, latency_ms: i64) -> String {
        json!({
            "model": "claude-opus-5-5",
            "status": status,
            "latencyMs": latency_ms,
            "request": {
                "method": lazy_slot("request.method"),
                "path": "/v1/messages",
                "headers": lazy_slot("request.headers"),
                "body": lazy_slot("request.body"),
            },
            "response": {
                "headers": lazy_slot("response.headers"),
                "body": lazy_slot("response.body"),
            },
            "upstreamRequest": {
                "url": "https://api.example.com/v1/messages",
                "headers": lazy_slot("upstreamRequest.headers"),
                "body": lazy_slot("upstreamRequest.body"),
            },
            "upstreamResponse": {
                "status": 200,
                "headers": lazy_slot("upstreamResponse.headers"),
                "body": lazy_slot("upstreamResponse.body"),
            },
        })
        .to_string()
    }

    /// 单行求值。报文放在 `RefCell` 里现取 —— 逐行回调的闭包是 `FnMut`，
    /// 捕获进来的报文不能直接 move 出去（那要 `FnOnce`）。
    fn hit(source: &str, payload: Payload) -> Result<(bool, Vec<usize>), String> {
        let slot = RefCell::new(Some(payload));
        let out = filter(source, 1, |_| Row {
            meta: meta(200, 1200),
            payload: slot.take().expect("只求值一行"),
        })?;
        Ok((out.mask[0], out.dirty))
    }

    #[test]
    fn empty_source_matches_everything() {
        // 表达式为空时**一行都不该造** —— 造行意味着可能去碰报文。
        let out = filter("", 3, |_| panic!("空表达式不该去造行")).unwrap();
        assert_eq!(out.mask, vec![true, true, true]);
        assert!(!out.truncated);
    }

    #[test]
    fn expression_form_is_supported() {
        assert_eq!(hit(r#"ctx.status === 200"#, Payload::default()).unwrap().0, true);
    }

    #[test]
    fn statement_form_is_supported() {
        let src = "if (ctx.latencyMs > 1000) { return true; }\nreturn false;";
        assert_eq!(hit(src, Payload::default()).unwrap().0, true);
    }

    #[test]
    fn bodies_are_reachable_when_the_expression_reads_them() {
        // 「按 body 筛」是这个功能存在的理由，必须真的能读到底。
        let (got, dirty) = hit(
            r#"ctx.request.body.messages[0].content === "hi""#,
            body(r#"{"messages":[{"role":"user","content":"hi"}]}"#),
        )
        .unwrap();
        assert!(got);
        assert_eq!(dirty, vec![0], "读过报文就要如实报出来，好让调用方拿真报文重算");
    }

    #[test]
    fn indexing_a_body_also_counts_as_reading_it() {
        // 换 getter 而不是换函数，就是为了让 `["body"]` 这种写法也不会漏判 ——
        // 漏判的后果是调用方以为「这行没用报文」，于是拿缺席的报文当结论。
        let (got, dirty) =
            hit(r#"ctx.request["body"]["messages"][0]["content"] === "hi""#, body(r#"{"messages":[{"role":"user","content":"hi"}]}"#))
                .unwrap();
        assert!(got);
        assert_eq!(dirty, vec![0]);
    }

    #[test]
    fn stringifying_the_whole_ctx_also_counts_as_reading_it() {
        let (_, dirty) = hit(r#"JSON.stringify(ctx.request.body).includes("hi")"#, body(r#"{"a":"hi"}"#)).unwrap();
        assert_eq!(dirty, vec![0]);
    }

    #[test]
    fn a_missing_body_is_null_not_a_crash() {
        // 进行中的请求、没捕获报文的请求都走这条：读它该得到 undefined/null
        // （假值），而不是抛异常让整批查询失败。
        let (got, dirty) = hit(r#"ctx.upstreamResponse.body?.usage == null"#, Payload::default()).unwrap();
        assert!(got);
        assert_eq!(dirty, vec![0]);
    }

    #[test]
    fn a_metadata_only_expression_never_touches_payloads() {
        // 这是「第一趟不带报文」成立的前提：没读过就报 dirty == []，
        // 调用方据此就能确认结果已经定音，不必再取报文重算。
        let (_, dirty) = hit(r#"ctx.status === 200 && ctx.latencyMs > 1000"#, body(r#"{"a":1}"#)).unwrap();
        assert!(dirty.is_empty());
    }

    #[test]
    fn a_body_far_larger_than_the_heap_limit_is_harmless_when_unread() {
        // **out of memory 那个 bug 的回归测试**：报文比 JS 堆上限还大，
        // 只要表达式没读它，就不该有任何代价。第一版会把整条塞进堆里，必炸。
        let huge = RefCell::new(Some(vec![b'x'; 80 * 1024 * 1024]));
        let out = filter(r#"ctx.status === 200"#, 1, |_| Row {
            meta: meta(200, 10),
            payload: Payload { request_body: huge.borrow_mut().take(), ..Default::default() },
        })
        .unwrap();
        assert_eq!(out.mask, vec![true]);
        assert!(out.dirty.is_empty());
    }

    #[test]
    fn a_long_metadata_scan_is_cheap() {
        // 第一版这条会撞上总预算（300 行 × 100KB 解析 ≈ 0.5s），因为它在解析
        // 表达式根本用不到的报文。现在按需取用，扫描的代价与报文无关。
        let big = vec![b'y'; 100 * 1024];
        let out = filter(r#"ctx.status === 200"#, 300, |_| Row {
            meta: meta(200, 10),
            payload: Payload { request_body: Some(big.clone()), ..Default::default() },
        })
        .unwrap();
        assert_eq!(out.mask.len(), 300);
        assert!(!out.truncated);
    }

    #[test]
    fn reading_bodies_stops_at_the_payload_budget() {
        // 真读报文时扫描必须被**预算**截住，并如实标 truncated：5000 条 × 1.7MB
        // 全解析要几分钟，宁可给半个答案也要标出来这是半个。
        let budgets = Budgets { payload_bytes: 4 * 1024 * 1024, ..Default::default() };
        let payload = || body(&format!(r#"{{"pad":"{}"}}"#, "z".repeat(1024 * 1024)));
        let out = run(r#"ctx.request.body.pad.length > 0"#, 200, budgets, false, |_| Row {
            meta: meta(200, 10),
            payload: payload(),
        })
        .unwrap();

        assert!(out.truncated);
        assert!(out.mask.len() < 200, "不该把 200 行全解析完");
        assert!(out.mask.iter().all(|h| *h));
        assert_eq!(out.dirty.len(), out.mask.len(), "每一行都读过报文");
    }

    #[test]
    fn a_non_json_body_degrades_to_a_string() {
        // 截断的、非 JSON 的报文恰恰可能是用户要筛的东西。
        let payload = Payload {
            request_body: Some(b"not json at all".to_vec()),
            ..Default::default()
        };
        assert_eq!(hit(r#"ctx.request.body.includes("json")"#, payload).unwrap().0, true);
    }

    #[test]
    fn a_streamed_response_exposes_its_text_as_the_body() {
        // 流式响应没有完整 body，只有拼接文本 —— 不回落到它就等于 ctx.response.body
        // 对绝大多数流式请求恒为 null。
        let payload = Payload {
            response_stream_text: Some("hello there".into()),
            ..Default::default()
        };
        assert_eq!(hit(r#"ctx.response.body === "hello there""#, payload).unwrap().0, true);
    }

    #[test]
    fn an_uncaptured_direction_is_null() {
        assert_eq!(hit(r#"ctx.response.body === null"#, Payload::default()).unwrap().0, true);
    }

    #[test]
    fn syntax_error_is_reported() {
        let err = filter("ctx.status === ", 1, |_| Row { meta: meta(200, 1), payload: Payload::default() })
            .unwrap_err();
        assert!(!err.is_empty(), "语法错必须带出原因给用户看");
    }

    #[test]
    fn throw_is_reported_not_swallowed() {
        // 脚本自己 throw 时不能静默变成「没匹配」，那会误导用户去改筛选条件。
        let err = hit(r#"throw new Error("boom")"#, Payload::default()).unwrap_err();
        assert!(err.contains("boom"), "应带上原始错误：{err}");
    }

    #[test]
    fn hitting_the_memory_limit_says_something_useful() {
        // QuickJS 只会抛一句 `out of memory`，对用户毫无指向 —— 换成能指导动作的话。
        let err = hit(r#""x".repeat(256 * 1024 * 1024).length > 0"#, Payload::default()).unwrap_err();
        assert!(err.contains("内存不足"), "要给出可行动的错因，实际是：{err}");
    }

    #[test]
    fn non_boolean_return_is_coerced_to_truthiness() {
        // `ctx.model` 这种返回字符串的写法很自然，该按真假值处理。
        assert_eq!(hit("ctx.model", Payload::default()).unwrap().0, true);
        assert_eq!(hit("''", Payload::default()).unwrap().0, false);
    }

    #[test]
    fn validate_accepts_both_forms_and_empty() {
        assert!(validate("").is_ok());
        assert!(validate("ctx.status === 200").is_ok());
        assert!(validate("return ctx.status === 200").is_ok());
    }

    #[test]
    fn validate_reports_syntax_error() {
        assert!(validate("ctx.status === ").is_err());
    }

    #[test]
    fn a_scope_leak_between_rows_does_not_happen() {
        // 用户写 `const x = 1` 时，第二行不该撞上「重复声明」——
        // 每行都在同一个箭头函数的调用里，作用域是干净的。
        let out = filter("const x = ctx.status; return x === 200;", 2, |_| Row {
            meta: meta(200, 1),
            payload: Payload::default(),
        })
        .unwrap();
        assert_eq!(out.mask, vec![true, true]);
    }

    #[test]
    fn a_probe_row_that_throws_on_a_missing_body_is_deferred_not_failed() {
        // 没写可选链的 `ctx.request.body.tools` 打在 null 上会抛错。探测趟里
        // 这**不是**用户的错（报文本来就没带），该交给终趟重算 ——
        // 让整批查询失败会把一个正常的表达式变成不可用。
        let out = probe(r#"ctx.request.body.tools.length === 1"#, 1, |_| Row {
            meta: meta(200, 1),
            payload: Payload::default(),
        })
        .unwrap();
        assert_eq!(out.dirty, vec![0]);
        assert_eq!(out.mask, vec![false], "占位为不命中，终趟会覆盖它");
    }

    #[test]
    fn the_final_pass_still_reports_a_genuine_throw() {
        // 终趟带上真报文还抛，那就是表达式自己的问题，必须如实报出来。
        let err = filter(r#"ctx.request.body.tools.length === 1"#, 1, |_| Row {
            meta: meta(200, 1),
            payload: Payload::default(),
        })
        .unwrap_err();
        assert!(err.contains("tools"), "应带上原始错因：{err}");
    }

    #[test]
    fn a_probe_row_that_reads_a_missing_body_is_deferred() {
        // 写了可选链的写法不抛错，但同样「读了报文」—— 也一样要重算。
        let out = probe(r#"ctx.request.body?.tools?.length === 1"#, 1, |_| Row {
            meta: meta(200, 1),
            payload: Payload::default(),
        })
        .unwrap();
        assert_eq!(out.dirty, vec![0]);
        assert_eq!(out.mask, vec![false]);
    }

    #[test]
    fn a_timeout_is_an_error_even_in_the_probe_pass() {
        // 超时不是「缺报文」能解释的，别被 probe 吞掉 —— 吞掉的后果是
        // 用户写了个死循环却看不到任何提示。
        let budgets = Budgets { row: Duration::from_millis(50), ..Default::default() };
        let err = run("while (true) {}", 1, budgets, true, |_| Row {
            meta: meta(200, 1),
            payload: Payload::default(),
        })
        .unwrap_err();
        assert!(err.contains("超时"), "实际是：{err}");
    }

    #[test]
    fn a_probe_row_needing_no_body_is_settled_green() {
        // 探测趟里「没读报文」的行结果是定音了的，不该被丢进 dirty 重算 ——
        // 否则每次筛选都要白跑一遍第二趟。
        let out = probe(r#"ctx.status >= 400"#, 1, |_| Row {
            meta: meta(200, 1),
            payload: Payload::default(),
        })
        .unwrap();
        assert!(out.dirty.is_empty());
        assert_eq!(out.mask, vec![false]);
    }

    #[test]
    fn method_and_headers_are_also_on_demand() {
        // 它们也住在 captures 里。放进元数据就要 JOIN 那张几 MB 一行的表 ——
        // 于是「只看 header 的表达式」会顺带把报文页全翻出来，那是 800ms 的由来。
        let payload = Payload {
            request_method: Some("POST".into()),
            request_headers: Some(r#"{"x-test":"1"}"#.into()),
            upstream_response_headers: Some(r#"{"x-up":"2"}"#.into()),
            ..Default::default()
        };
        let src = r#"ctx.request.method === "POST" && ctx.request.headers["x-test"] === "1"
            && ctx.upstreamResponse.headers["x-up"] === "2""#;
        assert_eq!(hit(src, payload).unwrap().0, true);
    }

    #[test]
    fn unread_method_and_headers_leave_the_row_undirty() {
        let out = filter("ctx.status === 200", 1, |_| Row {
            meta: meta(200, 1),
            payload: Payload { request_method: Some("POST".into()), ..Default::default() },
        })
        .unwrap();
        assert!(out.dirty.is_empty(), "没读就不该被拖去第二趟");
    }

    #[test]
    fn a_broken_headers_blob_degrades_to_a_string() {
        let payload = Payload { request_headers: Some("not json".into()), ..Default::default() };
        assert_eq!(hit(r#"ctx.request.headers.includes("json")"#, payload).unwrap().0, true);
    }

    #[test]
    fn the_js_sentinel_literal_matches_the_rust_one() {
        // 两处哨兵必须逐字节一致：Rust 写进元数据，JS 用来认出来。
        // 对不上就是「表达式读到的永远是 null」这种最难受的坏法 —— 它不报错。
        assert_eq!(LAZY_SENTINEL_JS.replace("\\u0000", "\u{0}"), LAZY_SENTINEL);
    }

    #[test]
    fn each_direction_gets_its_own_payload() {
        // 缓存按哨兵值而不是属性名：四个方向都有 `body`，按名字缓存会串味。
        let payload = Payload {
            request_body: Some(br#"{"tag":"request"}"#.to_vec()),
            upstream_request_body: Some(br#"{"tag":"upstream"}"#.to_vec()),
            ..Default::default()
        };
        let src = r#"ctx.request.body.tag === "request" && ctx.upstreamRequest.body.tag === "upstream""#;
        assert_eq!(hit(src, payload).unwrap().0, true);
    }


}
