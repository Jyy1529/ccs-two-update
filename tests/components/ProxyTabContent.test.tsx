import { screen, waitFor } from "@testing-library/react";
import { renderManagedUi as render } from "../utils/safetyTestUtils";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";
import { ProxyTabContent } from "@/components/settings/ProxyTabContent";
import type { SettingsFormState } from "@/hooks/useSettings";

vi.mock("react-i18next", () => ({
  useTranslation: () => ({
    t: (key: string, options?: { defaultValue?: string }) =>
      options?.defaultValue ?? key,
  }),
}));

vi.mock("@/hooks/useProxyStatus", () => ({
  useProxyStatus: () => ({
    isRunning: false,
    takeoverStatus: {},
    startProxyServer: vi.fn(),
    stopWithRestore: vi.fn(),
    isPending: false,
  }),
}));

vi.mock("@/components/proxy", () => ({
  ProxyPanel: () => null,
}));
vi.mock("@/components/proxy/AutoFailoverConfigPanel", () => ({
  AutoFailoverConfigPanel: () => null,
}));
vi.mock("@/components/proxy/FailoverQueueManager", () => ({
  FailoverQueueManager: () => null,
}));
vi.mock("@/components/settings/RectifierConfigPanel", () => ({
  RectifierConfigPanel: () => null,
}));
vi.mock("@/components/settings/GlobalProxySettings", () => ({
  GlobalProxySettings: () => null,
}));
vi.mock("@/components/ConfirmDialog", () => ({
  ConfirmDialog: () => null,
}));

describe("ProxyTabContent provider retry switch", () => {
  it("defaults to enabled and auto-saves both switch states", async () => {
    const user = userEvent.setup();
    const onAutoSave = vi.fn().mockResolvedValue(true);
    const { rerender } = render(
      <ProxyTabContent
        settings={{ language: "en" } as SettingsFormState}
        onAutoSave={onAutoSave}
      />,
    );

    await user.click(
      screen.getByRole("button", {
        name: /settings\.advanced\.proxy\.title/,
      }),
    );

    const retrySwitch = await screen.findByRole("switch", {
      name: "Enable Provider automatic retry",
    });
    expect(retrySwitch).toBeChecked();

    await user.click(retrySwitch);
    await waitFor(() =>
      expect(onAutoSave).toHaveBeenCalledWith({ providerRetryEnabled: false }),
    );

    onAutoSave.mockClear();
    rerender(
      <ProxyTabContent
        settings={
          {
            language: "en",
            providerRetryEnabled: false,
          } as SettingsFormState
        }
        onAutoSave={onAutoSave}
      />,
    );

    expect(retrySwitch).not.toBeChecked();
    await user.click(retrySwitch);
    await waitFor(() =>
      expect(onAutoSave).toHaveBeenCalledWith({ providerRetryEnabled: true }),
    );
  });
});
