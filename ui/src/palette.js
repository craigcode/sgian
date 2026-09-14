/**
 * Command palette command registry and fuzzy filter.
 */

function fuzzyMatch(haystack, needle) {
  const text = haystack.toLowerCase();
  const query = needle.toLowerCase().trim();
  if (!query) return true;
  if (text.includes(query)) return true;
  let index = 0;
  for (const char of query) {
    index = text.indexOf(char, index);
    if (index === -1) return false;
    index += 1;
  }
  return true;
}

function commandSearchText(command) {
  return [command.id, command.label, ...(command.keywords || [])].join(" ");
}

/**
 * Filter palette commands by fuzzy/substring match on label, keywords, and id.
 *
 * @param {Array<{ id: string, label: string, keywords?: string[], run: Function }>} commands
 * @param {string} query
 */
export function filterPaletteCommands(commands, query) {
  const trimmed = String(query || "").trim();
  if (!trimmed) return commands.slice();
  return commands.filter((command) => fuzzyMatch(commandSearchText(command), trimmed));
}

/**
 * Build the workbench command palette entries for a controller instance.
 *
 * @param {object} controller
 */
export function buildPaletteCommands(controller) {
  const state = controller.state;
  const commands = [
    {
      id: "split-row",
      label: "Split pane right",
      keywords: ["split", "row", "horizontal", "vertical"],
      run: () => void controller.splitActive("row"),
    },
    {
      id: "split-column",
      label: "Split pane down",
      keywords: ["split", "column", "vertical"],
      run: () => void controller.splitActive("column"),
    },
    {
      id: "close-pane",
      label: "Close active pane",
      keywords: ["close", "kill", "remove"],
      run: () => void controller.closeActive(),
    },
    {
      id: "rename-pane",
      label: "Rename active pane",
      keywords: ["rename", "title"],
      run: () => controller.beginRename(state.activePaneId),
    },
    {
      id: "search",
      label: "Search scrollback",
      keywords: ["find", "grep"],
      run: () => controller.openSearchBar(),
    },
    {
      id: "zoom-toggle",
      label: "Toggle zoom on active pane",
      keywords: ["zoom", "maximize", "fullscreen"],
      run: () => controller.toggleZoomActive(),
    },
    {
      id: "swap-panes",
      label: "Swap active pane with partner",
      keywords: ["swap", "exchange"],
      run: () => controller.swapActiveWithPartner(),
    },
    {
      id: "new-agent",
      label: "New agent pane",
      keywords: ["agent", "create", "split"],
      run: () => void controller.newAgentPane(),
    },
    {
      id: "settings",
      label: "Open settings",
      keywords: ["preferences", "config"],
      run: () => controller.openSettingsModal(),
    },
    {
      id: "session-overview",
      label: "Open session overview",
      keywords: ["overview", "workspace", "panes", "session"],
      run: () => controller.openOverview(),
    },
    {
      id: "lease-take",
      label: "Take keyboard for active pane",
      keywords: ["lease", "keyboard", "take", "hold", "takeover"],
      run: () => void controller.takeLease(state.activePaneId),
    },
    {
      id: "lease-release",
      label: "Release keyboard (with hand-back note)",
      keywords: ["lease", "keyboard", "release", "note", "hand back"],
      run: () => controller.openReleaseDialog(state.activePaneId),
    },
  ];

  if (typeof controller.focusNextAttentionPane === "function") {
    commands.push({
      id: "focus-attention",
      label: "Focus next agent needing input",
      keywords: ["attention", "agent", "input", "focus"],
      run: () => controller.focusNextAttentionPane(),
    });
  }

  if (state.updateVersion) {
    commands.push({
      id: "install-update",
      label: `Install update v${state.updateVersion}`,
      keywords: ["update", "upgrade", "restart"],
      run: () => void controller.installUpdate(),
    });
  }

  for (const profile of state.profiles || []) {
    if (!profile?.name) continue;
    commands.push({
      id: `new-pane-profile-${profile.name}`,
      label: `New pane with profile: ${profile.name}`,
      keywords: ["profile", profile.name, profile.kind || "pane", "new", "create"],
      run: () => void controller.newPaneWithProfile(profile.name),
    });
  }

  return commands;
}
