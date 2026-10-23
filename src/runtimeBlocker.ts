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
