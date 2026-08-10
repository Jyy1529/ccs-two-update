import { fireEvent, render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { useState } from "react";
import { describe, expect, it } from "vitest";
import {
  defaultLocalProxyRetryPolicy,
  normalizeLocalProxyRetryPolicy,
  ProviderRetryPolicyConfig,
} from "@/components/providers/forms/ProviderRetryPolicyConfig";
import type { LocalProxyRetryPolicy } from "@/types";

function Harness({ initial }: { initial?: LocalProxyRetryPolicy }) {
  const [policy, setPolicy] = useState(
    initial ?? defaultLocalProxyRetryPolicy(),
  );
  return <ProviderRetryPolicyConfig value={policy} onChange={setPolicy} />;
}

describe("ProviderRetryPolicyConfig", () => {
  it("shows the disabled default policy and all four preset error types", () => {
    render(<Harness />);

    expect(screen.getByLabelText("Additional retries")).toHaveValue(0);
    expect(screen.getByLabelText("Retry interval (ms)")).toHaveValue(1000);
    expect(screen.getByLabelText("Error message contains")).toHaveValue(
      "We're currently experiencing high demand, which may cause temporary errors.",
    );
    expect(screen.getByLabelText("Rate limit (HTTP 429)")).not.toBeChecked();
    expect(screen.getByLabelText("Overloaded (HTTP 503)")).not.toBeChecked();
    expect(
      screen.getByLabelText("Other server errors (HTTP 5xx)"),
    ).not.toBeChecked();
    expect(
      screen.getByLabelText("Network and timeout errors"),
    ).not.toBeChecked();
  });

  it("requires a trigger when retries are enabled", async () => {
    const user = userEvent.setup();
    render(<Harness />);

    fireEvent.change(screen.getByLabelText("Additional retries"), {
      target: { value: "2" },
    });
    await user.clear(screen.getByLabelText("Error message contains"));

    expect(screen.getByRole("alert")).toHaveTextContent(
      "Choose at least one error type or enter an error message",
    );

    await user.click(screen.getByLabelText("Rate limit (HTTP 429)"));
    expect(screen.queryByRole("alert")).not.toBeInTheDocument();
  });

  it("reports numeric bounds and normalizes message lines", () => {
    const policy: LocalProxyRetryPolicy = {
      maxRetries: 51,
      retryDelayMs: 0,
      customMessages: [" Busy ", "busy", "", "Other"],
      errorTypes: ["network", "network"],
    };
    const normalized = normalizeLocalProxyRetryPolicy(policy);

    expect(normalized).toEqual({
      maxRetries: 50,
      retryDelayMs: 1,
      customMessages: ["Busy", "Other"],
      errorTypes: ["network"],
    });

    const { unmount } = render(<Harness initial={policy} />);
    expect(screen.getByRole("alert")).toHaveTextContent("0 to 50");

    unmount();
    render(
      <Harness
        initial={{
          ...policy,
          maxRetries: 1,
          retryDelayMs: 60_001,
        }}
      />,
    );
    expect(screen.getByRole("alert")).toHaveTextContent("1 to 60000 ms");
  });
});
