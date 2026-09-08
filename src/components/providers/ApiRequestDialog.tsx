import { useEffect, useMemo, useRef, useState } from "react";
import {
  Copy,
  Eye,
  EyeOff,
  Loader2,
  Play,
  RefreshCw,
  Square,
  X,
} from "lucide-react";
import { useTranslation } from "react-i18next";
import { toast } from "sonner";
import type { Provider } from "@/types";
import type { AppId } from "@/lib/api";
import {
  cancelApiRequest,
  executeApiRequest,
  type ApiRequest,
  type ApiResponse,
  type ApiResponseHead,
} from "@/lib/api/api-request";
import { fetchModelsForConfig } from "@/lib/api/model-fetch";
import {
  API_REQUEST_ENDPOINTS,
  DEFAULT_API_PROMPT,
  buildApiRequest,
  buildCurlCommand,
  getRequestProviderConfig,
  maskApiRequest,
  type ApiProtocol,
  type CurlShell,
} from "@/utils/apiRequest";
import { extractErrorMessage } from "@/utils/errorUtils";
import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Textarea } from "@/components/ui/textarea";
import { Switch } from "@/components/ui/switch";
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/tabs";
import { ModelDropdown } from "@/components/providers/forms/shared/ModelDropdown";

interface Props {
  provider: Provider;
  appId: AppId;
  onClose: () => void;
}

const selectClass =
  "h-9 min-w-0 rounded-md border border-input bg-background px-3 text-sm focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring disabled:opacity-50";

export function ApiRequestDialog({ provider, appId, onClose }: Props) {
  const { t } = useTranslation();
  const initial = useMemo(() => {
    try {
      return { config: getRequestProviderConfig(provider, appId), error: "" };
    } catch (error) {
      return { config: null, error: extractErrorMessage(error) };
    }
  }, [provider, appId]);
  const config = initial.config;
  const [protocol, setProtocol] = useState<ApiProtocol>(
    config?.protocol ?? "chat-completions",
  );
  const [model, setModel] = useState(config?.model ?? "");
  const [models, setModels] = useState(config?.models ?? []);
  const [prompt, setPrompt] = useState(DEFAULT_API_PROMPT);
  const [stream, setStream] = useState(true);
  const [maxTokens, setMaxTokens] = useState(4096);
  const [timeoutSecs, setTimeoutSecs] = useState(120);
  const [shell, setShell] = useState<CurlShell>("bash");
  const [showSecrets, setShowSecrets] = useState(false);
  const [tab, setTab] = useState("request");
  const [fetchingModels, setFetchingModels] = useState(false);
  const [fetchError, setFetchError] = useState("");
  const [running, setRunning] = useState(false);
  const [canCancel, setCanCancel] = useState(false);
  const [error, setError] = useState("");
  const [head, setHead] = useState<ApiResponseHead | null>(null);
  const [response, setResponse] = useState<ApiResponse | null>(null);
  const [body, setBody] = useState("");
  const [lastRequest, setLastRequest] = useState<ApiRequest | null>(null);
  const requestId = useRef<string | null>(null);
  const mounted = useRef(true);

  useEffect(() => {
    mounted.current = true;
    return () => {
      mounted.current = false;
      if (requestId.current) {
        void cancelApiRequest(requestId.current).catch(() => {
          console.warn(
            "API request cancellation failed during dialog teardown",
          );
        });
      }
    };
  }, []);

  const draft = useMemo(() => {
    if (!config) return { request: null, error: initial.error };
    try {
      return {
        request: buildApiRequest(config, {
          protocol,
          model,
          prompt,
          stream,
          maxTokens,
          timeoutSecs,
        }),
        error: "",
      };
    } catch (error) {
      return { request: null, error: extractErrorMessage(error) };
    }
  }, [
    config,
    initial.error,
    protocol,
    model,
    prompt,
    stream,
    maxTokens,
    timeoutSecs,
  ]);

  const message = (text: string) =>
    text.startsWith("apiRequest.") ? t(text) : text;
  const curl = (request: ApiRequest) =>
    buildCurlCommand(showSecrets ? request : maskApiRequest(request), shell);
  const copy = async (text: string) => {
    try {
      await navigator.clipboard.writeText(text);
      toast.success(t("apiRequest.copied"));
    } catch {
      toast.error(t("apiRequest.copyFailed"));
    }
  };

  const fetchModels = async () => {
    if (!config) return;
    if (!config.apiKey || !config.baseUrl) {
      setFetchError("apiRequest.missingKey");
      return;
    }
    setFetchingModels(true);
    setFetchError("");
    try {
      const fetched = await fetchModelsForConfig(
        config.baseUrl,
        config.apiKey,
        config.isFullUrl,
        undefined,
        config.headers["user-agent"],
        {
          apiFormat:
            config.protocol === "messages"
              ? "anthropic-messages"
              : config.protocol === "gemini"
                ? "google-generative-ai"
                : "openai-completions",
          requestHeaders: config.headers,
        },
      );
      if (!mounted.current) return;
      setModels([
        ...new Map(
          [...fetched, ...config.models].map((item) => [item.id, item]),
        ).values(),
      ]);
      if (!model && fetched[0]) setModel(fetched[0].id);
      if (!fetched.length) setFetchError("apiRequest.noModels");
    } catch (error) {
      if (mounted.current) setFetchError(extractErrorMessage(error));
    } finally {
      if (mounted.current) setFetchingModels(false);
    }
  };

  const send = async () => {
    if (!draft.request || requestId.current) return;
    const id = crypto.randomUUID();
    const decoder = new TextDecoder();
    requestId.current = id;
    setRunning(true);
    setCanCancel(false);
    setError("");
    setHead(null);
    setResponse(null);
    setBody("");
    setLastRequest(draft.request);
    setTab("response");
    try {
      const result = await executeApiRequest(id, draft.request, (event) => {
        if (requestId.current !== id) return;
        if (!mounted.current) {
          if (event.type === "started")
            void cancelApiRequest(id).catch(console.error);
          return;
        }
        if (event.type === "started") setCanCancel(true);
        else if (event.type === "headers") setHead(event);
        else {
          // Decode once, outside React's replayable state updater.
          const chunk = decoder.decode(new Uint8Array(event.data), {
            stream: true,
          });
          setBody((previous) => previous + chunk);
        }
      });
      if (mounted.current) {
        setHead(result);
        setBody(result.body);
        setResponse(result);
        setError(result.error ?? "");
      }
    } catch (error) {
      if (mounted.current) {
        const remaining = decoder.decode();
        if (remaining) setBody((previous) => previous + remaining);
        setError(extractErrorMessage(error));
      }
    } finally {
      requestId.current = null;
      if (mounted.current) {
        setRunning(false);
        setCanCancel(false);
      }
    }
  };

  const cancel = async () => {
    if (!requestId.current) return;
    try {
      await cancelApiRequest(requestId.current);
      setCanCancel(false);
    } catch (error) {
      setError(extractErrorMessage(error));
    }
  };

  return (
    <Dialog
      open
      onOpenChange={(open) => {
        if (!open && !running) onClose();
      }}
    >
      <DialogContent
        className="w-[calc(100vw-2rem)] max-w-5xl overflow-hidden"
        onEscapeKeyDown={(event) => {
          if (running) event.preventDefault();
        }}
      >
        <DialogHeader className="relative py-4 pr-14">
          <DialogTitle>
            {t("apiRequest.title")} · {provider.name}
          </DialogTitle>
          <DialogDescription>{t("apiRequest.description")}</DialogDescription>
          <Button
            variant="ghost"
            size="icon"
            className="absolute right-3 top-3"
            aria-label={t("apiRequest.close")}
            disabled={running}
            onClick={onClose}
          >
            <X className="h-4 w-4" />
          </Button>
        </DialogHeader>
        <div className="min-h-0 space-y-3 overflow-y-auto px-6 py-3">
          <p className="break-all font-mono text-xs text-muted-foreground">
            {config?.baseUrl}
          </p>
          <fieldset
            disabled={running}
            className="grid min-w-0 gap-3 sm:grid-cols-2"
          >
            <div className="min-w-0 space-y-1.5">
              <Label htmlFor="api-request-protocol">
                {t("apiRequest.endpoint")}
              </Label>
              <select
                id="api-request-protocol"
                className={`${selectClass} w-full`}
                value={protocol}
                onChange={(event) =>
                  setProtocol(event.target.value as ApiProtocol)
                }
              >
                {API_REQUEST_ENDPOINTS.map((endpoint) => (
                  <option key={endpoint.id} value={endpoint.id}>
                    {endpoint.label}
                  </option>
                ))}
              </select>
            </div>
            <div className="min-w-0 space-y-1.5">
              <Label htmlFor="api-request-model">{t("apiRequest.model")}</Label>
              <div className="flex min-w-0 gap-1">
                <Input
                  id="api-request-model"
                  className="min-w-0"
                  value={model}
                  onChange={(event) => setModel(event.target.value)}
                  placeholder={t("apiRequest.modelPlaceholder")}
                />
                <ModelDropdown models={models} onSelect={setModel} />
                <Button
                  variant="outline"
                  size="icon"
                  className="shrink-0"
                  disabled={fetchingModels || !config?.apiKey}
                  aria-label={t("apiRequest.fetchModels")}
                  title={t("apiRequest.fetchModels")}
                  onClick={() => void fetchModels()}
                >
                  {fetchingModels ? (
                    <Loader2 className="h-4 w-4 animate-spin" />
                  ) : (
                    <RefreshCw className="h-4 w-4" />
                  )}
                </Button>
              </div>
              {fetchError && (
                <p
                  role="alert"
                  className="break-words text-xs text-destructive"
                >
                  {message(fetchError)}
                </p>
              )}
            </div>
            <div className="space-y-1.5 sm:col-span-2">
              <Label htmlFor="api-request-prompt">
                {t("apiRequest.prompt")}
              </Label>
              <Textarea
                id="api-request-prompt"
                className="min-h-[56px]"
                rows={2}
                value={prompt}
                onChange={(event) => setPrompt(event.target.value)}
              />
            </div>
            <div className="flex flex-wrap items-center gap-3 sm:col-span-2">
              <Switch
                id="api-request-stream"
                checked={stream}
                onCheckedChange={setStream}
                disabled={running}
              />
              <Label htmlFor="api-request-stream">
                {t("apiRequest.stream")}
              </Label>
              <Label htmlFor="api-request-timeout" className="sm:ml-auto">
                {t("apiRequest.timeout")}
              </Label>
              <Input
                id="api-request-timeout"
                type="number"
                min={1}
                max={600}
                className="w-24"
                value={timeoutSecs}
                onChange={(event) => setTimeoutSecs(Number(event.target.value))}
              />
              {protocol === "messages" && (
                <>
                  <Label htmlFor="api-request-max-tokens">max_tokens</Label>
                  <Input
                    id="api-request-max-tokens"
                    type="number"
                    min={1}
                    className="w-28"
                    value={maxTokens}
                    onChange={(event) =>
                      setMaxTokens(Number(event.target.value))
                    }
                  />
                </>
              )}
            </div>
          </fieldset>
          <Tabs value={tab} onValueChange={setTab}>
            <div className="flex flex-wrap items-center gap-2">
              <TabsList>
                <TabsTrigger value="request">
                  {t("apiRequest.request")}
                </TabsTrigger>
                <TabsTrigger value="response">
                  {t("apiRequest.response")}
                </TabsTrigger>
              </TabsList>
              <select
                aria-label={t("apiRequest.shell")}
                className={`${selectClass} ml-auto w-44 shrink-0`}
                value={shell}
                onChange={(event) => setShell(event.target.value as CurlShell)}
              >
                <option value="bash">Bash / zsh</option>
                <option value="powershell">PowerShell 7.3+</option>
              </select>
              <Button
                size="icon"
                variant="ghost"
                aria-label={t(
                  showSecrets ? "apiRequest.hideKey" : "apiRequest.showKey",
                )}
                title={t(
                  showSecrets ? "apiRequest.hideKey" : "apiRequest.showKey",
                )}
                onClick={() => setShowSecrets(!showSecrets)}
              >
                {showSecrets ? (
                  <EyeOff className="h-4 w-4" />
                ) : (
                  <Eye className="h-4 w-4" />
                )}
              </Button>
            </div>
            <TabsContent value="request" className="space-y-2">
              {draft.request ? (
                <pre
                  aria-label={t("apiRequest.request")}
                  className="max-h-80 overflow-auto rounded-md border bg-muted/30 p-3 font-mono text-xs whitespace-pre-wrap break-all"
                >
                  {curl(draft.request)}
                </pre>
              ) : (
                <p role="alert" className="text-sm text-destructive">
                  {message(draft.error)}
                </p>
              )}
              <p className="text-xs text-muted-foreground">
                {t("apiRequest.copyIncludesKey")}
              </p>
              {config?.isFullUrl && (
                <p className="text-xs text-muted-foreground">
                  {t("apiRequest.fullUrlHint")}
                </p>
              )}
            </TabsContent>
            <TabsContent value="response" className="space-y-3">
              <div role="status" className="flex flex-wrap gap-3 text-sm">
                {head && (
                  <span
                    className={
                      head.status >= 400 ? "text-destructive" : "font-medium"
                    }
                  >
                    HTTP {head.status}
                  </span>
                )}
                {running ? (
                  <span className="flex items-center gap-1.5">
                    <Loader2 className="h-3.5 w-3.5 animate-spin" />
                    {t(head ? "apiRequest.receiving" : "apiRequest.connecting")}
                  </span>
                ) : (
                  response && (
                    <span>
                      {t(
                        response.complete
                          ? "apiRequest.complete"
                          : "apiRequest.incomplete",
                      )}{" "}
                      · {response.durationMs} ms
                    </span>
                  )
                )}
                {!running && !response && error && (
                  <span>{t("apiRequest.incomplete")}</span>
                )}
              </div>
              {error && (
                <p
                  role="alert"
                  className="break-words text-sm text-destructive"
                >
                  {message(error)}
                </p>
              )}
              <pre
                aria-label={t("apiRequest.response")}
                className="min-h-32 max-h-80 overflow-auto rounded-md border bg-muted/30 p-3 font-mono text-xs whitespace-pre-wrap break-all"
              >
                {body ||
                  t(
                    running
                      ? "apiRequest.waiting"
                      : head
                        ? "apiRequest.emptyResponse"
                        : "apiRequest.noResponse",
                  )}
              </pre>
              {head && (
                <details>
                  <summary className="cursor-pointer text-sm">
                    {t("apiRequest.responseHeaders")}
                  </summary>
                  <pre className="mt-2 overflow-auto font-mono text-xs whitespace-pre-wrap break-all">
                    {head.headers
                      .map(([name, value]) => `${name}: ${value}`)
                      .join("\n")}
                  </pre>
                </details>
              )}
              {lastRequest && (
                <details>
                  <summary className="cursor-pointer text-sm">
                    {t("apiRequest.sentRequest")}
                  </summary>
                  <pre className="mt-2 overflow-auto font-mono text-xs whitespace-pre-wrap break-all">
                    {curl(lastRequest)}
                  </pre>
                </details>
              )}
            </TabsContent>
          </Tabs>
        </div>
        <div className="flex flex-wrap items-center justify-end gap-2 border-t px-6 py-4">
          <Button
            variant="outline"
            disabled={!draft.request}
            onClick={() =>
              draft.request && void copy(buildCurlCommand(draft.request, shell))
            }
          >
            <Copy className="mr-1.5 h-4 w-4" />
            {t("apiRequest.copyCurl")}
          </Button>
          <Button
            variant="outline"
            disabled={!body || running}
            onClick={() => void copy(body)}
          >
            {t("apiRequest.copyResponse")}
          </Button>
          {running ? (
            <Button
              variant="destructive"
              disabled={!canCancel}
              onClick={() => void cancel()}
            >
              <Square className="mr-1.5 h-4 w-4" />
              {t("apiRequest.cancel")}
            </Button>
          ) : (
            <Button disabled={!draft.request} onClick={() => void send()}>
              <Play className="mr-1.5 h-4 w-4" />
              {t("apiRequest.send")}
            </Button>
          )}
        </div>
      </DialogContent>
    </Dialog>
  );
}
