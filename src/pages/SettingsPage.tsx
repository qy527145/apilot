import { useEffect, useState } from "react";
import { useMutation, useQueryClient } from "@tanstack/react-query";
import { openPath } from "@tauri-apps/plugin-opener";
import { AlertTriangle, FolderOpen, Save } from "lucide-react";
import { toast } from "sonner";

import { ErrorState } from "@/components/common/StatCard";
import { PageShell } from "@/components/layout/PageShell";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Skeleton } from "@/components/ui/skeleton";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { Switch } from "@/components/ui/switch";
import { Textarea } from "@/components/ui/textarea";
import { qk, useAppInfo, useSettings } from "@/hooks/queries";
import { api, type AppSettings, type ProxyMode } from "@/lib/api";

const PROXY_MODE_LABEL: Record<ProxyMode, string> = {
  system: "跟随环境变量",
  direct: "始终直连",
  manual: "使用指定代理",
};

const PROXY_MODE_HINT: Record<ProxyMode, string> = {
  system:
    "按 HTTPS_PROXY / ALL_PROXY / HTTP_PROXY（大小写都看）决定。没配就直连。",
  direct: "不走任何代理，环境变量里配了也不走。",
  manual: "所有渠道默认走下面这个地址，一个地址都不豁免。留空等同于直连。",
};

export default function SettingsPage() {
  const qc = useQueryClient();
  const { data, isLoading, isError, refetch } = useSettings();
  const { data: info } = useAppInfo();
  const [form, setForm] = useState<AppSettings | null>(null);

  useEffect(() => {
    if (data) setForm(data);
  }, [data]);

  const save = useMutation({
    mutationFn: (settings: AppSettings) => api.updateSettings(settings),
    onSuccess: (s) => {
      qc.setQueryData(qk.settings, s);
      qc.invalidateQueries({ queryKey: qk.gateway });
      toast.success("设置已保存");
    },
  });

  const set = <K extends keyof AppSettings>(key: K, value: AppSettings[K]) =>
    setForm((f) => (f ? { ...f, [key]: value } : f));

  const num = (v: string) => (v === "" ? 0 : Number(v));

  // 两个改写脚本的行内错因。保存前先过一遍校验：脚本写坏了只会静默不生效，
  // 那种错用户自己很难发现（与模型脚本同一个理由）。
  const [scriptErrors, setScriptErrors] = useState<{
    request?: string | null;
    response?: string | null;
    delta?: string | null;
  }>({});

  const checkScript = async (kind: "request" | "response" | "delta") => {
    if (!form) return;
    const source =
      kind === "request"
        ? form.request_script
        : kind === "response"
          ? form.response_script
          : form.response_delta_script;
    try {
      const err = await api.validateRewriteScript(source);
      setScriptErrors((e) => ({ ...e, [kind]: err }));
      if (!err) toast.success("脚本语法通过");
    } catch {
      // call 已经弹过 toast，这里只需要把行内状态清掉。
      setScriptErrors((e) => ({ ...e, [kind]: null }));
    }
  };

  return (
    <PageShell
      title="设置"
      description="监听、超时、捕获与缓存等全局配置"
      actions={
        <Button
          onClick={() => form && save.mutate(form)}
          disabled={!form || save.isPending}
        >
          <Save className="size-4" />
          {save.isPending ? "保存中…" : "保存设置"}
        </Button>
      }
    >
      {isLoading ? (
        <div className="space-y-4">
          <Skeleton className="h-40 w-full" />
          <Skeleton className="h-40 w-full" />
        </div>
      ) : isError || !form ? (
        <ErrorState onRetry={() => refetch()} />
      ) : (
        <div className="space-y-6">
          <Card>
            <CardHeader>
              <CardTitle className="text-sm">监听</CardTitle>
            </CardHeader>
            <CardContent className="space-y-4">
              <div className="grid grid-cols-1 gap-4 sm:grid-cols-2">
                <div className="space-y-2">
                  <Label>监听地址</Label>
                  <Input
                    value={form.listen_host}
                    onChange={(e) => set("listen_host", e.target.value)}
                    placeholder="127.0.0.1"
                    // 关着局域网访问时，保存时会被收回 127.0.0.1 —— 与其让用户
                    // 填一个存不下的地址，不如直接锁住。
                    disabled={!form.allow_lan}
                  />
                </div>
                <div className="space-y-2">
                  <Label>监听端口</Label>
                  <Input
                    type="number"
                    value={form.listen_port}
                    onChange={(e) => set("listen_port", num(e.target.value))}
                  />
                </div>
              </div>
              <div className="flex items-center justify-between rounded-md border px-3 py-2">
                <div>
                  <Label>启动时自动开启网关</Label>
                  <p className="text-muted-foreground text-xs">
                    应用启动后自动监听上述地址。
                  </p>
                </div>
                <Switch
                  checked={form.autostart_gateway}
                  onCheckedChange={(v) => set("autostart_gateway", v)}
                />
              </div>
              <div className="flex items-center justify-between rounded-md border px-3 py-2">
                <div>
                  <Label>允许局域网访问</Label>
                  <p className="text-muted-foreground text-xs">
                    关闭时只绑定本机（127.0.0.1），同一局域网内的其他设备连不上。
                    打开后绑定 0.0.0.0，也可以在地址栏里指定某一张网卡。
                  </p>
                </div>
                <Switch
                  checked={form.allow_lan}
                  onCheckedChange={(v) => set("allow_lan", v)}
                />
              </div>
              <div className="flex items-center justify-between rounded-md border px-3 py-2">
                <div>
                  <Label>允许任意网页调用（CORS）</Label>
                  <p className="text-muted-foreground text-xs">
                    让公网上的网页客户端（如 https://lm.example.com）能直接调用
                    本网关。关掉后跨源网页请求会被挡回去；开着的代价是任何网页
                    都能花掉你的渠道额度。
                  </p>
                </div>
                <Switch
                  checked={form.allow_browser_origins}
                  onCheckedChange={(v) => set("allow_browser_origins", v)}
                />
              </div>
            </CardContent>
          </Card>

          <Card>
            <CardHeader>
              <CardTitle className="text-sm">超时（毫秒）</CardTitle>
            </CardHeader>
            <CardContent className="space-y-4">
              {/* 四个值按请求的生命周期排：发出去 → 响应头 → 第一个字节 → 之后每一段。
                  分开列是因为中转站排队时卡在**第一步**，而那一格以前根本不存在。 */}
              <p className="text-muted-foreground text-xs">
                按请求的生命周期排：发出 → 响应头 → 第一个字节 → 之后每段之间。
                「响应头超时」是上游收下请求却迟迟不给回应时的兜底，超时会换下一个候选渠道。
              </p>
              <div className="grid grid-cols-1 gap-4 sm:grid-cols-2">
                <div className="space-y-2">
                  <Label>响应头超时</Label>
                  <Input
                    type="number"
                    value={form.headers_timeout_ms}
                    onChange={(e) =>
                      set("headers_timeout_ms", num(e.target.value))
                    }
                  />
                </div>
                <div className="space-y-2">
                  <Label>首字节超时</Label>
                  <Input
                    type="number"
                    value={form.first_byte_timeout_ms}
                    onChange={(e) =>
                      set("first_byte_timeout_ms", num(e.target.value))
                    }
                  />
                </div>
                <div className="space-y-2">
                  <Label>空闲超时</Label>
                  <Input
                    type="number"
                    value={form.idle_timeout_ms}
                    onChange={(e) => set("idle_timeout_ms", num(e.target.value))}
                  />
                </div>
                <div className="space-y-2">
                  <Label>请求总超时</Label>
                  <Input
                    type="number"
                    value={form.request_timeout_ms}
                    onChange={(e) =>
                      set("request_timeout_ms", num(e.target.value))
                    }
                  />
                </div>
              </div>
            </CardContent>
          </Card>

          <Card>
            <CardHeader>
              <CardTitle className="text-sm">网络代理</CardTitle>
            </CardHeader>
            <CardContent className="space-y-4">
              <div className="grid grid-cols-1 gap-4 sm:grid-cols-2">
                <div className="space-y-2">
                  <Label>出站方式</Label>
                  <Select
                    value={form.proxy.mode}
                    onValueChange={(v) =>
                      set("proxy", { ...form.proxy, mode: v as ProxyMode })
                    }
                  >
                    <SelectTrigger className="w-full">
                      <SelectValue />
                    </SelectTrigger>
                    <SelectContent>
                      {(Object.keys(PROXY_MODE_LABEL) as ProxyMode[]).map((m) => (
                        <SelectItem key={m} value={m}>
                          {PROXY_MODE_LABEL[m]}
                        </SelectItem>
                      ))}
                    </SelectContent>
                  </Select>
                </div>
                <div className="space-y-2">
                  <Label>代理地址</Label>
                  <Input
                    value={form.proxy.url ?? ""}
                    disabled={form.proxy.mode !== "manual"}
                    onChange={(e) =>
                      set("proxy", { ...form.proxy, url: e.target.value })
                    }
                    placeholder="socks5://127.0.0.1:1080"
                  />
                </div>
              </div>

              <div className="flex items-center justify-between rounded-md border px-3 py-2">
                <div>
                  <Label>忽略 TLS 证书校验</Label>
                  <p className="text-muted-foreground text-xs">
                    不装 CA 也能让抓包工具解密 HTTPS。对所有出站连接生效，
                    与走不走代理无关。
                  </p>
                </div>
                <Switch
                  checked={form.proxy.insecure_tls}
                  onCheckedChange={(v) =>
                    set("proxy", { ...form.proxy, insecure_tls: v })
                  }
                />
              </div>

              {form.proxy.insecure_tls && (
                <div className="border-amber-500/40 bg-amber-500/10 flex items-start gap-3 rounded-md border p-3">
                  <AlertTriangle className="mt-0.5 size-4 shrink-0 text-amber-500" />
                  <p className="text-muted-foreground text-xs">
                    证书链和主机名都不再校验，出站请求被中间人截改将无法发现。
                    只建议在本地抓包调试时开启，排查完记得关掉。
                  </p>
                </div>
              )}

              <p className="text-muted-foreground text-xs">
                {PROXY_MODE_HINT[form.proxy.mode]}
                <br />
                支持 <code>http://</code>、<code>https://</code>、
                <code>socks5://</code>、<code>socks5h://</code>
                （<code>socks5h</code> 由代理解析域名，能绕开 DNS 污染）。
                <br />
                <strong className="font-medium">
                  「使用指定代理」是你下的一条明确指令，回环与内网地址也照走
                </strong>
                —— 抓包、走公司代理时不会有地址被悄悄放过。本地模型服务（ollama /
                LM Studio）请在那条渠道上选「直连」。「跟随环境变量」才自动豁免
                回环与私有网段（<code>127.0.0.1</code>、<code>192.168.*</code> 等）。
                单个渠道可以在渠道对话框里单独设置。
              </p>
            </CardContent>
          </Card>

          <Card>
            <CardHeader>
              <CardTitle className="text-sm">请求捕获与历史保留</CardTitle>
            </CardHeader>
            <CardContent className="space-y-4">
              <div className="flex items-center justify-between rounded-md border px-3 py-2">
                <div>
                  <Label>启用捕获</Label>
                  <p className="text-muted-foreground text-xs">
                    记录请求 / 响应体，供监控页查看明细。
                  </p>
                </div>
                <Switch
                  checked={form.capture_enabled}
                  onCheckedChange={(v) => set("capture_enabled", v)}
                />
              </div>
              <div className="grid grid-cols-1 gap-4 sm:grid-cols-2">
                <div className="space-y-2">
                  <Label>捕获保留条数</Label>
                  <Input
                    type="number"
                    value={form.capture_max_entries}
                    onChange={(e) => set("capture_max_entries", num(e.target.value))}
                  />
                  <p className="text-muted-foreground text-xs">
                    流式与非流式请求都受这个上限约束。正文按 zstd 压缩落库，
                    压缩与解压都不在请求转发的热路径上；监控页可随时清空历史。
                  </p>
                </div>
                <div className="space-y-2">
                  <Label>明细保留天数</Label>
                  <Input
                    type="number"
                    min={0}
                    value={form.log_retention_days}
                    onChange={(e) => set("log_retention_days", num(e.target.value))}
                  />
                  <p className="text-muted-foreground text-xs">
                    请求明细与捕获原文都按它清理，改完立即生效；填 0 表示不按时间清理。
                    与条数上限同时生效，谁先到算谁 —— 想只留最近一天，填 1 即可。
                  </p>
                </div>
              </div>
            </CardContent>
          </Card>

          <Card>
            <CardHeader>
              <CardTitle className="text-sm">提示缓存</CardTitle>
            </CardHeader>
            <CardContent className="space-y-4">
              <div className="flex items-center justify-between rounded-md border px-3 py-2">
                <div>
                  <Label>跨协议转 Anthropic 时自动打缓存断点</Label>
                  <p className="text-muted-foreground text-xs">
                    客户端说 OpenAI 协议、上游是 Anthropic 时，转换会丢掉
                    <code> cache_control</code>。打开后网关在 system / tools /
                    最后一条消息上补断点，长上下文能命中上游提示缓存。直通请求不碰
                    （客户端自己的断点原样保留）。缓存写入按上游规则略贵，
                    上下文会被复用的 Agent 会话通常是净收益。
                  </p>
                </div>
                <Switch
                  checked={form.prompt_cache_injection}
                  onCheckedChange={(v) => set("prompt_cache_injection", v)}
                />
              </div>
            </CardContent>
          </Card>

          <Card>
            <CardHeader>
              <CardTitle className="text-sm">请求 / 响应改写脚本</CardTitle>
            </CardHeader>
            <CardContent className="space-y-5">
              <p className="text-muted-foreground text-xs leading-relaxed">
                脚本以 <code>resolve(ctx)</code> 为入口，<code>ctx.json</code> 是 IR
                的 JSON 字符串：<code>const o = JSON.parse(ctx.json); …; return
                JSON.stringify(o)</code>。返回 null / 空串表示不改写；
                语法错、抛错、超时都按不改写处理，**绝不会让请求失败**。
                请求脚本在缓存键之前生效，改过请求时会关闭同协议直通（需要重编码）。
              </p>

              <div className="space-y-2">
                <div className="flex items-center justify-between">
                  <Label>请求改写</Label>
                  <Button
                    variant="outline"
                    size="sm"
                    onClick={() => checkScript("request")}
                  >
                    校验语法
                  </Button>
                </div>
                <Textarea
                  value={form.request_script}
                  onChange={(e) => set("request_script", e.target.value)}
                  placeholder={
                    "function resolve(ctx) {\n  const o = JSON.parse(ctx.json);\n  o.temperature = 0;\n  return JSON.stringify(o);\n}"
                  }
                  className="min-h-40 font-mono text-xs"
                  spellCheck={false}
                />
                {scriptErrors.request ? (
                  <p className="text-destructive text-xs whitespace-pre-wrap">
                    {scriptErrors.request}
                  </p>
                ) : null}
              </div>

              <div className="space-y-2">
                <div className="flex items-center justify-between">
                  <Label>响应改写（非流式）</Label>
                  <Button
                    variant="outline"
                    size="sm"
                    onClick={() => checkScript("response")}
                  >
                    校验语法
                  </Button>
                </div>
                <Textarea
                  value={form.response_script}
                  onChange={(e) => set("response_script", e.target.value)}
                  placeholder={
                    "function resolve(ctx) {\n  const o = JSON.parse(ctx.json);\n  // 例如把回答里的内网地址脱敏\n  return JSON.stringify(o);\n}"
                  }
                  className="min-h-40 font-mono text-xs"
                  spellCheck={false}
                />
                {scriptErrors.response ? (
                  <p className="text-destructive text-xs whitespace-pre-wrap">
                    {scriptErrors.response}
                  </p>
                ) : null}
                <p className="text-muted-foreground text-xs">
                  契约是「完整响应进、完整响应出」，所以只作用于非流式请求。
                  流式请求请用下面的增量脚本。
                </p>
              </div>

              <div className="space-y-2">
                <div className="flex items-center justify-between">
                  <Label>流式增量改写</Label>
                  <Button
                    variant="outline"
                    size="sm"
                    onClick={() => checkScript("delta")}
                  >
                    校验语法
                  </Button>
                </div>
                <Textarea
                  value={form.response_delta_script}
                  onChange={(e) => set("response_delta_script", e.target.value)}
                  placeholder={
                    "// ctx.kind === \"delta\"，ctx.json 是**一条**增量\nfunction resolve(ctx) {\n  const d = JSON.parse(ctx.json);\n  if (d.kind === \"text\") d.text = d.text.replace(/secret/gi, \"***\");\n  return JSON.stringify(d);\n}\n// 返回 [] 可以丢弃这条增量"
                  }
                  className="min-h-40 font-mono text-xs"
                  spellCheck={false}
                />
                {scriptErrors.delta ? (
                  <p className="text-destructive text-xs whitespace-pre-wrap">
                    {scriptErrors.delta}
                  </p>
                ) : null}
                <p className="text-muted-foreground text-xs">
                  每解出一条增量调用一次，改完**立即**下发，首字延迟不变。
                  代价是脚本看不到完整回答：文本增量可能把一个词切成两半，
                  跨 chunk 的模式匹配要自己处理；且流式不再走同协议直通
                  （必须重新编码）。返回单个对象、对象数组（可拆成多条）
                  或 <code>[]</code>（丢弃这条）。
                </p>
              </div>
            </CardContent>
          </Card>

          <Card>
            <CardHeader className="flex-row items-center justify-between">
              <CardTitle className="text-sm">数据目录</CardTitle>
              <FolderOpen className="text-muted-foreground size-4" />
            </CardHeader>
            <CardContent className="space-y-2 text-xs">
              <div className="flex flex-wrap gap-2">
                <span className="text-muted-foreground w-24 shrink-0">Home</span>
                <code className="break-all">{info?.apilot_home ?? "—"}</code>
              </div>
              <div className="flex flex-wrap gap-2">
                <span className="text-muted-foreground w-24 shrink-0">数据库</span>
                <code className="break-all">{info?.db_path ?? "—"}</code>
              </div>
              <div className="flex flex-wrap items-center gap-2">
                <span className="text-muted-foreground w-24 shrink-0">日志</span>
                <code className="break-all">{info?.log_dir ?? "—"}</code>
                <Button
                  variant="ghost"
                  size="sm"
                  disabled={!info?.log_dir}
                  onClick={() => info?.log_dir && openPath(info.log_dir)}
                >
                  打开
                </Button>
              </div>
              <p className="text-muted-foreground text-xs">
                按天滚动、保留 7 天。打包后的窗口程序没有控制台，
                日志文件是排障时唯一能看到现场的地方。
              </p>
              <div className="flex flex-wrap gap-2">
                <span className="text-muted-foreground w-24 shrink-0">版本</span>
                <code>
                  {info?.name ?? "Apilot"} v{info?.version ?? "—"}
                </code>
              </div>
            </CardContent>
          </Card>
        </div>
      )}
    </PageShell>
  );
}
