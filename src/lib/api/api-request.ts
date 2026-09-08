import { Channel, invoke } from "@tauri-apps/api/core";

export interface ApiRequest {
  url: string;
  headers: Record<string, string>;
  body: string;
  timeoutSecs: number;
}

export interface ApiResponseHead {
  status: number;
  headers: [string, string][];
}

export interface ApiResponse extends ApiResponseHead {
  body: string;
  durationMs: number;
  complete: boolean;
  error: string | null;
}

export type ApiRequestEvent =
  | { type: "started" }
  | ({ type: "headers" } & ApiResponseHead)
  | { type: "chunk"; data: number[] };

export function executeApiRequest(
  requestId: string,
  request: ApiRequest,
  onEvent: (event: ApiRequestEvent) => void,
): Promise<ApiResponse> {
  const onData = new Channel<ApiRequestEvent>();
  onData.onmessage = onEvent;
  return invoke("execute_api_request", { requestId, request, onData });
}

export function cancelApiRequest(requestId: string): Promise<void> {
  return invoke("cancel_api_request", { requestId });
}
