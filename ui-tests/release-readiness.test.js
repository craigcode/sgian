// @vitest-environment node
import { expect, it } from "vitest";
import { releaseBlockers } from "../scripts/check-release-readiness.mjs";

const environment = { deployment_branch_policy: { protected_branches: true },
  protection_rules: [{ type: "required_reviewers", reviewers: [{ type: "User", reviewer: { id: 1 } }] }] };
const common = ["APPLE_CERTIFICATE", "APPLE_CERTIFICATE_PASSWORD", "APPLE_SIGNING_IDENTITY", "APPLE_API_KEY_P8",
  "APPLE_API_KEY", "APPLE_API_ISSUER", "SPARKLE_PRIVATE_KEY", "TAURI_SIGNING_PRIVATE_KEY"];
const azure = ["AZURE_CLIENT_ID", "AZURE_TENANT_ID", "AZURE_SUBSCRIPTION_ID"];
const vars = { SPARKLE_PUBLIC_KEY: Buffer.alloc(32, 1).toString("base64"), TRUSTED_SIGNING_ENDPOINT: "https://eus.codesigning.azure.net/",
  TRUSTED_SIGNING_ACCOUNT: "sgian", TRUSTED_SIGNING_PROFILE: "public", WINDOWS_PUBLISHER: "CN=Release Publisher" };

it("accepts complete Azure metadata without requiring the optional Linux key password", () => {
  expect(releaseBlockers(environment, [...common, ...azure], vars)).toEqual([]);
});
it("uses the configured PFX instead of requiring Azure metadata", () => {
  expect(releaseBlockers(environment, [...common, "WINDOWS_CERTIFICATE", "WINDOWS_CERTIFICATE_PASSWORD"],
    { SPARKLE_PUBLIC_KEY: vars.SPARKLE_PUBLIC_KEY })).toEqual([]);
});
it("does not silently fall back to Azure when a configured PFX is incomplete", () => {
  expect(releaseBlockers(environment, [...common, ...azure, "WINDOWS_CERTIFICATE"], vars)).toEqual([
    "Missing environment secret: WINDOWS_CERTIFICATE_PASSWORD",
  ]);
});
it("reports missing inputs and protection without exposing values", () => {
  const blockers = releaseBlockers({}, [], {});
  expect(blockers).toContain("native-release needs a required release reviewer");
  expect(blockers).toContain("native-release must restrict deployment to protected branches");
  expect(blockers).toContain("Missing environment secret: APPLE_CERTIFICATE");
  expect(blockers).toContain("Missing environment variable: SPARKLE_PUBLIC_KEY");
});
it.each([
  ["SPARKLE_PUBLIC_KEY", "invalid-key", "32-byte"],
  ["TRUSTED_SIGNING_ENDPOINT", "http://example.com", "HTTPS"],
  ["WINDOWS_PUBLISHER", "CN=Sgian Development", "production certificate"],
])("rejects invalid public configuration %s", (name, value, message) => {
  expect(releaseBlockers(environment, [...common, ...azure], { ...vars, [name]: value }).join("\n")).toContain(message);
});
