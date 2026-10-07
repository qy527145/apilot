import {
  createContext,
  useCallback,
  useContext,
  useEffect,
  useId,
  useMemo,
  useState,
  type ReactNode,
} from "react";

/**
 * 「未保存改动」的登记处。
 *
 * 项目没有 react-router（`App.tsx` 单壳 + `currentView` 状态切换），所以
 * react-router 的 `useBlocker` 用不上。切页发生在 `Shell::setView`，
 * 但脏状态长在页面深处的表单里 —— 这里做一个登记表，让表单把它自己的
 * 脏状态上报，切换时由 `Shell` 查一次。
 *
 * 拆成两个 Context 是为了收窄重渲染面：`register` / `unregister` 是稳定的，
 * 只有真正关心「脏不脏」的 `Shell` 才会因为脏状态变化而重渲染。合成一个
 * Context 的话，每次脏状态变化都会让所有调用 `useUnsavedChanges` 的表单
 * 一起重渲染，而且它们上报用的 effect 依赖也跟着变，容易绕出注册循环。
 */
const RegisterContext = createContext<{
  register: (id: string, dirty: boolean) => void;
  unregister: (id: string) => void;
} | null>(null);

const DirtyContext = createContext(false);

export function UnsavedChangesProvider({ children }: { children: ReactNode }) {
  const [dirtyIds, setDirtyIds] = useState<ReadonlySet<string>>(
    () => new Set(),
  );

  const register = useCallback((id: string, dirty: boolean) => {
    setDirtyIds((prev) => {
      // 状态没变就原样返回：不改引用，React 会跳过这次渲染。
      if (prev.has(id) === dirty) return prev;
      const next = new Set(prev);
      if (dirty) next.add(id);
      else next.delete(id);
      return next;
    });
  }, []);

  const unregister = useCallback((id: string) => {
    setDirtyIds((prev) => {
      if (!prev.has(id)) return prev;
      const next = new Set(prev);
      next.delete(id);
      return next;
    });
  }, []);

  const value = useMemo(
    () => ({ register, unregister }),
    [register, unregister],
  );

  return (
    <RegisterContext.Provider value={value}>
      <DirtyContext.Provider value={dirtyIds.size > 0}>
        {children}
      </DirtyContext.Provider>
    </RegisterContext.Provider>
  );
}

/**
 * 表单用这个上报自己的脏状态。`isDirty` 每次变化都会重新登记。
 *
 * 登记与注销拆成两个 effect：注销只在卸载时发生。若把注销写进第一个
 * effect 的清理函数，`isDirty` 每次翻转都会先注销再重新登记，中间那一帧
 * Shell 读到的是"没有未保存改动"，恰好此时切页就漏掉了拦截。
 */
export function useUnsavedChanges(isDirty: boolean): void {
  const ctx = useContext(RegisterContext);
  const id = useId();

  useEffect(() => {
    ctx?.register(id, isDirty);
  }, [ctx, id, isDirty]);

  useEffect(() => {
    if (!ctx) return;
    return () => ctx.unregister(id);
  }, [ctx, id]);
}

/** 切页前查一次：现在有没有未保存的改动。 */
export function useHasUnsavedChanges(): boolean {
  return useContext(DirtyContext);
}
