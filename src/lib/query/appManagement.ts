import { useQuery } from "@tanstack/react-query";
import { appManagementApi, configGuardApi } from "@/lib/api/appManagement";
import type { AppId } from "@/lib/api/types";

export const appManagementKeys = {
  state: ["appManagement"] as const,
  guard: (appId: AppId) => ["configGuard", appId] as const,
};

export function useAppManagementState(enabled = true) {
  return useQuery({
    queryKey: appManagementKeys.state,
    queryFn: appManagementApi.getState,
    enabled,
    staleTime: 5_000,
    refetchInterval: 5_000,
    retry: false,
  });
}

export function useAppManagement(appId: AppId, enabled = true) {
  const query = useAppManagementState(enabled);
  const entry = query.data?.apps.find((app) => app.appId === appId);
  return {
    ...query,
    entry,
    // Missing, unreadable or transitional state must never enable live writes.
    canWrite:
      query.isSuccess && entry?.enabled === true && entry.phase === "managed",
    canReview:
      query.isSuccess &&
      entry?.enabled === true &&
      (entry.phase === "managed" || entry.phase === "pending_review"),
  };
}

export function useConfigGuard(appId: AppId, enabled = true) {
  return useQuery({
    queryKey: appManagementKeys.guard(appId),
    queryFn: () => configGuardApi.getState(appId),
    enabled,
    retry: false,
    refetchOnWindowFocus: true,
  });
}
