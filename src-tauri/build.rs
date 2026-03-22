fn main() {
    #[cfg(target_os = "windows")]
    tauri_build::try_build(
        tauri_build::Attributes::new()
            .windows_attributes(
                tauri_build::WindowsAttributes::new()
                    .app_manifest(include_str!("zapret-gui.exe.manifest"))
            )
    ).expect("tauri-build failed");

    #[cfg(not(target_os = "windows"))]
    tauri_build::build()
}
