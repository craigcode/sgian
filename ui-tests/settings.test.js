import { describe, it, expect } from "vitest";
import {
  parseShellArgsText,
  shellArgsToText,
  parseEnvText,
  envToText,
  parseThemeText,
  themeToText,
  parseProfilesText,
  profilesToText,
  populateFormFromConfig,
  validateSettingsForm,
  serializeSettingsForm,
  settingsPassthroughConfig,
  isRestorePolicyValid,
} from "../ui/src/settings.js";

describe("parseShellArgsText", () => {
  it("splits newline-separated args into an array", () => {
    expect(parseShellArgsText("-l\n-c\n--login")).toEqual(["-l", "-c", "--login"]);
  });

  it("trims whitespace per line", () => {
    expect(parseShellArgsText("  -l  \n  -c  ")).toEqual(["-l", "-c"]);
  });

  it("returns empty array for empty text", () => {
    expect(parseShellArgsText("")).toEqual([]);
    expect(parseShellArgsText("   \n  ")).toEqual([]);
  });
});

describe("shellArgsToText", () => {
  it("joins args with newlines", () => {
    expect(shellArgsToText(["-l", "-c"])).toBe("-l\n-c");
  });

  it("returns empty string for null/empty", () => {
    expect(shellArgsToText(null)).toBe("");
    expect(shellArgsToText([])).toBe("");
  });
});

describe("parseEnvText", () => {
  it("parses KEY=VALUE lines into a map", () => {
    expect(parseEnvText("FOO=bar\nBAZ=qux")).toEqual({ FOO: "bar", BAZ: "qux" });
  });

  it("preserves values containing equals signs", () => {
    expect(parseEnvText("EQ=a=b=c")).toEqual({ EQ: "a=b=c" });
  });

  it("ignores comments and empty lines", () => {
    expect(parseEnvText("# comment\nFOO=bar\n\n")).toEqual({ FOO: "bar" });
  });

  it("ignores lines without equals or with empty key", () => {
    expect(parseEnvText("noequals\n=bad\nGOOD=ok")).toEqual({ GOOD: "ok" });
  });

  it("returns empty object for empty text", () => {
    expect(parseEnvText("")).toEqual({});
    expect(parseEnvText("   ")).toEqual({});
  });
});

describe("envToText", () => {
  it("converts a map to sorted KEY=VALUE lines", () => {
    expect(envToText({ B: "2", A: "1" })).toBe("A=1\nB=2");
  });

  it("returns empty string for null/empty", () => {
    expect(envToText(null)).toBe("");
    expect(envToText({})).toBe("");
  });
});

describe("parseThemeText", () => {
  it("parses valid JSON object", () => {
    expect(parseThemeText('{"background":"#000"}')).toEqual({ background: "#000" });
  });

  it("returns null for empty text", () => {
    expect(parseThemeText("")).toBeNull();
    expect(parseThemeText("   ")).toBeNull();
  });

  it("throws for invalid JSON", () => {
    expect(() => parseThemeText("{bad}")).toThrow();
  });

  it("throws for non-object JSON", () => {
    expect(() => parseThemeText('"hello"')).toThrow();
    expect(() => parseThemeText("[1,2]")).toThrow();
  });
});

describe("themeToText", () => {
  it("converts a theme object to pretty JSON", () => {
    const text = themeToText({ background: "#000" });
    expect(JSON.parse(text)).toEqual({ background: "#000" });
  });

  it("returns empty string for null", () => {
    expect(themeToText(null)).toBe("");
  });
});

describe("profilesToText", () => {
  it("converts a profiles array to pretty JSON", () => {
    const text = profilesToText([{ name: "dev", kind: "shell" }]);
    expect(JSON.parse(text)).toEqual([{ name: "dev", kind: "shell" }]);
  });

  it("returns [] for an empty array", () => {
    expect(profilesToText([])).toBe("[]");
  });

  it("returns empty string for null", () => {
    expect(profilesToText(null)).toBe("");
  });
});

describe("parseProfilesText", () => {
  it("parses valid JSON array", () => {
    expect(parseProfilesText('[{"name":"dev"}]')).toEqual([{ name: "dev" }]);
  });

  it("returns undefined for empty text", () => {
    expect(parseProfilesText("")).toBeUndefined();
  });

  it("throws for non-array JSON", () => {
    expect(() => parseProfilesText('{"name":"dev"}')).toThrow("profiles must be a JSON array");
  });
});

describe("populateFormFromConfig", () => {
  it("maps a full config to form field strings", () => {
    const config = {
      shell: "/bin/bash",
      shell_args: ["-l", "-c"],
      env: { FOO: "bar" },
      font_family: "Fira Code",
      font_size: 14,
      theme: { background: "#000" },
      idle_shutdown_secs: 30,
      restore_policy: "auto_respawn",
    };
    const form = populateFormFromConfig(config);
    expect(form.shell).toBe("/bin/bash");
    expect(form.shell_args).toBe("-l\n-c");
    expect(form.env).toBe("FOO=bar");
    expect(form.font_family).toBe("Fira Code");
    expect(form.font_size).toBe("14");
    expect(form.theme).toContain('"background"');
    expect(form.idle_shutdown_secs).toBe("30");
    expect(form.restore_policy).toBe("auto_respawn");
  });

  it("handles null config with empty strings", () => {
    const form = populateFormFromConfig(null);
    expect(form).toEqual({
      shell: "",
      shell_args: "",
      env: "",
      font_family: "",
      font_size: "",
      theme: "",
      profiles: "",
      idle_shutdown_secs: "",
      restore_policy: "",
    });
  });

  it("handles partial config", () => {
    const form = populateFormFromConfig({ font_size: 16 });
    expect(form.font_size).toBe("16");
    expect(form.shell).toBe("");
  });
});

describe("validateSettingsForm", () => {
  const validValues = {
    shell: "/bin/bash",
    shell_args: "-l",
    env: "FOO=bar",
    font_family: "Fira Code",
    font_size: "14",
    theme: '{"background":"#000"}',
    profiles: '[{"name":"dev","kind":"shell"}]',
    idle_shutdown_secs: "30",
    restore_policy: "auto_respawn",
  };

  it("returns valid for a fully valid form", () => {
    const result = validateSettingsForm(validValues);
    expect(result.valid).toBe(true);
    expect(result.errors).toEqual({});
  });

  it("returns valid for an empty form (all optionals)", () => {
    const result = validateSettingsForm({
      shell: "",
      shell_args: "",
      env: "",
      font_family: "",
      font_size: "",
      theme: "",
      profiles: "",
      idle_shutdown_secs: "",
      restore_policy: "",
    });
    expect(result.valid).toBe(true);
  });

  it("flags non-numeric font_size", () => {
    const result = validateSettingsForm({ ...validValues, font_size: "big" });
    expect(result.valid).toBe(false);
    expect(result.errors.font_size).toBeDefined();
  });

  it("flags zero font_size", () => {
    const result = validateSettingsForm({ ...validValues, font_size: "0" });
    expect(result.valid).toBe(false);
    expect(result.errors.font_size).toBeDefined();
  });

  it("flags negative idle_shutdown_secs", () => {
    const result = validateSettingsForm({ ...validValues, idle_shutdown_secs: "-5" });
    expect(result.valid).toBe(false);
    expect(result.errors.idle_shutdown_secs).toBeDefined();
  });

  it("flags unknown restore_policy", () => {
    const result = validateSettingsForm({ ...validValues, restore_policy: "nonsense" });
    expect(result.valid).toBe(false);
    expect(result.errors.restore_policy).toBeDefined();
  });

  it("flags malformed env line", () => {
    const result = validateSettingsForm({ ...validValues, env: "FOO=bar\nbadline" });
    expect(result.valid).toBe(false);
    expect(result.errors.env).toBeDefined();
  });

  it("flags invalid JSON theme", () => {
    const result = validateSettingsForm({ ...validValues, theme: "{bad}" });
    expect(result.valid).toBe(false);
    expect(result.errors.theme).toBeDefined();
  });

  it("flags non-object JSON theme", () => {
    const result = validateSettingsForm({ ...validValues, theme: "[1,2]" });
    expect(result.valid).toBe(false);
    expect(result.errors.theme).toBeDefined();
  });

  it("flags invalid JSON profiles", () => {
    const result = validateSettingsForm({ ...validValues, profiles: "{bad}" });
    expect(result.valid).toBe(false);
    expect(result.errors.profiles).toBeDefined();
  });

  it("flags non-array JSON profiles", () => {
    const result = validateSettingsForm({ ...validValues, profiles: '{"name":"dev"}' });
    expect(result.valid).toBe(false);
    expect(result.errors.profiles).toBe("profiles must be a JSON array");
  });

  it("per-field validation flags the offending field and preserves the others", () => {
    const result = validateSettingsForm({
      ...validValues,
      font_size: "bad",
      restore_policy: "nonsense",
    });
    expect(result.valid).toBe(false);
    expect(result.errors.font_size).toBeDefined();
    expect(result.errors.restore_policy).toBeDefined();
    expect(result.errors.env).toBeUndefined();
    expect(result.errors.idle_shutdown_secs).toBeUndefined();
    expect(result.errors.theme).toBeUndefined();
    expect(result.errors.shell).toBeUndefined();
  });
});

describe("serializeSettingsForm", () => {
  it("maps form values to a valid config object with correct types", () => {
    const config = serializeSettingsForm({
      shell: "/bin/bash",
      shell_args: "-l\n-c",
      env: "FOO=bar\nBAZ=qux",
      font_family: "Fira Code",
      font_size: "14",
      theme: '{"background":"#000"}',
      profiles: '[{"name":"dev","kind":"agent","agent_backend":"droid"}]',
      idle_shutdown_secs: "30",
      restore_policy: "auto_respawn",
    });
    expect(config).toEqual({
      shell: "/bin/bash",
      shell_args: ["-l", "-c"],
      env: { FOO: "bar", BAZ: "qux" },
      font_family: "Fira Code",
      font_size: 14,
      theme: { background: "#000" },
      profiles: [{ name: "dev", kind: "agent", agent_backend: "droid" }],
      idle_shutdown_secs: 30,
      restore_policy: "auto_respawn",
    });
    // Type checks.
    expect(typeof config.font_size).toBe("number");
    expect(typeof config.idle_shutdown_secs).toBe("number");
    expect(Array.isArray(config.shell_args)).toBe(true);
    expect(typeof config.env).toBe("object");
    expect(typeof config.theme).toBe("object");
  });

  it("omits empty/unset optionals", () => {
    const config = serializeSettingsForm({
      shell: "",
      shell_args: "",
      env: "",
      font_family: "",
      font_size: "",
      theme: "",
      idle_shutdown_secs: "",
      restore_policy: "",
    });
    expect(config).toEqual({});
  });

  it("omits only unset fields and keeps the rest", () => {
    const config = serializeSettingsForm({
      shell: "/bin/zsh",
      shell_args: "",
      env: "",
      font_family: "Menlo",
      font_size: "",
      theme: "",
      idle_shutdown_secs: "",
      restore_policy: "",
    });
    expect(config).toEqual({
      shell: "/bin/zsh",
      font_family: "Menlo",
    });
  });

  it("converts font_size string to number", () => {
    const config = serializeSettingsForm({
      font_size: "18",
    });
    expect(config.font_size).toBe(18);
    expect(typeof config.font_size).toBe("number");
  });

  it("converts idle_shutdown_secs string to number", () => {
    const config = serializeSettingsForm({
      idle_shutdown_secs: "0",
    });
    expect(config.idle_shutdown_secs).toBe(0);
  });

  it("trims shell and font_family", () => {
    const config = serializeSettingsForm({
      shell: "  /bin/sh  ",
      font_family: "  Menlo  ",
    });
    expect(config.shell).toBe("/bin/sh");
    expect(config.font_family).toBe("Menlo");
  });
});

describe("settingsPassthroughConfig", () => {
  it("preserves full-config fields that the form does not edit", () => {
    expect(
      settingsPassthroughConfig({
        scrub_env: ["TOKEN"],
        agent_permission_mode: "plan",
        agent_claude_bin: "C:\\tools\\claude.exe",
        profiles: [{ name: "dev", kind: "shell" }],
        font_size: 15,
        env: { FOO: "bar" },
      }),
    ).toEqual({
      scrub_env: ["TOKEN"],
      agent_permission_mode: "plan",
      agent_claude_bin: "C:\\tools\\claude.exe",
      profiles: [{ name: "dev", kind: "shell" }],
    });
  });

  it("handles absent config", () => {
    expect(settingsPassthroughConfig(null)).toEqual({});
  });
});

describe("isRestorePolicyValid", () => {
  it("accepts auto_respawn", () => {
    expect(isRestorePolicyValid("auto_respawn")).toBe(true);
  });

  it("accepts restore_on_demand", () => {
    expect(isRestorePolicyValid("restore_on_demand")).toBe(true);
  });

  it("accepts empty", () => {
    expect(isRestorePolicyValid("")).toBe(true);
    expect(isRestorePolicyValid(null)).toBe(true);
  });

  it("rejects unknown values", () => {
    expect(isRestorePolicyValid("nonsense")).toBe(false);
  });
});
