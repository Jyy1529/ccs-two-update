import { invoke } from "@tauri-apps/api/core";
import type { AppId } from "./types";
import type { FetchedModel } from "./model-fetch";

export type ValidationProtocol =
  | "openai_chat"
  | "openai_responses"
  | "anthropic"
  | "gemini";
export type ValidationMode = "direct" | "ccs";
export type ValidationProbe =
  | "call"
  | "stream"
  | "tools"
  | "structured"
  | "image"
  | "output_limit"
  | "cache"
  | "thinking"
  | "signature"
  | "cross_signature"
  | "comparison";

export interface ValidationTargetInput {
  appId: AppId;
  providerId: string;
  model: string;
  protocol?: ValidationProtocol;
}

export interface PrepareValidationRequest {
  target: ValidationTargetInput;
  mode: ValidationMode;
  probes: ValidationProbe[];
  comparisonTarget?: ValidationTargetInput;
  repeatCount?: number;
}

export interface ValidationTargetSummary {
  appId: AppId;
  providerId: string;
  providerName: string;
  endpoint: string;
  credentialLabel: string;
  model: string;
  protocol: ValidationProtocol;
}

export interface ValidationPlan {
  id: string;
  target: ValidationTargetSummary;
  mode: ValidationMode;
  probes: ValidationProbe[];
  maxRequests: number;
  maxOutputTokens: number;
  maxDurationSeconds: number;
  estimatedCostUsd: string | null;
  warnings: string[];
  expiresAt: string;
  comparisonTarget?: ValidationTargetSummary;
}

export type ValidationResultStatus =
  | "passed"
  | "failed"
  | "not_applicable"
  | "inconclusive"
  | "not_tested";

export interface ValidationProbeResult {
  probe: ValidationProbe;
  status: ValidationResultStatus;
  summary: string;
  evidence: Array<{ label: string; value: string }>;
  requestCount: number;
  durationMs: number;
}

export interface ValidationRun {
  id: string;
  plan: ValidationPlan;
  status: "running" | "completed" | "cancelled" | "failed" | "interrupted";
  startedAt: string;
  finishedAt?: string;
  results: ValidationProbeResult[];
}

export const modelValidationApi = {
  fetchModels(
    target: Omit<ValidationTargetInput, "model"> & { model?: string },
  ): Promise<FetchedModel[]> {
    return invoke("fetch_model_validation_models", { target });
  },
  prepare(request: PrepareValidationRequest): Promise<ValidationPlan> {
    return invoke("prepare_model_validation", { request });
  },
  start(planId: string): Promise<ValidationRun> {
    return invoke("start_model_validation", { planId });
  },
  get(runId: string): Promise<ValidationRun> {
    return invoke("get_model_validation", { runId });
  },
  list(
    appId?: AppId,
    providerId?: string,
    limit = 20,
  ): Promise<ValidationRun[]> {
    return invoke("list_model_validations", { appId, providerId, limit });
  },
  cancel(runId: string): Promise<boolean> {
    return invoke("cancel_model_validation", { runId });
  },
};
