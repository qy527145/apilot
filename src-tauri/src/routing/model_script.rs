//! 用户 JS 脚本的执行器 —— 「自定义规则」那条路的落点。
//!
//! 脚本被刻意限制成 `ctx` 的**纯函数**：没有 I/O、没有网络、没有 `hasChannels()`。
//! 纯换来两件事：同一份脚本 + 同一组入参永远同解，所以结果可以安全地缓存；
//! 以及整套逻辑能脱离网关单测。脚本里能用的只有 JS 自身的字符串与正则能力。
//!
//! 三条不变量，破了任何一条都会把网关拖下水：
//!
//! 1. 语法错、抛异常、超时、返回非字符串 —— 一律**不改写模型**，绝不失败请求；
//! 2. 引擎活在本线程，不跨线程、不跨 await（`Runtime`/`Context` 根本不是 `Send`）；
//! 3. 结果按 (脚本, 模型, 客户端) 缓存，命中就完全不碰 JS 引擎。
//!
//! 关于超时：`set_interrupt_handler` 只在解释器的轮询点触发（循环回边、条件跳转），
//! 所以 `while(true){}` 拦得住；但脚本里一次长时间的原生调用 —— 最可能是用户自己
//! 写的灾难性回溯正则 —— 仍可能冲过截止时刻。内存上限是那道兜底，但**它不是万能的**，
//! 别把这里当成一个能挡住任意恶意代码的沙箱：脚本来源是本机用户自己。

use std::cell::{Cell, RefCell};
use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::rc::Rc;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use once_cell::sync::Lazy;
use rquickjs::{Context, Ctx, Function, Object, Runtime};

use super::model_policy::ScriptInput;

/// 单次求值的时间上限。
///
/// 超时抛的是**不可捕获**的异常，脚本里的 try/catch 拦不住。选 25ms 是因为
/// 它只在缓存未命中时才用得上（命中完全不进引擎），而用户的脚本理应只做几个
/// 字符串比较 —— 真撞上 25ms 的，多半是写坏了。
const TIMEOUT: Duration = Duration::from_millis(25);

/// 脚本可用的堆上限，挡 `"x".repeat(1e9)` 这类把网关一起撑爆的写法。
///
/// 只在没开 `rust-alloc` 特性时有效 —— 开了它这就是个静默的空操作，
/// 所以 `Cargo.toml` 里那个特性是**故意**关掉的。
const MEMORY_LIMIT: usize = 8 * 1024 * 1024;

/// 结果缓存的条数上限。
const MEMO_CAPACITY: usize = 1024;

/// 跑一段用户脚本，返回要用的模型名。`None` = 不改写，用客户端请求的那个。
pub fn run(source: &str, input: &ScriptInput<'_>) -> Option<String> {
    let source = source.trim();
    if source.is_empty() {
        return None;
    }

    let key = memo_key(source, input);
    {
        // 守卫只活在花括号里：不跨下面的求值 —— 那段可能跑满 TIMEOUT。
        // 本函数是同步的，所以也不存在"跨 await 持锁"的问题。
        let cache = MEMO.lock().unwrap();
        if let Some(hit) = cache.get(&key).filter(|e| e.matches(source, input)) {
            return hit.out.as_deref().map(String::from);
        }
    }

    let out = evaluate(source, input);

    let mut cache = MEMO.lock().unwrap();
    // 条目数约等于「脚本 × 模型 × 客户端」的组合数，实践中到不了上限；
    // 真到了就整表清空，比维护一条几乎不触发的淘汰路径划算。
    if cache.len() >= MEMO_CAPACITY {
        cache.clear();
    }
    cache.insert(key, MemoEntry::new(source, input, out.as_deref()));
    out
}

// ---------------------------------------------------------------------------
// 结果缓存
// ---------------------------------------------------------------------------

struct MemoEntry {
    /// 原样存一份做命中核对 —— 64 位哈希只当**索引**，不当身份。
    /// 万一撞了，最多算一次未命中并覆盖，绝不会把 A 脚本的结果回给 B 脚本。
    source: Box<str>,
    model: Box<str>,
    client: Box<str>,
    out: Option<Box<str>>,
}

impl MemoEntry {
    fn new(source: &str, input: &ScriptInput<'_>, out: Option<&str>) -> Self {
        Self {
            source: source.into(),
            model: input.model.into(),
            client: input.client.into(),
            out: out.map(Into::into),
        }
    }

    fn matches(&self, source: &str, input: &ScriptInput<'_>) -> bool {
        self.source.as_ref() == source
            && self.model.as_ref() == input.model
            && self.client.as_ref() == input.client
    }
}

static MEMO: Lazy<Mutex<HashMap<u64, MemoEntry>>> = Lazy::new(|| Mutex::new(HashMap::new()));

fn memo_key(source: &str, input: &ScriptInput<'_>) -> u64 {
    // 按元组 hash：`str` 的 Hash 带长度，("ab","c") 与 ("a","bc") 不会撞。
    let mut hasher = DefaultHasher::new();
    (source, input.model, input.client).hash(&mut hasher);
    hasher.finish()
}

// ---------------------------------------------------------------------------
// 引擎
// ---------------------------------------------------------------------------

thread_local! {
    /// 每线程一份 QuickJS 引擎，惰性创建 —— 没人配自定义脚本时一分钱不花。
    ///
    /// 不用 `static Mutex<Engine>`：没开 `parallel` 特性时 `Runtime`/`Context`
    /// 根本没有 `Send`/`Sync`（那两个 unsafe impl 是 cfg 在 parallel 下的），
    /// 塞进 static 编译不过。而为了这点用途去开 `parallel`，会换来一把全局锁
    /// 和一个 tokio 依赖。线程本地还顺带把锁也省了。
    static ENGINE: RefCell<Option<Engine>> = const { RefCell::new(None) };
}

struct Engine {
    /// 不另存一个 `Runtime` 字段：`Context::full` 会 clone 一份 runtime 的
    /// 引用计数，它自己就把 JSRuntime 养着了。内存上限与 interrupt handler
    /// 都设在同一个 JSRuntime 上，跟着它一起活。多存一个字段只是摆设，
    /// 还会招来"字段从未被读取"的警告。
    ctx: Context,
    /// interrupt handler 读的截止时刻。handler 只注册一次、每次求值前改写它，
    /// 比每次求值都重挂一遍 handler 便宜。
    deadline: Rc<Cell<Option<Instant>>>,
}

/// 把用户脚本包进一个立即执行的箭头函数里，返回里面的 `resolve`。
///
/// **必须包这一层**：全局作用域是在所有求值之间共享的，用户写
/// `const resolve = …` 时第二次求值会撞上"重复声明"而直接报错 ——
/// 换个脚本就再也不生效了，而且报的错跟真实原因毫无关系。
/// 包进函数体之后，每次求值拿到的都是干净的作用域。
///
/// 顺带解决另一件事：末尾那句 `return resolve` 而不是去 `globals()` 上找，
/// `function resolve(ctx){…}` 与 `const resolve = ctx => …` 两种写法都拿得到。
fn wrapped(source: &str) -> String {
    format!("(() => {{\n{source}\n;return resolve;\n}})()")
}

impl Engine {
    fn new() -> rquickjs::Result<Self> {
        let rt = Runtime::new()?;
        rt.set_memory_limit(MEMORY_LIMIT);
        // 栈上限**故意**保持默认的 256 KiB：调大只会让写坏的递归脚本更接近
        // 打穿宿主栈（那是直接崩进程，而不是抛一个能兜住的异常）。
        // 写坏了的脚本撞上限时抛的是可捕获的 RangeError，走的就是"不改写"那条路。

        let deadline: Rc<Cell<Option<Instant>>> = Rc::new(Cell::new(None));
        let watch = deadline.clone();
        // 返回 true → QuickJS 抛不可捕获的异常。
        rt.set_interrupt_handler(Some(Box::new(move || {
            watch.get().is_some_and(|at| Instant::now() >= at)
        })));

        let ctx = Context::full(&rt)?;
        Ok(Self { ctx, deadline })
    }

    fn call(&self, source: &str, input: &ScriptInput<'_>) -> Option<String> {
        self.deadline.set(Some(Instant::now() + TIMEOUT));

        let out = self.ctx.with(|ctx| match Self::invoke(&ctx, source, input) {
            Ok(v) => v,
            Err(e) => {
                // 必须把挂起的异常取走：不清掉的话，下一次求值会立刻在同一份
                // 挂起异常上翻车 —— 一个写坏的脚本会连累后面所有请求。
                let _ = ctx.catch();
                tracing::warn!(error = %e, "模型脚本执行失败，按不改写处理");
                None
            }
        });

        self.deadline.set(None);
        out
    }

    /// 只求值顶层、不调用 `resolve`，给编辑框做行内报错用。
    fn check(&self, source: &str) -> Result<(), String> {
        self.deadline.set(Some(Instant::now() + TIMEOUT));

        let out = self.ctx.with(|ctx| {
            let func: rquickjs::Result<Function> = ctx.eval(wrapped(source));
            match func {
                Ok(_) => Ok(()),
                Err(e) => {
                    // 同 call()：挂起的异常必须取走，否则下一次求值会立刻翻车。
                    let _ = ctx.catch();
                    Err(e.to_string())
                }
            }
        });

        self.deadline.set(None);
        out
    }

    fn invoke(
        ctx: &Ctx<'_>,
        source: &str,
        input: &ScriptInput<'_>,
    ) -> rquickjs::Result<Option<String>> {
        let func: Function = ctx.eval(wrapped(source))?;

        let arg = Object::new(ctx.clone())?;
        arg.set("model", input.model)?;
        arg.set("client", input.client)?;
        arg.set("protocol", input.protocol)?;

        // `Option<String>` 的 FromJs 对 null/undefined 短路成 None，字符串走
        // `Some`，其它类型（数字、对象）在转换里报错。三种情况这里都不用自己判断。
        let out: Option<String> = func.call((arg,))?;
        Ok(out
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty()))
    }
}

fn evaluate(source: &str, input: &ScriptInput<'_>) -> Option<String> {
    #[cfg(test)]
    EVAL_COUNT.with(|c| c.set(c.get() + 1));

    ENGINE.with(|slot| {
        let mut slot = slot.borrow_mut();
        if slot.is_none() {
            match Engine::new() {
                Ok(engine) => *slot = Some(engine),
                Err(e) => {
                    // 引擎起不来（例如 QuickJS 分配失败）也不该让请求失败。
                    tracing::warn!(error = %e, "模型脚本引擎初始化失败，自定义规则不生效");
                    return None;
                }
            }
        }
        // 上面的 borrow_mut 活到这一行；求值路径里没有任何东西会再进 ENGINE
        // （脚本拿不到宿主回调），所以不会有重入 panic。
        slot.as_ref().expect("上面刚填过").call(source, input)
    })
}

/// 检查脚本能不能用，`Err` 是给用户看的错因。
///
/// 给编辑框做行内报错用。它求值顶层语句（所以顶层的 `throw` 也会被报出来），
/// 但**不调用** `resolve` —— 那需要一个真实的请求上下文，而这里没有。
pub fn validate(source: &str) -> Result<(), String> {
    if source.trim().is_empty() {
        return Ok(()); // 空脚本 = 不改写，是合法的
    }

    ENGINE.with(|slot| {
        let mut slot = slot.borrow_mut();
        if slot.is_none() {
            match Engine::new() {
                Ok(engine) => *slot = Some(engine),
                Err(e) => return Err(format!("脚本引擎初始化失败: {e}")),
            }
        }
        slot.as_ref().expect("上面刚填过").check(source)
    })
}

// 实际进了 JS 引擎的次数。给测试钉住"命中缓存就不该再求值"。
#[cfg(test)]
thread_local! {
    static EVAL_COUNT: Cell<u64> = const { Cell::new(0) };
}

#[cfg(test)]
pub(crate) fn eval_count() -> u64 {
    EVAL_COUNT.with(Cell::get)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 每个用例用**互不相同**的脚本源码：memo 是全局的、按源码寻址，
    /// 源码撞了就会跨用例串味。
    fn input<'a>(model: &'a str, client: &'a str) -> ScriptInput<'a> {
        ScriptInput {
            model,
            client,
            protocol: "anthropic",
        }
    }

    #[test]
    fn a_function_returning_a_string_rewrites_the_model() {
        let src = "function resolve(ctx) { return 'fn-' + ctx.model; }";
        assert_eq!(run(src, &input("m1", "codex")).as_deref(), Some("fn-m1"));
    }

    #[test]
    fn a_const_arrow_function_is_accepted() {
        // 钉住末尾那个 `;resolve` —— 少了它，箭头函数拿不到函数值。
        let src = "const resolve = ctx => 'arrow-' + ctx.model;";
        assert_eq!(run(src, &input("m2", "codex")).as_deref(), Some("arrow-m2"));
    }

    #[test]
    fn ctx_exposes_model_client_and_protocol() {
        let src = "function resolve(ctx) { return [ctx.model, ctx.client, ctx.protocol].join('|'); }";
        assert_eq!(
            run(src, &input("m3", "gemini-cli")).as_deref(),
            Some("m3|gemini-cli|anthropic")
        );
    }

    #[test]
    fn null_undefined_and_empty_string_do_not_rewrite() {
        let yes = "function resolve(ctx) { return 'n1-' + ctx.model; }";
        let no = "function resolve(ctx) { return null; }";
        let undef = "function resolve(ctx) { }";
        let blank = "function resolve(ctx) { return '   '; }";

        assert_eq!(run(yes, &input("n1", "codex")).as_deref(), Some("n1-n1"));
        assert_eq!(run(no, &input("n1", "codex")), None);
        assert_eq!(run(undef, &input("n1", "codex")), None);
        assert_eq!(run(blank, &input("n1", "codex")), None);
    }

    #[test]
    fn a_non_string_return_does_not_rewrite() {
        let src = "function resolve(ctx) { return 42; }";
        assert_eq!(run(src, &input("m4", "codex")), None);
    }

    #[test]
    fn a_throwing_script_does_not_fail_the_request() {
        let src = "function resolve(ctx) { throw new Error('boom'); }";
        assert_eq!(run(src, &input("m5", "codex")), None);
    }

    #[test]
    fn a_syntax_error_does_not_fail_the_request() {
        assert_eq!(run("function resolve( {", &input("m6", "codex")), None);
    }

    #[test]
    fn a_script_without_a_resolve_function_does_not_fail_the_request() {
        assert_eq!(run("const other = 1;", &input("m7", "codex")), None);
        // 定义了 resolve 但不是函数也一样。
        assert_eq!(run("const resolve = 1;", &input("m7", "codex")), None);
    }

    #[test]
    fn a_broken_script_does_not_poison_the_next_one() {
        // 一个写坏的脚本不该连累后面的：挂起的异常必须在同一个 ctx 上清掉。
        let bad = "function resolve(ctx) { return nope.undefined.thing; }";
        let good = "function resolve(ctx) { return 'after-' + ctx.model; }";

        assert_eq!(run(bad, &input("m8", "codex")), None);
        assert_eq!(run(good, &input("m8", "codex")).as_deref(), Some("after-m8"));
    }

    #[test]
    fn an_infinite_loop_is_interrupted_within_the_timeout() {
        let src = "function resolve(ctx) { while (true) {} }";
        let started = Instant::now();
        assert_eq!(run(src, &input("m9", "codex")), None);
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "死循环该被 interrupt handler 打断，实际花了 {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn runaway_recursion_does_not_crash_the_process() {
        let src = "function resolve(ctx) { return resolve(ctx); }";
        assert_eq!(run(src, &input("m10", "codex")), None);
    }

    #[test]
    fn a_result_is_memoized_so_a_second_call_skips_the_engine() {
        let src = "function resolve(ctx) { return 'memo-' + ctx.model; }";
        let before = eval_count();

        assert_eq!(run(src, &input("m11", "codex")).as_deref(), Some("memo-m11"));
        assert_eq!(run(src, &input("m11", "codex")).as_deref(), Some("memo-m11"));

        assert_eq!(eval_count(), before + 1, "第二次该直接吃缓存");
    }

    #[test]
    fn a_different_model_gets_its_own_memo_entry() {
        let src = "function resolve(ctx) { return 'per-model-' + ctx.model; }";
        let before = eval_count();

        assert_eq!(run(src, &input("m12a", "codex")).as_deref(), Some("per-model-m12a"));
        assert_eq!(run(src, &input("m12b", "codex")).as_deref(), Some("per-model-m12b"));

        assert_eq!(eval_count(), before + 2);
    }

    #[test]
    fn two_scripts_declaring_resolve_with_const_do_not_collide() {
        // 全局作用域是所有求值共享的。不把脚本包进新作用域的话，第二段
        // `const resolve` 会撞上"重复声明"直接报错 —— 换个脚本就再也不生效了。
        let a = "const resolve = ctx => 'a-' + ctx.model;";
        let b = "const resolve = ctx => 'b-' + ctx.model;";

        assert_eq!(run(a, &input("m17", "codex")).as_deref(), Some("a-m17"));
        assert_eq!(run(b, &input("m17", "codex")).as_deref(), Some("b-m17"));
    }

    #[test]
    fn a_changed_script_does_not_reuse_the_old_result() {        let a = "function resolve(ctx) { return 'old-' + ctx.model; }";
        let b = "function resolve(ctx) { return 'new-' + ctx.model; }";

        assert_eq!(run(a, &input("m13", "codex")).as_deref(), Some("old-m13"));
        assert_eq!(
            run(b, &input("m13", "codex")).as_deref(),
            Some("new-m13"),
            "换了脚本就该重算，不能拿老结果"
        );
    }

    #[test]
    fn a_failed_script_is_memoized_as_a_failure() {
        // 失败结果也缓存：同一份脚本对同一组入参是确定的，重试没有意义。
        let src = "function resolve(ctx) { throw new Error('always'); }";
        assert_eq!(run(src, &input("m14", "codex")), None);
        assert_eq!(run(src, &input("m14", "codex")), None);
    }

    #[test]
    fn an_empty_script_is_a_noop() {
        assert_eq!(run("", &input("m15", "codex")), None);
        assert_eq!(run("   \n  ", &input("m15", "codex")), None);
    }

    // ---- 语法校验（给编辑框做行内报错）----

    #[test]
    fn validate_accepts_usable_scripts() {
        assert!(validate("function resolve(ctx) { return ctx.model; }").is_ok());
        assert!(validate("const resolve = ctx => ctx.model;").is_ok());
        // 空脚本是合法的：它等于"不改写"。
        assert!(validate("").is_ok());
        assert!(validate("   ").is_ok());
    }

    #[test]
    fn validate_reports_a_syntax_error() {
        assert!(validate("function resolve( {").is_err());
    }

    #[test]
    fn validate_reports_a_missing_resolve() {
        let err = validate("const other = 1;");
        assert!(err.is_err(), "没有 resolve 函数的脚本不该通过校验");

        let err = validate("const resolve = 1;");
        assert!(err.is_err(), "resolve 不是函数也该报出来");
    }

    #[test]
    fn validate_does_not_run_the_resolve_function() {
        // 只求值顶层：写坏的函数体现在不该被执行到。
        assert!(validate("function resolve(ctx) { return boom.undefined; }").is_ok());
    }

    #[test]
    fn validate_survives_a_top_level_infinite_loop() {
        let started = Instant::now();
        assert!(validate("while (true) {}").is_err());
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "顶层死循环也要被超时打断，实际花了 {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn validate_leaves_the_engine_usable_afterwards() {
        // 校验失败后挂起的异常要被清掉，否则紧接着的求值会跟着翻车。
        assert!(validate("function resolve( {").is_err());

        let src = "function resolve(ctx) { return 'still-works-' + ctx.model; }";
        assert_eq!(
            run(src, &input("m16", "codex")).as_deref(),
            Some("still-works-m16")
        );
    }
}
