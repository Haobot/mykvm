import test from 'node:test';
import assert from 'node:assert/strict';
import {
  SCREEN_WARP_RANGES,
  clampToRange,
  screenWarpNeedsCalibration,
  sharedTransAlpha,
} from './screenWarp.ts';

test('slider ranges match the backend sanitize bounds and defaults', () => {
  // params.rs: REGION_SETTLE_MS (150, 800) default 300.
  assert.deepEqual(SCREEN_WARP_RANGES.regionSettleMs, {
    min: 150,
    max: 800,
    step: 50,
    default: 300,
  });
  // params.rs: REGION_MARGIN (1.0, 1.5) default 1.15.
  assert.deepEqual(SCREEN_WARP_RANGES.regionMargin, {
    min: 1.0,
    max: 1.5,
    step: 0.05,
    default: 1.15,
  });
  // params.rs: TRANS_ALPHA (0, 120) default 60.
  assert.deepEqual(SCREEN_WARP_RANGES.transAlpha, {
    min: 0,
    max: 120,
    step: 5,
    default: 60,
  });
});

test('clampToRange keeps in-range values and pulls outliers back in', () => {
  const { regionSettleMs } = SCREEN_WARP_RANGES;
  assert.equal(clampToRange(regionSettleMs, 300), 300);
  assert.equal(clampToRange(regionSettleMs, 0), 150, 'below min clamps up');
  assert.equal(clampToRange(regionSettleMs, 99_999), 800, 'above max clamps down');
  assert.equal(clampToRange(regionSettleMs, Number.NaN), 150, 'NaN falls back to min');
});

test('sharedTransAlpha writes both transAlpha fields with one slider', () => {
  assert.deepEqual(sharedTransAlpha(60), { transAlphaYaw: 60, transAlphaPitch: 60 });
  assert.deepEqual(sharedTransAlpha(0), { transAlphaYaw: 0, transAlphaPitch: 0 });
  assert.deepEqual(
    sharedTransAlpha(500),
    { transAlphaYaw: 120, transAlphaPitch: 120 },
    'out-of-range slider values clamp to the backend bound',
  );
  assert.deepEqual(
    sharedTransAlpha(-5),
    { transAlphaYaw: 0, transAlphaPitch: 0 },
    'negative slider values clamp to zero',
  );
});

test('screenWarpNeedsCalibration only warns when enabled without regions', () => {
  const calibrated = { regions: [{}, {}] };
  assert.equal(
    screenWarpNeedsCalibration({ screenWarpEnabled: true, screenCalibration: calibrated }),
    false,
    'enabled + calibrated: no hint',
  );
  assert.equal(
    screenWarpNeedsCalibration({ screenWarpEnabled: true, screenCalibration: null }),
    true,
    'enabled + no calibration object: hint',
  );
  assert.equal(
    screenWarpNeedsCalibration({
      screenWarpEnabled: true,
      screenCalibration: { regions: [] },
    }),
    true,
    'enabled + empty regions: hint',
  );
  assert.equal(
    screenWarpNeedsCalibration({
      screenWarpEnabled: false,
      screenCalibration: null,
    }),
    false,
    'disabled: switch itself already means no effect, no hint',
  );
  assert.equal(
    screenWarpNeedsCalibration({}),
    false,
    'undefined switch (old layout snapshot): defaults to off, no hint',
  );
});
