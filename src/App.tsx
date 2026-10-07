import { useEffect, useState } from "react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";

import { Sidebar } from "@/components/layout/Sidebar";
import { VIEW_STORAGE_KEY, isViewKey, type ViewKey } from "@/components/layout/nav";
import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { Toaster } from "@/components/ui/sonner";
import { useAppInfo, useGatewayStatus } from "@/hooks/queries";
import {
  UnsavedChangesProvider,
  useHasUnsavedChanges,
} from "@/lib/unsaved";

import OverviewPage from "@/pages/OverviewPage";
import ClientsPage from "@/pages/ClientsPage";
import ProvidersPage from "@/pages/ProvidersPage";
import ModelsPage from "@/pages/ModelsPage";
import RoutingPage from "@/pages/RoutingPage";
import TrafficPage from "@/pages/TrafficPage";
import BillingPage from "@/pages/BillingPage";
import CachePage from "@/pages/CachePage";
import SettingsPage from "@/pages/SettingsPage";

/**
 * Apilot 前端外壳：单壳 + 侧边栏导航 + currentView 状态切换（无 react-router）。
 * 当前页持久化到 localStorage。
 *
 * 布局说明：Tauri 默认窗口为 1200x800（定义在 src-tauri/tauri.conf.json，
 * 本项目约定不改动该文件）。侧边栏固定 224px，主内容区自适应并内部滚动；
 * 所有表格容器带 overflow-x-auto，窄窗口下横向滚动而不撑破布局。
 */
const queryClient = new QueryClient({
  defaultOptions: {
    queries: {
      staleTime: 3000,
      refetchOnWindowFocus: false,
      retry: 1,
    },
  },
});

function Shell() {
  const [view, setView] = useState<ViewKey>(() => {
    const saved = localStorage.getItem(VIEW_STORAGE_KEY);
    return isViewKey(saved) ? saved : "overview";
  });
  /** 被未保存改动拦下来的目标页；非空即表示确认弹窗开着。 */
  const [pendingView, setPendingView] = useState<ViewKey | null>(null);
  const hasUnsaved = useHasUnsavedChanges();

  useEffect(() => {
    localStorage.setItem(VIEW_STORAGE_KEY, view);
  }, [view]);

  // 切页先过一道未保存检查。挡住的是「离开当前页」这个动作本身，
  // 所以放在这里而不是各页面里 —— 页面自己不知道用户要去哪。
  const selectView = (next: ViewKey) => {
    if (next === view) return;
    if (hasUnsaved) setPendingView(next);
    else setView(next);
  };

  const { data: info } = useAppInfo();
  const { data: gateway } = useGatewayStatus();

  return (
    <div className="flex h-screen w-screen overflow-hidden">
      <Sidebar
        current={view}
        onSelect={selectView}
        version={info?.version}
        running={gateway?.running}
      />
      <main className="min-w-0 flex-1">
        {view === "overview" && <OverviewPage onNavigate={selectView} />}
        {view === "clients" && <ClientsPage onNavigate={selectView} />}
        {view === "providers" && <ProvidersPage />}
        {view === "models" && <ModelsPage onNavigate={selectView} />}
        {view === "routing" && <RoutingPage />}
        {view === "traffic" && <TrafficPage />}
        {view === "billing" && <BillingPage />}
        {view === "cache" && <CachePage />}
        {view === "settings" && <SettingsPage />}
      </main>

      <Dialog
        open={pendingView !== null}
        onOpenChange={(o) => !o && setPendingView(null)}
      >
        <DialogContent className="sm:max-w-md">
          <DialogHeader>
            <DialogTitle>有未保存的更改</DialogTitle>
            <DialogDescription className="text-xs">
              当前页面还有改动没有保存，离开后这些改动会丢失。
            </DialogDescription>
          </DialogHeader>
          <DialogFooter>
            <Button variant="outline" onClick={() => setPendingView(null)}>
              留在本页
            </Button>
            <Button
              variant="destructive"
              onClick={() => {
                setView(pendingView as ViewKey);
                setPendingView(null);
              }}
            >
              放弃更改并离开
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
    </div>
  );
}

export default function App() {
  return (
    <QueryClientProvider client={queryClient}>
      <UnsavedChangesProvider>
        <Shell />
      </UnsavedChangesProvider>
      <Toaster />
    </QueryClientProvider>
  );
}
