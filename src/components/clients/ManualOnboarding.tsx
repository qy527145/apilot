import { useState } from "react";
import { Info, Plug } from "lucide-react";

import { CopyButton } from "@/components/common/CopyButton";
import { RawBody } from "@/components/common/RawBody";
import { Card, CardContent, CardHeader } from "@/components/ui/card";
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/tabs";
import { useGatewayStatus } from "@/hooks/queries";
import { ALL_PROTOCOLS, PROTOCOL_LABEL, type Protocol } from "@/lib/api";
import { AUTH_NOTE, ONBOARDING, fillBase } from "@/lib/clientOnboarding";

/** 数据目录里那种 `code` 小片段。 */
function Code({ children }: { children: React.ReactNode }) {
  return (
    <code className="bg-muted rounded px-1 py-0.5 font-mono text-[11px]">
      {children}
    </code>
  );
}

/**
 * 网关地址。
 *
 * 监听 `0.0.0.0` 时不能照抄给客户端 —— 那是个「监听所有网卡」的写法，
 * 不是一个可连接的目标，客户端拿去会解析失败。展示成回环才对得上实际用法。
 */
function gatewayBase(gateway?: { host: string; port: number } | null): string | null {
  if (!gateway) return null;
  const host =
    gateway.host === "0.0.0.0" || gateway.host === "::" || gateway.host === ""
      ? "127.0.0.1"
      : gateway.host;
  return `http://${host}:${gateway.port}`;
}

/**
 * 手动接入说明。
 *
 * 接管功能只覆盖三个主流客户端，自研 / 第三方 Agent 客户端得自己填 base_url。
 * 这张卡片把「填什么、发什么」一次说清，免得用户去翻源码猜路径。
 */
export function ManualOnboarding() {
  const { data: gateway } = useGatewayStatus();
  const [protocol, setProtocol] = useState<Protocol>("anthropic");

  const base = gatewayBase(gateway);
  const running = !!gateway?.running;
  // 网关没跑也给一份示例（用默认端口），只是标注一下"当前未运行"。
  const shownBase = base ?? "http://127.0.0.1:8787";

  return (
    <Card className="py-0">
      <CardHeader className="gap-2 py-3">
        <div className="flex flex-wrap items-center gap-2">
          <Plug className="size-4" />
          <span className="font-medium">手动接入其它客户端</span>
        </div>
        <p className="text-muted-foreground text-xs">
          上面的接管只覆盖 Claude Code / Codex / Gemini CLI。任何能改
          <Code>base_url</Code> 的 Agent 客户端都可以直接指向 Apilot ——
          Apilot 同时提供三种协议的入口，客户端用哪种就发哪个地址。
        </p>
      </CardHeader>

      <CardContent className="space-y-4 pt-0">
        <div className="flex flex-wrap items-center gap-x-3 gap-y-1 text-xs">
          <span className="text-muted-foreground">网关地址</span>
          <span className="font-mono">{shownBase}</span>
          <CopyButton text={shownBase} />
          {!running && (
            <span className="text-amber-600 dark:text-amber-400">
              网关当前未运行，地址按默认端口给出
            </span>
          )}
        </div>

        <Tabs value={protocol} onValueChange={(v) => setProtocol(v as Protocol)}>
          <TabsList>
            {ALL_PROTOCOLS.map((p) => (
              <TabsTrigger key={p} value={p}>
                {PROTOCOL_LABEL[p]}
              </TabsTrigger>
            ))}
          </TabsList>

          {ALL_PROTOCOLS.map((p) => {
            const item = ONBOARDING[p];
            return (
              <TabsContent key={p} value={p} className="space-y-4 pt-3">
                <dl className="grid grid-cols-1 gap-x-6 gap-y-2 text-xs sm:grid-cols-2">
                  <div className="flex gap-2">
                    <dt className="text-muted-foreground shrink-0">入口</dt>
                    <dd className="font-mono">
                      POST {item.path}
                      <span className="text-muted-foreground ml-1 font-sans">
                        （不带 <Code>/v1</Code> 的写法也通）
                      </span>
                    </dd>
                  </div>
                  <div className="flex gap-2">
                    <dt className="text-muted-foreground shrink-0">必填</dt>
                    <dd>{item.required}</dd>
                  </div>
                  <div className="flex gap-2">
                    <dt className="text-muted-foreground shrink-0">流式终止</dt>
                    <dd className="font-mono">{item.terminator}</dd>
                  </div>
                </dl>

                <RawBody text={fillBase(item.curl, shownBase)} rawMode />

                <div className="space-y-1.5">
                  <p className="text-xs font-medium">客户端环境变量</p>
                  <div className="overflow-hidden rounded-md border">
                    {item.env.map((e) => (
                      <div
                        key={e.name}
                        className="flex flex-wrap items-baseline gap-x-3 gap-y-0.5 border-b px-3 py-2 text-xs last:border-b-0"
                      >
                        <span className="font-mono font-medium">{e.name}</span>
                        <span className="font-mono">{fillBase(e.value, shownBase)}</span>
                        <span className="text-muted-foreground">{e.note}</span>
                      </div>
                    ))}
                  </div>
                </div>

                {item.notes.length > 0 && (
                  <ul className="text-muted-foreground list-disc space-y-1 pl-5 text-xs">
                    {item.notes.map((n, i) => (
                      <li key={i}>{n}</li>
                    ))}
                  </ul>
                )}
              </TabsContent>
            );
          })}
        </Tabs>

        <div className="bg-muted/30 flex items-start gap-2 rounded-md border p-3">
          <Info className="text-muted-foreground mt-0.5 size-4 shrink-0" />
          <div className="space-y-1.5 text-xs">
            <p className="font-medium">鉴权（三种协议一致）</p>
            {AUTH_NOTE.map((n, i) => (
              <p key={i} className="text-muted-foreground">
                {n}
              </p>
            ))}
          </div>
        </div>

        <p className="text-muted-foreground text-xs">
          想看「什么时候直通、什么时候转换、转换会丢什么」，见仓库里的{" "}
          <Code>docs/PROTOCOL_MATRIX.md</Code>。
        </p>
      </CardContent>
    </Card>
  );
}
