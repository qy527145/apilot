import { useState } from "react";
import { CalendarClock, Undo2 } from "lucide-react";

import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import {
  Popover,
  PopoverContent,
  PopoverTrigger,
} from "@/components/ui/popover";
import { Separator } from "@/components/ui/separator";

/**
 * 监控页的「自定义时间范围」。
 *
 * 与预设页签是**互斥**的：选了任一预设，自定义就是关着的（`active`）。
 *
 * 做成弹层而不是把两个输入框摊在时间那一行：那行已经有五个页签，再摆两个原生
 * 时间框既挤又难看，而且没选完之前它们一直是空的，看不出是个什么东西。收进弹层
 * 后，触发按钮本身就显示所选区间（`09-15 14:30 至 09-15 16:00`），一眼看得见。
 *
 * 改动**即时生效**（与页面其它筛选控件一致），没有「应用」按钮：原生时间框是
 * 选完才给值，不会一个字一个字地触发查询。
 */

/** `datetime-local` 要的 `YYYY-MM-DDTHH:mm`，本地时间。 */
export type LocalTime = string;

/**
 * 时间戳 → `datetime-local` 的值。
 *
 * **不能用 `toISOString().slice(0, 16)`**：那是 UTC，东八区下用户会看到凭空
 * 少 8 小时的时间，而且再读回来又差一次。只能按本地时间字段拼。
 */
function toLocalInput(ms: number): LocalTime {
  const d = new Date(ms);
  const pad = (n: number) => String(n).padStart(2, "0");
  return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}T${pad(
    d.getHours(),
  )}:${pad(d.getMinutes())}`;
}

function startOfDay(ms: number): number {
  const d = new Date(ms);
  d.setHours(0, 0, 0, 0);
  return d.getTime();
}

const MINUTE = 60_000;
const DAY = 24 * 60 * MINUTE;

/** 常用区间，点一下就把两端填好。 */
const QUICK: { label: string; range: () => { from: LocalTime; to: LocalTime } }[] = [
  {
    label: "最近 10 分钟",
    range: () => ({ from: toLocalInput(Date.now() - 10 * MINUTE), to: toLocalInput(Date.now()) }),
  },
  {
    label: "最近 1 小时",
    range: () => ({ from: toLocalInput(Date.now() - 60 * MINUTE), to: toLocalInput(Date.now()) }),
  },
  {
    label: "今天",
    range: () => ({ from: toLocalInput(startOfDay(Date.now())), to: toLocalInput(Date.now()) }),
  },
  {
    label: "昨天",
    range: () => {
      const today = startOfDay(Date.now());
      return { from: toLocalInput(today - DAY), to: toLocalInput(today - MINUTE) };
    },
  },
];

/** 一端 → [日期, 时间]（`09-15` / `14:30`）。空值或写坏了给 `null`，
 *  由调用方决定怎么显示 —— 别让 `Invalid Date` 漏到界面上。 */
function parts(value: LocalTime): [string, string] | null {
  const d = new Date(value);
  if (!value || Number.isNaN(d.getTime())) return null;
  const local = toLocalInput(d.getTime());
  return [local.slice(5, 10), local.slice(11, 16)];
}

/** 触发按钮上的摘要。同一天只写一次日期：`09-15 14:30 至 16:00` —— 重复的
 *  日期段既不好读，又白白把按钮撑宽（宽了就得截断，一截断就白显示了）。 */
function summary(from: LocalTime, to: LocalTime): string {
  const a = parts(from);
  const b = parts(to);
  if (a && b) {
    return a[0] === b[0]
      ? `${a[0]} ${a[1]} 至 ${b[1]}`
      : `${a[0]} ${a[1]} 至 ${b[0]} ${b[1]}`;
  }
  if (a) return `${a[0]} ${a[1]} 起`;
  if (b) return `至 ${b[0]} ${b[1]}`;
  return "自定义";
}

interface TimeRangePickerProps {
  /** 当前筛选正用着自定义范围（页签那边没有任何一个是选中的）。 */
  active: boolean;
  from: LocalTime;
  to: LocalTime;
  onChange: (next: { from: LocalTime; to: LocalTime }) => void;
  /** 清空两端。把时间条件退回「不限」由调用方决定 —— 那是页面的状态。 */
  onClear: () => void;
}

export function TimeRangePicker({ active, from, to, onChange, onClear }: TimeRangePickerProps) {
  const [open, setOpen] = useState(false);
  const filled = Boolean(from || to);

  return (
    <Popover open={open} onOpenChange={setOpen}>
      <PopoverTrigger asChild>
        <Button variant={active ? "default" : "outline"} size="sm" className="max-w-[19rem]">
          <CalendarClock className="size-4 shrink-0" />
          {/* 不设字号：Input / Button 自带的那套就是全站输入控件的尺寸，
              再叠一个 `text-xs` 会跟 `md:text-sm` 抢，谁赢取决于工具类的排布顺序。 */}
          <span className="truncate font-mono">
            {active || filled ? summary(from, to) : "自定义"}
          </span>
        </Button>
      </PopoverTrigger>

      <PopoverContent align="start" className="w-72 p-0">
        <div className="space-y-3 p-4">
          <div className="flex items-center justify-between">
            <div className="text-sm font-medium">自定义时间范围</div>
            {filled && (
              <Button variant="ghost" size="sm" className="h-7 text-xs" onClick={onClear}>
                <Undo2 className="size-3.5" />
                清空
              </Button>
            )}
          </div>

          <div className="grid gap-1.5">
            <Label htmlFor="time-range-from" className="text-muted-foreground">
              起始
            </Label>
            <Input
              id="time-range-from"
              type="datetime-local"
              className="h-8 font-mono"
              value={from}
              onChange={(e) => onChange({ from: e.target.value, to })}
            />
          </div>

          <div className="grid gap-1.5">
            <Label htmlFor="time-range-to" className="text-muted-foreground">
              结束
            </Label>
            <Input
              id="time-range-to"
              type="datetime-local"
              className="h-8 font-mono"
              value={to}
              onChange={(e) => onChange({ from, to: e.target.value })}
            />
          </div>

          <Separator />

          <div className="grid grid-cols-2 gap-1.5">
            {QUICK.map((q) => (
              <Button
                key={q.label}
                variant="outline"
                size="sm"
                className="h-7 font-normal"
                onClick={() => {
                  onChange(q.range());
                  setOpen(false);
                }}
              >
                {q.label}
              </Button>
            ))}
          </div>

          <p className="text-muted-foreground text-[11px] leading-relaxed">
            两端都可以留空，留空的那端就是不限。区间是闭区间（含起止那一分钟）。
          </p>
        </div>
      </PopoverContent>
    </Popover>
  );
}
