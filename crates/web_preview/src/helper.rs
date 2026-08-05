#[cfg(target_os = "macos")]
#[path = "cef_paths.rs"]
// The helper only resolves and loads the framework; the rest of the module is for the
// browser process.
#[allow(dead_code)]
mod cef_paths;

#[cfg(target_os = "macos")]
fn main() {
    use cef::*;

    // The browser process resolves the framework the same way, so both ends of the
    // subprocess launch agree on which build of Chromium is running.
    let Some((framework_dir, _)) = cef_paths::find_framework() else {
        eprintln!("web_preview_helper: Chromium Embedded Framework not found");
        std::process::exit(1);
    };
    if !cef_paths::load_cef_library(&framework_dir) {
        eprintln!(
            "web_preview_helper: failed to load CEF library from {}",
            framework_dir.display()
        );
        std::process::exit(1);
    }
    let _ = api_hash(sys::CEF_API_VERSION_LAST, 0);

    let args = cef::args::Args::new();
    let exit_code = execute_process(Some(args.as_main_args()), None, std::ptr::null_mut());
    std::process::exit(exit_code);
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("web_preview_helper: only supported on macOS");
    std::process::exit(1);
}
