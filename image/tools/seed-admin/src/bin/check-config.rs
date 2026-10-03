// Spike-only: load a node.toml the way bun does and report errors.
fn main() {
    for path in std::env::args().skip(1) {
        match reliaburger::config::node::NodeConfig::from_file(std::path::Path::new(&path)) {
            Ok(_) => println!("{path}: ok"),
            Err(e) => {
                println!("{path}: {e}");
                std::process::exit(1)
            }
        }
    }
}
