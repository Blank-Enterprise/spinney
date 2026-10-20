mod actions;
mod config;
mod conflict;
mod convert;
mod creds;
mod diff;
mod git;
mod github;
mod graph;
mod project;
mod refs;
mod ssh;
mod stage;
mod stash;
mod status;
mod ui;

fn main() -> eframe::Result {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("credential") {
        std::process::exit(creds::helper_main(&args[1..]));
    }
    eframe::run_native(
        "Spinney",
        eframe::NativeOptions::default(),
        Box::new(|cc| Ok(Box::new(ui::SpinneyApp::new(cc.egui_ctx.clone())))),
    )
}
