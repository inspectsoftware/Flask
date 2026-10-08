#![windows_subsystem = "windows"]

mod activity;
mod app;
mod connections;
mod format;
mod origin_view;
mod performance;
mod processes;
mod rules;
mod sampler;
mod services;
mod settings;
mod startup;

/// Window class, also the single-instance key. Development builds use their
/// own so they can run beside an installed copy.
const CLASS: &str = if cfg!(feature = "unelevated") { "SysCentral.Flask.Dev" } else { "SysCentral.Flask" };

fn main() {
    if !sc_ui::single_instance(CLASS) {
        return;
    }
    // Flask always runs elevated (see app.manifest); this lets it open
    // processes owned by other accounts.
    sc_core::actions::enable_debug_privilege();

    let settings = settings::Settings::load();
    let opts = sc_ui::WindowOptions {
        title: "Flask",
        class: CLASS,
        size: (1280.0, 800.0),
        min_size: app::MIN_SIZE,
        custom_frame: true,
        placement: settings.window,
    };
    let _ = sc_ui::run(opts, |win| app::App::new(win, settings));
}
