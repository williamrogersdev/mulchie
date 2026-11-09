// The Tauri bridge. `withGlobalTauri` exposes it as window.__TAURI__.
const T = window.__TAURI__;

export const invoke = (cmd, args) => T.core.invoke(cmd, args);
export const listen = (name, fn) => (T?.event ? T.event.listen(name, fn) : Promise.resolve(() => {}));
