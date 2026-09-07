import { http, HttpResponse } from "msw";
import { describe, expect, it } from "vitest";
import {
  modelValidationApi,
  type PrepareValidationRequest,
} from "@/lib/api/modelValidation";
import { server } from "../msw/server";

describe("validation IPC contracts", () => {
  it("passes fixed target identifiers without a credential or live-provider switch", async () => {
    const calls: Array<{ command: string; body: unknown }> = [];
    for (const command of [
      "prepare_model_validation",
      "start_model_validation",
      "get_model_validation",
      "list_model_validations",
      "cancel_model_validation",
    ]) {
      server.use(
        http.post(`http://tauri.local/${command}`, async ({ request }) => {
          calls.push({ command, body: await request.json() });
          return HttpResponse.json({});
        }),
      );
    }
    const request: PrepareValidationRequest = {
      target: {
        appId: "claude",
        providerId: "key-a",
        model: "synthetic-claude",
        protocol: "anthropic",
      },
      mode: "ccs",
      probes: ["call", "signature", "cross_signature"],
      comparisonTarget: {
        appId: "claude",
        providerId: "key-b",
        model: "synthetic-claude",
        protocol: "anthropic",
      },
    };
    await modelValidationApi.prepare(request);
    await modelValidationApi.start("plan-a");
    await modelValidationApi.get("run-a");
    await modelValidationApi.list("claude", "key-a", 10);
    await modelValidationApi.cancel("run-a");
    expect(calls).toEqual([
      { command: "prepare_model_validation", body: { request } },
      { command: "start_model_validation", body: { planId: "plan-a" } },
      { command: "get_model_validation", body: { runId: "run-a" } },
      {
        command: "list_model_validations",
        body: { appId: "claude", providerId: "key-a", limit: 10 },
      },
      { command: "cancel_model_validation", body: { runId: "run-a" } },
    ]);
  });
});
