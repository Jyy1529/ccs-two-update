import { describe, expect, it } from "vitest";

import { buildFetchedCatalogSelection } from "@/components/providers/forms/CodexFormFields";

describe("Codex fetched model context selection", () => {
  it("fills the exact fetched context window and preserves a custom display name", () => {
    expect(
      buildFetchedCatalogSelection(
        {
          model: "old-model",
          displayName: "My reviewer",
          contextWindow: "128000",
        },
        {
          id: "gpt-5.6-sol",
          ownedBy: "openai",
          contextWindow: 262144,
        },
      ),
    ).toEqual({
      model: "gpt-5.6-sol",
      displayName: "My reviewer",
      contextWindow: 262144,
    });
  });

  it("clears a stale context window when the fetched model has no trusted value", () => {
    expect(
      buildFetchedCatalogSelection(
        {
          model: "old-model",
          displayName: "",
          contextWindow: "1000000",
        },
        {
          id: "unknown-model",
          ownedBy: null,
          contextWindow: null,
        },
      ),
    ).toEqual({
      model: "unknown-model",
      displayName: "unknown-model",
      contextWindow: "",
    });
  });
});
