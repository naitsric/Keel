#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    // Claude Code runs Keel itself before each Bash command of a step that can edit. That answers
    // and leaves before anything of the app starts.
    let mut args = std::env::args_os().skip(1);
    if args
        .next()
        .is_some_and(|arg| arg == keel_harness::bash_edits::HOOK_ARG)
    {
        let code = args.next().map_or(0, |workspace| {
            keel_harness::bash_edits::run_hook(std::path::Path::new(&workspace))
        });
        std::process::exit(code);
    }
    keel_lib::run()
}
