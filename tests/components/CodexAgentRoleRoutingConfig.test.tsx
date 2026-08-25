import { act, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { useState } from "react";
import { beforeAll, describe, expect, it, vi } from "vitest";
import {
  CodexAgentRoleRoutingConfig,
  defaultCodexAgentRoleRouting,
} from "@/components/providers/forms/CodexAgentRoleRoutingConfig";
import { fetchModelsForConfig } from "@/lib/api/model-fetch";
import type { CodexAgentRoleRouting, Provider } from "@/types";

const providers: Record<string, Provider> = {
  "provider-a": {
    id: "provider-a",
    name: "Provider A",
    settingsConfig: {
      config: 'model = "gpt-a"',
      modelCatalog: { models: [{ model: "gpt-a" }] },
    },
  },
  "provider-b": {
    id: "provider-b",
    name: "Provider B",
    settingsConfig: {
      auth: { OPENAI_API_KEY: "provider-b-key" },
      config:
        'base_url = "https://provider-b.example/v1"\nmodel = "frontend-pro"',
      modelCatalog: {
        models: [{ model: "frontend-pro" }, { model: "frontend-fast" }],
      },
    },
  },
  "provider-c": {
    id: "provider-c",
    name: "Provider C",
    settingsConfig: {
      auth: { OPENAI_API_KEY: "provider-c-key" },
      config:
        'base_url = "https://provider-c.example/v1"\nmodel = "frontend-c"',
      modelCatalog: { models: [{ model: "frontend-c" }] },
    },
  },
};

vi.mock("@/lib/query", () => ({
  useProvidersQuery: () => ({
    data: { providers, currentProviderId: "provider-a" },
    isLoading: false,
  }),
}));

vi.mock("@/lib/api/model-fetch", () => ({
  fetchModelsForConfig: vi.fn(async () => []),
  showFetchModelsError: vi.fn(),
}));

beforeAll(() => {
  Element.prototype.scrollIntoView = vi.fn();
});

function Harness({
  initial = defaultCodexAgentRoleRouting(),
  onRequestAddProvider,
  ownerBaseUrl,
  ownerApiKey,
}: {
  initial?: CodexAgentRoleRouting;
  onRequestAddProvider?: (onCreated: (providerId: string) => void) => void;
  ownerBaseUrl?: string;
  ownerApiKey?: string;
}) {
  const [value, setValue] = useState(initial);
  return (
    <CodexAgentRoleRoutingConfig
      value={value}
      onChange={setValue}
      ownerProviderId="provider-a"
      ownerDefaultModel="gpt-a"
      ownerCatalogModels={[{ model: "gpt-a" }]}
      ownerBaseUrl={ownerBaseUrl}
      ownerApiKey={ownerApiKey}
      onRequestAddProvider={onRequestAddProvider}
    />
  );
}

describe("CodexAgentRoleRoutingConfig", () => {
  it("starts collapsed with routing disabled", async () => {
    const user = userEvent.setup();
    render(<Harness />);

    const header = screen.getByRole("button", {
      name: /Frontend\/backend subagent model routing/i,
    });
    expect(header).toHaveAttribute("aria-expanded", "false");
    expect(
      screen.queryByLabelText("Frontend provider"),
    ).not.toBeInTheDocument();

    await user.click(header);

    const enabledSwitch = screen.getByLabelText("Enable agent role routing");
    expect(enabledSwitch).not.toBeChecked();
    expect(screen.getByLabelText("Frontend provider")).toHaveTextContent(
      "Follow current provider",
    );

    enabledSwitch.focus();
    await user.keyboard(" ");
    expect(enabledSwitch).toBeChecked();
    expect(header).toHaveAttribute("aria-expanded", "true");
  });

  it("selects a provider returned by the add-provider callback", async () => {
    const user = userEvent.setup();
    let notifyCreated: ((providerId: string) => void) | undefined;
    const onRequestAddProvider = vi.fn(
      (onCreated: (providerId: string) => void) => {
        notifyCreated = onCreated;
      },
    );
    render(<Harness onRequestAddProvider={onRequestAddProvider} />);

    await user.click(
      screen.getByRole("button", {
        name: /Frontend\/backend subagent model routing/i,
      }),
    );
    await user.click(screen.getByRole("button", { name: "Add provider" }));

    expect(onRequestAddProvider).toHaveBeenCalledTimes(1);
    expect(notifyCreated).toBeDefined();
    act(() => notifyCreated?.("provider-c"));
    expect(screen.getByLabelText("Frontend provider")).toHaveTextContent(
      "Provider C",
    );
  });

  it("excludes the current provider and exposes target provider model candidates", async () => {
    const user = userEvent.setup();
    render(
      <Harness
        initial={{
          enabled: true,
          frontend: {
            providerId: "provider-b",
            upstreamModel: "frontend-fast",
          },
        }}
      />,
    );

    await user.click(
      screen.getByRole("button", {
        name: /Frontend\/backend subagent model routing/i,
      }),
    );

    expect(screen.getByLabelText("Frontend provider")).toHaveTextContent(
      "Provider B",
    );
    await user.click(screen.getByLabelText("Frontend provider"));
    expect(screen.getByRole("option", { name: "Provider B" })).toBeVisible();
    expect(
      screen.queryByRole("option", { name: "Provider A" }),
    ).not.toBeInTheDocument();
    await user.keyboard("{Escape}");
    expect(screen.getByLabelText("Frontend upstream model")).toHaveAttribute(
      "list",
      "codex-role-upstream-models",
    );
    expect(
      document.querySelector(
        '#codex-role-upstream-models option[value="frontend-fast"]',
      ),
    ).toBeInTheDocument();

    const upstreamModel = screen.getByLabelText("Frontend upstream model");
    await user.clear(upstreamModel);
    await user.type(upstreamModel, "unlisted-upstream-model");
    expect(upstreamModel).toHaveValue("unlisted-upstream-model");
  });

  it("keeps unavailable providers visible and blank capability settings inherited", async () => {
    const user = userEvent.setup();
    render(
      <Harness
        initial={{
          enabled: true,
          frontend: { providerId: "missing-provider" },
        }}
      />,
    );

    await user.click(
      screen.getByRole("button", {
        name: /Frontend\/backend subagent model routing/i,
      }),
    );

    expect(screen.getByLabelText("Frontend provider")).toHaveTextContent(
      "Unavailable provider",
    );
    expect(screen.getByLabelText("Codex capability model")).toHaveValue("");
    expect(screen.getByLabelText("Backend capability model")).toHaveValue("");
    for (const control of screen.getAllByLabelText("Reasoning effort")) {
      expect(control).toHaveTextContent("Follow main agent");
    }
  });

  it("uses neutral wording when a capability model is absent from visible provider data", async () => {
    const user = userEvent.setup();
    render(
      <Harness
        initial={{
          enabled: true,
          frontend: { model: "user-managed-catalog-model" },
        }}
      />,
    );

    await user.click(
      screen.getByRole("button", {
        name: /Frontend\/backend subagent model routing/i,
      }),
    );

    expect(
      screen.getByText(
        "This model is not listed in the provider data shown here. The active shared catalog may include additional user-managed models.",
      ),
    ).toHaveClass("text-muted-foreground");
    expect(
      screen.queryByText(/fallback model metadata/i),
    ).not.toBeInTheDocument();
    expect(screen.queryByRole("alert")).not.toBeInTheDocument();
  });

  it("ignores model results from a provider that is no longer selected", async () => {
    const user = userEvent.setup();
    let resolveFetch: ((value: Array<{ id: string }>) => void) | undefined;
    vi.mocked(fetchModelsForConfig).mockImplementationOnce(
      () =>
        new Promise((resolve) => {
          resolveFetch = resolve as (value: Array<{ id: string }>) => void;
        }),
    );

    render(
      <Harness
        initial={{
          enabled: true,
          frontend: { providerId: "provider-b" },
        }}
      />,
    );
    await user.click(
      screen.getByRole("button", {
        name: /Frontend\/backend subagent model routing/i,
      }),
    );
    await user.click(screen.getByRole("button", { name: "Fetch models" }));
    expect(fetchModelsForConfig).toHaveBeenCalled();
    expect(resolveFetch).toBeDefined();

    await user.click(screen.getByLabelText("Frontend provider"));
    await user.click(screen.getByRole("option", { name: "Provider C" }));
    resolveFetch?.([{ id: "stale-provider-b-model" }]);

    await waitFor(() => {
      expect(screen.getByLabelText("Frontend provider")).toHaveTextContent(
        "Provider C",
      );
    });
    expect(
      document.querySelector(
        '#codex-role-upstream-models option[value="stale-provider-b-model"]',
      ),
    ).not.toBeInTheDocument();
  });

  it("ignores model results after the followed provider credentials change", async () => {
    const user = userEvent.setup();
    let resolveFetch: ((value: Array<{ id: string }>) => void) | undefined;
    vi.mocked(fetchModelsForConfig).mockImplementationOnce(
      () =>
        new Promise((resolve) => {
          resolveFetch = resolve as (value: Array<{ id: string }>) => void;
        }),
    );

    const { rerender } = render(
      <Harness
        ownerBaseUrl="https://owner-old.example/v1"
        ownerApiKey="owner-old-key"
      />,
    );
    await user.click(
      screen.getByRole("button", {
        name: /Frontend\/backend subagent model routing/i,
      }),
    );
    await user.click(screen.getByRole("button", { name: "Fetch models" }));
    expect(fetchModelsForConfig).toHaveBeenCalledWith(
      "https://owner-old.example/v1",
      "owner-old-key",
      false,
      undefined,
      undefined,
    );
    expect(resolveFetch).toBeDefined();

    rerender(
      <Harness
        ownerBaseUrl="https://owner-new.example/v1"
        ownerApiKey="owner-new-key"
      />,
    );
    resolveFetch?.([{ id: "stale-owner-model" }]);

    await waitFor(() => {
      expect(
        document.querySelector(
          '#codex-role-upstream-models option[value="stale-owner-model"]',
        ),
      ).not.toBeInTheDocument();
    });
  });
});
