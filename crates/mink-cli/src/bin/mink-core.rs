fn main() {
    mink_cli::cli::install_panic_hook();
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Err(e) = mink_cli::cli::bootstrap_sandbox_if_needed(&args) {
        eprintln!("Error: {e}");
        std::process::exit(1);
    }
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("Error: failed to start async runtime: {e}");
            std::process::exit(1);
        }
    };
    match runtime.block_on(mink_cli::cli::main_entry(args)) {
        Ok(exit) => {
            if exit.code != 0 {
                std::process::exit(exit.code);
            }
        }
        Err(e) => {
            eprintln!("Error: {e}");
            std::process::exit(1);
        }
    }
}
