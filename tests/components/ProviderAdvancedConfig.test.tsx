import { useState } from "react";
import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";
import {
  ProviderAdvancedConfig,
  ProviderAdvancedOptionsSection,
  type PricingModelSourceOption,
} from "@/components/providers/forms/ProviderAdvancedConfig";
import { defaultLocalProxyRetryPolicy } from "@/components/providers/forms/ProviderRetryPolicyConfig";

vi.mock("@/components/providers/forms/CodexAgentRoleRoutingConfig", () => ({
  CodexAgentRoleRoutingConfig: ({
    onRequestAddProvider,
  }: {
    onRequestAddProvider?: (onCreated: (providerId: string) => void) => void;
  }) => (
    <button
      type="button"
      onClick={() => onRequestAddProvider?.(() => undefined)}
    >
      request-role-provider
    </button>
  ),
}));

function Harness() {
  const [pricingConfig, setPricingConfig] = useState<{
    enabled: boolean;
    costMultiplier?: string;
    pricingModelSource: PricingModelSourceOption;
  }>({
    enabled: false,
    pricingModelSource: "inherit",
  });
  const [retryPolicy, setRetryPolicy] = useState(
    defaultLocalProxyRetryPolicy(),
  );

  return (
    <ProviderAdvancedOptionsSection>
      <ProviderAdvancedConfig
        pricingConfig={pricingConfig}
        onPricingConfigChange={setPricingConfig}
        retryPolicy={retryPolicy}
        onRetryPolicyChange={setRetryPolicy}
      />
    </ProviderAdvancedOptionsSection>
  );
}

describe("ProviderAdvancedConfig", () => {
  it("keeps retry and pricing under a collapsed Advanced Options section", async () => {
    const user = userEvent.setup();
    render(<Harness />);

    const advancedButton = screen.getByRole("button", {
      name: "Advanced Options",
    });
    expect(advancedButton).toHaveAttribute("aria-expanded", "false");
    expect(
      screen.queryByRole("button", {
        name: "Local proxy automatic retry",
      }),
    ).not.toBeInTheDocument();

    await user.click(advancedButton);

    const retryButton = screen.getByRole("button", {
      name: "Local proxy automatic retry",
    });
    expect(retryButton).toHaveAttribute("aria-expanded", "false");
    expect(
      screen.queryByRole("spinbutton", { name: "Additional retries" }),
    ).not.toBeInTheDocument();

    const pricingHeader = screen
      .getByText("Pricing configuration")
      .closest('[role="button"]');
    expect(pricingHeader).toHaveAttribute("aria-expanded", "false");

    await user.click(retryButton);
    expect(screen.getByLabelText("Additional retries")).toHaveValue(0);
  });

  it("expands pricing exactly once when its switch receives keyboard input", async () => {
    const user = userEvent.setup();
    render(<Harness />);

    await user.click(
      screen.getByRole("button", {
        name: "Advanced Options",
      }),
    );
    const pricingHeader = screen
      .getByText("Pricing configuration")
      .closest('[role="button"]');
    const pricingSwitch = screen.getByRole("switch", {
      name: "Use separate configuration",
    });

    expect(pricingHeader).toHaveAttribute("aria-expanded", "false");
    pricingSwitch.focus();
    await user.keyboard(" ");
    expect(pricingHeader).toHaveAttribute("aria-expanded", "true");
  });

  it("passes the add-provider request callback to role routing", async () => {
    const user = userEvent.setup();
    const onRequestAddProvider = vi.fn();

    render(
      <ProviderAdvancedConfig
        pricingConfig={{ enabled: false, pricingModelSource: "inherit" }}
        onPricingConfigChange={vi.fn()}
        codexAgentRoleRouting={{ enabled: false }}
        onCodexAgentRoleRoutingChange={vi.fn()}
        onRequestAddProvider={onRequestAddProvider}
      />,
    );

    await user.click(
      screen.getByRole("button", { name: "request-role-provider" }),
    );
    expect(onRequestAddProvider).toHaveBeenCalledTimes(1);
  });
});
