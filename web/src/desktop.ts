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
