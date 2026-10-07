import { Copy } from "lucide-react";
import { toast } from "sonner";

import { Button } from "@/components/ui/button";

/** 复制一段文本到剪贴板。报文预览与接入示例共用。 */
export function CopyButton({ text }: { text: string }) {
  return (
    <Button
      variant="ghost"
      size="sm"
      className="h-6 px-2 text-[11px]"
      onClick={() => {
        navigator.clipboard
          .writeText(text)
          .then(() => toast.success("已复制到剪贴板"))
          .catch(() => toast.error("复制失败，请手动选中复制"));
      }}
    >
      <Copy className="size-3" />
      复制
    </Button>
  );
}
