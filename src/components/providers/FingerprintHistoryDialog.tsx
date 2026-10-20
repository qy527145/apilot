import { useQuery } from "@tanstack/react-query";
import { History } from "lucide-react";

import { FingerprintResultView } from "@/components/providers/FingerprintResultView";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { Button } from "@/components/ui/button";
import { ScrollArea } from "@/components/ui/scroll-area";
import { Skeleton } from "@/components/ui/skeleton";
import { qk } from "@/hooks/queries";
import { api } from "@/lib/api";

/**
 * 历史检测详情。结果由后端按 id 从 `fingerprint_detections.result_json`
 * 读回；「加入样本库」仍然可用，因为原始回答也在同一条记录里。
 */
export function FingerprintHistoryDialog({
  open,
  onOpenChange,
  detectionId,
}: {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  detectionId: number | null;
}) {
  const query = useQuery({
    queryKey: qk.fingerprintDetection(detectionId ?? 0),
    queryFn: () => api.getFingerprintDetection(detectionId!),
    enabled: open && detectionId !== null,
    retry: 1,
  });

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="flex max-h-[90vh] flex-col gap-3 overflow-hidden sm:max-w-3xl">
        <DialogHeader className="shrink-0">
          <DialogTitle className="flex items-center gap-2">
            <History className="size-4" />
            检测记录 #{detectionId ?? "—"}
          </DialogTitle>
          <DialogDescription className="text-xs">
            这是当时落库的完整结果；参考库版本与请求详情都按当时的状态保存。
          </DialogDescription>
        </DialogHeader>

        <ScrollArea className="min-h-0 flex-1 pr-2">
          {query.isLoading && <Skeleton className="h-64 w-full" />}
          {query.isError && (
            <p className="text-destructive py-6 text-xs">
              读取检测记录失败：{query.error instanceof Error ? query.error.message : "未知错误"}
            </p>
          )}
          {query.data && <FingerprintResultView result={query.data} />}
        </ScrollArea>

        <DialogFooter className="shrink-0">
          <Button onClick={() => onOpenChange(false)}>关闭</Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
