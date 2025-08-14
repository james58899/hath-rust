use vergen_gix::{Emitter, GixBuilder};

fn main() {
    // Windows manifest
    if cfg!(target_os = "windows") {
        add_manifest();
    }

    // Version
    if let Ok(gix) = GixBuilder::default().sha(true).build()
        && Emitter::new().fail_on_error().add_instructions(&gix).and_then(|e| e.emit()).is_ok()
    {
        return;
    }

    // Fallback
    println!("cargo:rustc-env=VERGEN_GIT_SHA=unknown");
}

#[cfg(not(windows))]
fn add_manifest() {
    unreachable!("Only add manifest on windows");
}

#[cfg(windows)]
fn add_manifest() {
    use std::{env, fs, path::PathBuf};

    let version = env!("CARGO_PKG_VERSION");
    let manifest = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<assembly xmlns="urn:schemas-microsoft-com:asm.v1" manifestVersion="1.0"
    xmlns:asmv3="urn:schemas-microsoft-com:asm.v3">
    <assemblyIdentity type="win32" name="James58899.Hath-rust" version="{version}.0" />
    <compatibility xmlns="urn:schemas-microsoft-com:compatibility.v1">
        <application>
            <!-- Windows 11 24H2 -->
            <supportedOS Id="{{8e0f7a12-bfb3-4fe8-b9a5-48fd50a15a9a}}" />
            <maxversiontested Id="10.0.26100.0" />
        </application>
    </compatibility>
    <trustInfo xmlns="urn:schemas-microsoft-com:asm.v2">
        <security>
            <requestedPrivileges xmlns="urn:schemas-microsoft-com:asm.v3">
                <requestedExecutionLevel level="asInvoker" uiAccess="false" />
            </requestedPrivileges>
        </security>
    </trustInfo>
    <asmv3:application>
        <asmv3:windowsSettings>
            <longPathAware xmlns="http://schemas.microsoft.com/SMI/2016/WindowsSettings">true</longPathAware>
            <activeCodePage xmlns="http://schemas.microsoft.com/SMI/2019/WindowsSettings">UTF-8</activeCodePage>
            <heapType xmlns="http://schemas.microsoft.com/SMI/2020/WindowsSettings">SegmentHeap</heapType>
        </asmv3:windowsSettings>
    </asmv3:application>
</assembly>"#
    );

    // write manifest
    let manifest_path = PathBuf::from(env::var("OUT_DIR").unwrap_or(".".into())).join("hath-rust.exe.manifest");
    fs::write(&manifest_path, &manifest).unwrap();

    let mut res = tauri_winres::WindowsResource::new();
    res.set("OriginalFilename", "hath-rust.exe");
    res.set("FileDescription", "hath-rust - Hentai@Home but rusty");
    res.set("LegalCopyright", "GPL-3.0-or-later");
    res.set_manifest_file(manifest_path.to_str().unwrap());
    res.compile().unwrap()
}
