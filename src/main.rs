//! Entry point for the `emotibit` command-line client. The body lives in `cli.rs`.

mod cli;

fn main() -> anyhow::Result<()> {
    cli::run()
}
