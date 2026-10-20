import type {
  FingerprintCandidate,
  FingerprintOfficialResult,
} from "@/lib/api";

/**
 * 对全部官方候选的分数做 τ=1 softmax。
 *
 * 它只是把分数压到 0–100% 方便直观比较，**不是校准概率**：校准温度是按三条
 * 有效回答拟合的，部分有效时不可用。结果详情和批量列表共用同一换算。
 */
export function candidateShares(
  candidates: FingerprintCandidate[],
): Map<string, number> {
  if (candidates.length === 0) return new Map();
  const maximum = Math.max(...candidates.map((item) => item.score));
  const weights = candidates.map((item) => Math.exp(item.score - maximum));
  const total = weights.reduce((sum, weight) => sum + weight, 0);
  return new Map(
    candidates.map((item, index) => [
      item.model,
      total > 0 ? weights[index] / total : 0,
    ]),
  );
}

export function topCandidate(
  official?: FingerprintOfficialResult | null,
): FingerprintCandidate | undefined {
  return official?.candidates[0];
}

/** Top-1 的分数占比；没有官方结果时返回 null。 */
export function topShare(
  official?: FingerprintOfficialResult | null,
): number | null {
  const top = topCandidate(official);
  if (!top) return null;
  return candidateShares(official?.candidates ?? []).get(top.model) ?? null;
}

export function formatPercentage(value?: number | null): string {
  return value == null ? "—" : `${(value * 100).toFixed(1)}%`;
}
