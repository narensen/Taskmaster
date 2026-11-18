// One cache for what the daemon says, so going back to a page shows it at once and
// refreshes behind it. Hovering a run or a file asks for it early.

import { QueryClient, useQuery } from "@tanstack/react-query";
import { api } from "./api";

export const queryClient = new QueryClient({
  defaultOptions: {
    queries: {
      staleTime: 5_000,
      gcTime: 10 * 60_000,
      refetchOnWindowFocus: true,
      retry: 1,
    },
  },
});

export const keys = {
  settings: ["settings"] as const,
  providers: ["providers"] as const,
  run: (id: string) => ["run", id] as const,
  file: (path: string) => ["file", path] as const,
  files: ["files"] as const,
  projects: ["projects"] as const,
  diff: (id: string) => ["diff", id, "run"] as const,
};

export function useSettings() {
  return useQuery({ queryKey: keys.settings, queryFn: api.settings, staleTime: 30_000 });
}

/** Asking every harness for its models is slow, so the answer is kept for a few minutes. */
export function useProviders() {
  return useQuery({ queryKey: keys.providers, queryFn: () => api.providers(), staleTime: 5 * 60_000 });
}

export function useProjectFiles() {
  return useQuery({ queryKey: keys.files, queryFn: api.files, staleTime: 20_000 });
}

export function useFile(path: string | null) {
  return useQuery({ queryKey: keys.file(path ?? ""), queryFn: () => api.file(path!), enabled: !!path, staleTime: 10_000 });
}

export function useRunDiff(id: string | null) {
  return useQuery({ queryKey: keys.diff(id ?? ""), queryFn: () => api.diff(id!, "run"), enabled: !!id, staleTime: 15_000 });
}

export function prefetchRun(id: string) {
  void queryClient.prefetchQuery({ queryKey: keys.run(id), queryFn: () => api.run(id), staleTime: 5_000 });
}

export function prefetchFile(path: string) {
  void queryClient.prefetchQuery({ queryKey: keys.file(path), queryFn: () => api.file(path), staleTime: 10_000 });
}

export function refreshSettings() {
  void queryClient.invalidateQueries({ queryKey: keys.settings });
}
