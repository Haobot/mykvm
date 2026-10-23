import test from 'node:test';
import assert from 'node:assert/strict';
import {
  blockingRuntimeDetail,
  createRuntimeBlockerTracker,
  dismissRuntimeBlocker,
  trackRuntimeBlocker,
} from './runtimeBlocker.ts';

const stage = (state: string, detail = state) => ({ state, detail });

test('only error stages block, capture first then inject', () => {
  assert.equal(blockingRuntimeDetail(stage('ready'), stage('ready')), null);
  assert.equal(blockingRuntimeDetail(stage('error', 'capture'), stage('error', 'inject')), 'capture');
  assert.equal(blockingRuntimeDetail(stage('ready'), stage('error', 'inject')), 'inject');
  assert.equal(blockingRuntimeDetail(stage('stubbed'), stage('idle')), null);
});

test('a persistent blocking condition opens the dialog only once', () => {
  const first = trackRuntimeBlocker(createRuntimeBlockerTracker(), 'secure keyboard entry on');
  assert.equal(first.show, 'secure keyboard entry on');
  assert.equal(first.clear, null);
  assert.equal(first.tracker.active, 'secure keyboard entry on');
  assert.equal(first.tracker.shown, 'secure keyboard entry on');

  // The next poll reports the same stale detail: no second dialog.
  const second = trackRuntimeBlocker(first.tracker, 'secure keyboard entry on');
  assert.equal(second.show, null);
  assert.equal(second.clear, null);
});

test('a dismissed warning stays quiet until the condition clears and comes back', () => {
  const shown = trackRuntimeBlocker(createRuntimeBlockerTracker(), 'secure keyboard entry on');
  const dismissed = dismissRuntimeBlocker(shown.tracker);
  assert.equal(dismissed.dismissed, 'secure keyboard entry on');
  assert.equal(dismissed.shown, null);

  const quiet = trackRuntimeBlocker(dismissed, 'secure keyboard entry on');
  assert.equal(quiet.show, null);

  const cleared = trackRuntimeBlocker(quiet.tracker, null);
  assert.equal(cleared.clear, null);
  assert.deepEqual(cleared.tracker, createRuntimeBlockerTracker());

  const again = trackRuntimeBlocker(cleared.tracker, 'secure keyboard entry on');
  assert.equal(again.show, 'secure keyboard entry on');
});

test('clearing the condition closes the dialog the app still shows', () => {
  const shown = trackRuntimeBlocker(createRuntimeBlockerTracker(), 'secure keyboard entry on');
  const cleared = trackRuntimeBlocker(shown.tracker, null);
  assert.equal(cleared.clear, 'secure keyboard entry on');
  assert.equal(cleared.show, null);
  assert.deepEqual(cleared.tracker, createRuntimeBlockerTracker());
});

test('a changed blocking detail replaces the dialog and is dismissable again', () => {
  const shown = trackRuntimeBlocker(createRuntimeBlockerTracker(), 'first');
  const dismissed = dismissRuntimeBlocker(shown.tracker);
  const next = trackRuntimeBlocker(dismissed, 'second');
  assert.equal(next.show, 'second');
  assert.equal(next.tracker.shown, 'second');
  assert.equal(next.tracker.dismissed, null);
});

test('dismissing without a runtime blocker is a no-op', () => {
  const tracker = createRuntimeBlockerTracker();
  assert.equal(dismissRuntimeBlocker(tracker), tracker);
});
