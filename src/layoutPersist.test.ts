import test from 'node:test';
import assert from 'node:assert/strict';
import { createLayoutPersistQueue } from './layoutPersist.ts';

test('同一布局变更只保存一次（StrictMode 双重入队与双重 flush 均安全）', async () => {
  let saveCount = 0;
  const saved: number[] = [];
  const save = async (value: number) => {
    saveCount += 1;
    saved.push(value);
  };
  const queue = createLayoutPersistQueue<number>();

  // 模拟 StrictMode 双重调用 setState 更新函数：同一值入队两次。
  queue.enqueue(7);
  queue.enqueue(7);
  // 模拟 StrictMode 双重执行提交后的 effect：flush 两次。
  await queue.flush(save);
  await queue.flush(save);

  assert.equal(saveCount, 1);
  assert.deepEqual(saved, [7]);
});

test('合并排队时只保存最新布局', async () => {
  const saved: number[] = [];
  const save = async (value: number) => {
    saved.push(value);
  };
  const queue = createLayoutPersistQueue<number>();

  queue.enqueue(1);
  queue.enqueue(2);
  await queue.flush(save);

  assert.deepEqual(saved, [2]);
});

test('连续变更按顺序各保存一次，不丢中间状态', async () => {
  const saved: number[] = [];
  const save = async (value: number) => {
    saved.push(value);
  };
  const queue = createLayoutPersistQueue<number>();

  queue.enqueue(1);
  const first = queue.flush(save);
  queue.enqueue(2);
  const second = queue.flush(save);
  await Promise.all([first, second]);

  assert.deepEqual(saved, [1, 2]);
});

test('空队列 flush 不触发保存', async () => {
  let saveCount = 0;
  const save = async () => {
    saveCount += 1;
  };
  const queue = createLayoutPersistQueue<number>();

  await queue.flush(save);

  assert.equal(saveCount, 0);
});

test('flush 等待保存完成后才返回，保存失败不阻断后续保存', async () => {
  const saved: number[] = [];
  let fail = true;
  const save = async (value: number) => {
    if (fail) {
      throw new Error('磁盘已满');
    }
    saved.push(value);
  };
  const queue = createLayoutPersistQueue<number>();

  queue.enqueue(1);
  // 首次保存失败：flush 以异常结束，但队列状态已消费。
  await assert.rejects(queue.flush(save), /磁盘已满/);

  fail = false;
  queue.enqueue(2);
  await queue.flush(save);

  assert.deepEqual(saved, [2]);
});
