import { NAV_ITEMS, type ViewKey } from "@/components/layout/nav";
import { cn } from "@/lib/utils";
import { Zap } from "lucide-react";

interface SidebarProps {
  current: ViewKey;
  onSelect: (view: ViewKey) => void;
  version?: string;
  running?: boolean;
}

export function Sidebar({ current, onSelect, version, running }: SidebarProps) {
  return (
    <aside className="bg-sidebar text-sidebar-foreground flex w-56 shrink-0 flex-col border-r">
      <div className="flex items-center gap-2 px-5 py-5">
        <div className="bg-primary text-primary-foreground flex size-8 items-center justify-center rounded-lg">
          <Zap className="size-4" />
        </div>
        <div className="min-w-0">
          <p className="truncate text-sm font-semibold">Apilot</p>
          <p className="text-muted-foreground truncate text-[10px]">
            LLM API 智能网关
          </p>
        </div>
      </div>

      <nav className="flex-1 space-y-1 overflow-y-auto px-3 pb-4">
        {NAV_ITEMS.map((item) => {
          const active = item.key === current;
          return (
            <button
              key={item.key}
              onClick={() => onSelect(item.key)}
              className={cn(
                "flex w-full cursor-pointer items-center gap-2.5 rounded-md px-3 py-2 text-sm transition-colors",
                active
                  ? "bg-sidebar-accent text-sidebar-accent-foreground font-medium"
                  : "text-muted-foreground hover:bg-sidebar-accent/60 hover:text-sidebar-accent-foreground",
              )}
            >
              <item.icon className="size-4 shrink-0" />
              <span className="truncate">{item.label}</span>
            </button>
          );
        })}
      </nav>

      <div className="border-t px-5 py-3">
        <div className="flex items-center gap-2">
          <span
            className={cn(
              "size-2 rounded-full",
              running ? "bg-emerald-500" : "bg-muted-foreground/40",
            )}
          />
          <span className="text-muted-foreground text-xs">
            {running ? "网关运行中" : "网关已停止"}
          </span>
        </div>
        {version && (
          <p className="text-muted-foreground/60 mt-1 text-[10px]">v{version}</p>
        )}
      </div>
    </aside>
  );
}
