//! The job supervisor as a standalone binary (`agora-supervise --job-dir D`), used by tests; production runs the
//! same code as `agora _supervise`.

use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let code = agora_run::supervise::main_from_args(&args);
    ExitCode::from(u8::try_from(code).unwrap_or(1))
}
