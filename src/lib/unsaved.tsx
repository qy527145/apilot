import {
  createContext,
  useCallback,
  useContext,
  useEffect,
  useId,
  useMemo,
  useRef,
  useState,
  type ReactNode,
} from "react";

/**
 * 一个表单在登记处里的条目。
 *
 * `isDirty` 与 `save` 都做成函数而不是值：调用方每次渲染都会产生新的闭包
 * （它们捕获了最新的草稿），登记值本身会让下面的 effect 每次渲染都重跑。
 */
interface UnsavedEntry {
  isDirty: () => boolean;
  /** 保存并返回是否成功。缺省表示这个表单没法从外部保存。 */
  save?: () => Promise<boolean>;
}

interface UnsavedApi {
  register: (id: string, entry: UnsavedEntry) => void;
  unregister: (id: string) => void;
  /** 脏状态变了，重算「有没有未保存改动」。 */
  refresh: () => void;
  /** 依次保存所有脏表单；任一失败即中止并返回 false。 */
  saveAll: () => Promise<boolean>;
}

/**
 * 「未保存改动」的登记处。
 *
 * 项目没有 react-router（`App.tsx` 单壳 + `currentView` 状态切换），所以
 * react-router 的 `useBlocker` 用不上。切页发生在 `Shell::setView`，
 * 但脏状态长在页面深处的表单里 —— 这里做一个登记表，让表单把它自己的
 * 脏状态与保存动作上报，切换时由 `Shell` 查一次、必要时调一次。
 *
 * 拆成两个 Context 是为了收窄重渲染面：`api` 是稳定的，只有真正关心
 * 「脏不脏」的 `Shell` 才会因为脏状态变化而重渲染。
 */
const ApiContext = createContext<UnsavedApi | null>(null);
const DirtyContext = createContext(false);

export function UnsavedChangesProvider({ children }: { children: ReactNode }) {
  const entries = useRef(new Map<string, UnsavedEntry>());
  const [hasUnsaved, setHasUnsaved] = useState(false);

  const recompute = useCallback(() => {
    setHasUnsaved([...entries.current.values()].some((e) => e.isDirty()));
  }, []);

  const api = useMemo<UnsavedApi>(
    () => ({
      register: (id, entry) => {
        entries.current.set(id, entry);
        recompute();
      },
      unregister: (id) => {
        entries.current.delete(id);
        recompute();
      },
      refresh: recompute,
      saveAll: async () => {
        // 快照一份再遍历：保存过程中的重渲染会改动这个 Map。
        for (const entry of [...entries.current.values()]) {
          if (!entry.isDirty()) continue;
          // 有脏表单却不知道怎么保存 —— 宁可让调用方停下来，也不能静默丢弃。
          if (!entry.save) return false;
          if (!(await entry.save())) return false;
        }
        return true;
      },
    }),
    [recompute],
  );

  return (
    <ApiContext.Provider value={api}>
      <DirtyContext.Provider value={hasUnsaved}>
        {children}
      </DirtyContext.Provider>
    </ApiContext.Provider>
  );
}

/**
 * 表单用这个上报自己的脏状态与保存动作。`save` 可选 —— 只上报脏状态、
 * 不提供保存的页面，切页时会走「放弃更改」那条路。
 */
export function useUnsavedChanges(
  isDirty: boolean,
  save?: () => Promise<boolean>,
): void {
  const api = useContext(ApiContext);
  const id = useId();

  const dirtyRef = useRef(isDirty);
  dirtyRef.current = isDirty;
  const saveRef = useRef(save);
  saveRef.current = save;

  // 登记与注销只在挂载 / 卸载时发生。转发器读 ref，所以永远看到最新的值，
  // 不必（也不能）把 isDirty / save 放进依赖数组 —— 那样每次编辑都会先注销
  // 再注册，中间那一帧 Shell 读到的是"没有未保存改动"。
  useEffect(() => {
    if (!api) return;
    api.register(id, {
      isDirty: () => dirtyRef.current,
      save: saveRef.current ? () => saveRef.current!() : undefined,
    });
    return () => api.unregister(id);
  }, [api, id]);

  // 转发器不会触发重渲染，脏状态翻转得单独通知一次。
  useEffect(() => {
    api?.refresh();
  }, [api, isDirty]);
}

/** 切页前查一次：现在有没有未保存的改动。 */
export function useHasUnsavedChanges(): boolean {
  return useContext(DirtyContext);
}

/** 切页弹窗里「保存并离开」用：把所有脏表单存下来，全成功才返回 true。 */
export function useSaveAllChanges(): () => Promise<boolean> {
  const api = useContext(ApiContext);
  return useCallback(() => api?.saveAll() ?? Promise.resolve(false), [api]);
}
