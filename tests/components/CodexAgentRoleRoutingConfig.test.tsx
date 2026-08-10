import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { useState } from "react";
import { describe, expect, it, vi } from "vitest";
import {
  CodexAgentRoleRoutingConfig,
  defaultCodexAgentRoleRouting,
} from "@/components/providers/forms/CodexAgentRoleRoutingConfig";
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
      config: 'model = "frontend-pro"',
      modelCatalog: {
        models: [{ model: "frontend-pro" }, { model: "frontend-fast" }],
      },
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

function Harness({
  initial = defaultCodexAgentRoleRouting(),
  onRequestAddProvider,
}: {
  initial?: CodexAgentRoleRouting;
  onRequestAddProvider?: (onCreated: (providerId: string) => void) => void;
}) {
  const [value, setValue] = useState(initial);
  return (
    <CodexAgentRoleRoutingConfig
      value={value}
      onChange={setValue}
      ownerProviderId="provider-a"
      ownerDefaultModel="gpt-a"
      ownerCatalogModels={[{ model: "gpt-a" }]}
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

    await user.click(header);

    expect(
      screen.getByLabelText("Enable agent role routing"),
    ).not.toBeChecked();
    expect(screen.getByLabelText("Frontend provider")).toHaveTextContent(
      "Follow current provider",
    );
  });

  it("shows target provider model candidates and warns for unknown capability metadata", async () => {
    const user = userEvent.setup();
    render(
      <Harness
        initial={{
          enabled: true,
          frontend: {
            providerId: "provider-b",
            model: "frontend-fast",
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
    expect(screen.getByLabelText("Frontend upstream model")).toHaveAttribute(
      "list",
      "codex-role-upstream-models",
    );
    expect(
      document.querySelector(
        '#codex-role-upstream-models option[value="frontend-fast"]',
      ),
    ).toBeInTheDocument();
    expect(screen.getByRole("alert")).toHaveTextContent(
      "fallback model metadata",
    );
  });

  it("keeps an unavailable provider visible and selects a newly created provider", async () => {
    const user = userEvent.setup();
    const requestAdd = vi.fn((onCreated: (providerId: string) => void) => {
      onCreated("provider-b");
    });
    render(
      <Harness
        initial={{
          enabled: true,
          frontend: { providerId: "missing-provider" },
        }}
        onRequestAddProvider={requestAdd}
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

    await user.click(screen.getByRole("button", { name: "Add provider" }));

    expect(requestAdd).toHaveBeenCalledTimes(1);
    expect(screen.getByLabelText("Frontend provider")).toHaveTextContent(
      "Provider B",
    );
  });
});
