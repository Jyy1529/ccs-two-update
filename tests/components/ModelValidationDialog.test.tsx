import {
  act,
  cleanup,
  fireEvent,
  screen,
  waitFor,
} from "@testing-library/react";
import { beforeEach, afterEach, describe, expect, it, vi } from "vitest";
import { toast } from "sonner";
import { ModelValidationDialog } from "@/components/providers/ModelValidationDialog";
import {
  ModelValidationResults,
  validationEndpointLabel,
} from "@/components/providers/ModelValidationResults";
import {
  modelValidationApi,
  type ValidationProtocol,
} from "@/lib/api/modelValidation";
import type { AppId } from "@/lib/api/types";
import type { Provider } from "@/types";
import {
  initializeSafetyI18n,
  renderSafetyUi,
  safetyI18n,
  validationPlanFixture,
  validationRunFixture,
} from "../utils/safetyTestUtils";

const provider: Provider = {
  id: "key-a",
  name: "Key A",
  settingsConfig: { auth: { OPENAI_API_KEY: "synthetic-secret-not-for-ui" } },
};
const other: Provider = { id: "key-b", name: "Key B", settingsConfig: {} };
const scrollIntoViewDescriptor = Object.getOwnPropertyDescriptor(
  HTMLElement.prototype,
  "scrollIntoView",
);
const renderDialog = (appId: AppId = "codex", onOpenChange = vi.fn()) =>
  renderSafetyUi(
    <ModelValidationDialog
      open
      onOpenChange={onOpenChange}
      appId={appId}
      provider={provider}
      providers={{ [provider.id]: provider, [other.id]: other }}
    />,
  );

beforeEach(async () => {
  Object.defineProperty(HTMLElement.prototype, "scrollIntoView", {
    configurable: true,
    value: vi.fn(),
  });
  await initializeSafetyI18n();
  vi.spyOn(modelValidationApi, "list").mockResolvedValue([]);
  vi.spyOn(modelValidationApi, "prepare").mockResolvedValue(
    validationPlanFixture(),
  );
  vi.spyOn(modelValidationApi, "start").mockResolvedValue(
    validationRunFixture(),
  );
  vi.spyOn(modelValidationApi, "get").mockResolvedValue(validationRunFixture());
  vi.spyOn(modelValidationApi, "cancel").mockResolvedValue(true);
  vi.spyOn(modelValidationApi, "fetchModels").mockResolvedValue([
    { id: "synthetic-alpha", ownedBy: null },
    { id: "synthetic-beta", ownedBy: null },
  ]);
});
afterEach(async () => {
  cleanup();
  // Unmount cancels an active paid run; keep spies installed through cleanup.
  await Promise.resolve();
  vi.restoreAllMocks();
  if (scrollIntoViewDescriptor)
    Object.defineProperty(
      HTMLElement.prototype,
      "scrollIntoView",
      scrollIntoViewDescriptor,
    );
  else Reflect.deleteProperty(HTMLElement.prototype, "scrollIntoView");
});

async function prepareBasic() {
  fireEvent.change(screen.getByLabelText("Requested model"), {
    target: { value: "synthetic-model" },
  });
  fireEvent.click(
    screen.getByRole("button", { name: "Prepare tests and budget" }),
  );
  await screen.findByRole("region", {
    name: "Test plan and cost preview (no requests sent)",
  });
}

describe("model capability validation workflow", () => {
  it("fetches models using only the selected provider ID and does not start paid probes", async () => {
    renderDialog();
    expect(modelValidationApi.fetchModels).not.toHaveBeenCalled();
    fireEvent.click(screen.getByRole("button", { name: "Fetch Models" }));
    await screen.findByText(
      "Fetched 2 models. Select from the dropdown or enter an ID manually.",
    );
    expect(modelValidationApi.fetchModels).toHaveBeenCalledWith({
      appId: "codex",
      providerId: provider.id,
    });
    expect(modelValidationApi.prepare).not.toHaveBeenCalled();
    expect(modelValidationApi.start).not.toHaveBeenCalled();
    expect(
      JSON.stringify(vi.mocked(modelValidationApi.fetchModels).mock.calls),
    ).not.toContain("synthetic-secret");
    fireEvent.click(screen.getByRole("button", { name: "Select model" }));
    fireEvent.click(
      await screen.findByRole("option", { name: "synthetic-beta" }),
    );
    expect(screen.getByLabelText("Requested model")).toHaveValue(
      "synthetic-beta",
    );
  });

  it("keeps manual model entry available when the listing endpoint fails", async () => {
    vi.mocked(modelValidationApi.fetchModels).mockRejectedValue(
      new Error("HTTP 404"),
    );
    renderDialog();
    fireEvent.click(screen.getByRole("button", { name: "Fetch Models" }));
    expect(await screen.findByRole("alert")).toHaveTextContent(
      "Manual entry is still available",
    );
    await prepareBasic();
    expect(modelValidationApi.prepare).toHaveBeenCalledOnce();
    expect(modelValidationApi.start).not.toHaveBeenCalled();
  });

  it("discards a stale model list after the requested protocol changes", async () => {
    let finish!: (value: Array<{ id: string; ownedBy: null }>) => void;
    vi.mocked(modelValidationApi.fetchModels).mockImplementation(
      () =>
        new Promise((resolve) => {
          finish = resolve;
        }),
    );
    renderDialog();
    fireEvent.click(screen.getByRole("button", { name: "Fetch Models" }));
    fireEvent.change(screen.getByLabelText("Upstream protocol"), {
      target: { value: "anthropic" },
    });
    await act(async () => finish([{ id: "stale-model", ownedBy: null }]));
    expect(
      screen.queryByRole("button", { name: "Select model" }),
    ).not.toBeInTheDocument();
    expect(screen.queryByText(/Fetched 1 models/)).not.toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Fetch Models" })).toBeEnabled();
  });

  it("fetches comparison models with the explicitly selected comparison provider", async () => {
    renderDialog();
    fireEvent.click(
      screen.getByLabelText("Experimental repeated controlled comparison"),
    );
    fireEvent.change(screen.getByLabelText("Provider / individual member"), {
      target: { value: other.id },
    });
    const buttons = screen.getAllByRole("button", { name: "Fetch Models" });
    fireEvent.click(buttons[1]);
    await waitFor(() =>
      expect(modelValidationApi.fetchModels).toHaveBeenCalledWith({
        appId: "codex",
        providerId: other.id,
      }),
    );
    expect(modelValidationApi.start).not.toHaveBeenCalled();
  });

  it("never starts a request while preparing and defaults advanced / experimental probes off", async () => {
    renderDialog();
    expect(modelValidationApi.prepare).not.toHaveBeenCalled();
    expect(modelValidationApi.start).not.toHaveBeenCalled();
    expect(
      screen.getByLabelText("Experimental repeated controlled comparison"),
    ).not.toBeChecked();
    fireEvent.click(screen.getByText("Advanced diagnostics (off by default)"));
    for (const label of [
      "Output limit",
      "Cache creation and reads",
      "Thinking parameters and evidence",
      "Claude signature positive / negative controls",
      "Claude cross-provider signature control",
    ]) {
      expect(screen.getByLabelText(label)).not.toBeChecked();
    }
    await prepareBasic();
    expect(modelValidationApi.prepare).toHaveBeenCalledWith({
      target: { appId: "codex", providerId: "key-a", model: "synthetic-model" },
      mode: "direct",
      probes: ["call", "stream", "tools", "structured", "image"],
    });
    expect(modelValidationApi.start).not.toHaveBeenCalled();
    expect(screen.getByText("Unknown; charges may still apply")).toBeVisible();
    expect(screen.getByText("9")).toBeVisible();
    expect(
      screen.queryByText("synthetic-secret-not-for-ui"),
    ).not.toBeInTheDocument();
    expect(
      JSON.stringify(vi.mocked(modelValidationApi.prepare).mock.calls),
    ).not.toContain("synthetic-secret-not-for-ui");
    fireEvent.click(
      screen.getByRole("button", { name: "Confirm budget and start" }),
    );
    await waitFor(() =>
      expect(modelValidationApi.start).toHaveBeenCalledWith("plan-fixed-key-a"),
    );
    expect(await screen.findByText("Run status: Running")).toBeVisible();
  });

  it.each<ValidationProtocol>([
    "openai_chat",
    "openai_responses",
    "anthropic",
    "gemini",
  ])(
    "passes the chosen %s protocol without switching a provider",
    async (protocol) => {
      renderDialog();
      fireEvent.change(screen.getByLabelText("Upstream protocol"), {
        target: { value: protocol },
      });
      await prepareBasic();
      expect(modelValidationApi.prepare).toHaveBeenCalledWith(
        expect.objectContaining({
          target: {
            appId: "codex",
            providerId: "key-a",
            model: "synthetic-model",
            protocol,
          },
        }),
      );
    },
  );

  it("invalidates prepared consent when the model or protocol changes", async () => {
    renderDialog();
    await prepareBasic();
    expect(
      screen.getByRole("button", { name: "Confirm budget and start" }),
    ).toBeEnabled();
    fireEvent.change(screen.getByLabelText("Requested model"), {
      target: { value: "different-model" },
    });
    expect(
      screen.getByRole("button", { name: "Confirm budget and start" }),
    ).toBeDisabled();
    expect(
      screen.queryByRole("region", {
        name: "Test plan and cost preview (no requests sent)",
      }),
    ).not.toBeInTheDocument();
    expect(modelValidationApi.start).not.toHaveBeenCalled();
  });

  it("requires an explicit cross-provider target and includes the fixed second member", async () => {
    renderDialog();
    fireEvent.change(screen.getByLabelText("Requested model"), {
      target: { value: "synthetic-claude" },
    });
    fireEvent.change(screen.getByLabelText("Upstream protocol"), {
      target: { value: "anthropic" },
    });
    fireEvent.click(screen.getByText("Advanced diagnostics (off by default)"));
    fireEvent.click(
      screen.getByLabelText("Claude cross-provider signature control"),
    );
    fireEvent.click(
      screen.getByRole("button", { name: "Prepare tests and budget" }),
    );
    expect(modelValidationApi.prepare).not.toHaveBeenCalled();
    expect(screen.getByRole("alert")).toHaveTextContent("Enter a model");
    fireEvent.change(screen.getByLabelText("Provider / individual member"), {
      target: { value: "key-b" },
    });
    fireEvent.change(screen.getByLabelText("Second target model"), {
      target: { value: "synthetic-other" },
    });
    fireEvent.change(screen.getByLabelText("Second target protocol"), {
      target: { value: "anthropic" },
    });
    fireEvent.click(
      screen.getByRole("button", { name: "Prepare tests and budget" }),
    );
    await waitFor(() =>
      expect(modelValidationApi.prepare).toHaveBeenCalledWith(
        expect.objectContaining({
          comparisonTarget: {
            appId: "codex",
            providerId: "key-b",
            model: "synthetic-other",
            protocol: "anthropic",
          },
        }),
      ),
    );
  });

  it.each([
    { repeatCount: 0, accepted: false },
    { repeatCount: 1, accepted: false },
    { repeatCount: 2, accepted: true },
    { repeatCount: 2.5, accepted: false },
    { repeatCount: 3, accepted: true },
    { repeatCount: 5, accepted: true },
    { repeatCount: 6, accepted: false },
    { repeatCount: 10, accepted: false },
    { repeatCount: 11, accepted: false },
  ])(
    "keeps repeated comparisons bounded for $repeatCount repetitions",
    async ({ repeatCount, accepted }) => {
      renderDialog();
      fireEvent.change(screen.getByLabelText("Requested model"), {
        target: { value: "synthetic-model" },
      });
      fireEvent.click(
        screen.getByLabelText("Experimental repeated controlled comparison"),
      );
      fireEvent.change(screen.getByLabelText("Provider / individual member"), {
        target: { value: "key-b" },
      });
      fireEvent.change(screen.getByLabelText("Second target model"), {
        target: { value: "second-model" },
      });
      fireEvent.change(
        screen.getByRole("spinbutton", { name: /^Repetitions/ }),
        { target: { value: String(repeatCount) } },
      );
      fireEvent.click(
        screen.getByRole("button", { name: "Prepare tests and budget" }),
      );
      if (accepted) {
        await waitFor(() =>
          expect(modelValidationApi.prepare).toHaveBeenCalledWith(
            expect.objectContaining({
              repeatCount,
              probes: expect.arrayContaining(["comparison"]),
            }),
          ),
        );
      } else {
        expect(modelValidationApi.prepare).not.toHaveBeenCalled();
        expect(screen.getByRole("alert")).toHaveTextContent("Enter a model");
      }
      expect(modelValidationApi.start).not.toHaveBeenCalled();
    },
  );

  it("advertises the backend repetition limits on the input", () => {
    renderDialog();
    fireEvent.click(
      screen.getByLabelText("Experimental repeated controlled comparison"),
    );
    const repetitions = screen.getByRole("spinbutton", {
      name: /^Repetitions/,
    });
    expect(repetitions).toHaveAttribute("min", "2");
    expect(repetitions).toHaveAttribute("max", "5");
    expect(repetitions).toHaveAccessibleName("Repetitions (2–5)");
  });

  it("disallows a ccs route for apps without a forwarding adapter", async () => {
    renderDialog("opencode");
    expect(
      screen.getByRole("option", {
        name: "Through ccs conversion / forwarding",
      }),
    ).toBeDisabled();
    await prepareBasic();
    expect(modelValidationApi.prepare).toHaveBeenCalledWith(
      expect.objectContaining({
        mode: "direct",
        target: expect.objectContaining({ appId: "opencode" }),
      }),
    );
  });

  it("does not start an expired plan", async () => {
    vi.mocked(modelValidationApi.prepare).mockResolvedValue({
      ...validationPlanFixture(),
      expiresAt: "2020-01-01T00:00:00Z",
    });
    renderDialog();
    await prepareBasic();
    expect(
      await screen.findByText("This plan expired. Prepare it again."),
    ).toBeVisible();
    expect(
      screen.getByRole("button", { name: "Confirm budget and start" }),
    ).toBeDisabled();
    expect(modelValidationApi.start).not.toHaveBeenCalled();
  });

  it("cancels before closing and retains the stopped result", async () => {
    const onClose = vi.fn();
    renderDialog("codex", onClose);
    await prepareBasic();
    fireEvent.click(
      screen.getByRole("button", { name: "Confirm budget and start" }),
    );
    await screen.findByText("Run status: Running");
    vi.mocked(modelValidationApi.get).mockResolvedValue(
      validationRunFixture("cancelled"),
    );
    fireEvent.click(
      screen.getByRole("button", { name: "Cancel test and close" }),
    );
    await waitFor(() =>
      expect(modelValidationApi.cancel).toHaveBeenCalledWith("run-1"),
    );
    await waitFor(() => expect(onClose).toHaveBeenCalledWith(false));
    expect(screen.getByText("Run status: Cancelled")).toBeVisible();
  });

  it("does not pretend cancellation succeeded on a backend failure", async () => {
    vi.mocked(modelValidationApi.cancel).mockRejectedValue(
      new Error("cancel unavailable"),
    );
    const onClose = vi.fn();
    renderDialog("codex", onClose);
    await prepareBasic();
    fireEvent.click(
      screen.getByRole("button", { name: "Confirm budget and start" }),
    );
    await screen.findByText("Run status: Running");
    fireEvent.click(
      screen.getByRole("button", { name: "Cancel test and close" }),
    );
    expect(await screen.findByRole("alert")).toHaveTextContent(
      "Cancellation could not be confirmed",
    );
    expect(onClose).not.toHaveBeenCalled();
  });

  it("does not let an older in-flight poll replace a confirmed cancelled result", async () => {
    let finishPoll!: (run: ReturnType<typeof validationRunFixture>) => void;
    vi.mocked(modelValidationApi.get)
      .mockImplementationOnce(
        () =>
          new Promise((resolve) => {
            finishPoll = resolve;
          }),
      )
      .mockResolvedValue(validationRunFixture("cancelled"));
    const view = renderDialog();
    await prepareBasic();
    fireEvent.click(
      screen.getByRole("button", { name: "Confirm budget and start" }),
    );
    await screen.findByText("Run status: Running");
    await waitFor(() =>
      expect(modelValidationApi.get).toHaveBeenCalledTimes(1),
    );
    fireEvent.click(screen.getByRole("button", { name: "Cancel test" }));
    expect(await screen.findByText("Run status: Cancelled")).toBeVisible();
    await act(async () => finishPoll(validationRunFixture()));
    expect(screen.getByText("Run status: Cancelled")).toBeVisible();
    expect(
      view.queryClient.getQueryData(["modelValidationRun", "run-1"]),
    ).toEqual(expect.objectContaining({ status: "cancelled" }));
  });

  it("keeps cancellation available when polling fails and never treats that as completion", async () => {
    vi.mocked(modelValidationApi.get).mockRejectedValue(
      new Error("poll unavailable"),
    );
    renderDialog();
    await prepareBasic();
    fireEvent.click(
      screen.getByRole("button", { name: "Confirm budget and start" }),
    );
    expect(await screen.findByRole("alert")).toHaveTextContent(
      "Could not read run status",
    );
    expect(screen.getByText("Run status: Running")).toBeVisible();
    expect(screen.getByRole("button", { name: "Cancel test" })).toBeEnabled();
  });

  it("opens a recorded historical result without preparing or starting new requests", async () => {
    const historical = validationRunFixture("interrupted");
    vi.mocked(modelValidationApi.list).mockResolvedValue([historical]);
    vi.mocked(modelValidationApi.get).mockResolvedValue(historical);
    renderDialog();
    fireEvent.click(
      await screen.findByRole("button", {
        name: /2026-09-06T00:00:00Z.*synthetic-model.*Interrupted/,
      }),
    );
    expect(await screen.findByText("Run status: Interrupted")).toBeVisible();
    expect(
      screen.getByText("No individual results were recorded for this run."),
    ).toBeVisible();
    expect(modelValidationApi.prepare).not.toHaveBeenCalled();
    expect(modelValidationApi.start).not.toHaveBeenCalled();
  });

  it("requests cancellation on unmount and stops frontend polling", async () => {
    const view = renderDialog();
    await prepareBasic();
    fireEvent.click(
      screen.getByRole("button", { name: "Confirm budget and start" }),
    );
    await screen.findByText("Run status: Running");
    view.unmount();
    await waitFor(() =>
      expect(modelValidationApi.cancel).toHaveBeenCalledWith("run-1"),
    );
    const calls = vi.mocked(modelValidationApi.get).mock.calls.length;
    await act(async () => {
      await new Promise((resolve) => setTimeout(resolve, 1100));
    });
    expect(modelValidationApi.get).toHaveBeenCalledTimes(calls);
  });

  it("cancels a run whose start resolves after navigation", async () => {
    let finishStart!: (value: ReturnType<typeof validationRunFixture>) => void;
    vi.mocked(modelValidationApi.start).mockImplementation(
      () =>
        new Promise((resolve) => {
          finishStart = resolve;
        }),
    );
    const view = renderDialog();
    await prepareBasic();
    fireEvent.click(
      screen.getByRole("button", { name: "Confirm budget and start" }),
    );
    await waitFor(() => expect(modelValidationApi.start).toHaveBeenCalled());
    view.unmount();
    await act(async () => finishStart(validationRunFixture()));
    await waitFor(() =>
      expect(modelValidationApi.cancel).toHaveBeenCalledWith("run-1"),
    );
  });

  it("reports failure to cancel a late-started run after navigation", async () => {
    let finishStart!: (run: ReturnType<typeof validationRunFixture>) => void;
    vi.mocked(modelValidationApi.start).mockImplementation(
      () =>
        new Promise((resolve) => {
          finishStart = resolve;
        }),
    );
    vi.mocked(modelValidationApi.cancel).mockRejectedValue(
      new Error("cancel unavailable"),
    );
    const notify = vi.spyOn(toast, "error");
    const view = renderDialog();
    await prepareBasic();
    fireEvent.click(
      screen.getByRole("button", { name: "Confirm budget and start" }),
    );
    view.unmount();
    await act(async () => finishStart(validationRunFixture()));
    await waitFor(() =>
      expect(notify).toHaveBeenCalledWith(
        "Cancellation could not be confirmed. Retry; backend plan limits still apply.",
      ),
    );
  });

  it("does not cancel a running validation when the UI language changes", async () => {
    renderDialog();
    await prepareBasic();
    fireEvent.click(
      screen.getByRole("button", { name: "Confirm budget and start" }),
    );
    await screen.findByText("Run status: Running");
    await act(async () => {
      await safetyI18n.changeLanguage("ja");
    });
    expect(modelValidationApi.cancel).not.toHaveBeenCalled();
    expect(screen.getByText("Run status: Running")).toBeVisible();
  });

  it("renders per-probe outcomes without turning completed into an authenticity verdict", () => {
    const run = validationRunFixture("completed");
    run.results = [
      {
        probe: "call",
        status: "passed",
        summary: "Call worked",
        evidence: [{ label: "response model", value: "self-reported" }],
        requestCount: 1,
        durationMs: 200,
      },
      {
        probe: "signature",
        status: "inconclusive",
        summary: "Unrelated HTTP 400",
        evidence: [],
        requestCount: 2,
        durationMs: 100,
      },
      {
        probe: "cache",
        status: "not_applicable",
        summary: "No native cache",
        evidence: [],
        requestCount: 0,
        durationMs: 0,
      },
      {
        probe: "image",
        status: "failed",
        summary: "Image not accepted",
        evidence: [],
        requestCount: 1,
        durationMs: 100,
      },
      {
        probe: "comparison",
        status: "not_tested",
        summary: "Not selected",
        evidence: [],
        requestCount: 0,
        durationMs: 0,
      },
    ];
    renderSafetyUi(<ModelValidationResults run={run} />);
    for (const state of [
      "Passed",
      "Inconclusive",
      "Not applicable",
      "Unexpected behavior",
      "Not tested",
    ])
      expect(screen.getByText(state)).toBeVisible();
    expect(screen.getByText("Run status: Completed")).toBeVisible();
    fireEvent.click(screen.getByText("View redacted evidence"));
    expect(screen.getByText("self-reported")).toBeVisible();
    expect(screen.queryByText(/98%|100%/)).not.toBeInTheDocument();
  });

  it("redacts endpoint credentials, query strings and fragments", () => {
    expect(
      validationEndpointLabel(
        "https://user:secret@synthetic.invalid/v1?api_key=secret#token",
      ),
    ).toBe("https://synthetic.invalid/v1");
    expect(validationEndpointLabel("secret-is-not-a-url")).toBe("—");
  });
});
