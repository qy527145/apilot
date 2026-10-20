import * as React from "react";
import * as ScrollAreaPrimitive from "@radix-ui/react-scroll-area";

import { cn } from "@/lib/utils";

function ScrollArea({
  className,
  children,
  ...props
}: React.ComponentProps<typeof ScrollAreaPrimitive.Root>) {
  return (
    <ScrollAreaPrimitive.Root
      data-slot="scroll-area"
      // Root 是 **flex 列、Viewport 是 flex-1**，不是 Viewport 上的 `height:100%`。
      //
      // 百分比高度要求祖先有**确定**高度，而这里的高度几乎总是来自 `max-height`
      // （对话框的 `max-h-[90vh]`、页面上那些 `max-h-[520px]`）。那种情况下
      // `height:100%` 解不出参照物，退回成 auto —— Viewport 被内容撑到几千像素，
      // 外面那层 `overflow-hidden` 一剪，滚动条就没有落点了：内容比容器高得多，
      // 却一个滚动条都不出现，中间的选项直接看不见（现场：批量指纹检测的设置页）。
      //
      // flex 不依赖确定高度：`flex-1` + `min-h-0` 在确定性高度、`max-height`、
      // 以及内容更矮（此时 Root 收成内容高度）三种情形下都成立。
      className={cn("relative flex flex-col overflow-hidden", className)}
      {...props}
    >
      <ScrollAreaPrimitive.Viewport
        data-slot="scroll-area-viewport"
        className="focus-visible:ring-ring/50 min-h-0 w-full flex-1 rounded-[inherit] transition-[color,box-shadow] outline-none focus-visible:ring-[3px] focus-visible:outline-1"
      >
        {children}
      </ScrollAreaPrimitive.Viewport>
      <ScrollBar />
      <ScrollAreaPrimitive.Corner />
    </ScrollAreaPrimitive.Root>
  );
}

function ScrollBar({
  className,
  orientation = "vertical",
  ...props
}: React.ComponentProps<typeof ScrollAreaPrimitive.ScrollAreaScrollbar>) {
  return (
    <ScrollAreaPrimitive.ScrollAreaScrollbar
      data-slot="scroll-area-scrollbar"
      orientation={orientation}
      className={cn(
        "flex touch-none p-px transition-colors select-none",
        orientation === "vertical" &&
          "h-full w-2.5 border-l border-l-transparent",
        orientation === "horizontal" &&
          "h-2.5 flex-col border-t border-t-transparent",
        className,
      )}
      {...props}
    >
      <ScrollAreaPrimitive.ScrollAreaThumb
        data-slot="scroll-area-thumb"
        className="bg-border relative flex-1 rounded-full"
      />
    </ScrollAreaPrimitive.ScrollAreaScrollbar>
  );
}

export { ScrollArea, ScrollBar };
