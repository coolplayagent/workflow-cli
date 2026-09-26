fn main() {
    std::process::exit(workflow_cli::run(
        std::env::args().skip(1),
        &mut std::io::stdout(),
        &mut std::io::stderr(),
    ));
}
