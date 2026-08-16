import { fireEvent, render, screen, within } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { ProviderFeatureScopeSettings } from "@/components/settings/ProviderFeatureScopeSettings";
import type { SettingsFormState } from "@/hooks/useSettings";

vi.mock("react-i18next", () => ({
  useTranslation: () => ({ t: (key: string) => key }),
}));

vi.mock("@/components/ProviderIcon", () => ({
  ProviderIcon: () => null,
}));

const settings: SettingsFormState = {
  showInTray: true,
  minimizeToTrayOnClose: true,
  language: "en",
  providerFeatureScopes: {
    localProxyRetry: { enabled: true, apps: ["claude", "codex"] },
    agentRoleRouting: { enabled: true, apps: ["codex"] },
    autoReviewRouting: { enabled: true, apps: ["codex"] },
  },
};

function featureCard(titleKey: string): HTMLElement {
  const card = screen.getByText(titleKey).closest(".rounded-md");
  if (!(card instanceof HTMLElement)) {
    throw new Error(`Feature card not found: ${titleKey}`);
  }
  return card;
}

describe("ProviderFeatureScopeSettings", () => {
  it("renders all feature rows and limits role routing to Codex", () => {
    render(
      <ProviderFeatureScopeSettings settings={settings} onChange={vi.fn()} />,
    );

    expect(
      screen.getByText("settings.featureScopes.localProxyRetry.title"),
    ).toBeInTheDocument();
    expect(
      screen.getByText("settings.featureScopes.agentRoleRouting.title"),
    ).toBeInTheDocument();
    expect(
      screen.getByText("settings.featureScopes.autoReviewRouting.title"),
    ).toBeInTheDocument();

    const roleCard = featureCard(
      "settings.featureScopes.agentRoleRouting.title",
    );
    expect(within(roleCard).getAllByRole("button")).toHaveLength(1);
    expect(
      within(roleCard).getByRole("button", { name: "apps.codex" }),
    ).toBeInTheDocument();
    expect(
      within(roleCard).getByRole("switch", {
        name: "settings.featureScopes.agentRoleRouting.title",
      }),
    ).toBeChecked();
  });

  it("disables local proxy retry through the feature switch", () => {
    const onChange = vi.fn();
    render(
      <ProviderFeatureScopeSettings settings={settings} onChange={onChange} />,
    );

    fireEvent.click(
      within(
        featureCard("settings.featureScopes.localProxyRetry.title"),
      ).getByRole("switch"),
    );

    expect(onChange).toHaveBeenCalledWith({
      providerFeatureScopes: {
        ...settings.providerFeatureScopes,
        localProxyRetry: {
          ...settings.providerFeatureScopes!.localProxyRetry,
          enabled: false,
        },
      },
    });
  });

  it("removes Claude from the local proxy retry scope", () => {
    const onChange = vi.fn();
    render(
      <ProviderFeatureScopeSettings settings={settings} onChange={onChange} />,
    );

    fireEvent.click(
      within(
        featureCard("settings.featureScopes.localProxyRetry.title"),
      ).getByRole("button", { name: "apps.claudeCode" }),
    );

    const update = onChange.mock.calls[0][0];
    expect(update.providerFeatureScopes.localProxyRetry.apps).not.toContain(
      "claude",
    );
  });
});
