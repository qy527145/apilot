import { Loader2 } from "lucide-react";

import { Badge } from "@/components/ui/badge";
import type { FingerprintProgress } from "@/lib/events";

/**
 * 每条挑战的实时状态卡片：等待中 / 请求中 / 已返回，附带提示词、解析数字、
 * 耗时、协议、错误与回包预览。单模型检测和批量检测的当前模型共用。
 */
export function FingerprintProgressCards({
  total,
  challenges,
}: {
  total: number;
  challenges: Record<number, FingerprintProgress>;
}) {
  return (
    <div className="space-y-2">
      {Array.from({ length: total }, (_, index) => {
        const item = challenges[index];
        return (
          <div key={index} className="rounded border px-2 py-2 text-xs">
            <div className="flex flex-wrap items-center gap-2">
              <span className="font-mono font-medium">
                {item?.challenge_id ?? `挑战 ${index + 1}`}
              </span>
              {!item && <Badge variant="outline">等待中</Badge>}
              {item?.phase === "requesting" && (
                <Badge variant="secondary">
                  <Loader2 className="mr-1 size-3 animate-spin" />
                  请求中
                </Badge>
              )}
              {item?.phase === "answered" && (
                <Badge variant={item.ok ? "success" : "destructive"}>
                  {item.ok ? "已返回" : "失败"}
                </Badge>
              )}
              {item?.challenge_id && (
                <span className="text-muted-foreground">
                  期望 {item.expected_count} 个整数
                </span>
              )}
              {item?.phase === "answered" && (
                <span className="text-muted-foreground tabular-nums">
                  {item.parsed_numbers}/{item.minimum_numbers} 个数字 ·{" "}
                  {item.latency_ms} ms · {item.wire || "—"}
                </span>
              )}
            </div>
            {item?.challenge_prompt && (
              <details className="mt-1">
                <summary className="text-muted-foreground cursor-pointer text-[11px]">
                  查看提示词
                </summary>
                <p className="mt-1 text-[11px] whitespace-pre-wrap">
                  {item.challenge_prompt}
                </p>
              </details>
            )}
            {item?.error && (
              <p className="text-destructive mt-1 text-[11px] whitespace-pre-wrap">
                {item.error}
              </p>
            )}
            {item?.preview && (
              <p className="text-muted-foreground mt-1 text-[11px] whitespace-pre-wrap">
                回包预览：{item.preview}
              </p>
            )}
          </div>
        );
      })}
    </div>
  );
}
