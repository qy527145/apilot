import { useEffect, useState } from "react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";

import { Sidebar } from "@/components/layout/Sidebar";
import { VIEW_STORAGE_KEY, isViewKey, type ViewKey } from "@/components/layout/nav";
import { Toaster } from "@/components/ui/sonner";
import { useAppInfo, useGatewayStatus } from "@/hooks/queries";

import OverviewPage from "@/pages/OverviewPage";
import ClientsPage from "@/pages/ClientsPage";
import ProvidersPage from "@/pages/ProvidersPage";
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

  useEffect(() => {
    localStorage.setItem(VIEW_STORAGE_KEY, view);
  }, [view]);

  const { data: info } = useAppInfo();
  const { data: gateway } = useGatewayStatus();

  return (
    <div className="flex h-screen w-screen overflow-hidden">
      <Sidebar
        current={view}
        onSelect={setView}
        version={info?.version}
        running={gateway?.running}
      />
      <main className="min-w-0 flex-1">
        {view === "overview" && <OverviewPage onNavigate={setView} />}
        {view === "clients" && <ClientsPage onNavigate={setView} />}
        {view === "providers" && <ProvidersPage />}
        {view === "routing" && <RoutingPage />}
        {view === "traffic" && <TrafficPage />}
        {view === "billing" && <BillingPage />}
        {view === "cache" && <CachePage />}
        {view === "settings" && <SettingsPage />}
      </main>
    </div>
  );
}

export default function App() {
  return (
    <QueryClientProvider client={queryClient}>
      <Shell />
      <Toaster />
    </QueryClientProvider>
  );
}
