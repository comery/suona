// Prevents an extra console window on Windows in release builds.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    // `suona --scan` runs the collectors headlessly and prints the unified
    // event stream.  Handy for verifying data sources without the GUI.
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--scan") {
        let days = args
            .iter()
            .position(|a| a == "--days")
            .and_then(|i| args.get(i + 1))
            .and_then(|v| v.parse::<i64>().ok())
            .unwrap_or(7);
        // Read the saved configuration so the headless scan matches what the
        // running app would report — including any agents switched off.
        let settings = suona_lib::app::load_settings_from_disk();
        let snapshot = suona_lib::app::scan(days, &settings.agents);
        match serde_json::to_string_pretty(&snapshot) {
            Ok(json) => println!("{json}"),
            Err(e) => eprintln!("failed to serialise snapshot: {e}"),
        }
        return;
    }

    suona_lib::run();
}
