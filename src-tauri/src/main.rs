#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    if std::env::args().nth(1).as_deref() == Some(file_history_tracker_lib::extract::PDF_HELPER_ARG) {
        std::process::exit(file_history_tracker_lib::extract::run_pdf_helper());
    }
    file_history_tracker_lib::run()
}
