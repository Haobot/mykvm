import test from 'node:test';
import assert from 'node:assert/strict';
import {
  SCREEN_CAL_CORNER_ORDER,
  canFinishScreenCal,
  cornerOrdinal,
  nextCorner,
  showScreenCalTopologyWarning,
  sortScreensByBoard,
  type HaloScreenCalPlanItem,
  type HaloScreenCalProgress,
} from './screenCalWizard.ts';

function planItem(index: number, x: number, y: number): HaloScreenCalPlanItem {
  return {
    screenIndex: index,
    screenId: `screen-${index}`,
    label: index + 1,
    x,
    y,
    width: 1920,
    height: 1080,
  };
}

function progress(
  acceptedScreens: number,
  totalScreens: number,
  cornersDone: number,
): HaloScreenCalProgress {
  return { acceptedScreens, totalScreens, cornersDone };
}

test('corner order is 左上→右上→右下→左下 and wraps via the caller', () => {
  assert.deepEqual(SCREEN_CAL_CORNER_ORDER, ['tl', 'tr', 'br', 'bl']);
  // Within one screen the wizard walks the order in sequence…
  assert.equal(nextCorner(0), 'tl');
  assert.equal(nextCorner(1), 'tr');
  assert.equal(nextCorner(2), 'br');
  assert.equal(nextCorner(3), 'bl');
  // …and after an accepted region (corners reset by the caller) it restarts.
  assert.equal(nextCorner(4), 'tl');
  assert.equal(nextCorner(7), 'bl');
});

test('corner ordinal is 1-based and repeats per screen', () => {
  assert.equal(cornerOrdinal(0), 1);
  assert.equal(cornerOrdinal(1), 2);
  assert.equal(cornerOrdinal(3), 4);
  assert.equal(cornerOrdinal(4), 1, 'next screen restarts at 第 1 角');
  assert.equal(cornerOrdinal(6), 3);
});

test('plan list is board x then y sorted with 1..N labels (plan rendering)', () => {
  const screens = [
    { x: 1920, y: 0 },
    { x: 0, y: 1080 },
    { x: 0, y: 0 },
    { x: 1920, y: -540 },
  ];
  const sorted = sortScreensByBoard(screens);
  // x ascending, ties by y: left pair first, then the right pair.
  assert.deepEqual(
    sorted.map((s) => `${s.x},${s.y}`),
    ['0,0', '0,1080', '1920,-540', '1920,0'],
  );
  // The UI renders the numbered badge from the sorted position, mirroring
  // the backend plan (label = index + 1).
  const plan = sorted.map((screen, index) => planItem(index, screen.x, screen.y));
  assert.deepEqual(
    plan.map((item) => item.label),
    [1, 2, 3, 4],
  );
  assert.equal(plan[1].y, 1080);
});

test('finish button enables only when every planned screen is accepted', () => {
  // Nothing started: disabled.
  assert.equal(canFinishScreenCal(progress(0, 0, 0)), false);
  // Mid-wizard: disabled.
  assert.equal(canFinishScreenCal(progress(1, 2, 4)), false);
  assert.equal(canFinishScreenCal(progress(1, 2, 5)), false);
  // All screens accepted: enabled regardless of the corner counter.
  assert.equal(canFinishScreenCal(progress(2, 2, 8)), true);
  // Single screen fully accepted.
  assert.equal(canFinishScreenCal(progress(1, 1, 4)), true);
});

test('topology warning shows only when the calibration is stale', () => {
  // No calibration yet (null/undefined from the backend): no warning.
  assert.equal(showScreenCalTopologyWarning(null), false);
  assert.equal(showScreenCalTopologyWarning(undefined), false);
  // Topology still matches: no warning.
  assert.equal(showScreenCalTopologyWarning(true), false);
  // Layout changed since capture: warn.
  assert.equal(showScreenCalTopologyWarning(false), true);
});
