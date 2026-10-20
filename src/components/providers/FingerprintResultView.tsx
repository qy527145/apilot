import { useEffect, useMemo, useState } from "react";
import { useMutation, useQueryClient } from "@tanstack/react-query";
import { CheckCircle2, Loader2, RefreshCw } from "lucide-react";
import { toast } from "sonner";

import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Checkbox } from "@/components/ui/checkbox";
import { Input } from "@/components/ui/input";
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
import { candidateShares } from "@/lib/fingerprint";

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
 * 一次检测的完整结果：结论、官方/本地排名、三次请求诊断、加入样本库。
 *
 * 检测对话框与历史记录详情共用这个组件；「加入样本库」按检测 id 回查
 * 后端落库的原始回答，所以历史记录里的按钮同样可用。
 */
export function FingerprintResultView({
  result: initial,
  onUpdated,
}: {
  result: FingerprintDetection;
  /** 单条挑战重试后通知父级（批量列表需要同步摘要）。 */
  onUpdated?: (result: FingerprintDetection) => void;
}) {
  const qc = useQueryClient();
  /** 单条挑战重试后要原地更新结果，所以这里保留一份可变状态。 */
  const [result, setResult] = useState(initial);
  const [label, setLabel] = useState(result.declared_model);
  const [selected, setSelected] = useState<number[]>([]);
  const [note, setNote] = useState("");
  const [added, setAdded] = useState<number | null>(null);

  useEffect(() => {
    setResult(initial);
  }, [initial.id]);

  useEffect(() => {
    setLabel(result.declared_model);
    setSelected(
      result.attempts
        .filter((attempt) => attempt.accepted)
        .map((attempt) => attempt.index),
    );
    setNote("");
    setAdded(null);
  }, [result.id, result.declared_model]);

  const retry = useMutation({
    mutationFn: (index: number) => api.retryFingerprintAttempt(result.id, index),
    onSuccess: (updated) => {
      setResult(updated);
      onUpdated?.(updated);
      qc.invalidateQueries({ queryKey: qk.fingerprintDetections });
      qc.invalidateQueries({ queryKey: qk.fingerprintDetection(updated.id) });
      toast.success("已重试该挑战并更新检测结果");
    },
  });

  const add = useMutation({
    mutationFn: () =>
      api.addFingerprintSamples(
        result.id,
        label.trim(),
        selected,
        note.trim() || null,
      ),
    onSuccess: (count) => {
      setAdded(count);
      qc.invalidateQueries({ queryKey: qk.fingerprintLabels });
      qc.invalidateQueries({ queryKey: qk.fingerprintSamplesAll });
      toast.success(
        count > 0 ? `已加入 ${count} 条本地样本` : "这些回答已经在本地样本库中",
      );
    },
  });

  const suggestions = Array.from(
    new Set(
      [
        result.declared_model,
        result.upstream_model,
        result.conclusion.model ?? "",
        ...(result.official?.candidates.slice(0, 3).map((item) => item.model) ?? []),
        ...(result.local?.candidates.slice(0, 3).map((item) => item.label) ?? []),
      ].filter((value) => value.trim().length > 0),
    ),
  );

  const officialRows = (() => {
    if (!result.official) return [];
    const rows = result.official.candidates.slice(0, 10);
    const declared = result.official.declared;
    if (declared && !rows.some((item) => item.model === declared.model)) {
      const found = result.official.candidates.find(
        (item) => item.model === declared.model,
      );
      if (found) rows.push(found);
    }
    return rows;
  })();

  /** 分数占比；与批量列表共用同一个 τ=1 换算。 */
  const scoreShares = useMemo(
    () => candidateShares(result.official?.candidates ?? []),
    [result.official],
  );

  const acceptedAttempts = result.attempts.filter((attempt) => attempt.accepted);
  const canAdd =
    (result.status === "ok" || result.status === "partial") &&
    label.trim().length > 0 &&
    selected.length > 0 &&
    !add.isPending;

  return (
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
          <span className="text-muted-foreground text-xs">{result.elapsed_ms} ms</span>
        </div>
        <div className="mt-2 text-sm font-medium">
          {result.conclusion.display_name ??
            result.conclusion.model ??
            "没有给出参考结论"}
        </div>
        <p className="text-muted-foreground mt-1 text-xs">{result.conclusion.note}</p>
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
                  <TableHead className="text-right">占比*</TableHead>
                  <TableHead className="text-right">置信度</TableHead>
                </TableRow>
              </TableHeader>
              <TableBody>
                {officialRows.map((item) => {
                  const index = result.official!.candidates.indexOf(item) + 1;
                  const declared = result.official!.declared?.model === item.model;
                  return (
                    <TableRow
                      key={item.model}
                      className={declared ? "bg-accent/50" : undefined}
                    >
                      <TableCell className="text-xs tabular-nums">{index}</TableCell>
                      <TableCell className="font-mono text-xs">
                        {item.model}
                        {declared && (
                          <Badge className="ml-2" variant="outline">
                            声明
                          </Badge>
                        )}
                      </TableCell>
                      <TableCell className="text-xs">{item.family_name}</TableCell>
                      <TableCell className="text-right text-xs tabular-nums">
                        {scoreText(item.score)}
                      </TableCell>
                      <TableCell className="text-right text-xs tabular-nums">
                        {scoreShares.has(item.model)
                          ? `≈ ${((scoreShares.get(item.model) ?? 0) * 100).toFixed(1)}%`
                          : "—"}
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
              官方置信度是参考库内身份的闭集相对概率，不包含库外模型。
              {!result.official.calibrated && " 本次校准不可用（有效回答不足 3 条）。"}
            </p>
            <p className="text-muted-foreground mt-1 text-[11px]">
              占比* = 对全部候选分数做 softmax（τ=1）后的相对份额，只用于直观比较，
              不是校准概率；三条有效回答时以「置信度」列为准。
            </p>
          </>
        ) : (
          <p className="text-muted-foreground text-xs">
            官方侧没有生成排名：
            {result.status === "not_model"
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
                        <Badge className="ml-2" variant="outline">
                          本地独有
                        </Badge>
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
              className="rounded border px-2 py-1.5 text-xs"
            >
              <div className="flex flex-wrap items-center gap-2">
                <Badge variant={attempt.accepted ? "success" : "destructive"}>
                  {attempt.accepted ? "有效" : "不足"}
                </Badge>
                <span className="font-mono">
                  {attempt.challenge_id} · 期望 {attempt.expected_count}
                </span>
                <span className="text-muted-foreground tabular-nums">
                  {attempt.parsed_numbers}/{attempt.minimum_numbers} 个数字 ·{" "}
                  {attempt.latency_ms} ms · {attempt.wire || "—"}
                </span>
                {attempt.error && (
                  <span className="text-destructive whitespace-normal">
                    {attempt.error}
                  </span>
                )}
                <Button
                  size="sm"
                  variant="outline"
                  className="ml-auto h-6 px-2 text-[11px]"
                  disabled={retry.isPending}
                  onClick={() => retry.mutate(attempt.index)}
                >
                  {retry.isPending && retry.variables === attempt.index ? (
                    <Loader2 className="size-3 animate-spin" />
                  ) : (
                    <RefreshCw className="size-3" />
                  )}
                  {retry.isPending && retry.variables === attempt.index
                    ? "重试中…"
                    : "重试这条"}
                </Button>
              </div>
              <details className="mt-1">
                <summary className="text-muted-foreground cursor-pointer text-[11px]">
                  查看提示词与请求详情
                </summary>
                <div className="mt-1 space-y-1 text-[11px]">
                  <p className="whitespace-pre-wrap">
                    {attempt.challenge_prompt || "（这条记录没有保存提示词）"}
                  </p>
                  {attempt.url && (
                    <p className="text-muted-foreground font-mono break-all">
                      {attempt.url}
                    </p>
                  )}
                  {attempt.text_preview && (
                    <p className="text-muted-foreground whitespace-pre-wrap">
                      回包预览：{attempt.text_preview}
                    </p>
                  )}
                  {attempt.text && (
                    <details>
                      <summary className="text-muted-foreground cursor-pointer">
                        查看原始回答
                      </summary>
                      <p className="mt-1 break-all whitespace-pre-wrap">
                        {attempt.text}
                      </p>
                    </details>
                  )}
                  {attempt.numbers && attempt.numbers.length > 0 && (
                    <details>
                      <summary className="text-muted-foreground cursor-pointer">
                        查看解析后的数字（{attempt.numbers.length} 个）
                      </summary>
                      <p className="mt-1 font-mono break-all">
                        {attempt.numbers.join(", ")}
                      </p>
                    </details>
                  )}
                </div>
              </details>
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
            <label key={attempt.index} className="flex items-center gap-2 text-xs">
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
          <Button size="sm" disabled={!canAdd} onClick={() => add.mutate()}>
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
  );
}
