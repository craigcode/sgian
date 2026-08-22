// settings.js — pure logic for the settings modal.
//
// Form-to-config serialization, per-field validation, and env/shell_args/theme
// parsing helpers. All functions are pure (no DOM, no Tauri) so they can be
// unit-tested directly in vitest.

const VALID_RESTORE_POLICIES = ["auto_respawn", "restore_on_demand"];

/**
 * Parse a newline-separated list of shell arguments into a string array.
 * Empty lines and surrounding whitespace are stripped. Arguments are split on
 * newlines (one arg per line) so users can paste multi-arg payloads safely.
 *
 * @param {string} text
 * @returns {string[]}
 */
export function parseShellArgsText(text) {
  if (!text || !text.trim()) return [];
  return text
    .split("\n")
    .map((line) => line.trim())
    .filter((line) => line.length > 0);
}

/**
 * Convert a shell-args array back to the textarea text representation.
 *
 * @param {string[] | null} arr
 * @returns {string}
 */
export function shellArgsToText(arr) {
  if (!Array.isArray(arr) || arr.length === 0) return "";
  return arr.join("\n");
}

/**
 * Parse a newline-separated "KEY=VALUE" list into a string→string map.
 * Lines without `=` are ignored. Empty keys are ignored. Surrounding whitespace
 * on the key is stripped; the value is taken verbatim after the first `=`.
 *
 * @param {string} text
 * @returns {Record<string, string>}
 */
export function parseEnvText(text) {
  if (!text || !text.trim()) return {};
  const env = {};
  for (const rawLine of text.split("\n")) {
    const line = rawLine.trim();
    if (!line || line.startsWith("#")) continue;
    const eqIdx = line.indexOf("=");
    if (eqIdx <= 0) continue; // no `=` or empty key
    const key = line.slice(0, eqIdx).trim();
    const value = line.slice(eqIdx + 1);
    if (!key) continue;
    env[key] = value;
  }
  return env;
}

/**
 * Convert an env map back to the textarea text representation.
 * Keys are sorted for deterministic output.
 *
 * @param {Record<string, string> | null} env
 * @returns {string}
 */
export function envToText(env) {
  if (!env || typeof env !== "object") return "";
  return Object.keys(env)
    .sort()
    .map((key) => `${key}=${env[key] ?? ""}`)
    .join("\n");
}

/**
 * Parse a theme JSON string into an object.
 * Returns null when the text is empty (no theme configured).
 * Throws when the text is non-empty but invalid JSON or not an object.
 *
 * @param {string} text
 * @returns {object | null}
 */
export function parseThemeText(text) {
  if (!text || !text.trim()) return null;
  const parsed = JSON.parse(text);
  if (typeof parsed !== "object" || Array.isArray(parsed) || parsed === null) {
    throw new Error("theme must be a JSON object");
  }
  return parsed;
}

/**
 * Convert a theme object to the textarea text representation (pretty-printed JSON).
 *
 * @param {object | null} theme
 * @returns {string}
 */
export function themeToText(theme) {
  if (!theme || typeof theme !== "object") return "";
  return JSON.stringify(theme, null, 2);
}

/**
 * Parse a profiles JSON string into an array.
 * Returns undefined when the text is empty so callers can omit the field and
 * preserve passthrough values on save.
 *
 * @param {string} text
 * @returns {object[] | undefined}
 */
export function parseProfilesText(text) {
  if (!text || !text.trim()) return undefined;
  const parsed = JSON.parse(text);
  if (!Array.isArray(parsed)) {
    throw new Error("profiles must be a JSON array");
  }
  return parsed;
}

/**
 * Convert a profiles array to the textarea text representation.
 *
 * @param {object[] | null | undefined} profiles
 * @returns {string}
 */
export function profilesToText(profiles) {
  if (!Array.isArray(profiles)) return "";
  if (profiles.length === 0) return "[]";
  return JSON.stringify(profiles, null, 2);
}

/**
 * Map a backend config object (from `get_config`) to the form-field value shape.
 * Each form field is a string (textareas/inputs) so the DOM can be populated
 * directly.
 *
 * @param {object | null} config
 * @returns {{ shell: string, shell_args: string, env: string, font_family: string, font_size: string, theme: string, profiles: string, idle_shutdown_secs: string, restore_policy: string }}
 */
export function populateFormFromConfig(config) {
  const c = config || {};
  return {
    shell: c.shell ?? "",
    shell_args: shellArgsToText(c.shell_args),
    env: envToText(c.env),
    font_family: c.font_family ?? "",
    font_size: c.font_size != null ? String(c.font_size) : "",
    theme: themeToText(c.theme),
    profiles: profilesToText(c.profiles),
    idle_shutdown_secs: c.idle_shutdown_secs != null ? String(c.idle_shutdown_secs) : "",
    restore_policy: c.restore_policy ?? "",
  };
}

/**
 * Per-field validation of the form values.
 * Returns `{ valid, errors }` where `errors` maps field-name → message.
 * Only fields with errors appear in the errors object; valid fields are absent.
 *
 * @param {{ shell: string, shell_args: string, env: string, font_family: string, font_size: string, theme: string, profiles: string, idle_shutdown_secs: string, restore_policy: string }} values
 * @returns {{ valid: boolean, errors: Record<string, string> }}
 */
export function validateSettingsForm(values) {
  const errors = {};

  // font_size: must be a positive integer if provided.
  if (values.font_size && values.font_size.trim()) {
    const n = Number(values.font_size);
    if (!Number.isFinite(n) || n <= 0 || !Number.isInteger(n)) {
      errors.font_size = "font size must be a positive whole number";
    }
  }

  // idle_shutdown_secs: must be a non-negative integer if provided (0 = disabled).
  if (values.idle_shutdown_secs && values.idle_shutdown_secs.trim()) {
    const n = Number(values.idle_shutdown_secs);
    if (!Number.isFinite(n) || n < 0 || !Number.isInteger(n)) {
      errors.idle_shutdown_secs = "idle shutdown must be a non-negative whole number";
    }
  }

  // restore_policy: must be a known value if provided.
  if (values.restore_policy && values.restore_policy.trim()) {
    if (!VALID_RESTORE_POLICIES.includes(values.restore_policy)) {
      errors.restore_policy = "restore policy must be 'auto_respawn' or 'restore_on_demand'";
    }
  }

  // env: each non-comment, non-empty line must have KEY=VALUE format.
  if (values.env && values.env.trim()) {
    for (const rawLine of values.env.split("\n")) {
      const line = rawLine.trim();
      if (!line || line.startsWith("#")) continue;
      const eqIdx = line.indexOf("=");
      if (eqIdx <= 0) {
        errors.env = "each env line must be KEY=VALUE";
        break;
      }
    }
  }

  // theme: must be valid JSON object if provided.
  if (values.theme && values.theme.trim()) {
    try {
      const parsed = JSON.parse(values.theme);
      if (typeof parsed !== "object" || Array.isArray(parsed) || parsed === null) {
        errors.theme = "theme must be a JSON object";
      }
    } catch {
      errors.theme = "theme must be valid JSON";
    }
  }

  // profiles: must be valid JSON array if provided.
  if (values.profiles && values.profiles.trim()) {
    try {
      parseProfilesText(values.profiles);
    } catch (error) {
      errors.profiles =
        error instanceof Error && error.message === "profiles must be a JSON array"
          ? error.message
          : "profiles must be valid JSON array";
    }
  }

  return { valid: Object.keys(errors).length === 0, errors };
}

/**
 * Serialize form-field values into a backend config object with correct keys
 * and types. Empty/unset optionals are omitted from the result.
 *
 * @param {{ shell: string, shell_args: string, env: string, font_family: string, font_size: string, theme: string, profiles: string, idle_shutdown_secs: string, restore_policy: string }} values
 * @returns {object}
 */
export function serializeSettingsForm(values) {
  const config = {};

  if (values.shell && values.shell.trim()) {
    config.shell = values.shell.trim();
  }

  const shellArgs = parseShellArgsText(values.shell_args);
  if (shellArgs.length > 0) {
    config.shell_args = shellArgs;
  }

  const env = parseEnvText(values.env);
  if (Object.keys(env).length > 0) {
    config.env = env;
  }

  if (values.font_family && values.font_family.trim()) {
    config.font_family = values.font_family.trim();
  }

  if (values.font_size && values.font_size.trim()) {
    config.font_size = Number(values.font_size);
  }

  const theme = parseThemeText(values.theme);
  if (theme) {
    config.theme = theme;
  }

  const profiles = parseProfilesText(values.profiles);
  if (profiles !== undefined) {
    config.profiles = profiles;
  }

  if (values.idle_shutdown_secs && values.idle_shutdown_secs.trim()) {
    config.idle_shutdown_secs = Number(values.idle_shutdown_secs);
  }

  if (values.restore_policy && values.restore_policy.trim()) {
    config.restore_policy = values.restore_policy.trim();
  }

  return config;
}

/**
 * Preserve full-config fields that are intentionally not exposed by the form.
 * The backend's write_config operation replaces the workspace config rather
 * than patching it, so omitting these would silently reset agent and env-scrub
 * settings whenever an unrelated appearance field is saved.
 */
export function settingsPassthroughConfig(config) {
  if (!config || typeof config !== "object") return {};
  const passthrough = {};
  for (const key of [
    "scrub_env",
    "agent_permission_mode",
    "agent_claude_bin",
    "agent_droid_bin",
    "profiles",
  ]) {
    if (Object.prototype.hasOwnProperty.call(config, key)) {
      passthrough[key] = config[key];
    }
  }
  return passthrough;
}

/**
 * Returns true when the restore policy value is valid (or empty).
 */
export function isRestorePolicyValid(value) {
  if (!value || !value.trim()) return true;
  return VALID_RESTORE_POLICIES.includes(value);
}
