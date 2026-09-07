import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import { describe, expect, it } from "vitest";
import en from "@/i18n/locales/en.json";
import zh from "@/i18n/locales/zh.json";
import zhTW from "@/i18n/locales/zh-TW.json";
import ja from "@/i18n/locales/ja.json";

function flatten(
  value: unknown,
  prefix = "",
  result: Record<string, string> = {},
) {
  if (typeof value === "string") result[prefix] = value;
  else if (value && typeof value === "object")
    Object.entries(value).forEach(([key, child]) =>
      flatten(child, prefix ? `${prefix}.${key}` : key, result),
    );
  return result;
}
const reference = flatten({
  appManagement: en.appManagement,
  configGuard: en.configGuard,
  modelValidation: en.modelValidation,
});

describe("management, protection and validation translations", () => {
  it.each([
    ["zh", zh],
    ["zh-TW", zhTW],
    ["ja", ja],
    ["en", en],
  ] as const)(
    "covers all safety feature keys and variables in %s",
    (_locale, tree) => {
      const actual = flatten(tree);
      for (const [key, value] of Object.entries(reference)) {
        expect(actual[key], key).toBeTruthy();
        expect(actual[key].match(/\{\{[^}]+\}\}/g) ?? [], key).toEqual(
          value.match(/\{\{[^}]+\}\}/g) ?? [],
        );
      }
      expect(actual["common.retry"]).toBeTruthy();
    },
  );

  it("contains every statically used safety translation", () => {
    for (const file of [
      "src/components/settings/AppManagementSettings.tsx",
      "src/components/management/AppManagementNotice.tsx",
      "src/components/management/ConfigGuardPanel.tsx",
      "src/components/providers/ModelValidationDialog.tsx",
      "src/components/providers/ValidationModelPicker.tsx",
      "src/components/providers/ModelValidationResults.tsx",
    ]) {
      const source = readFileSync(resolve(file), "utf8");
      for (const [, key] of source.matchAll(
        /"((?:appManagement|configGuard|modelValidation)\.[A-Za-z.]+)"/g,
      ))
        expect(reference[key], key).toBeTruthy();
    }
  });
});
