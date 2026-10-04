import type { ReactNode } from "react";

import { Card, CardContent } from "@/components/ui/card";
import { Skeleton } from "@/components/ui/skeleton";
import { cn } from "@/lib/utils";

interface StatCardProps {
  label: string;
  value: ReactNode;
  hint?: ReactNode;
  icon?: ReactNode;
  loading?: boolean;
  className?: string;
}

export function StatCard({
  label,
  value,
  hint,
  icon,
  loading,
  className,
}: StatCardProps) {
  return (
    <Card className={cn("gap-0 py-5", className)}>
      <CardContent className="flex items-start justify-between gap-3">
        <div className="min-w-0 space-y-1.5">
          <p className="text-muted-foreground text-xs font-medium">{label}</p>
          {loading ? (
            <Skeleton className="h-7 w-24" />
          ) : (
            <p className="truncate text-2xl font-semibold tabular-nums">
              {value}
            </p>
          )}
          {hint && (
            <p className="text-muted-foreground truncate text-xs">{hint}</p>
          )}
        </div>
        {icon && (
          <div className="bg-muted text-muted-foreground rounded-md p-2">
            {icon}
          </div>
        )}
      </CardContent>
    </Card>
  );
}

export function TableSkeleton({ rows = 5, cols = 6 }: { rows?: number; cols?: number }) {
  return (
    <div className="space-y-2 p-1">
      {Array.from({ length: rows }).map((_, r) => (
        <div key={r} className="flex items-center gap-3">
          {Array.from({ length: cols }).map((_, c) => (
            <Skeleton key={c} className="h-6 flex-1" />
          ))}
        </div>
      ))}
    </div>
  );
}

interface ErrorStateProps {
  message?: string;
  onRetry?: () => void;
}

export function ErrorState({ message, onRetry }: ErrorStateProps) {
  return (
    <div className="flex flex-col items-center justify-center gap-2 rounded-lg border border-dashed px-6 py-10 text-center">
      <p className="text-sm font-medium text-destructive">加载失败</p>
      <p className="text-muted-foreground max-w-md text-xs">
        {message ?? "请检查网关是否正在运行，然后重试。"}
      </p>
      {onRetry && (
        <button
          onClick={onRetry}
          className="text-primary mt-1 text-xs underline underline-offset-4"
        >
          重新加载
        </button>
      )}
    </div>
  );
}
