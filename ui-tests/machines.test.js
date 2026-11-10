import { describe, expect, it } from "vitest";
import {
  formatMachineSubtitle,
  normalizeSavedMachine,
  normalizeWorkspaceTargets,
  workspaceBasename,
} from "../ui/src/machines.js";

describe("workspaceBasename", () => {
  it("extracts basenames across Unix and Windows paths", () => {
    expect(workspaceBasename("/Users/op/repos/sgian")).toBe("sgian");
    expect(workspaceBasename("/Users/op/repos/sgian/")).toBe("sgian");
    expect(workspaceBasename("C:\\Users\\op\\repos\\sgian\\")).toBe("sgian");
    expect(workspaceBasename("")).toBe("workspace");
    expect(workspaceBasename(null)).toBe("workspace");
  });
});

describe("formatMachineSubtitle", () => {
  it("formats SSH default port, custom port, and SSM targets", () => {
    expect(
      formatMachineSubtitle({
        transport: "ssh",
        host: "dev@box.example",
        port: 22,
        workspace: "/srv/app",
      }),
    ).toBe("dev@box.example · /srv/app");
    expect(
      formatMachineSubtitle({
        transport: "ssh",
        host: "dev@box.example",
        port: 2222,
        workspace: "/srv/app",
      }),
    ).toBe("dev@box.example:2222 · /srv/app");
    expect(
      formatMachineSubtitle({
        transport: "ssm",
        target: "i-0123456789abcdef0",
        workspace: "/home/ec2-user/repo",
      }),
    ).toBe("i-0123456789abcdef0 · /home/ec2-user/repo");
  });
});

describe("normalizeWorkspaceTargets", () => {
  it("normalizes machine entries and drops invalid entries", () => {
    expect(normalizeSavedMachine(null)).toBeNull();
    expect(normalizeSavedMachine({ name: "   " })).toBeNull();
    const snapshot = normalizeWorkspaceTargets({
      local_workspace: "/work/local",
      active_machine: "gpu",
      credential_from_env: true,
      machines: [
        {
          name: "gpu",
          transport: "ssh",
          host: "op@gpu.internal",
          port: 2222,
          workspace: "/srv/gpu",
          token_configured: true,
          tunnel_present: true,
        },
        { name: "" },
      ],
    });
    expect(snapshot).toEqual({
      localWorkspace: "/work/local",
      activeMachine: "gpu",
      credentialFromEnv: true,
      machines: [
        {
          name: "gpu",
          transport: "ssh",
          host: "op@gpu.internal",
          port: 2222,
          workspace: "/srv/gpu",
          target: null,
          token_file: null,
          token_configured: true,
          tunnel_present: true,
        },
      ],
    });
  });
});
