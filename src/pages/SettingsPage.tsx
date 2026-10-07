import { useEffect, useState } from "react";
import { useMutation, useQueryClient } from "@tanstack/react-query";
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
  manual: "所有渠道默认走下面这个地址。留空等同于直连。",
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
            </CardContent>
          </Card>

          <Card>
            <CardHeader>
              <CardTitle className="text-sm">超时（毫秒）</CardTitle>
            </CardHeader>
            <CardContent>
              <div className="grid grid-cols-1 gap-4 sm:grid-cols-3">
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
                回环与私有网段（<code>127.0.0.1</code>、<code>192.168.*</code>{" "}
                等）始终直连，本地模型服务不会因为这里配了代理而连不上。
                单个渠道可以在渠道对话框里单独设置。
              </p>
            </CardContent>
          </Card>

          <Card>
            <CardHeader>
              <CardTitle className="text-sm">请求捕获</CardTitle>
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
              <div className="space-y-2 sm:max-w-xs">
                <Label>捕获保留条数</Label>
                <Input
                  type="number"
                  value={form.capture_max_entries}
                  onChange={(e) => set("capture_max_entries", num(e.target.value))}
                />
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
