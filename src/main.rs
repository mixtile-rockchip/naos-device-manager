fn main() -> std::process::ExitCode {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    naos_core::cli::run(&args, &mut std::io::stdout(), &mut std::io::stderr()).into()
}
