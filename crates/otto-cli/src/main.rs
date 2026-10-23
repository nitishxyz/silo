fn main() -> std::process::ExitCode {
    otto_cli::main_from(std::env::args_os().collect())
}
