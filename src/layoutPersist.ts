/**
 * 布局保存队列：把"持久化"从 setState 更新函数中挪到渲染提交之后。
 *
 * 背景（分级优化方案 1.3）：updateLayout 曾在 setSnapshot 的更新函数体内直接
 * 发起 save_layout IPC；React StrictMode 会双重调用更新函数（用于暴露非纯
 * 更新），导致同一布局变更被保存两次。队列把两件事拆开：
 * - `enqueue` 可在更新函数内安全重复调用（只记录待保存值）；
 * - `flush(save)` 在渲染提交后的 effect 中调用，同一批入队只会保存一次，
 *   连续多次入队则按顺序各保存一次，保证后保存的总是最新布局。
 * 保存函数放在 flush 时传入，让组件能用当次渲染的闭包（如最新的 i18n 文案）。
 */
export function createLayoutPersistQueue<T>() {
  let pending: { value: T } | null = null;
  let chain: Promise<void> = Promise.resolve();

  const enqueue = (value: T) => {
    pending = { value };
  };

  const flush = (save: (value: T) => Promise<unknown>): Promise<void> => {
    const queued = pending;
    if (queued === null) {
      return chain;
    }
    pending = null;
    const run = chain.then(async () => {
      await save(queued.value);
    });
    // 调用方通过 `run` 感知失败；链条本身归位为已完成，
    // 一次保存失败不会阻断（或污染）后续保存。
    chain = run.then(
      () => {},
      () => {},
    );
    return run;
  };

  return { enqueue, flush };
}
