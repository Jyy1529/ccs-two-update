import { act, fireEvent, render, screen } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { BalanceQueryTemplateDialog } from "@/components/providers/BalanceQueryTemplateDialog";
import { providerGroupsApi } from "@/lib/api/providerGroups";
import type { UsageResult } from "@/types";

describe("BalanceQueryTemplateDialog", () => {
  it("submits a valid template with parsed headers and query", () => {
    const onSubmit = vi.fn();
    render(
      <BalanceQueryTemplateDialog
        open
        onOpenChange={vi.fn()}
        onSubmit={onSubmit}
      />,
    );

    fireEvent.change(screen.getByLabelText("Name"), {
      target: { value: "Relay balance" },
    });
    fireEvent.change(screen.getByLabelText("Headers JSON"), {
      target: { value: '{"Authorization":"Bearer {{apiKey}}"}' },
    });
    fireEvent.click(screen.getByRole("button", { name: "Save template" }));

    expect(onSubmit).toHaveBeenCalledWith(
      expect.objectContaining({
        name: "Relay balance",
        headers: { Authorization: "Bearer {{apiKey}}" },
        query: {},
      }),
    );
  });

  it("rejects malformed JSON without submitting", () => {
    const onSubmit = vi.fn();
    render(
      <BalanceQueryTemplateDialog
        open
        onOpenChange={vi.fn()}
        onSubmit={onSubmit}
      />,
    );

    fireEvent.change(screen.getByLabelText("Headers JSON"), {
      target: { value: "{" },
    });
    fireEvent.click(screen.getByRole("button", { name: "Save template" }));

    expect(onSubmit).not.toHaveBeenCalled();
    expect(screen.getByRole("alert")).toBeInTheDocument();
  });

  it("disables template fields while saving without disabling cancellation", () => {
    const onOpenChange = vi.fn();
    const props = { onOpenChange, onSubmit: vi.fn() };
    const { rerender } = render(
      <BalanceQueryTemplateDialog open pending {...props} />,
    );

    for (const label of ["Name", "Base URL", "Temporary API Key"]) {
      expect(screen.getByLabelText(label)).toBeDisabled();
    }
    expect(
      screen.getByRole("button", { name: "Test template" }),
    ).toBeDisabled();
    expect(
      screen.getByRole("button", { name: "Save template" }),
    ).toBeDisabled();
    fireEvent.click(screen.getByRole("button", { name: "Cancel" }));
    expect(onOpenChange).toHaveBeenCalledWith(false);

    rerender(<BalanceQueryTemplateDialog open pending={false} {...props} />);
    expect(screen.getByLabelText("Name")).toBeEnabled();
    expect(screen.getByRole("button", { name: "Save template" })).toBeEnabled();
  });

  it("uses temporary credentials only for testing and clears them when closed", async () => {
    const query = vi
      .spyOn(providerGroupsApi, "queryBalanceByCredentials")
      .mockResolvedValue({
        success: true,
        data: [{ remaining: 12.5, unit: "USD" }],
      });
    const props = {
      onOpenChange: vi.fn(),
      onSubmit: vi.fn(),
      appId: "claude" as const,
    };
    const { rerender } = render(<BalanceQueryTemplateDialog open {...props} />);
    fireEvent.change(screen.getByLabelText("Base URL"), {
      target: { value: "https://relay.example/v1" },
    });
    fireEvent.change(screen.getByLabelText("Temporary API Key"), {
      target: { value: "fixture-temporary-key" },
    });
    fireEvent.click(screen.getByRole("button", { name: "Test template" }));
    expect(await screen.findByRole("status")).toHaveTextContent(
      "Test succeeded: 12.5 USD",
    );
    expect(query).toHaveBeenCalledWith(
      expect.objectContaining({
        appType: "claude",
        baseUrl: "https://relay.example/v1",
        apiKey: "fixture-temporary-key",
        template: expect.objectContaining({
          headers: { Authorization: "Bearer {{apiKey}}" },
        }),
      }),
    );
    fireEvent.click(screen.getByRole("button", { name: "Save template" }));
    expect(props.onSubmit).toHaveBeenCalledTimes(1);
    const saved = props.onSubmit.mock.calls[0][0];
    expect(saved.headers).toEqual({ Authorization: "Bearer {{apiKey}}" });
    expect(JSON.stringify(saved)).not.toContain("fixture-temporary-key");
    expect(saved).not.toHaveProperty("apiKey");
    rerender(<BalanceQueryTemplateDialog open={false} {...props} />);
    rerender(<BalanceQueryTemplateDialog open {...props} />);
    expect(screen.getByLabelText("Temporary API Key")).toHaveValue("");
    expect(screen.getByLabelText("Base URL")).toHaveValue("");
    expect(screen.queryByRole("status")).not.toBeInTheDocument();
  });

  it("ignores a test response that arrives after the dialog has been reopened", async () => {
    let finish!: (result: UsageResult) => void;
    vi.spyOn(providerGroupsApi, "queryBalanceByCredentials").mockReturnValue(
      new Promise((resolve) => {
        finish = resolve;
      }),
    );
    const props = { onOpenChange: vi.fn(), onSubmit: vi.fn() };
    const { rerender } = render(<BalanceQueryTemplateDialog open {...props} />);
    fireEvent.change(screen.getByLabelText("Base URL"), {
      target: { value: "https://relay.example" },
    });
    fireEvent.change(screen.getByLabelText("Temporary API Key"), {
      target: { value: "fixture-temporary-key" },
    });
    fireEvent.click(screen.getByRole("button", { name: "Test template" }));
    expect(screen.getByLabelText("Temporary API Key")).toBeDisabled();
    rerender(<BalanceQueryTemplateDialog open={false} {...props} />);
    rerender(<BalanceQueryTemplateDialog open {...props} />);
    await act(async () =>
      finish({ success: true, data: [{ remaining: 999, unit: "USD" }] }),
    );
    expect(screen.queryByRole("status")).not.toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Test template" })).toBeEnabled();
    expect(screen.getByLabelText("Temporary API Key")).toHaveValue("");
  });
});
