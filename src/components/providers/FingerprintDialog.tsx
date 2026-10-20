import { useEffect, useState } from "react";
import { useMutation, useQueryClient } from "@tanstack/react-query";
import {
  AlertTriangle,
  CheckCircle2,
  Fingerprint,
  Loader2,
  Play,
  RefreshCw,
  XCircle,
} from "lucide-react";
import { toast } from "sonner";

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
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@/components/ui/table";
import { Textarea } from "@/components/ui/textarea";
import { qk } from "@/hooks/queries";
import {
  api,
  FINGERPRINT_BASIS_LABEL,
  FINGERPRINT_LEVEL_LABEL,
  FINGERPRINT_STATUS_LABEL,
  type FingerprintDetection,
} from "@/lib/api";

interface FingerprintDialogProps {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  providerId: number;
  providerName: string;
  model: string;
}

function probabilityText(value?: number | null) {
  return value == null ? "—" : `${(value * 100).toFixed(1)}%`;
}

function scoreText(value: number) {
  return Number.isFinite(value) ? value.toFixed(3) : "—";
}

function levelVariant(level: string) {
  if (level === "high") return "success" as const;
  if (level === "medium") return "warning" as const;
  return "destructive" as const;
}

/**
 * 指纹检测对话框：确认花费 → 串行请求 3 条挑战 → 官方/本地结果 → 加入样本库。
 *
 * 检测本身是一条 Tauri 命令，前端拿不到逐条进度；用 spinner + 明确的耗时提示
 * 比假装有进度条诚实。关闭对话框不会取消后端请求，结果仍会落库。
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
  const [running, setRunning] = useState(false);
  const [result, setResult] = useState<FingerprintDetection | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [label, setLabel] = useState("");
  const [selected, setSelected] = useState<number[]>([]);
  const [note, setNote] = useState("");
  const [added, setAdded] = useState<number | null>(null);

  useEffect(() => {
    if (!open) return;
    setConfirmed(false);
    setRunning(false);
    setResult(null);
    setError(null);
    setLabel("");
    setSelected([]);
    setNote("");
    setAdded(null);
  }, [open]);

  const start = async () => {
    setRunning(true);
    setError(null);
    setResult(null);
    setAdded(null);
    try {
      const detection = await api.detectModelFingerprint(providerId, model);
      setResult(detection);
      setLabel(detection.declared_model);
      setSelected(
        detection.attempts
          .filter((attempt) => attempt.accepted)
          .map((attempt) => attempt.index),
      );
    } catch (err) {
      setError(err instanceof Error ? err.message : "检测失败，请重试");
    } finally {
      setRunning(false);
    }
  };

  const add = useMutation({
    mutationFn: () => {
      if (!result) throw new Error("没有可加入的检测结果");
      return api.addFingerprintSamples(
        result.id,
        label.trim(),
        selected,
        note.trim() || null,
      );
    },
    onSuccess: (count) => {
      setAdded(count);
      qc.invalidateQueries({ queryKey: qk.fingerprintLabels });
      qc.invalidateQueries({ queryKey: qk.fingerprintSamplesAll });
      toast.success(
        count > 0 ? `已加入 ${count} 条本地样本` : "这些回答已经在本地样本库中",
      );
    },
  });

  const suggestions = result
    ? Array.from(
        new Set(
          [
            result.declared_model,
            result.upstream_model,
            result.conclusion.model ?? "",
            ...(result.official?.candidates.slice(0, 3).map((item) => item.model) ?? []),
            ...(result.local?.candidates.slice(0, 3).map((item) => item.label) ?? []),
          ].filter((value) => value.trim().length > 0),
        ),
      )
    : [];

  const officialRows = (() => {
    if (!result?.official) return [];
    const rows = result.official.candidates.slice(0, 10);
    const declared = result.official.declared;
    if (declared && !rows.some((item) => item.model === declared.model)) {
      const found = result.official.candidates.find((item) => item.model === declared.model);
      if (found) rows.push(found);
    }
    return rows;
  })();

  const acceptedAttempts = result?.attempts.filter((attempt) => attempt.accepted) ?? [];
  const canAdd =
    !!result &&
    (result.status === "ok" || result.status === "partial") &&
    label.trim().length > 0 &&
    selected.length > 0 &&
    !add.isPending;

  return (
    <Dialog
      open={open}
      onOpenChange={(next) => {
        if (!next && running) {
          toast.message("检测仍在后台进行，完成后会写入检测记录");
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
                串行发送 3 次真实请求，每次最多 8192 output token。带思考的模型可能需要数十秒到几分钟。
                检测请求不写监控日志、不计费、不查缓存，但会消耗该渠道的 API 额度。
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
            <div className="flex flex-col items-center gap-3 py-12 text-sm">
              <Loader2 className="size-6 animate-spin" />
              <div>正在请求 3 个挑战…</div>
              <p className="text-muted-foreground max-w-md text-center text-xs">
                三条请求是串行的，避免撞上渠道限流。关闭对话框不会取消后端请求；
                完成后结果仍会写入检测记录。
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

          {result && !running && (
            <div className="space-y-4">
              <div className="rounded-lg border px-3 py-3">
                <div className="flex flex-wrap items-center gap-2">
                  <Badge variant={levelVariant(result.conclusion.level)}>
                    可信度 {FINGERPRINT_LEVEL_LABEL[result.conclusion.level] ?? result.conclusion.level}
                  </Badge>
                  <Badge variant="outline">
                    {FINGERPRINT_BASIS_LABEL[result.conclusion.basis] ?? result.conclusion.basis}
                  </Badge>
                  <Badge variant="secondary">
                    {FINGERPRINT_STATUS_LABEL[result.status] ?? result.status}
                  </Badge>
                  <span className="text-muted-foreground text-xs">
                    {result.elapsed_ms} ms
                  </span>
                </div>
                <div className="mt-2 text-sm font-medium">
                  {result.conclusion.display_name ??
                    result.conclusion.model ??
                    "没有给出参考结论"}
                </div>
                <p className="text-muted-foreground mt-1 text-xs">
                  {result.conclusion.note}
                </p>
              </div>

              <div className="grid gap-2 rounded-md border px-3 py-2 text-xs sm:grid-cols-2">
                <div>
                  <span className="text-muted-foreground">声明模型：</span>
                  <span className="font-mono">{result.declared_model}</span>
                </div>
                <div>
                  <span className="text-muted-foreground">上游模型：</span>
                  <span className="font-mono">{result.upstream_model}</span>
                </div>
                {result.official?.declared ? (
                  <div className="sm:col-span-2">
                    <span className="text-muted-foreground">声明模型在官方库：</span>
                    第 {result.official.declared.rank} 名 ·{" "}
                    {probabilityText(result.official.declared.probability)}
                    {result.official.declared.matched_as === "upstream" && (
                      <span className="text-muted-foreground">（按上游名匹配）</span>
                    )}
                  </div>
                ) : (
                  <div className="text-muted-foreground sm:col-span-2">
                    声明模型不在官方参考库中；下面的官方结果是库内最接近的候选。
                  </div>
                )}
              </div>

              <section>
                <h3 className="mb-1 text-xs font-medium">官方参考库排名</h3>
                {result.official ? (
                  <>
                    <Table>
                      <TableHeader>
                        <TableRow>
                          <TableHead className="w-10">#</TableHead>
                          <TableHead>模型</TableHead>
                          <TableHead>家族</TableHead>
                          <TableHead className="text-right">分数</TableHead>
                          <TableHead className="text-right">置信度</TableHead>
                        </TableRow>
                      </TableHeader>
                      <TableBody>
                        {officialRows.map((item) => {
                          const index = result.official!.candidates.indexOf(item) + 1;
                          const declared = result.official!.declared?.model === item.model;
                          return (
                            <TableRow key={item.model} className={declared ? "bg-accent/50" : undefined}>
                              <TableCell className="text-xs tabular-nums">{index}</TableCell>
                              <TableCell className="font-mono text-xs">
                                {item.model}
                                {declared && <Badge className="ml-2" variant="outline">声明</Badge>}
                              </TableCell>
                              <TableCell className="text-xs">{item.family_name}</TableCell>
                              <TableCell className="text-right text-xs tabular-nums">
                                {scoreText(item.score)}
                              </TableCell>
                              <TableCell className="text-right text-xs tabular-nums">
                                {probabilityText(item.probability)}
                              </TableCell>
                            </TableRow>
                          );
                        })}
                      </TableBody>
                    </Table>
                    <p className="text-muted-foreground mt-1 text-[11px]">
                      官方置信度是参考库内 60 个身份的闭集相对概率，不包含库外模型。
                      {!result.official.calibrated && " 本次校准不可用，只显示排名。"}
                    </p>
                  </>
                ) : (
                  <p className="text-muted-foreground text-xs">
                    官方侧没有生成排名：{result.status === "not_model"
                      ? "回答的相邻数字变化不像语言模型（可能是程序生成、排序或打乱的结果）。"
                      : "有效回答不足，无法评分。"}
                  </p>
                )}
              </section>

              <section>
                <h3 className="mb-1 text-xs font-medium">本地样本库</h3>
                {result.local && result.local.candidates.length > 0 ? (
                  <>
                    <Table>
                      <TableHeader>
                        <TableRow>
                          <TableHead className="w-10">#</TableHead>
                          <TableHead>权威模型</TableHead>
                          <TableHead className="text-right">样本数</TableHead>
                          <TableHead className="text-right">距离比</TableHead>
                          <TableHead className="text-right">本地置信度</TableHead>
                        </TableRow>
                      </TableHeader>
                      <TableBody>
                        {result.local.candidates.slice(0, 10).map((item, index) => (
                          <TableRow key={item.label}>
                            <TableCell className="text-xs tabular-nums">{index + 1}</TableCell>
                            <TableCell className="font-mono text-xs">
                              {item.label}
                              {!item.official && (
                                <Badge className="ml-2" variant="outline">本地独有</Badge>
                              )}
                            </TableCell>
                            <TableCell className="text-right text-xs tabular-nums">
                              {item.samples}
                            </TableCell>
                            <TableCell className="text-right text-xs tabular-nums">
                              {item.distance_ratio.toFixed(2)}
                            </TableCell>
                            <TableCell className="text-right text-xs tabular-nums">
                              {item.probability == null
                                ? item.samples < 6
                                  ? "样本不足"
                                  : "未校准"
                                : `≈ ${probabilityText(item.probability)}`}
                            </TableCell>
                          </TableRow>
                        ))}
                      </TableBody>
                    </Table>
                    <p className="text-muted-foreground mt-1 text-[11px]">
                      本地置信度只在同一 label ≥6 条样本、且至少两个 label 时拟合；
                      它与官方概率口径不同，不合成单一概率。当前{" "}
                      {result.local.eligible_labels} 个 label 达到概率门槛。
                    </p>
                  </>
                ) : (
                  <p className="text-muted-foreground text-xs">
                    本地样本库为空，或没有 ≥3 条样本的 label。加入样本后会在下一次检测中出现。
                  </p>
                )}
              </section>

              <section>
                <h3 className="mb-1 text-xs font-medium">3 次请求</h3>
                <div className="space-y-1">
                  {result.attempts.map((attempt) => (
                    <div
                      key={attempt.index}
                      className="flex flex-wrap items-center gap-2 rounded border px-2 py-1.5 text-xs"
                    >
                      <Badge variant={attempt.accepted ? "success" : "destructive"}>
                        {attempt.accepted ? "有效" : "不足"}
                      </Badge>
                      <span className="font-mono">
                        {attempt.challenge_id} · 期望 {attempt.expected_count}
                      </span>
                      <span className="text-muted-foreground tabular-nums">
                        {attempt.parsed_numbers}/{attempt.minimum_numbers} 个数字 ·{" "}
                        {attempt.latency_ms} ms
                      </span>
                      {attempt.error && (
                        <span className="text-destructive whitespace-normal">
                          {attempt.error}
                        </span>
                      )}
                    </div>
                  ))}
                </div>
              </section>

              <div className="text-muted-foreground flex flex-wrap items-center gap-2 text-[11px]">
                <span>
                  参考库：{result.library.source === "remote" ? "远端" : "内置"} ·{" "}
                  {result.library.built_at || "—"} · {result.library.model_count} 个身份
                </span>
                <span className="font-mono">{result.library.sha256.slice(0, 16)}…</span>
              </div>

              <section className="space-y-2 rounded-md border px-3 py-3">
                <h3 className="flex items-center gap-2 text-xs font-medium">
                  <CheckCircle2 className="size-4" />
                  加入本地样本库
                </h3>
                <p className="text-muted-foreground text-[11px]">
                  只有你确认这些回答确实来自某个模型时才加入。权威模型由你指定；
                  错误标注会污染本地结果。重复回答会被跳过。
                </p>
                <Input
                  list="fingerprint-label-suggestions"
                  value={label}
                  onChange={(event) => setLabel(event.target.value)}
                  placeholder="权威模型，例如 gpt-5.5"
                  className="h-8 text-xs"
                />
                <datalist id="fingerprint-label-suggestions">
                  {suggestions.map((value) => (
                    <option key={value} value={value} />
                  ))}
                </datalist>
                <div className="space-y-1">
                  {acceptedAttempts.length === 0 && (
                    <p className="text-muted-foreground text-xs">没有有效回答可加入。</p>
                  )}
                  {acceptedAttempts.map((attempt) => (
                    <label
                      key={attempt.index}
                      className="flex items-center gap-2 text-xs"
                    >
                      <Checkbox
                        checked={selected.includes(attempt.index)}
                        onCheckedChange={(checked) =>
                          setSelected((current) =>
                            checked === true
                              ? Array.from(new Set([...current, attempt.index]))
                              : current.filter((index) => index !== attempt.index),
                          )
                        }
                      />
                      挑战 {attempt.index + 1} · {attempt.challenge_id} ·{" "}
                      {attempt.parsed_numbers} 个数字
                    </label>
                  ))}
                </div>
                <Textarea
                  value={note}
                  onChange={(event) => setNote(event.target.value)}
                  placeholder="备注（可选），例如：来自官方渠道 / 疑似替换"
                  className="min-h-14 text-xs"
                />
                <div className="flex flex-wrap items-center gap-2">
                  <Button
                    size="sm"
                    disabled={!canAdd}
                    onClick={() => add.mutate()}
                  >
                    {add.isPending && <Loader2 className="size-4 animate-spin" />}
                    加入样本库
                  </Button>
                  {added != null && (
                    <span className="text-muted-foreground text-xs">
                      已加入 {added} 条（重复的已跳过）
                    </span>
                  )}
                </div>
              </section>
            </div>
          )}
        </ScrollArea>

        <DialogFooter className="shrink-0">
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
              <Button
                disabled={!confirmed || running}
                onClick={start}
              >
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
