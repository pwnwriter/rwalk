#![allow(dead_code)]

use crate::{
    constants::{ERROR, INFO, SAVE_FILE, SUCCESS, WARNING},
    tree::{Tree, TreeData},
    utils::{apply_filters, apply_transformations, parse_wordlists},
};
use anyhow::Result;
use clap::Parser;
use cli::Opts;
use colored::Colorize;
use futures::future::abortable;
use futures::stream::StreamExt;
use indicatif::HumanDuration;
use parking_lot::Mutex;
use ptree::print_tree;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use signal_hook::consts::signal::*;
use signal_hook_tokio::Signals;
use std::{
    collections::HashMap,
    io::Write,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

use url::Url;

mod cli;
mod constants;
mod core;
mod tree;
mod utils;

#[derive(Serialize, Deserialize)]
struct Save {
    tree: Arc<Mutex<Tree<TreeData>>>,
    depth: Arc<Mutex<usize>>,
    wordlist_checksum: String,
    indexes: HashMap<String, Vec<usize>>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let opts = Opts::parse();
    if opts.no_color {
        colored::control::set_override(false);
    }
    if !opts.quiet {
        utils::banner();
    }
    if opts.interactive {
        cli::main_interactive().await
    } else {
        _main(opts.clone()).await
    }
}

pub async fn _main(opts: Opts) -> Result<()> {
    let mut words = parse_wordlists(&opts.wordlists);
    let before = words.len();
    apply_filters(&opts, &mut words)?;
    apply_transformations(&opts, &mut words);
    words.sort_unstable();
    words.dedup();
    let after = words.len();
    if before != after {
        println!(
            "{} {} words loaded, {} after deduplication and filters (-{}%)",
            INFO.to_string().blue(),
            before.to_string().bold(),
            after.to_string().bold(),
            ((before - after) as f64 / before as f64 * 100.0)
                .round()
                .to_string()
                .bold()
        );
    } else {
        println!(
            "{} {} words loaded",
            INFO.to_string().blue(),
            before.to_string().bold()
        );
    }
    if words.len() == 0 {
        println!("{} No words found in wordlists", ERROR.to_string().red());
        std::process::exit(1);
    }
    let depth = Arc::new(Mutex::new(0));
    let current_indexes: Arc<Mutex<HashMap<String, Vec<usize>>>> =
        Arc::new(Mutex::new(HashMap::new()));
    let saved = std::fs::read_to_string(opts.save_file.clone());
    let saved = if opts.resume {
        match saved {
            Ok(saved) => {
                let saved: Save = serde_json::from_str(&saved)?;
                if let Some(root) = &saved.tree.clone().lock().root {
                    if root.lock().data.url != opts.url.clone().unwrap() {
                        None
                    } else {
                        print_tree(&*root.lock())?;
                        println!(
                            "{} Found saved state crawled to depth {}",
                            INFO.to_string().blue(),
                            (*saved.depth.lock() + 1).to_string().blue()
                        );

                        *depth.lock() = *saved.depth.lock();
                        if saved.wordlist_checksum == {
                            let mut hasher = Sha256::new();
                            hasher.update(words.join("\n"));
                            format!("{:x}", hasher.finalize())
                        } {
                            *current_indexes.lock() = saved.indexes;
                        } else {
                            println!(
                                "{} Wordlists have changed, starting from scratch at depth {}",
                                WARNING.to_string().yellow(),
                                (*saved.depth.lock() + 1).to_string().yellow()
                            );
                        }
                        Some(saved.tree)
                    }
                } else {
                    None
                }
            }
            Err(_) => None,
        }
    } else {
        None
    };
    let has_saved = saved.is_some();
    let tree = if let Some(saved) = saved {
        saved
    } else {
        let t = Arc::new(Mutex::new(Tree::new()));
        t.lock().insert(
            TreeData {
                url: opts.url.clone().unwrap(),
                depth: 0,
                path: Url::parse(&opts.url.clone().unwrap())?
                    .path()
                    .to_string()
                    .trim_end_matches('/')
                    .to_string(),
                status_code: 0,
            },
            None,
        );
        t
    };

    let threads = opts
        .threads
        .unwrap_or(num_cpus::get() * 10)
        .max(1)
        .min(words.len());
    println!(
        "{} Starting crawler with {} threads",
        INFO.to_string().blue(),
        threads.to_string().bold(),
    );

    let watch = stopwatch::Stopwatch::start_new();

    println!(
        "{} Press {} to save state and exit",
        INFO.to_string().blue(),
        "Ctrl+C".bold()
    );
    let chunks = words
        .chunks(words.len() / threads)
        .map(|x| x.to_vec())
        .collect::<Vec<_>>();
    let chunks = Arc::new(chunks);
    let (task, handle) = abortable(core::start(
        opts.clone(),
        depth.clone(),
        tree.clone(),
        current_indexes.clone(),
        chunks.clone(),
        words.clone(),
    ));

    let main_thread = tokio::spawn(task);
    let aborted = Arc::new(AtomicBool::new(false));
    let (tx, mut rx) = tokio::sync::mpsc::channel::<()>(1);

    let ctrlc_tree = tree.clone();
    let ctrlc_depth = depth.clone();
    let ctrlc_words = words.clone();
    let ctrlc_aborted = aborted.clone();
    let ctrlc_save_file = opts.save_file.clone();
    let mut signals = Signals::new(&[SIGINT])?;
    let ctrlc_handle = signals.handle();

    let signals_task = tokio::spawn(async move {
        while let Some(signal) = signals.next().await {
            match signal {
                SIGINT => {
                    println!("{} Aborting...", INFO.to_string().blue().bold());
                    ctrlc_aborted.store(true, Ordering::Relaxed);
                    handle.abort();
                    if !opts.no_save {
                        let checksum = {
                            let mut hasher = Sha256::new();
                            hasher.update(ctrlc_words.join("\n"));
                            hasher.finalize()
                        };
                        let checksum = format!("{:x}", checksum);
                        let content = serde_json::to_string(&Save {
                            tree: ctrlc_tree.clone(),
                            depth: ctrlc_depth.clone(),
                            wordlist_checksum: checksum,
                            indexes: current_indexes.lock().clone(),
                        });
                        match content {
                            Ok(content) => {
                                let mut file = std::fs::File::create(&ctrlc_save_file).unwrap();
                                file.write_all(content.as_bytes()).unwrap();
                                file.flush().unwrap();
                                print!("\x1B[2K\r");
                                println!(
                                    "{} Saved state to {}",
                                    SUCCESS.to_string().green(),
                                    ctrlc_save_file.bold()
                                );
                            }
                            Err(_) => {}
                        }
                    }
                    tx.send(()).await.unwrap();
                }
                _ => unreachable!(),
            }
        }
    });
    let res = main_thread.await?;
    if let Ok(_) = res {
        println!(
            "{} Done in {} with an average of {} req/s",
            SUCCESS.to_string().green(),
            HumanDuration(watch.elapsed()).to_string().bold(),
            ((words.len() * *depth.lock()) as f64 / watch.elapsed().as_secs_f64())
                .round()
                .to_string()
                .bold()
        );

        let root = tree.lock().root.clone().unwrap().clone();

        print_tree(&*root.lock())?;

        // Remove save file if it's the default one
        if has_saved && opts.save_file == SAVE_FILE {
            std::fs::remove_file(opts.save_file.clone())?;
        }
        if opts.output.is_some() {
            let output = opts.output.clone().unwrap();
            let file_type = output.split(".").last().unwrap_or("json");
            let mut file = std::fs::File::create(opts.output.clone().unwrap())?;

            match file_type {
                "json" => {
                    file.write_all(serde_json::to_string(&*root.lock())?.as_bytes())?;
                    file.flush()?;
                }
                "csv" => {
                    let mut writer = csv::Writer::from_writer(file);
                    let mut nodes = Vec::new();
                    for depth in 0..*depth.lock() {
                        nodes.append(&mut tree.lock().get_nodes_at_depth(depth));
                    }
                    for node in nodes {
                        writer.serialize(node.lock().data.clone())?;
                    }
                    writer.flush()?;
                }
                "md" => {
                    let mut nodes = Vec::new();
                    for depth in 0..*depth.lock() {
                        nodes.append(&mut tree.lock().get_nodes_at_depth(depth));
                    }
                    for node in nodes {
                        let data = node.lock().data.clone();
                        let emoji = utils::get_emoji_for_status_code(data.status_code);
                        let url = data.url;
                        let path = data.path;
                        let depth = data.depth;
                        let status_code = data.status_code;
                        let line = format!(
                            "{}- [{} /{} {}]({})",
                            "  ".repeat(depth),
                            emoji,
                            path.trim_start_matches("/"),
                            if status_code == 0 {
                                "".to_string()
                            } else {
                                format!("({})", status_code)
                            },
                            url,
                        );
                        file.write_all(line.as_bytes())?;
                        file.write_all(b"\n")?;
                    }
                    file.flush()?;
                }
                "txt" => {
                    let mut nodes = Vec::new();
                    for depth in 0..*depth.lock() {
                        nodes.append(&mut tree.lock().get_nodes_at_depth(depth));
                    }
                    for node in nodes {
                        let data = node.lock().data.clone();
                        file.write_all(data.url.as_bytes())?;
                        file.write_all(b"\n")?;
                    }
                    file.flush()?;
                }
                _ => {
                    println!(
                        "{} Invalid output file type",
                        ERROR.to_string().red().bold()
                    );
                    std::process::exit(1);
                }
            }

            println!("{} Saved to {}", SUCCESS.to_string().green(), output.bold());
        }
    }
    if aborted.load(Ordering::Relaxed) {
        rx.recv().await;
    }
    // Terminate the signal stream.
    ctrlc_handle.close();
    signals_task.await?;
    Ok(())
}
