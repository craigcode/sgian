fn main() {
    tauri_build::try_build(tauri_build::Attributes::new().app_manifest(
        tauri_build::AppManifest::new().commands(&[
            "bootstrap_workspace",
            "create_pane",
            "close_pane",
            "rename_pane",
            "ensure_pane_terminal",
            "restart_pane_terminal",
            "write_to_pane",
            "resize_pane_terminal",
            "set_active_pane",
            "update_workspace_layout",
            "get_config",
            "write_config",
            "create_agent_pane",
            "send_agent_message",
            "agent_approval",
            "interrupt_agent",
            "install_update",
        ]),
    ))
    .expect("failed to build Sgian Tauri app");
}
