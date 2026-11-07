import {
  createContext,
  useCallback,
  useContext,
  useRef,
  useState,
  type ReactNode,
} from "react";

import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";

/**
 * 应用内确认框。
 *
 * **不要用 `window.confirm`**：macOS 的 WKWebView 不实现 JS 对话框，`confirm()`
 * 直接返回 `false` 而不弹窗 —— 表现为「点了删除没反应」，只在 Mac 上复现，
 * 在浏览器里开发时完全看不出来。应用内对话框在所有平台行为一致。
 */

export interface ConfirmOptions {
  title?: string;
  /** 一句话说明后果，可以带被删对象的名字。 */
  description: ReactNode;
  confirmLabel?: string;
  cancelLabel?: string;
  /** 删除这类不可逆操作置 true，确认按钮变红。 */
  destructive?: boolean;
}

type ConfirmFn = (options: ConfirmOptions | string) => Promise<boolean>;

const ConfirmContext = createContext<ConfirmFn | null>(null);

interface Pending {
  options: ConfirmOptions;
  resolve: (ok: boolean) => void;
}

export function ConfirmProvider({ children }: { children: ReactNode }) {
  const [pending, setPending] = useState<Pending | null>(null);
  // 用 ref 存 resolver 而不是把它放进 state：关闭动画期间 state 会先变空，
  // 那时再读就得从已经为 null 的 pending 上取，会把 promise 永远挂着。
  const current = useRef<Pending | null>(null);

  const confirm = useCallback<ConfirmFn>((input) => {
    const options: ConfirmOptions =
      typeof input === "string" ? { description: input } : input;

    return new Promise<boolean>((resolve) => {
      const next = { options, resolve };
      current.current = next;
      setPending(next);
    });
  }, []);

  const settle = (ok: boolean) => {
    current.current?.resolve(ok);
    current.current = null;
    setPending(null);
  };

  return (
    <ConfirmContext.Provider value={confirm}>
      {children}
      <Dialog
        open={pending !== null}
        onOpenChange={(open) => {
          // 点遮罩 / 按 Esc 等同于取消，而不是把 promise 挂在那里。
          if (!open) settle(false);
        }}
      >
        <DialogContent className="sm:max-w-md">
          <DialogHeader>
            <DialogTitle>{pending?.options.title ?? "确认操作"}</DialogTitle>
            <DialogDescription className="text-xs">
              {pending?.options.description}
            </DialogDescription>
          </DialogHeader>
          <DialogFooter>
            <Button variant="outline" onClick={() => settle(false)}>
              {pending?.options.cancelLabel ?? "取消"}
            </Button>
            <Button
              variant={pending?.options.destructive ? "destructive" : "default"}
              onClick={() => settle(true)}
            >
              {pending?.options.confirmLabel ?? "确认"}
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
    </ConfirmContext.Provider>
  );
}

/** 打开一个确认框，`await` 它拿到用户的选择。 */
export function useConfirm(): ConfirmFn {
  const fn = useContext(ConfirmContext);
  if (!fn) {
    throw new Error("useConfirm 必须在 ConfirmProvider 里使用");
  }
  return fn;
}
