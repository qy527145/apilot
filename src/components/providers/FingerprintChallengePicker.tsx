import { useState } from "react";
import { ListChecks, Shuffle } from "lucide-react";

import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Checkbox } from "@/components/ui/checkbox";
import { Input } from "@/components/ui/input";
import type { FingerprintChallenge } from "@/lib/api";

export type ChallengeMode = "random" | "manual";

/**
 * 挑战选择：随机（默认）或从 36 条里手动挑 3 条。
 * 单模型检测与批量检测共用。
 */
export function FingerprintChallengePicker({
  challenges,
  loading,
  mode,
  onModeChange,
  selected,
  onToggle,
}: {
  challenges: FingerprintChallenge[];
  loading: boolean;
  mode: ChallengeMode;
  onModeChange: (mode: ChallengeMode) => void;
  selected: string[];
  onToggle: (id: string) => void;
}) {
  const [search, setSearch] = useState("");
  const filtered = challenges.filter((challenge) => {
    const keyword = search.trim().toLowerCase();
    if (!keyword) return true;
    return (
      challenge.challenge_id.toLowerCase().includes(keyword) ||
      challenge.condition.toLowerCase().includes(keyword) ||
      challenge.prompt.toLowerCase().includes(keyword)
    );
  });

  return (
    <div className="space-y-2">
      <div className="flex flex-wrap items-center gap-2">
        <span className="text-xs font-medium">挑战</span>
        <div className="flex gap-1">
          <Button
            size="sm"
            variant={mode === "random" ? "default" : "outline"}
            onClick={() => onModeChange("random")}
          >
            <Shuffle className="size-4" />
            随机抽取
          </Button>
          <Button
            size="sm"
            variant={mode === "manual" ? "default" : "outline"}
            onClick={() => onModeChange("manual")}
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
            自己挑 3 条。提示词原样发送；选择顺序会保留在结果里。已选{" "}
            <span className="text-foreground font-medium">
              {selected.length}/3
            </span>
            {selected.length >= 3 && "（先取消一条才能换）"}
          </p>
          <Input
            value={search}
            onChange={(event) => setSearch(event.target.value)}
            placeholder="搜索挑战 ID / 条件 / 提示词"
            className="h-8 text-xs"
          />
          {loading ? (
            <div className="text-muted-foreground py-4 text-xs">正在加载挑战库…</div>
          ) : (
            <div className="max-h-72 space-y-1 overflow-y-auto rounded border p-1">
              {filtered.map((challenge) => {
                const checked = selected.includes(challenge.challenge_id);
                return (
                  <label
                    key={challenge.challenge_id}
                    className={`hover:bg-accent/50 flex cursor-pointer items-start gap-2 rounded px-2 py-1.5 text-xs ${
                      checked ? "bg-accent/40" : ""
                    }`}
                  >
                    <Checkbox
                      checked={checked}
                      onCheckedChange={() => onToggle(challenge.challenge_id)}
                      className="mt-0.5"
                    />
                    <div className="min-w-0 space-y-0.5">
                      <div className="flex flex-wrap items-center gap-2">
                        <span className="font-mono font-medium">
                          {challenge.challenge_id}
                        </span>
                        <Badge variant="outline">{challenge.condition}</Badge>
                        <span className="text-muted-foreground">
                          期望 {challenge.expected_count} 个整数
                        </span>
                      </div>
                      <p className="text-muted-foreground text-[11px] whitespace-pre-wrap">
                        {challenge.prompt}
                      </p>
                    </div>
                  </label>
                );
              })}
              {filtered.length === 0 && (
                <p className="text-muted-foreground px-2 py-3 text-xs">
                  没有匹配的挑战。
                </p>
              )}
            </div>
          )}
        </div>
      )}
    </div>
  );
}
