#![allow(dead_code)]

use anyhow::Result;
use clap::{CommandFactory, Parser, ValueEnum};
use clap_complete::{generate, Generator, Shell};
use clap_complete_nushell::Nushell;
use log::error;
use merge::Merge;
use rwalk::{
    _main,
    cli::{self, opts::Opts},
    utils,
};
use std::{fs::OpenOptions, path::Path, process};

#[tokio::main]
async fn main() -> Result<()> {
    utils::logger::init_logger();

    let mut opts = Opts::parse();
    if let Some(p) = opts.config {
        opts = Opts::from_path(p.clone()).await?;
        log::info!("Using config file: {}", p);
    } else if let Some(home) = dirs::home_dir() {
        let p = home.join(Path::new(".config/rwalk/config.toml"));
        if p.exists() {
            let path_opts = Opts::from_path(p.clone()).await?;
            opts.merge(path_opts);
            log::info!("Using config file: {}", p.display());
        }
    }

    if opts.generate_markdown {
        clap_markdown::print_help_markdown::<Opts>();
        process::exit(0);
    }
    if opts.generate_completions {
        for s in Shell::value_variants().iter() {
            let dir = Path::new("completions");
            if !dir.exists() {
                std::fs::create_dir_all(dir)?;
            }
            let mut file = OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .open(dir.join(s.file_name("rwalk")))?;
            generate(*s, &mut Opts::command(), "rwalk", &mut file);
        }

        // Generate completions for nushell
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open("completions/rwalk.nu")?;
        generate(Nushell, &mut Opts::command(), "rwalk", &mut file);

        log::info!("Generated completions");
        process::exit(0);
    }
    if opts.no_color {
        colored::control::set_override(false);
    }
    if !opts.quiet {
        utils::banner();
    }
    let res = if opts.interactive {
        cli::interactive::main().await
    } else {
        _main(opts.clone()).await
    };
    if let Err(e) = res {
        error!("{}", e);
        process::exit(1);
    }
    process::exit(0);
}
