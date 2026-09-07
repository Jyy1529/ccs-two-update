import { invoke } from "@tauri-apps/api/core";
import type { AppId } from "./types";

export type ManagementPhase =
  | "managed"
  | "unmanaged"
  | "pending_release"
  | "pending_review";

export interface AppManagementEntry {
  appId: AppId;
  enabled: boolean;
  phase: ManagementPhase;
  message?: string;
}

export interface AppManagementState {
  revision: string;
  apps: AppManagementEntry[];
}

export interface AppManagementPreview {
  id: string;
  appId: AppId;
  enabled: boolean;
  revision: string;
  files: Array<{ path: string; action: string }>;
  warnings: string[];
  conflicts: string[];
}

export interface ConfigGuardFile {
  id: string;
  path: string;
  format: string;
  protectedPaths: string[];
  protectFile: boolean;
  hasBaseline: boolean;
  // Opaque file/protection version token; always pass it back unchanged.
  revision: string;
}

export interface ConfigChangePreview {
  id: string;
  appId: AppId;
  path: string;
  conflicts: string[];
  changes: Array<{
    path: string;
    kind: string;
    before?: string;
    after?: string;
  }>;
  revision: string;
}

export interface ConfigGuardState {
  appId: AppId;
  files: ConfigGuardFile[];
  pendingChanges: ConfigChangePreview[];
  backups?: GuardBackup[];
  history?: GuardAudit[];
}

export interface GuardBackup {
  id: string;
  fileId: string;
  path: string;
  createdAt: string;
  source: string;
  fields: string[];
  beforeRevision: string;
  afterRevision: string;
  groupId: string;
}

export interface GuardAudit {
  id: string;
  createdAt: string;
  source: string;
  result: "applied" | "conflict" | "failed" | "kept_local";
  paths: string[];
  fields: string[];
}

// Management is device-local authority, deliberately separate from Settings.
// The UI sends backend-issued plan/file IDs, never arbitrary native paths.
export const appManagementApi = {
  getState(): Promise<AppManagementState> {
    return invoke("get_app_management_state");
  },
  preview(appId: AppId, enabled: boolean): Promise<AppManagementPreview> {
    return invoke("preview_app_management_change", { appId, enabled });
  },
  apply(planId: string): Promise<AppManagementState> {
    return invoke("apply_app_management_change", { planId });
  },
};

export const configGuardApi = {
  getState(appId: AppId): Promise<ConfigGuardState> {
    return invoke("get_config_guard_state", { appId });
  },
  setProtection(
    appId: AppId,
    fileId: string,
    protectedPaths: string[],
    protectFile: boolean,
    expectedRevision: string,
  ): Promise<ConfigGuardState> {
    return invoke("set_config_protection", {
      appId,
      fileId,
      protectedPaths,
      protectFile,
      expectedRevision,
    });
  },
  preview(appId: AppId, fileId: string): Promise<ConfigChangePreview> {
    return invoke("preview_config_change", { appId, fileId });
  },
  previewRestore(appId: AppId, backupId: string): Promise<ConfigChangePreview> {
    return invoke("preview_config_restore", { appId, backupId });
  },
  apply(
    previewId: string,
    resolution: "keep_local" | "apply_ccs",
  ): Promise<ConfigGuardState> {
    return invoke("apply_config_change", { previewId, resolution });
  },
};
