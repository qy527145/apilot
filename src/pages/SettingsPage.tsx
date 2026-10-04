import { useEffect, useState } from "react";
import { useMutation, useQueryClient } from "@tanstack/react-query";
import { FolderOpen, Save } from "lucide-react";
import { toast } from "sonner";

import { ErrorState } from "@/components/common/StatCard";
import { PageShell } from "@/components/layout/PageShell";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Skeleton } from "@/components/ui/skeleton";
import { Switch } from "@/components/ui/switch";
import { qk, useAppInfo, useSettings } from "@/hooks/queries";
import { api, type AppSettings } from "@/lib/api";

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
