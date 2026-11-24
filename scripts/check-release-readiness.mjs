import { execFileSync } from "node:child_process";
import { resolve } from "node:path";
import { pathToFileURL } from "node:url";
import { verifyReleaseConfig } from "./verify-release-config.mjs";

// Metadata only: GitHub never returns secret values from its list endpoint.
export function releaseBlockers(environment, secretNames, variables) {
  const secrets = new Set(secretNames);
  const blockers = [];
  if (environment.deployment_branch_policy?.protected_branches !== true) {
    blockers.push("native-release must restrict deployment to protected branches");
  }
  if (!environment.protection_rules?.some((rule) => rule.type === "required_reviewers" && rule.reviewers?.length)) {
    blockers.push("native-release needs a required release reviewer");
  }
  const requireSecrets = (names) => names.forEach((name) => {
    if (!secrets.has(name)) blockers.push(`Missing environment secret: ${name}`);
  });
  const requireVariables = (names) => names.forEach((name) => {
    if (!variables[name]?.trim()) blockers.push(`Missing environment variable: ${name}`);
  });
  requireSecrets(["APPLE_CERTIFICATE", "APPLE_CERTIFICATE_PASSWORD", "APPLE_SIGNING_IDENTITY",
    "APPLE_API_KEY_P8", "APPLE_API_KEY", "APPLE_API_ISSUER", "SPARKLE_PRIVATE_KEY", "TAURI_SIGNING_PRIVATE_KEY"]);
  requireVariables(["SPARKLE_PUBLIC_KEY"]);
  const sparkleKey = variables.SPARKLE_PUBLIC_KEY?.trim();
  if (sparkleKey && (!/^[A-Za-z0-9+/]{43}=$/.test(sparkleKey) || Buffer.from(sparkleKey, "base64").length !== 32)) {
    blockers.push("SPARKLE_PUBLIC_KEY must encode a 32-byte Ed25519 public key");
  }
  // Mirrors the workflow: a configured PFX takes precedence over Azure.
  if (secrets.has("WINDOWS_CERTIFICATE")) {
    requireSecrets(["WINDOWS_CERTIFICATE_PASSWORD"]);
  } else {
    requireSecrets(["AZURE_CLIENT_ID", "AZURE_TENANT_ID", "AZURE_SUBSCRIPTION_ID"]);
    requireVariables(["TRUSTED_SIGNING_ENDPOINT", "TRUSTED_SIGNING_ACCOUNT", "TRUSTED_SIGNING_PROFILE", "WINDOWS_PUBLISHER"]);
    if (variables.TRUSTED_SIGNING_ENDPOINT) {
      try {
        if (new URL(variables.TRUSTED_SIGNING_ENDPOINT).protocol !== "https:") throw Error();
      } catch { blockers.push("TRUSTED_SIGNING_ENDPOINT must be an HTTPS URL"); }
    }
    if (variables.WINDOWS_PUBLISHER && (!variables.WINDOWS_PUBLISHER.startsWith("CN=") || variables.WINDOWS_PUBLISHER.includes("Development"))) {
      blockers.push("WINDOWS_PUBLISHER must be the production certificate subject");
    }
  }
  return blockers;
}

if (process.argv[1] && import.meta.url === pathToFileURL(resolve(process.argv[1])).href) {
  try {
    const version = verifyReleaseConfig();
    const base = "repos/craigcode/sgian/environments/native-release";
    const api = (endpoint) => JSON.parse(execFileSync("gh", ["api", endpoint], { encoding: "utf8", stdio: ["ignore", "pipe", "pipe"] }));
    const pages = (endpoint, field) => JSON.parse(execFileSync("gh", ["api", "--paginate", "--slurp", `${endpoint}?per_page=100`],
      { encoding: "utf8", stdio: ["ignore", "pipe", "pipe"] })).flatMap((page) => page[field]);
    const environment = api(base);
    const secrets = pages(`${base}/secrets`, "secrets").map((secret) => secret.name);
    const variables = Object.fromEntries(pages(`${base}/variables`, "variables").map(({ name, value }) => [name, value]));
    const blockers = releaseBlockers(environment, secrets, variables);
    console.log(`Sgian ${version}: release configuration matches across all app manifests and lockfiles.`);
    console.log("Read-only GitHub preflight; no secret values read and no workflow dispatched.");
    if (blockers.length) {
      console.log(blockers.map((message) => `- ${message}`).join("\n"));
      process.exitCode = 1;
    } else {
      console.log("Signing inputs and environment protection are configured. Key validity and matching pairs still require signed builds.");
    }
    console.log("Clean signed installs, installed updates and manual acceptance remain separate release gates.");
  } catch (error) {
    // Avoid dumping subprocess output: report the action without variable values.
    console.error(error.status !== undefined ? "Cannot inspect GitHub release metadata. Check gh authentication and repository access." : error.message);
    process.exitCode = 1;
  }
}
