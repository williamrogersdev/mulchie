// Shared UI state. Modules read and write `st`; main.js fills `hooks`.

export const MODE = location.hash.replace('#', '') || 'dashboard';

export const st = {
  // The slim engine state, with `config` and `actions` attached from the
  // caches below; null until the first answer.
  S: null,
  // History samples, oldest first, at most HISTORY_MAX.
  H: [],
  view: 'overview',
  // {mode: engine|client|disconnected, port, engine_pid, detail, since_unix}
  conn: null,
  // Why the last state request failed, when it did.
  error: null,
  cfg: null, cfgRev: null,
  actions: [], actionsRev: null,
  integ: null,
  ui: {
    pSort: ['cpu_pct', -1], pKind: '', pSel: new Set(), pFocus: null,
    wSort: ['cpu_pct', -1], wFocus: null,
    wtF: 'all', wtSel: new Set(), wtSort: ['bytes', -1],
    bcF: 'stale', bcSel: new Set(),
    aSrc: '', cfgDirty: false,
    sSort: ['cpu_cores', -1], sIncAgent: false, limDirty: false,
    sSel: new Set(), sF: 'all',
  },
};

export const HISTORY_MAX = 600;

// Set by main.js so views can trigger a refresh, a render or navigation
// without importing main.js.
export const hooks = { refresh: async () => {}, render: () => {}, go: () => {} };
