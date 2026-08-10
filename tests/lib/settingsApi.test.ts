import { http, HttpResponse } from "msw";
import { describe, expect, expectTypeOf, it } from "vitest";

import { backupsApi, settingsApi } from "@/lib/api/settings";
import { server } from "../msw/server";

const TAURI_ENDPOINT = "http://tauri.local";

describe("settings API restore sync contracts", () => {
  it("invokes the dedicated post-import sync retry command", async () => {
    let requestBody: unknown;
    server.use(
      http.post(
        `${TAURI_ENDPOINT}/retry_post_import_sync`,
        async ({ request }) => {
          requestBody = await request.json();
          return HttpResponse.json(null);
        },
      ),
    );

    await settingsApi.retryPostImportSync();

    expect(requestBody).toEqual({});
  });

  it("forwards the camel-case local restore result", async () => {
    let requestBody: unknown;
    server.use(
      http.post(`${TAURI_ENDPOINT}/restore_db_backup`, async ({ request }) => {
        requestBody = await request.json();
        return HttpResponse.json({
          safetyBackupId: "safety-backup-1",
          warning: "post-import sync failed",
        });
      }),
    );

    const result = await backupsApi.restoreDbBackup("backup.db");

    expectTypeOf(result).toEqualTypeOf<{
      safetyBackupId: string;
      warning?: string;
    }>();
    expect(requestBody).toEqual({ filename: "backup.db" });
    expect(result).toEqual({
      safetyBackupId: "safety-backup-1",
      warning: "post-import sync failed",
    });
  });
});
