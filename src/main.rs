mod config;
mod creds;
mod diff;
mod git;
mod project;
mod stage;
mod status;

fn main() -> eframe::Result {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("credential") {
        std::process::exit(creds::helper_main(&args[1..]));
    }
    eframe::run_native(
        "Spinney",
        eframe::NativeOptions::default(),
        Box::new(|_cc| Ok(Box::new(App))),
    )
}

struct App;

impl eframe::App for App {
    fn ui(&mut self, _ui: &mut eframe::egui::Ui, _frame: &mut eframe::Frame) {}
}
