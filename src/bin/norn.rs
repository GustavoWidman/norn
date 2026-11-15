use clap::Parser;

#[derive(Parser)]
#[command(version, about = "bring cloudflare in line with the fleet's nix configuration")]
struct Args {}

fn main() {
    Args::parse();
}
