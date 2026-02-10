use clap::{Command, Parser};
use clap_complete::{generate, Generator, Shell};
use std::io;

/// Generate shell completions
#[derive(Parser, Debug)]
pub struct Cli {
    shell: Shell,
}

fn print_completions<G: Generator>(gen: G, cmd: &mut Command) {
    generate(gen, cmd, cmd.get_name().to_string(), &mut io::stdout());
}

pub fn completions(argv: Vec<String>) {
    let cli = Cli::parse_from(argv);
    print_completions(cli.shell, &mut super::build_cli());
}
