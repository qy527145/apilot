import { useState } from "react";
import { useMutation } from "@tanstack/react-query";
import { ArrowRight, FlaskConical, Plus, Trash2 } from "lucide-react";

import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { Switch } from "@/components/ui/switch";
import {
  ALL_PROTOCOLS,
  PROTOCOL_DEFAULT_PATH,
  PROTOCOL_LABEL,
  api,
  type CandidateBasis,
  type ClientSource,
  type Protocol,
  type SimulateResult,
} from "@/lib/api";
import { cn } from "@/lib/utils";

/** 接管写进客户端的令牌（`takeover/floor.rs::client_token`），做成快捷选项。 */
const TOKEN_PRESETS = ["apilot-claude-code", "apilot-codex", "apilot-gemini-cli"];

const SOURCE_LABEL: Record<ClientSource, string> = {
  token: "按令牌",
  header: "按 x-apilot-client",
  user_agent: "按 User-Agent",
  path: "按请求路径推断",
  unknown: "未识别",
};

const STAGE_LABEL: Record<SimulateResult["model_steps"][number]["stage"], string> = {
  request: "请求",
  policy: "模型策略",
  session_pin: "对话固定",
  rule: "路由规则",
};

const STRATEGY_LABEL = { priority: "优先级", latency: "延迟", weight: "权重" } as const;

function describeBasis(b: CandidateBasis): string {
  switch (b.kind) {
    case "model_policy":
      return b.pinned
        ? "模型渠道策略：手动指定的渠道"
        : `模型渠道策略：按${STRATEGY_LABEL[b.strategy]}`;
    case "model_policy_fallback":
      return "模型配了渠道策略，但没有可用渠道，落回选择器";
    case "declared_channels":
      return "声明了该模型的渠道优先，选择器选的渠道兜底";
    case "selector":
      return "选择器选出的渠道";
  }
}

/**
 * 规则测试：填一组请求条件，后端按线上同一套判定空跑一遍，
 * 看会落到哪个渠道的哪个模型。不发请求、不落日志、不计费。
 */
export function RouteSimulator() {
  const [protocol, setProtocol] = useState<Protocol>("anthropic");
  const [model, setModel] = useState("");
  const [token, setToken] = useState("");
  const [userAgent, setUserAgent] = useState("");
  const [headers, setHeaders] = useState<[string, string][]>([]);
  const [stream, setStream] = useState(true);
  const [hasTools, setHasTools] = useState(true);
  const [estTokens, setEstTokens] = useState("");
  const [sessionId, setSessionId] = useState("");

  const run = useMutation({
    mutationFn: () =>
      api.simulateRoute({
        protocol,
        model: model.trim(),
        token: token.trim() || null,
        user_agent: userAgent.trim() || null,
        headers: headers.filter(([k]) => k.trim()),
        stream,
        has_tools: hasTools,
        est_input_tokens: Number(estTokens) || 0,
        session_id: sessionId.trim() || null,
      }),
  });

  const result = run.data;

  return (
    <Card>
      <CardHeader className="flex-row items-center justify-between">
        <CardTitle className="text-sm">规则测试</CardTitle>
        <span className="text-xs text-muted-foreground">
          按当前配置模拟一次请求，不会真正发出
        </span>
      </CardHeader>
      <CardContent className="space-y-4">
        <div className="grid grid-cols-1 gap-3 md:grid-cols-3">
          <div className="space-y-1.5">
            <Label className="text-xs">入站协议</Label>
            <Select value={protocol} onValueChange={(v) => setProtocol(v as Protocol)}>
              <SelectTrigger className="w-full">
                <SelectValue />
              </SelectTrigger>
              <SelectContent>
                {ALL_PROTOCOLS.map((p) => (
                  <SelectItem key={p} value={p}>
                    {PROTOCOL_LABEL[p]}
                  </SelectItem>
                ))}
              </SelectContent>
            </Select>
          </div>
          <div className="space-y-1.5">
            <Label className="text-xs">请求的模型</Label>
            <Input
              value={model}
              onChange={(e) => setModel(e.target.value)}
              placeholder="claude-sonnet-4-5"
            />
          </div>
          <div className="space-y-1.5">
            <Label className="text-xs">令牌</Label>
            <Input
              value={token}
              onChange={(e) => setToken(e.target.value)}
              placeholder="apilot-codex"
              list="route-sim-token-presets"
            />
            <datalist id="route-sim-token-presets">
              {TOKEN_PRESETS.map((t) => (
                <option key={t} value={t} />
              ))}
            </datalist>
          </div>
          <div className="space-y-1.5">
            <Label className="text-xs">User-Agent（可选）</Label>
            <Input
              value={userAgent}
              onChange={(e) => setUserAgent(e.target.value)}
              placeholder="claude-cli/2.1.0"
            />
          </div>
          <div className="space-y-1.5">
            <Label className="text-xs">估算输入 token（可选）</Label>
            <Input
              value={estTokens}
              onChange={(e) => setEstTokens(e.target.value.replace(/\D/g, ""))}
              placeholder="0"
              inputMode="numeric"
            />
          </div>
          <div className="space-y-1.5">
            <Label className="text-xs">会话 ID（可选，测对话固定）</Label>
            <Input value={sessionId} onChange={(e) => setSessionId(e.target.value)} />
          </div>
        </div>

        <div className="space-y-1.5">
          <div className="flex items-center justify-between">
            <Label className="text-xs">其他请求头（规则按请求头匹配时用）</Label>
            <Button
              size="sm"
              variant="ghost"
              className="h-6 px-2 text-xs"
              onClick={() => setHeaders([...headers, ["", ""]])}
            >
              <Plus className="size-3" />
              添加
            </Button>
          </div>
          {headers.map(([k, v], i) => (
            <div key={i} className="flex gap-2">
              <Input
                className="h-8"
                value={k}
                placeholder="x-apilot-client"
                onChange={(e) =>
                  setHeaders(headers.map((h, j) => (j === i ? [e.target.value, h[1]] : h)))
                }
              />
              <Input
                className="h-8"
                value={v}
                onChange={(e) =>
                  setHeaders(headers.map((h, j) => (j === i ? [h[0], e.target.value] : h)))
                }
              />
              <Button
                size="icon"
                variant="ghost"
                className="size-8 shrink-0"
                onClick={() => setHeaders(headers.filter((_, j) => j !== i))}
              >
                <Trash2 className="size-4" />
              </Button>
            </div>
          ))}
        </div>

        <div className="flex flex-wrap items-center gap-6">
          <label className="flex items-center gap-2 text-xs">
            <Switch checked={stream} onCheckedChange={setStream} />
            流式
          </label>
          <label className="flex items-center gap-2 text-xs">
            <Switch checked={hasTools} onCheckedChange={setHasTools} />
            带工具
          </label>
          <Button
            size="sm"
            className="ml-auto"
            disabled={!model.trim() || run.isPending}
            onClick={() => run.mutate()}
          >
            <FlaskConical className="size-4" />
            测试
          </Button>
        </div>

        {result && <ResultView result={result} />}
      </CardContent>
    </Card>
  );
}

function ResultView({ result }: { result: SimulateResult }) {
  const { outcome } = result;
  return (
    <div className="space-y-4 rounded-md border bg-muted/30 p-3 text-xs">
      <div className="flex flex-wrap items-center gap-x-4 gap-y-1">
        <span>
          客户端 <Badge variant="secondary">{result.client}</Badge>
          <span className="ml-1 text-muted-foreground">
            （{SOURCE_LABEL[result.client_source]}）
          </span>
        </span>
        <span className="text-muted-foreground">
          {PROTOCOL_LABEL[result.protocol]} · {result.path}
          {result.path !== PROTOCOL_DEFAULT_PATH[result.protocol] && "（自定义路径）"}
        </span>
      </div>

      <div className="flex flex-wrap items-center gap-1.5">
        {result.model_steps.map((s, i) => (
          <span key={i} className="flex items-center gap-1.5">
            {i > 0 && <ArrowRight className="size-3 text-muted-foreground" />}
            <span className="text-muted-foreground">{STAGE_LABEL[s.stage]}</span>
            <code className="rounded bg-background px-1">{s.model || "（空）"}</code>
          </span>
        ))}
      </div>

      {result.rules.length > 0 && (
        <div className="space-y-1">
          <div className="text-muted-foreground">规则链求值</div>
          {result.rules.map((r) => (
            <div
              key={r.id}
              className={cn(
                "flex items-center gap-2 rounded px-2 py-1",
                r.decisive && "bg-primary/10",
                !r.matched && "opacity-60",
              )}
            >
              <Badge variant={r.matched ? "success" : "outline"}>
                {!r.enabled ? "已停用" : r.matched ? "命中" : "未命中"}
              </Badge>
              <span className="font-medium">{r.name}</span>
              <span className="truncate text-muted-foreground">
                {r.conditions} → {r.action}
              </span>
            </div>
          ))}
        </div>
      )}

      {outcome.kind === "rejected" && (
        <div className="text-destructive">请求会被拒绝（403）：{outcome.reason}</div>
      )}
      {outcome.kind === "no_channel" && (
        <div className="text-destructive">
          选择器「{outcome.selector}」没有可用渠道，请求会失败（503）
        </div>
      )}
      {outcome.kind === "routed" && (
        <div className="space-y-2">
          <div className="text-muted-foreground">
            {outcome.rule_id == null ? "未命中终结规则，走兜底" : "命中规则，走"}选择器「
            {outcome.selector}」 · {describeBasis(outcome.basis)}
          </div>
          <div className="space-y-1">
            {outcome.candidates.map((c, i) => (
              <div
                key={c.tag}
                className={cn(
                  "flex flex-wrap items-center gap-2 rounded px-2 py-1",
                  i === 0 ? "bg-primary/10" : "opacity-70",
                )}
              >
                <Badge variant={i === 0 ? "default" : "outline"}>
                  {i === 0 ? "主渠道" : `备选 ${i}`}
                </Badge>
                <span className="font-medium">{c.name}</span>
                <code className="rounded bg-background px-1">{c.upstream_model}</code>
                <Badge variant={c.passthrough ? "secondary" : "warning"}>
                  {c.passthrough ? "直通" : `转换为 ${PROTOCOL_LABEL[c.wire]}`}
                </Badge>
                <span className="truncate text-muted-foreground">{c.url}</span>
              </div>
            ))}
          </div>
          {outcome.notes.map((n) => (
            <div key={n} className="text-amber-600 dark:text-amber-400">
              {n}
            </div>
          ))}
        </div>
      )}
    </div>
  );
}
