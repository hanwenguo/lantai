use std::process::ExitCode;

fn main() -> ExitCode {
    match lantai::cli::run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}
