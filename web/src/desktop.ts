// The desktop app (desktop/src-tauri) opens this same web app and adds a few commands.
// In a browser none of this exists and these return null.

interface TauriGlobal {
  core: { invoke: <T>(command: string, args?: Record<string, unknown>) => Promise<T> };
}

export interface AvailableUpdate {
  version: string;
  current: string;
  date: string | null;
  notes: string | null;
}

function tauri(): TauriGlobal | null {
  return (window as unknown as { __TAURI__?: TauriGlobal }).__TAURI__ ?? null;
}

export function isDesktop(): boolean {
  return tauri() !== null;
}

export async function appVersion(): Promise<string | null> {
  const t = tauri();
  return t ? t.core.invoke<string>("app_version") : null;
}

/** The newer release on GitHub, or null when this is the newest. */
export async function checkForUpdate(): Promise<AvailableUpdate | null> {
  const t = tauri();
  if (!t) return null;
  return t.core.invoke<AvailableUpdate | null>("check_update");
}

/** Download, check the signature, install and restart. Resolves only on failure. */
export async function installUpdate(): Promise<void> {
  const t = tauri();
  if (!t) throw new Error("updates are installed by the desktop app");
  await t.core.invoke("install_update");
}

interface TauriWindow {
  minimize: () => Promise<void>;
  toggleMaximize: () => Promise<void>;
  close: () => Promise<void>;
  isMaximized: () => Promise<boolean>;
  onResized: (cb: () => void) => Promise<() => void>;
}

function currentWindow(): TauriWindow | null {
  const w = (window as unknown as { __TAURI__?: { window?: { getCurrentWindow: () => TauriWindow } } }).__TAURI__?.window;
  return w ? w.getCurrentWindow() : null;
}

export const isMac = /Mac|iPhone|iPad/.test(navigator.platform || navigator.userAgent);

/** The desktop window draws its own title bar; on macOS the system keeps its traffic lights. */
export function ownTitleBar(): boolean {
  return isDesktop() && !isMac;
}

export const windowControls = {
  minimize: () => void currentWindow()?.minimize(),
  toggleMaximize: () => void currentWindow()?.toggleMaximize(),
  close: () => void currentWindow()?.close(),
  isMaximized: async () => (await currentWindow()?.isMaximized()) ?? false,
  onResized: (cb: () => void) => currentWindow()?.onResized(cb) ?? Promise.resolve(() => {}),
};

/** ⌘ on a Mac, Ctrl elsewhere, for labels. */
export const MOD = isMac ? "⌘" : "Ctrl";
