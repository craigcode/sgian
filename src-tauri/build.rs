fn main() {
    // `sgian serve` embeds ../dist with include_dir!, which needs the directory
    // to exist at compile time. A daemon-only build (cargo test in CI) has no
    // frontend build; an empty directory then serves nothing, and `serve`
    // says so at runtime.
    let dist = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../dist");
    let _ = std::fs::create_dir_all(&dist);
    println!("cargo:rerun-if-changed={}", dist.display());
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
