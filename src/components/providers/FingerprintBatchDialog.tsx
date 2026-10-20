import { useEffect, useRef, useState } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import {
  AlertTriangle,
  ChevronDown,
  ChevronUp,
  Fingerprint,
  Loader2,
  Play,
  RefreshCw,
  Search,
} from "lucide-react";
import { toast } from "sonner";

import {
  FingerprintChallengePicker,
  type ChallengeMode,
} from "@/components/providers/FingerprintChallengePicker";
import { FingerprintProgressCards } from "@/components/providers/FingerprintProgressCards";
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
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { qk } from "@/hooks/queries";
import {
  api,
  FINGERPRINT_BASIS_LABEL,
  FINGERPRINT_LEVEL_LABEL,
  FINGERPRINT_STATUS_LABEL,
  type FingerprintDetection,
} from "@/lib/api";
import { useApilotEvent, type FingerprintProgress } from "@/lib/events";
import { formatPercentage, topCandidate, topShare } from "@/lib/fingerprint";

export interface BatchModel {
  model: string;
  upstream: string;
}

interface FingerprintBatchDialogProps {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  providerId: number;
  providerName: string;
  models: BatchModel[];
}

/**
 * 批量指纹检测：勾选多个模型，按并发数并行跑完整检测。
 *
 * 模型之间按「模型并发」并行（默认 3），每个模型内部仍按开关并行/串行发送
 * 3 条挑战；峰值并发 = 模型并发 × (并行 ? 3 : 1)。结果逐个写入检测记录；
 * 每一行可以展开完整结果、加入本地样本库，或单独重测。
 */
export function FingerprintBatchDialog({
  open,
  onOpenChange,
  providerId,
  providerName,
  models,
}: FingerprintBatchDialogProps) {
  const qc = useQueryClient();
  const [selected, setSelected] = useState<string[]>([]);
  const [search, setSearch] = useState("");
  const [parallel, setParallel] = useState(true);
  const [concurrency, setConcurrency] = useState(3);
  const [mode, setMode] = useState<ChallengeMode>("random");
  const [selectedChallenges, setSelectedChallenges] = useState<string[]>([]);
  const [confirmed, setConfirmed] = useState(false);
  const [setup, setSetup] = useState(true);
  const [running, setRunning] = useState(false);
  const [results, setResults] = useState<Record<string, FingerprintDetection>>({});
  const [errors, setErrors] = useState<Record<string, string>>({});
  /** 当前在飞的模型。并行批量时会有多个。 */
  const [inFlight, setInFlight] = useState<string[]>([]);
  const [completed, setCompleted] = useState(0);
  const [progressByModel, setProgressByModel] = useState<
    Record<string, FingerprintProgress>
  >({});
  const [challengeProgressByModel, setChallengeProgressByModel] = useState<
    Record<string, Record<number, FingerprintProgress>>
  >({});
  const [expanded, setExpanded] = useState<string | null>(null);
  /** run_id → 模型名。并行时每条检测的进度事件靠它落到正确的行。 */
  const runToModelRef = useRef(new Map<string, string>());

  useEffect(() => {
    if (!open) return;
    // 上一批还在后台跑时重新打开对话框，保留现场，不要把它重置成确认页。
    if (running) return;
    setSelected(models.map((item) => item.model));
    setSearch("");
    setParallel(true);
    setConcurrency(3);
    setMode("random");
    setSelectedChallenges([]);
    setConfirmed(false);
    setSetup(true);
    setRunning(false);
    setResults({});
    setErrors({});
    setInFlight([]);
    setCompleted(0);
    setProgressByModel({});
    setChallengeProgressByModel({});
    setExpanded(null);
    runToModelRef.current.clear();
  }, [open]);

  const challengesQuery = useQuery({
    queryKey: qk.fingerprintChallenges,
    queryFn: api.listFingerprintChallenges,
    enabled: open,
    retry: 1,
  });

  useApilotEvent("apilot://fingerprint", (event) => {
    if (event.phase === "started") {
      runToModelRef.current.set(event.run_id, event.declared_model);
      setProgressByModel((current) => ({
        ...current,
        [event.declared_model]: event,
      }));
      setChallengeProgressByModel((current) => ({
        ...current,
        [event.declared_model]: {},
      }));
      return;
    }
    const model = runToModelRef.current.get(event.run_id);
    if (!model) return;
    setProgressByModel((current) => ({ ...current, [model]: event }));
    if (event.challenge_index != null) {
      const index = event.challenge_index;
      setChallengeProgressByModel((current) => ({
        ...current,
        [model]: { ...(current[model] ?? {}), [index]: event },
      }));
    }
  });

  const runModel = async (model: string): Promise<FingerprintDetection | null> => {
    setInFlight((current) =>
      current.includes(model) ? current : [...current, model],
    );
    setProgressByModel((current) => {
      const next = { ...current };
      delete next[model];
      return next;
    });
    setChallengeProgressByModel((current) => ({ ...current, [model]: {} }));
    try {
      const detection = await api.detectModelFingerprint(
        providerId,
        model,
        parallel,
        mode === "manual" ? selectedChallenges : null,
        // 批量里逐条失败由行内展示，避免 N 个 toast 刷屏。
        true,
      );
      setResults((current) => ({ ...current, [model]: detection }));
      setErrors((current) => {
        const next = { ...current };
        delete next[model];
        return next;
      });
      qc.invalidateQueries({ queryKey: qk.fingerprintDetections });
      return detection;
    } catch (error) {
      // 重测失败时不要保留上一次的摘要，否则这一行会同时显示旧结果和错误。
      setResults((current) => {
        const next = { ...current };
        delete next[model];
        return next;
      });
      setErrors((current) => ({
        ...current,
        [model]: error instanceof Error ? error.message : "检测失败",
      }));
      return null;
    } finally {
      setInFlight((current) => current.filter((item) => item !== model));
    }
  };

  const startBatch = async () => {
    if (selected.length === 0) return;
    if (mode === "manual" && selectedChallenges.length !== 3) return;
    setSetup(false);
    setRunning(true);
    setResults({});
    setErrors({});
    setInFlight([]);
    setCompleted(0);
    setProgressByModel({});
    setChallengeProgressByModel({});
    setExpanded(null);
    runToModelRef.current.clear();

    // 小并发池：模型按 concurrency 并行，每个模型内部仍发 3 条挑战。
    const queue = [...selected];
    const workerCount = Math.max(1, Math.min(concurrency, queue.length));
    const workers = Array.from({ length: workerCount }, async () => {
      while (true) {
        const model = queue.shift();
        if (!model) break;
        await runModel(model);
        setCompleted((current) => current + 1);
      }
    });
    await Promise.all(workers);
    setRunning(false);
    qc.invalidateQueries({ queryKey: qk.fingerprintDetections });
  };

  const retryModel = async (model: string) => {
    setRunning(true);
    await runModel(model);
    setRunning(false);
  };

  const modelsByName = new Map(models.map((item) => [item.model, item]));
  const visibleModels = models.filter((item) => {
    const keyword = search.trim().toLowerCase();
    if (!keyword) return true;
    return (
      item.model.toLowerCase().includes(keyword) ||
      item.upstream.toLowerCase().includes(keyword)
    );
  });
  const total = selected.length;
  const failed = Object.keys(errors).length;
  const percent = total > 0 ? Math.min(100, (completed / total) * 100) : 0;
  const peak = concurrency * (parallel ? 3 : 1);
  const toggleModel = (model: string) => {
    setSelected((current) =>
      current.includes(model)
        ? current.filter((item) => item !== model)
        : [...current, model],
    );
  };
  const toggleChallenge = (id: string) => {
    setSelectedChallenges((current) => {
      if (current.includes(id)) return current.filter((item) => item !== id);
      if (current.length >= 3) return current;
      return [...current, id];
    });
  };
  const canStart =
    confirmed &&
    !running &&
    selected.length > 0 &&
    (mode === "random" || selectedChallenges.length === 3);

  return (
    <Dialog
      open={open}
      onOpenChange={(next) => {
        if (!next && running) {
          toast.message(
            "批量检测仍在后台进行，完成后可在「指纹样本 → 检测记录」查看",
          );
        }
        onOpenChange(next);
      }}
    >
      <DialogContent className="flex max-h-[90vh] flex-col gap-3 overflow-hidden sm:max-w-4xl">
        <DialogHeader className="shrink-0">
          <DialogTitle className="flex items-center gap-2">
            <Fingerprint className="size-4" />
            批量指纹检测 · {providerName}
          </DialogTitle>
          <DialogDescription className="text-xs">
            模型之间按并发数并行，每个模型内部按设置发送 3 条挑战。结果会逐个写入检测记录。
          </DialogDescription>
        </DialogHeader>

        <ScrollArea className="min-h-0 flex-1 pr-2">
          {setup && (
            <div className="space-y-4">
              <div className="rounded-md border border-amber-500/30 bg-amber-500/10 px-3 py-2 text-xs text-amber-700 dark:text-amber-400">
                <div className="mb-1 flex items-center gap-2 font-medium">
                  <AlertTriangle className="size-4" />
                  会消耗额度
                </div>
                已选 {selected.length} 个模型，共 {selected.length * 3} 次真实请求
                （每个模型 3 条挑战）。模型之间按并发数并行；当前峰值并发约{" "}
                {peak} 个请求，遇到限流就调小模型并发或关掉单模型并行。
              </div>

              <div className="space-y-2 rounded-md border px-3 py-3">
                <div className="flex flex-wrap items-center justify-between gap-2">
                  <span className="text-xs font-medium">
                    模型（已选 {selected.length}/{models.length}）
                  </span>
                  <div className="flex gap-1">
                    <Button
                      size="sm"
                      variant="ghost"
                      onClick={() => setSelected(models.map((item) => item.model))}
                    >
                      全选
                    </Button>
                    <Button
                      size="sm"
                      variant="ghost"
                      onClick={() => setSelected([])}
                    >
                      清空
                    </Button>
                  </div>
                </div>
                <div className="relative">
                  <Search className="text-muted-foreground pointer-events-none absolute top-1/2 left-2 size-3.5 -translate-y-1/2" />
                  <Input
                    value={search}
                    onChange={(event) => setSearch(event.target.value)}
                    placeholder="搜索模型 / 上游名"
                    className="h-8 pl-7 text-xs"
                  />
                </div>
                <div className="max-h-56 space-y-1 overflow-y-auto rounded border p-1">
                  {visibleModels.map((item) => {
                    const checked = selected.includes(item.model);
                    return (
                      <label
                        key={item.model}
                        className={`flex cursor-pointer items-center gap-2 rounded px-2 py-1.5 text-xs hover:bg-accent/50 ${
                          checked ? "bg-accent/40" : ""
                        }`}
                      >
                        <Checkbox
                          checked={checked}
                          onCheckedChange={() => toggleModel(item.model)}
                        />
                        <span className="font-mono">{item.model}</span>
                        {item.upstream && item.upstream !== item.model && (
                          <span className="text-muted-foreground truncate font-mono text-[11px]">
                            → {item.upstream}
                          </span>
                        )}
                      </label>
                    );
                  })}
                  {visibleModels.length === 0 && (
                    <p className="text-muted-foreground px-2 py-3 text-xs">
                      没有匹配的模型。
                    </p>
                  )}
                </div>
              </div>

              <label className="flex items-start gap-2 text-xs">
                <Checkbox
                  checked={parallel}
                  onCheckedChange={(checked) => setParallel(checked === true)}
                  className="mt-0.5"
                />
                <span>
                  <span className="font-medium">每个模型并行发送 3 条请求</span>
                  <span className="text-muted-foreground">
                    （只影响单个模型内部；容易限流的渠道可取消）
                  </span>
                </span>
              </label>

              <div className="flex flex-wrap items-center gap-2 text-xs">
                <span className="font-medium">模型并发</span>
                <Select
                  value={String(concurrency)}
                  onValueChange={(value) => setConcurrency(Number(value))}
                >
                  <SelectTrigger className="h-8 w-24 text-xs">
                    <SelectValue />
                  </SelectTrigger>
                  <SelectContent>
                    {[1, 2, 3, 4, 5, 8, 10].map((value) => (
                      <SelectItem key={value} value={String(value)}>
                        {value} 个
                      </SelectItem>
                    ))}
                  </SelectContent>
                </Select>
                <span className="text-muted-foreground">
                  同时检测的模型数；当前峰值并发约 {peak} 个请求
                </span>
              </div>

              <FingerprintChallengePicker
                challenges={challengesQuery.data ?? []}
                loading={challengesQuery.isLoading}
                mode={mode}
                onModeChange={setMode}
                selected={selectedChallenges}
                onToggle={toggleChallenge}
              />
              {mode === "random" && (
                <p className="text-muted-foreground text-[11px]">
                  批量随机模式下每个模型会各自抽 3 条；想用同一组挑战横向对比，
                  请切到「手动选择」。
                </p>
              )}

              <label className="flex items-start gap-2 text-xs">
                <Checkbox
                  checked={confirmed}
                  onCheckedChange={(checked) => setConfirmed(checked === true)}
                  className="mt-0.5"
                />
                我了解会消耗该渠道的额度，并确认开始批量检测。
              </label>
            </div>
          )}

          {!setup && (
            <div className="space-y-3">
              <div className="space-y-1">
                <div className="flex flex-wrap items-center justify-between gap-2 text-xs">
                  <span>
                    {running
                      ? inFlight.length > 0
                        ? `正在检测 ${inFlight.length} 个模型：${inFlight
                            .slice(0, 3)
                            .join("、")}${inFlight.length > 3 ? "…" : ""}`
                        : "正在准备下一条…"
                      : "批量检测结束"}
                  </span>
                  <span className="text-muted-foreground tabular-nums">
                    已完成 {completed}/{total}
                    {failed > 0 ? ` · ${failed} 个失败` : ""}
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
                {selected.map((model) => {
                  const result = results[model];
                  const error = errors[model];
                  const active = inFlight.includes(model);
                  const waiting = !result && !error && !active;
                  const expandedRow = expanded === model;
                  const meta = modelsByName.get(model);
                  const rowProgress = progressByModel[model];
                  const rowChallenges = challengeProgressByModel[model] ?? {};
                  return (
                    <div key={model} className="rounded-md border">
                      <div className="flex flex-wrap items-center gap-2 px-3 py-2 text-xs">
                        {active ? (
                          <Badge variant="secondary">
                            <Loader2 className="mr-1 size-3 animate-spin" />
                            检测中
                          </Badge>
                        ) : result ? (
                          <Badge
                            variant={
                              result.status === "ok"
                                ? "success"
                                : result.status === "partial"
                                  ? "warning"
                                  : "destructive"
                            }
                          >
                            {FINGERPRINT_STATUS_LABEL[result.status] ?? result.status}
                          </Badge>
                        ) : error ? (
                          <Badge variant="destructive">失败</Badge>
                        ) : (
                          <Badge variant="outline">等待中</Badge>
                        )}
                        <span className="font-mono font-medium">{model}</span>
                        {meta?.upstream && meta.upstream !== model && (
                          <span className="text-muted-foreground truncate font-mono text-[11px]">
                            → {meta.upstream}
                          </span>
                        )}
                        {result && (
                          <BatchSummary result={result} />
                        )}
                        {result && (
                          <Button
                            size="sm"
                            variant="ghost"
                            className="ml-auto h-6 px-2 text-[11px]"
                            onClick={() => setExpanded(expandedRow ? null : model)}
                          >
                            {expandedRow ? (
                              <ChevronUp className="size-3" />
                            ) : (
                              <ChevronDown className="size-3" />
                            )}
                            {expandedRow ? "收起" : "查看详情"}
                          </Button>
                        )}
                        {error && (
                          <>
                            <span className="text-destructive basis-full whitespace-normal">
                              {error}
                            </span>
                            <Button
                              size="sm"
                              variant="outline"
                              className="ml-auto h-6 px-2 text-[11px]"
                              disabled={running || active}
                              onClick={() => retryModel(model)}
                            >
                              <RefreshCw className="size-3" />
                              重试这个模型
                            </Button>
                          </>
                        )}
                        {result && (
                          <Button
                            size="sm"
                            variant="outline"
                            className="h-6 px-2 text-[11px]"
                            disabled={running || active}
                            onClick={() => retryModel(model)}
                          >
                            <RefreshCw className="size-3" />
                            重测
                          </Button>
                        )}
                        {waiting && (
                          <span className="text-muted-foreground text-[11px]">
                            等待队列中
                          </span>
                        )}
                      </div>

                      {active && (
                        <div className="border-t px-3 py-2">
                          <FingerprintProgressCards
                            total={rowProgress?.total ?? 3}
                            challenges={rowChallenges}
                          />
                        </div>
                      )}

                      {result && expandedRow && (
                        <div className="border-t px-3 py-3">
                          <FingerprintResultView
                            result={result}
                            onUpdated={(updated) =>
                              setResults((current) => ({
                                ...current,
                                [model]: updated,
                              }))
                            }
                          />
                        </div>
                      )}
                    </div>
                  );
                })}
              </div>

              {!running && failed > 0 && (
                <p className="text-muted-foreground text-[11px]">
                  失败的模型可以单独重试，或关闭对话框后在「指纹样本 → 检测记录」里查看已完成的结果。
                </p>
              )}
            </div>
          )}
        </ScrollArea>

        <DialogFooter className="bg-background relative z-10 shrink-0 pt-2">
          {!setup ? (
            <>
              {!running && (
                <Button variant="ghost" onClick={() => setSetup(true)}>
                  返回设置
                </Button>
              )}
              <Button
                variant="outline"
                disabled={running}
                onClick={startBatch}
              >
                <RefreshCw className="size-4" />
                重新检测全部
              </Button>
              <Button onClick={() => onOpenChange(false)}>关闭</Button>
            </>
          ) : (
            <>
              <Button variant="outline" onClick={() => onOpenChange(false)}>
                取消
              </Button>
              <Button disabled={!canStart} onClick={startBatch}>
                <Play className="size-4" />
                开始批量检测
              </Button>
            </>
          )}
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}

/** 一行结果摘要：官方 Top（概率/占比）、本地 Top、结论。 */
function BatchSummary({ result }: { result: FingerprintDetection }) {
  const top = topCandidate(result.official);
  const share = topShare(result.official);
  const officialValue =
    top?.probability != null
      ? formatPercentage(top.probability)
      : share != null
        ? `≈ ${formatPercentage(share)}`
        : "—";
  const local = result.local?.candidates[0];
  return (
    <div className="flex flex-wrap items-center gap-x-3 gap-y-1 text-[11px]">
      <span>
        <span className="text-muted-foreground">官方：</span>
        <span className="font-mono">{top?.model ?? "—"}</span>{" "}
        <span className="text-muted-foreground tabular-nums">{officialValue}</span>
      </span>
      <span>
        <span className="text-muted-foreground">本地：</span>
        <span className="font-mono">{local?.label ?? "—"}</span>{" "}
        <span className="text-muted-foreground tabular-nums">
          {local?.probability != null
            ? `≈ ${formatPercentage(local.probability)}`
            : local
              ? `${local.samples} 条`
              : "无样本"}
        </span>
      </span>
      <Badge variant="outline">
        {FINGERPRINT_BASIS_LABEL[result.conclusion.basis] ?? result.conclusion.basis}
      </Badge>
      <span className="text-muted-foreground">
        可信度 {FINGERPRINT_LEVEL_LABEL[result.conclusion.level] ?? result.conclusion.level}
      </span>
    </div>
  );
}
