import { useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { toast } from "sonner";

import { PageShell } from "@/components/layout/PageShell";
import { RuleEditor } from "@/components/routing/RuleEditor";
import { RuleList } from "@/components/routing/RuleList";
import { SelectorPanel } from "@/components/routing/SelectorPanel";
import { qk } from "@/hooks/queries";
import { api, type RouteRule } from "@/lib/api";
import { useApilotEvent } from "@/lib/events";

export default function RoutingPage() {
  const [editorOpen, setEditorOpen] = useState(false);
  const [editing, setEditing] = useState<RouteRule | null>(null);

  const providers = useQuery({
    queryKey: qk.providers,
    queryFn: api.listProviders,
    retry: 1,
  });

  const selectors = useQuery({
    queryKey: qk.selectors,
    queryFn: api.listSelectors,
    retry: 1,
  });

  const rules = useQuery({
    queryKey: qk.routeRules,
    queryFn: api.listRouteRules,
    retry: 1,
  });

  // 后端自动切换选择器时给出提示
  useApilotEvent("apilot://selector-changed", (payload) => {
    toast.message(`选择器 ${payload.selector} 已切换到 ${payload.provider_tag}`, {
      description: payload.reason,
    });
  });

  return (
    <PageShell title="路由" description="选择器热切换与路由规则链">
      <div className="space-y-6">
        <SelectorPanel
          selectors={selectors.data ?? []}
          providers={providers.data ?? []}
          isLoading={selectors.isLoading || providers.isLoading}
          isError={selectors.isError}
          onRetry={() => selectors.refetch()}
        />

        <RuleList
          rules={rules.data ?? []}
          isLoading={rules.isLoading}
          isError={rules.isError}
          onRetry={() => rules.refetch()}
          onCreate={() => {
            setEditing(null);
            setEditorOpen(true);
          }}
          onEdit={(rule) => {
            setEditing(rule);
            setEditorOpen(true);
          }}
        />
      </div>

      <RuleEditor
        open={editorOpen}
        onOpenChange={setEditorOpen}
        rule={editing}
        selectors={selectors.data ?? []}
      />
    </PageShell>
  );
}
