/// Runtime stage statuses are re-read by the window every couple of seconds.
export interface RuntimeStageStatusLike {
  state: string;
  detail: string;
}

/// Picks the blocking stage detail to surface, with the same priority the
/// window has always used: capture failures first, then inject failures.
export function blockingRuntimeDetail(
  capture: RuntimeStageStatusLike,
  inject: RuntimeStageStatusLike,
): string | null {
  if (capture.state === "error") {
    return capture.detail;
  }
  if (inject.state === "error") {
    return inject.detail;
  }
  return null;
}

export interface RuntimeBlockerTracker {
  /// Blocking detail currently reported by the backend; null once it clears.
  active: string | null;
  /// Detail the user closed; it is not surfaced again until it clears.
  dismissed: string | null;
  /// Detail the dialog currently shows, when the dialog belongs to a blocker.
  shown: string | null;
}

export interface RuntimeBlockerUpdate {
  tracker: RuntimeBlockerTracker;
  /// Message that should (re)open the dialog, or null.
  show: string | null;
  /// Message to close if the dialog is still showing it, or null.
  clear: string | null;
}

export function createRuntimeBlockerTracker(): RuntimeBlockerTracker {
  return { active: null, dismissed: null, shown: null };
}

/// Folds one status read into the blocker tracker.
///
/// A blocking condition has to be surfaced at most once per occurrence: the
/// window polls every two seconds, and re-raising the same dialog after the
/// user closed it is exactly what made the macOS Secure Keyboard Entry warning
/// look permanent. Once the condition clears the dialog is closed (unless the
/// user already closed it) and the next occurrence is allowed to show again.
///
/// The tracker is treated as immutable: callers keep the returned value.
export function trackRuntimeBlocker(
  tracker: RuntimeBlockerTracker,
  detail: string | null,
): RuntimeBlockerUpdate {
  if (detail === null) {
    return {
      tracker: createRuntimeBlockerTracker(),
      show: null,
      clear: tracker.shown,
    };
  }

  if (detail === tracker.shown || detail === tracker.dismissed) {
    return {
      tracker: { ...tracker, active: detail },
      show: null,
      clear: null,
    };
  }

  return {
    tracker: { active: detail, dismissed: null, shown: detail },
    show: detail,
    clear: null,
  };
}

/// Records that the user closed the dialog for the current occurrence.
export function dismissRuntimeBlocker(
  tracker: RuntimeBlockerTracker,
): RuntimeBlockerTracker {
  if (tracker.shown === null) {
    return tracker;
  }

  return { ...tracker, dismissed: tracker.shown, shown: null };
}

/// One identity-drift occurrence: (device id, peer transport key fingerprint).
/// The fingerprint is the drifted peer's transport public key, so a new
/// rotation of the same device is a new occurrence and may surface again.
export interface IdentityDriftSignal {
  deviceId: string;
  fingerprint: string;
}

/// Identity-drift channel of the dialog manager, sharing the runtimeBlocker
/// discipline: the window polls every two seconds, so one drift occurrence may
/// open the dialog at most once, 「later」 keeps this occurrence quiet, and the
/// dialog closes by itself once the drift clears (re-paired or peer offline).
export interface IdentityDriftTracker {
  /// Drift currently surfacing in the dialog; null when none shows.
  shown: IdentityDriftSignal | null;
  /// 「later」 per device: deviceId -> fingerprint of the quiet occurrence.
  dismissed: Record<string, string>;
}

export interface IdentityDriftUpdate {
  tracker: IdentityDriftTracker;
  /// Signal that should open the dialog, or null.
  show: IdentityDriftSignal | null;
  /// True when the currently shown drift resolved and the dialog must close.
  clear: boolean;
}

export function createIdentityDriftTracker(): IdentityDriftTracker {
  return { shown: null, dismissed: {} };
}

function driftKey(signal: IdentityDriftSignal): string {
  return JSON.stringify([signal.deviceId, signal.fingerprint]);
}

/// Folds one poll's drift list into the tracker (callers keep the result).
///
/// - the shown drift stays open while it is still in the list (no reopen);
/// - when it leaves the list the dialog is closed automatically;
/// - 「later」 remembers the occurrence, but only while that exact fingerprint
///   is still drifting, so the next rotation of the same device surfaces again;
/// - with several simultaneous drifts only the first one is shown (no stack).
export function trackIdentityDrift(
  tracker: IdentityDriftTracker,
  drifts: IdentityDriftSignal[],
): IdentityDriftUpdate {
  const shownStays =
    tracker.shown !== null &&
    drifts.some(
      (drift) => driftKey(drift) === driftKey(tracker.shown as IdentityDriftSignal),
    );

  // Drop 「later」 records whose drift is gone, so a new fingerprint on the
  // same device can surface again.
  const live = new Set(drifts.map(driftKey));
  const dismissed = Object.fromEntries(
    Object.entries(tracker.dismissed).filter(([deviceId, fingerprint]) =>
      live.has(driftKey({ deviceId, fingerprint })),
    ),
  );

  if (shownStays) {
    return {
      tracker: { shown: tracker.shown, dismissed },
      show: null,
      clear: false,
    };
  }

  const clear = tracker.shown !== null;
  const next = drifts.find(
    (drift) => dismissed[drift.deviceId] !== drift.fingerprint,
  );
  return {
    tracker: { shown: next ?? null, dismissed },
    show: next ?? null,
    clear,
  };
}

/// Records 「later」 for the shown drift: this occurrence stays quiet until the
/// drift clears or the device rotates its certificate again.
export function dismissIdentityDrift(
  tracker: IdentityDriftTracker,
): IdentityDriftTracker {
  if (tracker.shown === null) {
    return tracker;
  }

  return {
    shown: null,
    dismissed: {
      ...tracker.dismissed,
      [tracker.shown.deviceId]: tracker.shown.fingerprint,
    },
  };
}
