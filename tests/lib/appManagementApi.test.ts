import { http, HttpResponse } from "msw";
import { describe, expect, it } from "vitest";
import { appManagementApi, configGuardApi } from "@/lib/api/appManagement";
import { settingsApi } from "@/lib/api/settings";
import type { Settings } from "@/types";
import { server } from "../msw/server";

const root = "http://tauri.local";

describe("app management command contracts", () => {
  it("keeps authority out of generic settings saves and does not mutate the caller", async () => {
    let body: unknown;
    server.use(
      http.post(`${root}/save_settings`, async ({ request }) => {
        body = await request.json();
        return HttpResponse.json(true);
      }),
    );
    const staleSettings = {
      language: "en",
      managedApps: { codex: true },
    } as Settings & { managedApps: unknown };
    await settingsApi.save(staleSettings);
    expect(body).toEqual({ settings: { language: "en" } });
    expect(staleSettings.managedApps).toEqual({ codex: true });
  });

  it("uses camelCase parameters and backend-issued identifiers only", async () => {
    const calls: Array<{ command: string; body: unknown }> = [];
    for (const command of [
      "preview_app_management_change",
      "apply_app_management_change",
      "set_config_protection",
      "preview_config_change",
      "preview_config_restore",
      "apply_config_change",
    ]) {
      server.use(
        http.post(`${root}/${command}`, async ({ request }) => {
          calls.push({ command, body: await request.json() });
          return HttpResponse.json({});
        }),
      );
    }
    await appManagementApi.preview("codex", false);
    await appManagementApi.apply("backend-plan");
    await configGuardApi.setProtection(
      "codex",
      "backend-file",
      ["model_catalog_json"],
      false,
      "missing:rules-42",
    );
    await configGuardApi.preview("codex", "backend-file");
    await configGuardApi.apply("backend-preview", "keep_local");
    await configGuardApi.previewRestore("codex", "backend-backup");
    await configGuardApi.apply("backend-restore-preview", "apply_ccs");
    expect(calls).toEqual([
      {
        command: "preview_app_management_change",
        body: { appId: "codex", enabled: false },
      },
      {
        command: "apply_app_management_change",
        body: { planId: "backend-plan" },
      },
      {
        command: "set_config_protection",
        body: {
          appId: "codex",
          fileId: "backend-file",
          protectedPaths: ["model_catalog_json"],
          protectFile: false,
          expectedRevision: "missing:rules-42",
        },
      },
      {
        command: "preview_config_change",
        body: { appId: "codex", fileId: "backend-file" },
      },
      {
        command: "apply_config_change",
        body: { previewId: "backend-preview", resolution: "keep_local" },
      },
      {
        command: "preview_config_restore",
        body: { appId: "codex", backupId: "backend-backup" },
      },
      {
        command: "apply_config_change",
        body: { previewId: "backend-restore-preview", resolution: "apply_ccs" },
      },
    ]);
  });
});
