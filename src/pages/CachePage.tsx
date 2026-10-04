import { useEffect, useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Database, Eraser, HardDrive, PiggyBank } from "lucide-react";
import { toast } from "sonner";

import { ErrorState, StatCard } from "@/components/common/StatCard";
import { PageShell } from "@/components/layout/PageShell";
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
import { qk } from "@/hooks/queries";
import { api, type CachePolicy, type CacheScope } from "@/lib/api";
import { formatNumber, formatPercent, quotaToUsd } from "@/lib/utils";

function formatBytes(bytes: number): string {
  if (!bytes) return "0 B";
  const units = ["B", "KB", "MB", "GB"];
  let v = bytes;
  let i = 0;
  while (v >= 1024 && i < units.length - 1) {
    v /= 1024;
    i++;
  }
  return `${v.toFixed(i === 0 ? 0 : 1)} ${units[i]}`;
}

export default function CachePage() {
  const qc = useQueryClient();
  const [policy, setPolicy] = useState<CachePolicy>({
    enabled: true,
    ttl_secs: 3600,
    max_entries: 1000,
  });
  const [scopeKind, setScopeKind] = useState<CacheScope["kind"]>("all");
  const [scopeModel, setScopeModel] = useState("");

  const stats = useQuery({
    queryKey: qk.cacheStats,
    queryFn: api.cacheStats,
    refetchInterval: 10_000,
    retry: 1,
  });

  const policyQuery = useQuery({
    queryKey: qk.cachePolicy,
    queryFn: api.getCachePolicy,
    retry: 1,
  });

  useEffect(() => {
    if (policyQuery.data) setPolicy(policyQuery.data);
  }, [policyQuery.data]);

  const savePolicy = useMutation({
    mutationFn: (p: CachePolicy) => api.setCachePolicy(p),
    onSuccess: (p) => {
      qc.setQueryData(qk.cachePolicy, p);
      qc.invalidateQueries({ queryKey: qk.cacheStats });
      toast.success("缓存策略已保存");
    },
  });

  const clear = useMutation({
    mutationFn: (scope: CacheScope) => api.clearCache(scope),
    onSuccess: (n) => {
      qc.invalidateQueries({ queryKey: qk.cacheStats });
      toast.success(`已清空 ${n} 条缓存`);
    },
  });

  const doClear = () => {
    const scope: CacheScope =
      scopeKind === "model"
        ? { kind: "model", model: scopeModel.trim() || null }
        : { kind: scopeKind };
    if (scopeKind === "model" && !scope.model) {
      toast.error("请填写要清理的模型名");
      return;
    }
    clear.mutate(scope);
  };

  return (
    <PageShell title="缓存" description="响应缓存的命中情况与策略">
      <div className="space-y-6">
        <div className="grid grid-cols-1 gap-4 sm:grid-cols-2 xl:grid-cols-4">
          <StatCard
            label="缓存命中率"
            value={formatPercent(stats.data?.hit_rate)}
            icon={<Database className="size-4" />}
            loading={stats.isLoading}
            hint={`命中 ${formatNumber(stats.data?.hits)} / 未命中 ${formatNumber(
              stats.data?.misses,
            )}`}
          />
          <StatCard
            label="节省费用"
            value={quotaToUsd(stats.data?.saved_quota)}
            icon={<PiggyBank className="size-4" />}
            loading={stats.isLoading}
          />
          <StatCard
            label="缓存条目数"
            value={formatNumber(stats.data?.entries)}
            icon={<HardDrive className="size-4" />}
            loading={stats.isLoading}
          />
          <StatCard
            label="占用空间"
            value={formatBytes(stats.data?.total_bytes ?? 0)}
            icon={<HardDrive className="size-4" />}
            loading={stats.isLoading}
          />
        </div>

        {stats.isError && <ErrorState onRetry={() => stats.refetch()} />}

        <Card>
          <CardHeader>
            <CardTitle className="text-sm">缓存策略</CardTitle>
          </CardHeader>
          <CardContent className="space-y-5">
            <div className="flex items-center justify-between rounded-md border px-3 py-2">
              <div>
                <Label>启用缓存</Label>
                <p className="text-muted-foreground text-xs">
                  对相同请求直接返回缓存响应。
                </p>
              </div>
              <Switch
                checked={policy.enabled}
                onCheckedChange={(v) => setPolicy({ ...policy, enabled: v })}
              />
            </div>

            <div className="grid grid-cols-1 gap-4 sm:grid-cols-2">
              <div className="space-y-2">
                <Label>TTL（秒）</Label>
                <Input
                  type="number"
                  value={policy.ttl_secs}
                  onChange={(e) =>
                    setPolicy({ ...policy, ttl_secs: Number(e.target.value) || 0 })
                  }
                />
              </div>
              <div className="space-y-2">
                <Label>最大条目数</Label>
                <Input
                  type="number"
                  value={policy.max_entries}
                  onChange={(e) =>
                    setPolicy({
                      ...policy,
                      max_entries: Number(e.target.value) || 0,
                    })
                  }
                />
              </div>
            </div>

            <Button
              onClick={() => savePolicy.mutate(policy)}
              disabled={savePolicy.isPending}
            >
              {savePolicy.isPending ? "保存中…" : "保存策略"}
            </Button>
          </CardContent>
        </Card>

        <Card>
          <CardHeader>
            <CardTitle className="text-sm">清理缓存</CardTitle>
          </CardHeader>
          <CardContent className="space-y-4">
            <div className="flex flex-wrap items-end gap-3">
              <div className="space-y-2">
                <Label>清理范围</Label>
                <Select
                  value={scopeKind}
                  onValueChange={(v) => setScopeKind(v as CacheScope["kind"])}
                >
                  <SelectTrigger className="w-40">
                    <SelectValue />
                  </SelectTrigger>
                  <SelectContent>
                    <SelectItem value="all">全部</SelectItem>
                    <SelectItem value="expired">仅过期</SelectItem>
                    <SelectItem value="model">指定模型</SelectItem>
                  </SelectContent>
                </Select>
              </div>
              {scopeKind === "model" && (
                <div className="space-y-2">
                  <Label>模型名</Label>
                  <Input
                    value={scopeModel}
                    onChange={(e) => setScopeModel(e.target.value)}
                    placeholder="claude-3-5-sonnet"
                    className="w-64"
                  />
                </div>
              )}
              <Button
                variant="destructive"
                onClick={doClear}
                disabled={clear.isPending}
              >
                <Eraser className="size-4" />
                清空缓存
              </Button>
            </div>
            <p className="text-muted-foreground text-xs">
              清空操作不可撤销，正在进行的请求不受影响。
            </p>
          </CardContent>
        </Card>
      </div>
    </PageShell>
  );
}
