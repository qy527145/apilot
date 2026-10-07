import { useEffect, useRef, useState } from "react";
import { ArrowDown, ArrowUp, Plus, Trash2 } from "lucide-react";

import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { Tabs, TabsList, TabsTrigger } from "@/components/ui/tabs";
import { Textarea } from "@/components/ui/textarea";
import {
  api,
  type CustomRules,
  type MappingRow,
  type MatchKind,
} from "@/lib/api";
import { cn } from "@/lib/utils";

const MATCH_KIND_LABEL: Record<MatchKind, string> = {
  prefix: "前缀",
  glob: "通配（* 与 ?）",
  regex: "正则",
  exact: "完全相同",
  any: "兜底（全都命中）",
};

/** 需要写表达式的匹配方式；`any` 不需要。 */
const NEEDS_PATTERN: MatchKind[] = ["prefix", "glob", "regex", "exact"];

/** Radix 的 Select 不接受空串当选项值，只能拿哨兵顶。 */
const KEEP_TARGET = "__keep__";
const ANY_CLIENT = "__any_client__";

const SCRIPT_EXAMPLE = `function resolve(ctx) {
  // ctx = { model, client, protocol }
  // 返回字符串 = 换成它；返回 null = 不改写，用客户端请求的那个
  if (ctx.model.startsWith("claude-")) return "deepseek-v4-pro";
  return null;
}`;

export interface CustomRuleEditorProps {
  value: CustomRules;
  onChange: (next: CustomRules) => void;
  /** 可选的模型名（各渠道声明的模型取并集）。 */
  models: string[];
  /** 客户端标识 → 显示名，给「只对这个客户端」那一栏用。 */
  clients: Array<{ id: string; name: string }>;
}

/**
 * 「自定义规则」的双轨编辑器。
 *
 * 两套写法共用同一份 `CustomRules`：切换上面的页签时**只换 form**，
 * 另一套原样留着 —— 用户来回试的时候不用重打一遍。
 */
export function CustomRuleEditor({
  value,
  onChange,
  models,
  clients,
}: CustomRuleEditorProps) {
  const patch = (next: Partial<CustomRules>) => onChange({ ...value, ...next });

  return (
    <div className="space-y-3">
      <Tabs
        value={value.form}
        onValueChange={(v) => patch({ form: v as CustomRules["form"] })}
      >
        <TabsList>
          <TabsTrigger value="table">映射表</TabsTrigger>
          <TabsTrigger value="script">高级：JavaScript</TabsTrigger>
        </TabsList>
      </Tabs>

      {value.form === "script" ? (
        <ScriptEditor
          script={value.script ?? ""}
          onChange={(script) => patch({ script })}
        />
      ) : (
        <TableEditor
          rows={value.table}
          onChange={(table) => patch({ table })}
          models={models}
          clients={clients}
        />
      )}
    </div>
  );
}

// ---------------------------------------------------------------------------
// 映射表
// ---------------------------------------------------------------------------

function TableEditor({
  rows,
  onChange,
  models,
  clients,
}: {
  rows: MappingRow[];
  onChange: (rows: MappingRow[]) => void;
  models: string[];
  clients: Array<{ id: string; name: string }>;
}) {
  // 后端会把「还没写完」的行清掉（空表达式、编译不过的正则）—— 它必须这么做，
  // 一个空的前缀表达式等于匹配一切。所以这里保留一份本地草稿：半成品只留在
  // 界面上、不上报，否则刚点出来的空行会被后端一口吃掉，表现为
  // 「点了加一行，行没了」。
  const [draft, setDraft] = useState(rows);
  const emitted = useRef(rows);

  useEffect(() => {
    // 只在服务端真的改了什么时同步（比如规范化回填），否则会把草稿里的
    // 半成品行冲掉。比的是内容而不是对象身份。
    if (!sameRows(rows, emitted.current)) {
      emitted.current = rows;
      setDraft(rows);
    }
  }, [rows]);

  const update = (next: MappingRow[]) => {
    setDraft(next);

    const complete = next.filter(isCompleteRow);
    if (sameRows(complete, emitted.current)) return;
    emitted.current = complete;
    onChange(complete);
  };

  const replace = (index: number, row: MappingRow) => {
    const out = [...draft];
    out[index] = row;
    update(out);
  };

  const move = (index: number, to: number) => {
    if (to < 0 || to >= draft.length) return;
    const out = [...draft];
    const [row] = out.splice(index, 1);
    out.splice(to, 0, row);
    update(out);
  };

  return (
    <div className="space-y-2">
      {draft.length === 0 && (
        <p className="text-muted-foreground text-xs">
          还没有规则。加一行，或者用一行「兜底」接住剩下的模型。
        </p>
      )}

      {draft.map((row, i) => (
        <RowEditor
          // 行没有稳定 id，用下标当 key：上下移动时 React 复用同一行，
          // 行内那些"没提交就不上报"的输入框才不会串味。行数据真的换了时，
          // RowEditor 自己会把本地草稿同步过去。
          key={i}
          row={row}
          first={i === 0}
          last={i === draft.length - 1}
          clients={clients}
          models={models}
          onChange={(next) => replace(i, next)}
          onMoveUp={() => move(i, i - 1)}
          onMoveDown={() => move(i, i + 1)}
          onRemove={() => update(draft.filter((_, j) => j !== i))}
        />
      ))}

      <Button
        variant="outline"
        size="sm"
        className="h-7 text-[11px]"
        onClick={() =>
          update([
            ...draft,
            { match_kind: "prefix", pattern: "", target: null, client: null },
          ])
        }
      >
        <Plus className="size-3.5" />
        加一行
      </Button>

      <p className="text-muted-foreground text-[11px]">
        自上而下，第一条命中的生效；一条都没命中就不改写。
        表达式的改动失焦时保存，写不对的那一行会留在界面上等你改，
        不会被悄悄存下去。
      </p>
    </div>
  );
}

/** 行写完了没有。没写完的只留在本地草稿里，不上报。 */
function isCompleteRow(row: MappingRow): boolean {
  if (row.match_kind === "any") return true;
  const pattern = (row.pattern ?? "").trim();
  return pattern !== "" && patternError(row.match_kind, pattern) === null;
}

function sameRows(a: MappingRow[], b: MappingRow[]): boolean {
  return (
    a.length === b.length &&
    a.every((r, i) => {
      const o = b[i];
      return (
        r.match_kind === o.match_kind &&
        (r.pattern ?? "") === (o.pattern ?? "") &&
        (r.target ?? "") === (o.target ?? "") &&
        (r.client ?? "") === (o.client ?? "")
      );
    })
  );
}

function RowEditor({
  row,
  first,
  last,
  clients,
  models,
  onChange,
  onMoveUp,
  onMoveDown,
  onRemove,
}: {
  row: MappingRow;
  first: boolean;
  last: boolean;
  clients: Array<{ id: string; name: string }>;
  models: string[];
  onChange: (row: MappingRow) => void;
  onMoveUp: () => void;
  onMoveDown: () => void;
  onRemove: () => void;
}) {
  // 表达式在**失焦**时才提交：既不用每敲一个字就写一次库，
  // 也让"还没写完"的中间状态（比如一个半截正则）有机会被拦下。
  const [pattern, setPattern] = useState(row.pattern ?? "");
  const [error, setError] = useState<string | null>(null);
  const committed = useRef(row.pattern ?? "");

  // 外部改了这一行（换匹配方式等）就跟着同步，别让本地草稿把它盖回去。
  useEffect(() => {
    if ((row.pattern ?? "") !== committed.current) {
      committed.current = row.pattern ?? "";
      setPattern(row.pattern ?? "");
      setError(null);
    }
  }, [row.pattern]);

  const commit = () => {
    const trimmed = pattern.trim();
    const problem = patternError(row.match_kind, trimmed);
    setError(problem);
    // 写坏了就不上报：后端会把这行丢掉，用户只会看到"保存成功了但那行没了"。
    if (problem || trimmed === committed.current) return;
    committed.current = trimmed;
    onChange({ ...row, pattern: trimmed });
  };

  return (
    <div className="space-y-1">
      <div className="flex items-center gap-2">
        <Select
          value={row.client ?? ANY_CLIENT}
          onValueChange={(v) =>
            onChange({ ...row, client: v === ANY_CLIENT ? null : v })
          }
        >
          <SelectTrigger size="sm" className="h-8 w-32 shrink-0">
            <SelectValue />
          </SelectTrigger>
          <SelectContent>
            <SelectItem value={ANY_CLIENT}>全部客户端</SelectItem>
            {clients.map((c) => (
              <SelectItem key={c.id} value={c.id}>
                {c.name}
              </SelectItem>
            ))}
          </SelectContent>
        </Select>

        <Select
          value={row.match_kind}
          onValueChange={(v) => {
            const kind = v as MatchKind;
            setError(null);
            // 换成兜底之后表达式就没意义了，一并清掉，免得留下一个死值。
            onChange({
              ...row,
              match_kind: kind,
              pattern: NEEDS_PATTERN.includes(kind) ? row.pattern : null,
            });
          }}
        >
          <SelectTrigger size="sm" className="h-8 w-40 shrink-0">
            <SelectValue />
          </SelectTrigger>
          <SelectContent>
            {(Object.keys(MATCH_KIND_LABEL) as MatchKind[]).map((k) => (
              <SelectItem key={k} value={k}>
                {MATCH_KIND_LABEL[k]}
              </SelectItem>
            ))}
          </SelectContent>
        </Select>

        <Input
          className={cn(
            "h-8 flex-1 font-mono text-xs",
            error && "border-destructive",
          )}
          value={pattern}
          disabled={!NEEDS_PATTERN.includes(row.match_kind)}
          placeholder={
            row.match_kind === "any" ? "无需表达式" : "例如 claude-*"
          }
          onChange={(e) => setPattern(e.target.value)}
          onBlur={commit}
        />

        <Select
          value={row.target ?? KEEP_TARGET}
          onValueChange={(v) =>
            onChange({ ...row, target: v === KEEP_TARGET ? null : v })
          }
        >
          <SelectTrigger size="sm" className="h-8 w-44 shrink-0">
            <SelectValue />
          </SelectTrigger>
          <SelectContent>
            <SelectItem value={KEEP_TARGET}>保持原样（停下）</SelectItem>
            {models.map((m) => (
              <SelectItem key={m} value={m}>
                {m}
              </SelectItem>
            ))}
          </SelectContent>
        </Select>

        <Button
          variant="ghost"
          size="icon"
          className="size-8 shrink-0"
          disabled={first}
          onClick={onMoveUp}
          title="上移"
        >
          <ArrowUp className="size-3.5" />
        </Button>
        <Button
          variant="ghost"
          size="icon"
          className="size-8 shrink-0"
          disabled={last}
          onClick={onMoveDown}
          title="下移"
        >
          <ArrowDown className="size-3.5" />
        </Button>
        <Button
          variant="ghost"
          size="icon"
          className="size-8 shrink-0"
          onClick={onRemove}
          title="删掉这行"
        >
          <Trash2 className="size-3.5" />
        </Button>
      </div>

      {error && (
        <p className="text-destructive pl-1 text-[11px]">{error}</p>
      )}
    </div>
  );
}

/**
 * 表达式校验。后端用的是 `regex` crate，语法比 JS 窄 —— 环视与反向引用
 * 在那边直接编译不过，会被静默丢掉。所以这里主动拦下来并说明原因，
 * 而不是等保存之后那行凭空消失。
 */
function patternError(kind: MatchKind, pattern: string): string | null {
  if (!NEEDS_PATTERN.includes(kind)) return null;
  if (!pattern) return "表达式不能为空";

  if (kind === "regex") {
    try {
      new RegExp(pattern);
    } catch (e) {
      return `正则写不对：${(e as Error).message}`;
    }
    if (/\(\?<?[=!]|\\[1-9]/.test(pattern)) {
      return "不支持环视（?= ?! ?<= ?<!）与反向引用（\\1）—— 上游用的是 Rust 的 regex 语法";
    }
  }

  return null;
}

// ---------------------------------------------------------------------------
// JavaScript
// ---------------------------------------------------------------------------

function ScriptEditor({
  script,
  onChange,
}: {
  script: string;
  onChange: (script: string) => void;
}) {
  const [text, setText] = useState(script);
  const [error, setError] = useState<string | null>(null);
  const committed = useRef(script);

  useEffect(() => {
    if (script !== committed.current) {
      committed.current = script;
      setText(script);
      setError(null);
    }
  }, [script]);

  const commit = async () => {
    const trimmed = text.trim();
    if (trimmed === committed.current) return;

    // 校验交给后端：只有它手里的那个引擎说了才算数。
    // 脚本写坏了只会静默不生效，那种错用户自己很难发现。
    const problem = await api.validateModelScript(trimmed);
    setError(problem);
    if (problem) return;

    committed.current = trimmed;
    onChange(trimmed);
  };

  return (
    <div className="space-y-2">
      <div className="flex items-center justify-between">
        <Label className="text-xs">resolve 函数</Label>
        <Button
          variant="ghost"
          size="sm"
          className="h-6 px-2 text-[11px]"
          onClick={() => {
            setText(SCRIPT_EXAMPLE);
            setError(null);
          }}
        >
          填入示例
        </Button>
      </div>

      <Textarea
        className={cn(
          "min-h-40 font-mono text-xs",
          error && "border-destructive",
        )}
        value={text}
        spellCheck={false}
        placeholder={SCRIPT_EXAMPLE}
        onChange={(e) => setText(e.target.value)}
        onBlur={commit}
      />

      {error ? (
        <p className="text-destructive text-[11px]">{error}</p>
      ) : (
        <p className="text-muted-foreground text-[11px]">
          失焦时校验并保存。脚本是 <code>ctx</code> 的纯函数：入参
          <code className="mx-1">{"{ model, client, protocol }"}</code>
          ，返回字符串则改写，返回 <code>null</code> 则不改写。
          它跑不了 I/O，超时 25ms，写错了一律按不改写处理。
        </p>
      )}
    </div>
  );
}
