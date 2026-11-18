// Who plays each role in the next run. Nothing is built in: the choice comes from what the
// person saved for the project, then from what they last used, and a role with neither is
// left empty until they pick. The first time every role is picked, the answer is saved, so
// the question is asked once.

import { useCallback, useEffect, useMemo, useState } from "react";
import type { DaemonSettings, Models, Provider, RoleName, Roles } from "./api";
import { api } from "./api";
import { prefs, setPrefs, usePrefs, type RoleDefault } from "./prefs";
import { queryClient, keys } from "./query";

export type Seat = "master" | "mid" | "worker" | "checker";

/** The four seats a person picks for. The checker seat is the reviewer and the verifier. */
export const SEATS: { id: Seat; label: string; hint: string; roles: RoleName[] }[] = [
  { id: "master", label: "Master", hint: "Plans the run and owns the shared files", roles: ["master"] },
  { id: "mid", label: "Mid", hint: "Picks up a page a worker could not finish", roles: ["mid"] },
  { id: "worker", label: "Worker", hint: "Builds each page, one agent per page", roles: ["worker"] },
  { id: "checker", label: "Checker", hint: "Reviews the result and confirms each problem", roles: ["reviewer", "verifier"] },
];

export const ALL_ROLES: RoleName[] = ["master", "mid", "worker", "reviewer", "verifier"];

export type Choice = Partial<Record<RoleName, RoleDefault>>;

export function savedChoice(settings: DaemonSettings | null | undefined): Choice {
  const out: Choice = {};
  for (const role of ALL_ROLES) {
    const harness = settings?.roles?.[role];
    if (harness) out[role] = { harness, model: settings?.models?.[role] ?? null };
  }
  return out;
}

export function startingChoice(settings: DaemonSettings | null | undefined): Choice {
  const project = settings?.project ?? "";
  const last = prefs().lastRoles[project] ?? {};
  return { ...last, ...savedChoice(settings) };
}

export function seatChoice(choice: Choice, seat: Seat): RoleDefault | null {
  const roles = SEATS.find((s) => s.id === seat)!.roles;
  return choice[roles[0]!] ?? null;
}

export function missingSeats(choice: Choice): Seat[] {
  return SEATS.filter((s) => s.roles.some((r) => !choice[r]?.harness)).map((s) => s.id);
}

export function toRequest(choice: Choice): { roles: Roles; models: Models } {
  const roles = {} as Roles;
  const models: Models = {};
  for (const role of ALL_ROLES) {
    const pick = choice[role];
    if (!pick) continue;
    roles[role] = pick.harness;
    if (pick.model) models[role] = pick.model;
  }
  return { roles, models };
}

/** Why a choice cannot be used, from what the providers list returned, or null. */
export function problemWith(providers: Provider[], pick: RoleDefault | null): string | null {
  if (!pick) return "Choose a provider";
  if (!providers.length) return null;
  const p = providers.find((x) => x.id === pick.harness);
  if (!p) return `${pick.harness} is not a provider the daemon lists`;
  if (!p.installed) return `${p.name} is not installed`;
  if (pick.model && p.models.length && !p.models.some((m) => m.id === pick.model)) return `${p.name} no longer offers ${pick.model}`;
  return null;
}

/**
 * Save a choice for the project. Used for "remember for this project", and once on a fresh
 * install, when nothing was saved and the person has just picked every role.
 */
export async function rememberChoice(choice: Choice): Promise<DaemonSettings> {
  const { roles, models } = toRequest(choice);
  const settings = queryClient.getQueryData<DaemonSettings>(keys.settings);
  const merged: Models = { ...(settings?.models ?? {}) };
  for (const role of ALL_ROLES) if (roles[role]) merged[role] = models[role] ?? null;
  const saved = await api.setRoleDefaults({ roles, models: merged });
  queryClient.setQueryData(keys.settings, saved);
  return saved;
}

/** After a run starts: keep it as the last used choice, and save it when nothing was saved yet. */
export async function afterStart(choice: Choice, settings: DaemonSettings | null | undefined, remember: boolean) {
  const project = settings?.project ?? "";
  setPrefs((p) => ({ lastRoles: { ...p.lastRoles, [project]: choice } }));
  const nothingSaved = ALL_ROLES.every((r) => !settings?.roles?.[r]);
  if (remember || nothingSaved) await rememberChoice(choice).catch(() => undefined);
}

/** The editable choice for one composer, seeded from the saved and last used ones. */
export function useChoice(settings: DaemonSettings | null | undefined) {
  usePrefs();
  const [touched, setTouched] = useState(false);
  const [choice, setChoice] = useState<Choice>(() => startingChoice(settings));
  const seedKey = `${settings?.project ?? ""}|${JSON.stringify(settings?.roles ?? {})}|${JSON.stringify(settings?.models ?? {})}`;
  useEffect(() => {
    if (!touched) setChoice(startingChoice(settings));
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [seedKey, touched]);
  const setSeat = useCallback((seat: Seat, pick: RoleDefault) => {
    setTouched(true);
    const roles = SEATS.find((s) => s.id === seat)!.roles;
    setChoice((c) => {
      const next = { ...c };
      for (const r of roles) next[r] = pick;
      return next;
    });
  }, []);
  const missing = useMemo(() => missingSeats(choice), [choice]);
  const fresh = ALL_ROLES.every((r) => !settings?.roles?.[r]) && !Object.keys(prefs().lastRoles[settings?.project ?? ""] ?? {}).length;
  return { choice, setSeat, missing, fresh, touched };
}
