import type { ReactNode } from "react";

interface PageShellProps {
  title: string;
  description?: string;
  actions?: ReactNode;
  children: ReactNode;
}

/** 统一的页面容器：固定标题栏 + 可滚动内容区，避免窄窗口横向溢出 */
export function PageShell({
  title,
  description,
  actions,
  children,
}: PageShellProps) {
  return (
    <div className="flex h-full min-w-0 flex-col">
      <header className="flex shrink-0 flex-wrap items-center justify-between gap-3 border-b px-6 py-4">
        <div className="min-w-0">
          <h1 className="truncate text-lg font-semibold">{title}</h1>
          {description && (
            <p className="text-muted-foreground truncate text-xs">
              {description}
            </p>
          )}
        </div>
        {actions && (
          <div className="flex shrink-0 flex-wrap items-center gap-2">
            {actions}
          </div>
        )}
      </header>
      <div
        className="min-h-0 flex-1 overflow-y-auto overflow-x-hidden p-6"
        onScroll={(e) => {
          // `overflow-x-hidden` 只是不显示滚动条，容器**仍然可以被横向滚动**。
          // 内容比容器宽时（表格里那些不换行的数字列），某个宽元素一旦被聚焦，
          // 浏览器会把它滚进视野 —— 左侧那圈 24px 的 padding 就这么"消失"了，
          // 而且没有滚动条能拖回来。这里把横向位置钉死在 0。
          const el = e.currentTarget;
          if (el.scrollLeft !== 0) el.scrollLeft = 0;
        }}
      >
        {children}
      </div>
    </div>
  );
}
