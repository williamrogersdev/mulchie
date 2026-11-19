// The Tauri bridge. `withGlobalTauri` exposes it as window.__TAURI__.
const T = window.__TAURI__;

export const invoke = (cmd, args) => T.core.invoke(cmd, args);
// Events sent to this window only. The global `event.listen` also receives
// events the backend sends to the other window (`emit_to("dashboard", …)`),
// so the tray panel would run the dashboard's actions and take its
// visibility, and both windows would plan the same clear.
export const listen = (name, fn) => {
  const w = T?.webviewWindow?.getCurrentWebviewWindow?.();
  if (w) return w.listen(name, fn);
  return T?.event ? T.event.listen(name, fn) : Promise.resolve(() => {});
};
