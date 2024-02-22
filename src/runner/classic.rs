use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use crate::{
    cli::opts::Opts,
    utils::{
        constants::{ERROR, FUZZ_KEY, PROGRESS_CHARS, PROGRESS_TEMPLATE, SUCCESS, WARNING},
        tree::{Tree, TreeData},
    },
};
use anyhow::{anyhow, Result};
use colored::Colorize;
use indicatif::ProgressBar;
use itertools::Itertools;
use log::info;
use parking_lot::Mutex;
use reqwest::Client;
use serde_json::json;
use url::Url;

use super::Runner;

pub struct Classic {
    url: String,
    opts: Opts,
    tree: Arc<Mutex<Tree<TreeData>>>,
    words: Vec<String>,
    threads: usize,
}

impl Classic {
    pub fn new(
        url: String,
        opts: Opts,
        tree: Arc<Mutex<Tree<TreeData>>>,
        words: Vec<String>,
        threads: usize,
    ) -> Self {
        Self {
            url,
            opts,
            tree,
            words,
            threads,
        }
    }
    fn generate_urls(&self) -> Vec<String> {
        if self.opts.permutations {
            let token_count = self
                .url
                .matches(
                    self.opts
                        .fuzz_key
                        .clone()
                        .unwrap_or(FUZZ_KEY.to_string())
                        .as_str(),
                )
                .count();
            let combinations: Vec<_> = self.words.iter().permutations(token_count).collect();

            combinations
                .clone()
                .iter()
                .map(|c| {
                    let mut url = self.url.clone();
                    for word in c {
                        url = url.replace(
                            self.opts
                                .fuzz_key
                                .clone()
                                .unwrap_or(FUZZ_KEY.to_string())
                                .as_str(),
                            word,
                        );
                    }
                    url
                })
                .collect()
        } else {
            self.words
                .clone()
                .iter()
                .map(|c| {
                    let mut url = self.url.clone();
                    url = url.replace(
                        self.opts
                            .fuzz_key
                            .clone()
                            .unwrap_or(FUZZ_KEY.to_string())
                            .as_str(),
                        c,
                    );
                    url
                })
                .collect()
        }
    }

    // And another method for processing a chunk of URLs:
    async fn process_chunk(
        chunk: Vec<String>,
        client: Client,
        progress: ProgressBar,
        tree: Arc<Mutex<Tree<TreeData>>>,
        opts: Opts,
    ) -> Result<()> {
        for url in &chunk {
            let sender = super::client::get_sender(&opts, url, &client);

            let t1 = Instant::now();

            let response = sender.send().await;

            if let Some(throttle) = opts.throttle {
                if throttle > 0 {
                    let elapsed = t1.elapsed();
                    let sleep_duration = Duration::from_secs_f64(1.0 / throttle as f64);
                    if let Some(sleep) = sleep_duration.checked_sub(elapsed) {
                        tokio::time::sleep(sleep).await;
                    }
                }
            }
            match response {
                Ok(mut response) => {
                    let status_code = response.status().as_u16();
                    let mut text = String::new();
                    while let Ok(chunk) = response.chunk().await {
                        if let Some(chunk) = chunk {
                            text.push_str(&String::from_utf8_lossy(&chunk));
                        } else {
                            break;
                        }
                    }
                    let filtered = super::filters::check(
                        &opts,
                        &text,
                        status_code,
                        t1.elapsed().as_millis(),
                        None,
                    );

                    if filtered {
                        let additions = super::filters::parse_show(&opts, &text, &response);

                        progress.println(format!(
                            "{} {} {} {}{}",
                            if response.status().is_success() {
                                SUCCESS.to_string().green()
                            } else if response.status().is_redirection() {
                                WARNING.to_string().yellow()
                            } else {
                                ERROR.to_string().red()
                            },
                            response.status().as_str().bold(),
                            url,
                            format!("{}ms", t1.elapsed().as_millis().to_string().bold()).dimmed(),
                            additions.iter().fold("".to_string(), |acc, addition| {
                                format!(
                                    "{} | {}: {}",
                                    acc,
                                    addition.key.dimmed().bold(),
                                    addition.value.dimmed()
                                )
                            })
                        ));

                        let parsed = Url::parse(url)?;
                        let mut tree = tree.lock().clone();
                        let root_url = tree
                            .root
                            .clone()
                            .ok_or(anyhow!("Failed to get root URL from tree"))?
                            .lock()
                            .data
                            .url
                            .clone();
                        tree.insert(
                            TreeData {
                                url: url.clone(),
                                depth: 0,
                                path: parsed.path().to_string().replace(
                                    Url::parse(&root_url)?.path().to_string().as_str(),
                                    "",
                                ),
                                status_code,
                                extra: json!(additions),
                            },
                            tree.root.clone(),
                        );
                    }
                }
                Err(err) => {
                    if opts.hit_connection_errors && err.is_connect() {
                        progress.println(format!(
                            "{} {} {} {}",
                            SUCCESS.to_string().green(),
                            "Connection error".bold(),
                            url,
                            format!("{}ms", t1.elapsed().as_millis().to_string().bold()).dimmed()
                        ));
                        let parsed = Url::parse(url)?;
                        let mut tree = tree.lock().clone();
                        let root_url = tree
                            .root
                            .clone()
                            .ok_or(anyhow!("Failed to get root URL from tree"))?
                            .lock()
                            .data
                            .url
                            .clone();

                        tree.insert(
                            TreeData {
                                url: url.clone(),
                                depth: 0,
                                path: parsed.path().to_string().replace(
                                    Url::parse(&root_url)?.path().to_string().as_str(),
                                    "",
                                ),
                                status_code: 0,
                                extra: json!([]),
                            },
                            tree.root.clone(),
                        );
                    } else {
                        super::filters::print_error(&opts, &progress, url, err);
                    }
                }
            }
            progress.inc(1);
        }

        Ok(())
    }
}

impl Runner for Classic {
    async fn run(self) -> Result<()> {
        let spinner = ProgressBar::new_spinner();
        spinner.set_message("Generating URLs...".to_string());
        spinner.enable_steady_tick(Duration::from_millis(100));

        let urls: Vec<String> = self.generate_urls();
        spinner.finish_and_clear();
        info!("Generated {} URLs", urls.len().to_string().bold());

        let progress = ProgressBar::new(urls.len() as u64).with_style(
            indicatif::ProgressStyle::default_bar()
                .template(PROGRESS_TEMPLATE)?
                .progress_chars(PROGRESS_CHARS),
        );
        let chunks = urls.chunks(urls.len() / self.threads).collect::<Vec<_>>();
        let mut rxs = Vec::with_capacity(chunks.len());

        let client = super::client::build(&self.opts)?;

        for chunk in &chunks {
            let chunk = chunk.to_vec();
            let client = client.clone();
            let progress = progress.clone();
            let tree = self.tree.clone();
            let opts = self.opts.clone();
            let (tx, rx) = tokio::sync::mpsc::channel(1);
            tokio::spawn(async move {
                let res = Self::process_chunk(chunk, client, progress, tree, opts).await;
                tx.send(res).await.unwrap();
            });
            rxs.push(rx);
        }

        for mut rx in rxs {
            let res = rx
                .recv()
                .await
                .ok_or_else(|| anyhow!("Failed to receive result from worker thread"))?;
            if res.is_err() {
                return Err(res.err().unwrap());
            }
        }

        progress.finish_and_clear();

        Ok(())
    }
}
