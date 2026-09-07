import { act, fireEvent, screen, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { ProviderActions } from "@/components/providers/ProviderActions";
import { APP_IDS } from "@/config/appConfig";
import { useAppManagement } from "@/lib/query/appManagement";
import { appManagementApi } from "@/lib/api/appManagement";
import type { AppId } from "@/lib/api/types";
import {
  initializeSafetyI18n,
  managementFixture,
  renderSafetyUi,
} from "../utils/safetyTestUtils";

beforeEach(async () => {
  await initializeSafetyI18n();
});
afterEach(() => vi.restoreAllMocks());

describe("management enforcement in action controls", () => {
  it.each(APP_IDS)(
    "disables %s live use while retaining edit and manual validation",
    (appId) => {
      const onSwitch = vi.fn();
      const onEdit = vi.fn();
      const onValidate = vi.fn();
      renderSafetyUi(
        <ProviderActions
          appId={appId}
          isCurrent={false}
          isManagementDisabled
          onSwitch={onSwitch}
          onEdit={onEdit}
          onDelete={vi.fn()}
          onValidate={onValidate}
        />,
      );
      const action = screen.getByRole("button", { name: "Unmanaged" });
      expect(action).toBeDisabled();
      fireEvent.click(action);
      expect(onSwitch).not.toHaveBeenCalled();
      fireEvent.click(screen.getByRole("button", { name: "Edit" }));
      expect(onEdit).toHaveBeenCalled();
      fireEvent.click(
        screen.getByRole("button", {
          name: "Validate provider and model capabilities",
        }),
      );
      expect(onValidate).toHaveBeenCalled();
    },
  );

  it.each(APP_IDS)(
    "treats %s transitional state as unwritable independently of other apps",
    (appId) => {
      function Status({ target }: { target: AppId }) {
        const state = useAppManagement(target);
        return (
          <output data-testid={target}>
            {state.canWrite ? "writable" : "blocked"}
          </output>
        );
      }
      const otherApp = appId === "codex" ? "pi" : "codex";
      renderSafetyUi(
        <>
          <Status target={appId} />
          <Status target={otherApp} />
        </>,
        managementFixture({
          [appId]: { phase: "pending_review", enabled: true },
        }),
      );
      expect(screen.getByTestId(appId)).toHaveTextContent("blocked");
      expect(screen.getByTestId(otherApp)).toHaveTextContent("writable");
    },
  );

  it("disables the OpenClaw default-model picker while unmanaged", () => {
    const onSetAsDefault = vi.fn();
    renderSafetyUi(
      <ProviderActions
        appId="openclaw"
        isCurrent={false}
        isInConfig
        isManagementDisabled
        onSwitch={vi.fn()}
        onEdit={vi.fn()}
        onDelete={vi.fn()}
        onSetAsDefault={onSetAsDefault}
        defaultModelOptions={[{ id: "a" }, { id: "b" }]}
      />,
    );
    expect(screen.getByRole("button", { name: "Set Default" })).toBeDisabled();
    expect(onSetAsDefault).not.toHaveBeenCalled();
  });

  it("revokes cached write permission when reading the authority fails", async () => {
    function Status() {
      const state = useAppManagement("codex");
      return <output>{state.canWrite ? "writable" : "blocked"}</output>;
    }
    vi.spyOn(appManagementApi, "getState").mockRejectedValue(
      new Error("corrupt local authority"),
    );
    const { queryClient } = renderSafetyUi(<Status />, managementFixture());
    expect(screen.getByText("writable")).toBeVisible();
    await act(async () => {
      await queryClient.invalidateQueries({ queryKey: ["appManagement"] });
    });
    await waitFor(() => expect(screen.getByText("blocked")).toBeVisible());
  });
});
