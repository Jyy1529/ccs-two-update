import { useEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { Download, Loader2 } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import type { AppId } from "@/lib/api/types";
import type { FetchedModel } from "@/lib/api/model-fetch";
import {
  modelValidationApi,
  type ValidationProtocol,
} from "@/lib/api/modelValidation";
import type { Provider } from "@/types";
import { extractErrorMessage } from "@/utils/errorUtils";
import { ModelDropdown } from "./forms/shared/ModelDropdown";

export function ValidationModelPicker({
  id,
  appId,
  provider,
  protocol,
  value,
  onChange,
  disabled,
  enabled,
}: {
  id: string;
  appId: AppId;
  provider?: Provider;
  protocol: ValidationProtocol | "auto";
  value: string;
  onChange: (value: string) => void;
  disabled: boolean;
  enabled: boolean;
}) {
  const { t } = useTranslation();
  const [models, setModels] = useState<FetchedModel[]>([]);
  const [loading, setLoading] = useState(false);
  const [loaded, setLoaded] = useState(false);
  const [error, setError] = useState("");
  const generation = useRef(0);
  useEffect(() => {
    generation.current += 1;
    setModels([]);
    setLoading(false);
    setLoaded(false);
    setError("");
    return () => {
      generation.current += 1;
    };
  }, [
    appId,
    provider?.id,
    provider?.settingsConfig,
    provider?.meta,
    protocol,
    enabled,
  ]);

  const fetchModels = async () => {
    if (!provider || !enabled || disabled || loading) return;
    const request = ++generation.current;
    setLoading(true);
    setError("");
    setLoaded(false);
    setModels([]);
    try {
      const fetched = await modelValidationApi.fetchModels({
        appId,
        providerId: provider.id,
        ...(value.trim() ? { model: value.trim() } : {}),
        ...(protocol === "auto" ? {} : { protocol }),
      });
      if (request === generation.current) {
        setModels(fetched);
        setLoaded(true);
      }
    } catch (cause) {
      if (request === generation.current) setError(extractErrorMessage(cause));
    } finally {
      if (request === generation.current) setLoading(false);
    }
  };

  return (
    <div className="space-y-2">
      <div className="flex gap-1">
        <Input
          id={id}
          value={value}
          disabled={disabled || loading || !provider}
          onChange={(event) => onChange(event.target.value)}
          placeholder={t("modelValidation.modelPlaceholder")}
          autoComplete="off"
          className="min-w-0 flex-1"
        />
        {models.length > 0 && !disabled && !loading && (
          <ModelDropdown models={models} onSelect={onChange} />
        )}
        <Button
          type="button"
          variant="outline"
          className="shrink-0"
          disabled={disabled || loading || !provider}
          onClick={() => void fetchModels()}
        >
          {loading ? (
            <Loader2 className="h-4 w-4 animate-spin" />
          ) : (
            <Download className="h-4 w-4" />
          )}
          {t(
            loaded
              ? "modelValidation.refreshModels"
              : "providerForm.fetchModels",
          )}
        </Button>
      </div>
      <p className="text-xs text-muted-foreground">
        {t("modelValidation.fetchModelsHint")}
      </p>
      {loaded && (
        <p role="status" className="text-xs text-muted-foreground">
          {t(
            models.length
              ? "modelValidation.modelsLoaded"
              : "modelValidation.modelsEmpty",
            { count: models.length },
          )}
        </p>
      )}
      {error && (
        <p role="alert" className="text-xs text-destructive">
          {t("modelValidation.fetchModelsFallback")} {error}
        </p>
      )}
    </div>
  );
}
