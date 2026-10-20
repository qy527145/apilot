import { useEffect, useRef, useState } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import {
  AlertTriangle,
  Fingerprint,
  ListChecks,
  Loader2,
  Play,
  RefreshCw,
  Shuffle,
  XCircle,
} from "lucide-react";
import { toast } from "sonner";

import { FingerprintResultView } from "@/components/providers/FingerprintResultView";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Checkbox } from "@/components/ui/checkbox";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { Input } from "@/components/ui/input";
import { ScrollArea } from "@/components/ui/scroll-area";
import { qk } from "@/hooks/queries";
import { api, type FingerprintDetection } from "@/lib/api";
import { useApilotEvent, type FingerprintProgress } from "@/lib/events";

interface FingerprintDialogProps {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  providerId: number;
  providerName: string;
  model: string;
}

type ChallengeMode = "random" | "manual";

/**
 * 指纹检测对话框：确认花费 → 选择挑战/并行 → 逐条显示请求进度 → 结果与样本入库。
 *
 * 进度来自后端 `apilot://fingerprint` 事件：每条挑战开始/完成都会推一条，
 * 并行模式下哪条先回来就先更新哪张卡片。关闭对话框不会取消后端请求；
 * 完成后结果会写入「指纹样本 → 检测记录」。
 */
export function FingerprintDialog({
  open,
  onOpenChange,
  providerId,
  providerName,
  model,
}: FingerprintDialogProps) {
  const qc = useQueryClient();
  const [confirmed, setConfirmed] = useState(false);
  const [parallel, setParallel] = useState(true);
  const [mode, setMode] = useState<ChallengeMode>("random");
  const [selectedChallenges, setSelectedChallenges] = useState<string[]>([]);
  const [search, setSearch] = useState("");
  const [running, setRunning] = useState(false);
  const [result, setResult] = useState<FingerprintDetection | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [progress, setProgress] = useState<FingerprintProgress | null>(null);
  const [challengeProgress, setChallengeProgress] = useState<
    Record<number, FingerprintProgress>
  >({});
  const runIdRef = useRef<string | null>(null);

  useEffect(() => {
    if (!open) return;
    setConfirmed(false);
    setParallel(true);
    setMode("random");
    setSelectedChallenges([]);
    setSearch("");
    setRunning(false);
    setResult(null);
    setError(null);
    setProgress(null);
    setChallengeProgress({});
    runIdRef.current = null;
  }, [open]);

  const challengesQuery = useQuery({
    queryKey: qk.fingerprintChallenges,
    queryFn: api.listFingerprintChallenges,
    enabled: open,
    retry: 1,
  });

  useApilotEvent("apilot://fingerprint", (event) => {
    if (event.phase === "started") {
      runIdRef.current = event.run_id;
      setChallengeProgress({});
      setProgress(event);
      return;
    }
    if (runIdRef.current !== event.run_id) return;
    setProgress(event);
    if (event.challenge_index != null) {
      const index = event.challenge_index;
      setChallengeProgress((current) => ({ ...current, [index]: event }));
    }
  });

  const start = async () => {
    if (mode === "manual" && selectedChallenges.length !== 3) return;
    runIdRef.current = null;
    setProgress(null);
    setChallengeProgress({});
    setRunning(true);
    setError(null);
    setResult(null);
    try {
      const detection = await api.detectModelFingerprint(
        providerId,
        model,
        parallel,
        mode === "manual" ? selectedChallenges : null,
      );
      setResult(detection);
      qc.invalidateQueries({ queryKey: qk.fingerprintDetections });
    } catch (err) {
      setError(err instanceof Error ? err.message : "检测失败，请重试");
    } finally {
      setRunning(false);
    }
  };

  const challenges = challengesQuery.data ?? [];
  const filteredChallenges = challenges.filter((challenge) => {
    const keyword = search.trim().toLowerCase();
    if (!keyword) return true;
    return (
      challenge.challenge_id.toLowerCase().includes(keyword) ||
      challenge.condition.toLowerCase().includes(keyword) ||
      challenge.prompt.toLowerCase().includes(keyword)
    );
  });

  const toggleChallenge = (id: string) => {
    setSelectedChallenges((current) => {
      if (current.includes(id)) return current.filter((item) => item !== id);
      // 固定 3 条；已满时不替换，避免用户以为点了第四个却悄悄丢了前面一条。
      if (current.length >= 3) return current;
      return [...current, id];
    });
  };

  const total = progress?.total ?? 3;
  const completed = progress?.completed ?? 0;
  const percent = total > 0 ? Math.min(100, (completed / total) * 100) : 0;
  const canStart =
    confirmed &&
    !running &&
    (mode === "random" || selectedChallenges.length === 3);

  return (
    <Dialog
      open={open}
      onOpenChange={(next) => {
        if (!next && running) {
          toast.message("检测仍在后台进行，完成后会写入「指纹样本 → 检测记录」");
        }
        onOpenChange(next);
      }}
    >
      <DialogContent className="flex max-h-[90vh] flex-col gap-3 overflow-hidden sm:max-w-3xl">
        <DialogHeader className="shrink-0">
          <DialogTitle className="flex items-center gap-2">
            <Fingerprint className="size-4" />
            指纹检测 · <span className="font-mono text-sm">{model}</span>
          </DialogTitle>
          <DialogDescription className="text-xs">
            渠道 {providerName}。检测结果只是参考库闭集内的相对排名，不是模型身份的证明。
          </DialogDescription>
        </DialogHeader>

        <ScrollArea className="min-h-0 flex-1 pr-2">
          {!result && !running && !error && (
            <div className="space-y-4">
              <div className="rounded-md border border-amber-500/30 bg-amber-500/10 px-3 py-2 text-xs text-amber-700 dark:text-amber-400">
                <div className="mb-1 flex items-center gap-2 font-medium">
                  <AlertTriangle className="size-4" />
                  这会花额度
                </div>
                发送 3 次真实请求，每次最多 8192 output token。带思考的模型可能需要数十秒到几分钟。
                检测请求不写监控日志、不计费、不查缓存，但会消耗该渠道的 API 额度。
              </div>

              <label className="flex items-start gap-2 text-xs">
                <Checkbox
                  checked={parallel}
                  onCheckedChange={(checked) => setParallel(checked === true)}
                  className="mt-0.5"
                />
                <span>
                  <span className="font-medium">并行发送 3 条请求</span>
                  <span className="text-muted-foreground">
                    （更快；容易触发限流的渠道请取消，改为串行）
                  </span>
                </span>
              </label>

              <div className="space-y-2">
                <div className="flex flex-wrap items-center gap-2">
                  <span className="text-xs font-medium">挑战</span>
                  <div className="flex gap-1">
                    <Button
                      size="sm"
                      variant={mode === "random" ? "default" : "outline"}
                      onClick={() => setMode("random")}
                    >
                      <Shuffle className="size-4" />
                      随机抽取
                    </Button>
                    <Button
                      size="sm"
                      variant={mode === "manual" ? "default" : "outline"}
                      onClick={() => setMode("manual")}
                    >
                      <ListChecks className="size-4" />
                      手动选择
                    </Button>
                  </div>
                </div>
                {mode === "random" ? (
                  <p className="text-muted-foreground text-[11px]">
                    默认从 36 条固定挑战中随机抽 3 条。同一模型多测几轮会用到不同提示词，
                    覆盖更稳；参考库就是用这 36 条采集的。
                  </p>
                ) : (
                  <div className="space-y-2">
                    <p className="text-muted-foreground text-[11px]">
                      自己挑 3 条。提示词原样发送；选择顺序会保留在结果里。
                      已选{" "}
                      <span className="text-foreground font-medium">
                        {selectedChallenges.length}/3
                      </span>
                      {selectedChallenges.length >= 3 && "（先取消一条才能换）"}
                    </p>
                    <Input
                      value={search}
                      onChange={(event) => setSearch(event.target.value)}
                      placeholder="搜索挑战 ID / 条件 / 提示词"
                      className="h-8 text-xs"
                    />
                    {challengesQuery.isLoading ? (
                      <div className="text-muted-foreground py-4 text-xs">
                        正在加载挑战库…
                      </div>
                    ) : (
                      <div className="max-h-72 space-y-1 overflow-y-auto rounded border p-1">
                        {filteredChallenges.map((challenge) => {
                          const checked = selectedChallenges.includes(
                            challenge.challenge_id,
                          );
                          return (
                            <label
                              key={challenge.challenge_id}
                              className={`flex cursor-pointer items-start gap-2 rounded px-2 py-1.5 text-xs hover:bg-accent/50 ${
                                checked ? "bg-accent/40" : ""
                              }`}
                            >
                              <Checkbox
                                checked={checked}
                                onCheckedChange={() =>
                                  toggleChallenge(challenge.challenge_id)
                                }
                                className="mt-0.5"
                              />
                              <div className="min-w-0 space-y-0.5">
                                <div className="flex flex-wrap items-center gap-2">
                                  <span className="font-mono font-medium">
                                    {challenge.challenge_id}
                                  </span>
                                  <Badge variant="outline">
                                    {challenge.condition}
                                  </Badge>
                                  <span className="text-muted-foreground">
                                    期望 {challenge.expected_count} 个整数
                                  </span>
                                </div>
                                <p className="text-muted-foreground whitespace-pre-wrap text-[11px]">
                                  {challenge.prompt}
                                </p>
                              </div>
                            </label>
                          );
                        })}
                        {filteredChallenges.length === 0 && (
                          <p className="text-muted-foreground px-2 py-3 text-xs">
                            没有匹配的挑战。
                          </p>
                        )}
                      </div>
                    )}
                  </div>
                )}
              </div>

              <label className="flex items-start gap-2 text-xs">
                <Checkbox
                  checked={confirmed}
                  onCheckedChange={(checked) => setConfirmed(checked === true)}
                  className="mt-0.5"
                />
                我了解会消耗该渠道的额度，并确认开始检测。
              </label>
            </div>
          )}

          {running && (
            <div className="space-y-3">
              <div className="space-y-1">
                <div className="flex items-center justify-between text-xs">
                  <span>{progress?.message ?? "正在请求挑战…"}</span>
                  <span className="text-muted-foreground tabular-nums">
                    {completed}/{total} 完成 · {progress?.accepted ?? 0} 条有效
                  </span>
                </div>
                <div className="bg-muted h-1.5 overflow-hidden rounded-full">
                  <div
                    className="bg-primary h-full transition-all"
                    style={{ width: `${percent}%` }}
                  />
                </div>
              </div>

              <div className="space-y-2">
                {Array.from({ length: total }, (_, index) => {
                  const item = challengeProgress[index];
                  return (
                    <div key={index} className="rounded border px-2 py-2 text-xs">
                      <div className="flex flex-wrap items-center gap-2">
                        <span className="font-mono font-medium">
                          {item?.challenge_id ?? `挑战 ${index + 1}`}
                        </span>
                        {!item && (
                          <Badge variant="outline">等待中</Badge>
                        )}
                        {item?.phase === "requesting" && (
                          <Badge variant="secondary">
                            <Loader2 className="mr-1 size-3 animate-spin" />
                            请求中
                          </Badge>
                        )}
                        {item?.phase === "answered" && (
                          <Badge
                            variant={item.ok ? "success" : "destructive"}
                          >
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
                          <p className="mt-1 whitespace-pre-wrap text-[11px]">
                            {item.challenge_prompt}
                          </p>
                        </details>
                      )}
                      {item?.error && (
                        <p className="text-destructive mt-1 whitespace-pre-wrap text-[11px]">
                          {item.error}
                        </p>
                      )}
                      {item?.preview && (
                        <p className="text-muted-foreground mt-1 whitespace-pre-wrap text-[11px]">
                          回包预览：{item.preview}
                        </p>
                      )}
                    </div>
                  );
                })}
              </div>

              <p className="text-muted-foreground text-[11px]">
                关闭对话框不会取消后端请求；完成后会写入「指纹样本 → 检测记录」。
              </p>
            </div>
          )}

          {error && !running && (
            <div className="space-y-3 py-6">
              <div className="flex items-start gap-2 rounded-md border border-destructive/30 bg-destructive/10 px-3 py-2 text-xs text-destructive">
                <XCircle className="mt-0.5 size-4 shrink-0" />
                <span>{error}</span>
              </div>
              <Button size="sm" variant="outline" onClick={start}>
                <RefreshCw className="size-4" />
                重试
              </Button>
            </div>
          )}

          {result && !running && <FingerprintResultView result={result} />}
        </ScrollArea>

        <DialogFooter className="bg-background relative z-10 shrink-0 pt-2">
          {result ? (
            <>
              <Button variant="outline" onClick={start} disabled={running}>
                <RefreshCw className="size-4" />
                重新检测
              </Button>
              <Button onClick={() => onOpenChange(false)}>关闭</Button>
            </>
          ) : (
            <>
              <Button variant="outline" onClick={() => onOpenChange(false)}>
                取消
              </Button>
              <Button disabled={!canStart || running} onClick={start}>
                {running ? (
                  <Loader2 className="size-4 animate-spin" />
                ) : (
                  <Play className="size-4" />
                )}
                开始检测
              </Button>
            </>
          )}
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
