import { useEffect, useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { Braces, Check, CircleAlert, Info, Undo2 } from "lucide-react";

import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import {
  Popover,
  PopoverContent,
  PopoverTrigger,
} from "@/components/ui/popover";
import { ScrollArea } from "@/components/ui/scroll-area";
import { Separator } from "@/components/ui/separator";
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/tabs";
import { Textarea } from "@/components/ui/textarea";
import { api } from "@/lib/api";
import { cn } from "@/lib/utils";

/** 内置对象的结构说明。改这里要同步 `log_filter.rs::expr_context` 与 `lib/logExpr.ts`。 */
const STRUCTURE = `{
  // —— 每条请求都有的日志字段 ——
  id, ts, client,
  model,            // 实际路由到的模型（计费口径）
  requestModel,     // 客户端请求时写的名字
  upstreamModel,    // 真正发给上游的名字
  protocolIn, protocolOut, provider, path,
  status, upstreamStatus, isStream, error,
  latencyMs, ttfbMs,
  inputTokens, outputTokens, cacheReadTokens, cacheCreationTokens,
  quota, costUsd, cacheHit,

  // —— 四个方向的报文（未捕获时整体为 null）——
  request:          { method, path, headers, body },  // 客户端 → Apilot
  response:         { headers, body },                // Apilot → 客户端
  upstreamRequest:  { url, headers, body },           // Apilot → 上游
  upstreamResponse: { status, headers, body },        // 上游 → Apilot
}`;

/** 示例表达式。点一下就填进编辑框，省得对着空框发呆。 */
const EXAMPLES: { label: string; expr: string }[] = [
  {
    label: "按客户端请求的模型名",
    expr: 'ctx.requestModel === "claude-fable-5-1"',
  },
  {
    label: "只看被模型策略改写过的请求",
    expr: "ctx.model !== ctx.requestModel",
  },
  {
    label: "网关报错但上游是好的",
    expr: "ctx.status >= 400 && ctx.upstreamStatus === 200",
  },
  {
    label: "慢请求（超过 3 秒）",
    expr: "ctx.latencyMs > 3000",
  },
  {
    label: "请求体里带了工具定义",
    expr: '(JSON.stringify(ctx.request.body) ?? "").includes("tools")',
  },
  {
    label: "上游没回 usage 的流式响应",
    expr: "ctx.isStream && !ctx.upstreamResponse?.body?.usage",
  },
];

interface FilterExprPanelProps {
  /** 当前表达式。 */
  value: string;
  onChange: (v: string) => void;
}

/**
 * 监控页的「自定义表达式」筛选面板。
 *
 * 校验刻意留在后端（`validate_log_expr`）而不是用浏览器里的 `new Function`
 * 试一下：真正执行表达式的是 QuickJS，两边的语法边缘（可选链、正则断言等）
 * 并不完全一致。拿浏览器当法官会出现「这里说没问题、查询却报错」的错位。
 */
export function FilterExprPanel({ value, onChange }: FilterExprPanelProps) {
  const [open, setOpen] = useState(false);
  const [debounced, setDebounced] = useState(value);

  // 300ms 防抖：用户还在敲的时候没必要每一击都跑一次校验。
  useEffect(() => {
    const t = setTimeout(() => setDebounced(value), 300);
    return () => clearTimeout(t);
  }, [value]);

  const check = useQuery({
    queryKey: ["log-expr-check", debounced],
    queryFn: () => api.validateLogExpr(debounced),
    // 空表达式是合法的（等于不筛），不必打扰后端。
    enabled: debounced.trim().length > 0,
    staleTime: Infinity,
    retry: false,
  });

  const active = value.trim().length > 0;
  const error = check.data ?? null;

  return (
    <Popover open={open} onOpenChange={setOpen}>
      <PopoverTrigger asChild>
        <Button variant={active ? "default" : "outline"} size="sm">
          <Braces className="size-4" />
          表达式
          {active && (
            <Badge variant="secondary" className="ml-1 px-1.5 text-[10px]">
              已启用
            </Badge>
          )}
        </Button>
      </PopoverTrigger>

      <PopoverContent align="start" className="w-[34rem] p-0">
        <div className="space-y-3 p-4">
          <div className="flex items-center justify-between">
            <div className="text-sm font-medium">自定义表达式</div>
            {active && (
              <Button
                variant="ghost"
                size="sm"
                className="h-7 text-xs"
                onClick={() => onChange("")}
              >
                <Undo2 className="size-3.5" />
                清空
              </Button>
            )}
          </div>

          <Textarea
            className="min-h-24 font-mono text-xs"
            placeholder={`返回真值则该行保留，例如：\nctx.requestModel === "claude-fable-5-1"`}
            value={value}
            onChange={(e) => onChange(e.target.value)}
            spellCheck={false}
          />

          {/* 校验状态：没有它，用户只能从「查询失败」反推自己写错了什么。 */}
          <div className="flex min-h-5 items-center gap-1.5 text-xs">
            {!active ? (
              <span className="text-muted-foreground">留空 = 不筛</span>
            ) : error !== null ? (
              <>
                <CircleAlert className="text-destructive size-3.5 shrink-0" />
                <span className="text-destructive">{error}</span>
              </>
            ) : check.isFetching ? (
              <span className="text-muted-foreground">校验中…</span>
            ) : (
              <>
                <Check className="size-3.5 shrink-0 text-emerald-500" />
                <span className="text-emerald-500">表达式可用</span>
              </>
            )}
          </div>

          <Separator />

          <Tabs defaultValue="examples">
            <TabsList className="h-8">
              <TabsTrigger value="examples" className="text-xs">
                示例
              </TabsTrigger>
              <TabsTrigger value="structure" className="text-xs">
                内置对象
              </TabsTrigger>
            </TabsList>

            <TabsContent value="examples" className="mt-2">
              <div className="space-y-1">
                {EXAMPLES.map((ex) => (
                  <button
                    key={ex.expr}
                    type="button"
                    className={cn(
                      "hover:bg-accent w-full rounded-md px-2 py-1.5 text-left transition-colors",
                    )}
                    onClick={() => onChange(ex.expr)}
                  >
                    <div className="text-xs">{ex.label}</div>
                    <div className="text-muted-foreground truncate font-mono text-[11px]">
                      {ex.expr}
                    </div>
                  </button>
                ))}
              </div>
            </TabsContent>

            <TabsContent value="structure" className="mt-2">
              <ScrollArea className="h-56 rounded-md border">
                <pre className="text-muted-foreground p-3 font-mono text-[11px] leading-relaxed">
                  {STRUCTURE}
                </pre>
              </ScrollArea>
            </TabsContent>
          </Tabs>

          <div className="text-muted-foreground flex gap-1.5 text-[11px] leading-relaxed">
            <Info className="mt-0.5 size-3.5 shrink-0" />
            <span>
              body 能解析成 JSON 时是对象，否则是字符串；没有捕获到就是 null。
              「进行中」的请求只有请求侧元数据，响应侧一律为 null。
              表达式筛选最多扫描最近 5000 条匹配日志。
            </span>
          </div>
        </div>
      </PopoverContent>
    </Popover>
  );
}