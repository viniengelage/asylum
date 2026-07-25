#[cfg(target_os = "macos")]
fn main() {
    use cef::*;

    let loader =
        library_loader::LibraryLoader::new(&std::env::current_exe().unwrap(), false);
    assert!(loader.load(), "Failed to load CEF library");
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
