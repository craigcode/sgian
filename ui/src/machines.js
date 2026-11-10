/**
 * Pure helpers for saved remote workspace targets (`machines`, #114).
 */

export function workspaceBasename(rawPath) {
  if (typeof rawPath !== "string") return "workspace";
  const trimmed = rawPath.replace(/[\\/]+$/, "").trim();
  if (!trimmed) return "workspace";
  const parts = trimmed.split(/[\\/]+/);
  return parts[parts.length - 1] || trimmed;
}

export function formatMachineSubtitle(machine) {
  if (!machine || typeof machine !== "object") return "";
  const transport = machine.transport || "ssh";
  let endpoint = "";
  if (transport === "ssm") {
    endpoint = machine.target || "ssm";
  } else {
    const host = machine.host || "";
    const port =
      typeof machine.port === "number" && machine.port > 0 && machine.port !== 22
        ? `:${machine.port}`
        : "";
    endpoint = `${host}${port}`;
  }
  const workspace = machine.workspace || "";
  if (endpoint && workspace) return `${endpoint} · ${workspace}`;
  return endpoint || workspace;
}

export function normalizeSavedMachine(entry) {
  if (!entry || typeof entry !== "object" || typeof entry.name !== "string") {
    return null;
  }
  const name = entry.name.trim();
  if (!name) return null;
  return {
    name,
    transport: typeof entry.transport === "string" && entry.transport ? entry.transport : "ssh",
    host: typeof entry.host === "string" && entry.host ? entry.host : null,
    port: typeof entry.port === "number" && entry.port > 0 ? entry.port : null,
    workspace: typeof entry.workspace === "string" ? entry.workspace : "",
    target: typeof entry.target === "string" && entry.target ? entry.target : null,
    token_file: typeof entry.token_file === "string" && entry.token_file ? entry.token_file : null,
    token_configured: entry.token_configured === true,
    tunnel_present: entry.tunnel_present === true,
  };
}

export function normalizeWorkspaceTargets(payload) {
  if (!payload || typeof payload !== "object") {
    return {
      localWorkspace: null,
      activeMachine: null,
      credentialFromEnv: false,
      machines: [],
    };
  }
  const machines = Array.isArray(payload.machines)
    ? payload.machines.map(normalizeSavedMachine).filter(Boolean)
    : [];
  return {
    localWorkspace:
      typeof payload.local_workspace === "string" && payload.local_workspace
        ? payload.local_workspace
        : null,
    activeMachine:
      typeof payload.active_machine === "string" && payload.active_machine
        ? payload.active_machine
        : null,
    credentialFromEnv: payload.credential_from_env === true,
    machines,
  };
}
