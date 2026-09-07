import type { ReactElement } from "react";
import { render } from "@testing-library/react";
import { QueryClientProvider } from "@tanstack/react-query";
import { createInstance } from "i18next";
import { I18nextProvider } from "react-i18next";
import en from "@/i18n/locales/en.json";
import { APP_IDS } from "@/config/appConfig";
import type { AppId } from "@/lib/api/types";
import type {
  AppManagementEntry,
  AppManagementState,
  ConfigGuardState,
} from "@/lib/api/appManagement";
import type { ValidationPlan, ValidationRun } from "@/lib/api/modelValidation";
import { createTestQueryClient } from "./testQueryClient";

export const safetyI18n = createInstance();
export const initializeSafetyI18n = () =>
  safetyI18n.init({
    lng: "en",
    fallbackLng: "en",
    resources: { en: { translation: en } },
    interpolation: { escapeValue: false },
  });

export function managementFixture(
  overrides: Partial<Record<AppId, Partial<AppManagementEntry>>> = {},
): AppManagementState {
  return {
    revision: "management-1",
    apps: APP_IDS.map((appId) => ({
      appId,
      enabled: true,
      phase: "managed",
      ...overrides[appId],
    })),
  };
}

export function guardFixture(): ConfigGuardState {
  return {
    appId: "codex",
    files: [
      {
        id: "backend-file-id",
        path: "D:/isolated/codex/config.toml",
        format: "toml",
        protectedPaths: ["model_catalog_json"],
        protectFile: false,
        hasBaseline: true,
        revision: "file-1",
      },
    ],
    pendingChanges: [],
  };
}

export function validationPlanFixture(): ValidationPlan {
  return {
    id: "plan-fixed-key-a",
    target: {
      appId: "codex",
      providerId: "key-a",
      providerName: "Key A",
      endpoint: "https://synthetic.invalid/v1",
      credentialLabel: "Key …test",
      model: "synthetic-model",
      protocol: "openai_responses",
    },
    mode: "direct",
    probes: ["call", "stream", "tools", "structured", "image"],
    maxRequests: 9,
    maxOutputTokens: 2048,
    maxDurationSeconds: 120,
    estimatedCostUsd: null,
    warnings: ["Synthetic test only"],
    expiresAt: new Date(Date.now() + 60_000).toISOString(),
  };
}

export function validationRunFixture(
  status: ValidationRun["status"] = "running",
): ValidationRun {
  return {
    id: "run-1",
    plan: validationPlanFixture(),
    status,
    startedAt: "2026-09-06T00:00:00Z",
    results: [],
  };
}

export function renderSafetyUi(ui: ReactElement, state?: AppManagementState) {
  const queryClient = createTestQueryClient();
  if (state) queryClient.setQueryData(["appManagement"], state);
  return {
    queryClient,
    ...render(ui, {
      wrapper: ({ children }) => (
        <QueryClientProvider client={queryClient}>
          <I18nextProvider i18n={safetyI18n}>{children}</I18nextProvider>
        </QueryClientProvider>
      ),
    }),
  };
}

// Existing component unit tests assume management has already loaded. Give
// those tests a real query context without changing production's fail-closed
// default or mocking management behavior in the dedicated safety regressions.
export function renderManagedUi(ui: ReactElement, state = managementFixture()) {
  const queryClient = createTestQueryClient();
  queryClient.setQueryData(["appManagement"], state);
  return {
    queryClient,
    ...render(ui, {
      wrapper: ({ children }) => (
        <QueryClientProvider client={queryClient}>
          {children}
        </QueryClientProvider>
      ),
    }),
  };
}
