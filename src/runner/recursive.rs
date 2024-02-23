use colored::Colorize;
use serde_json::json;
use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{anyhow, Result};
use parking_lot::Mutex;

use crate::{
    cli::opts::Opts,
    utils::{
        constants::{DEFAULT_DEPTH, ERROR, PROGRESS_CHARS, PROGRESS_TEMPLATE, SUCCESS, WARNING},
        tree::{Tree, TreeData, TreeNode},
    },
};

pub struct Recursive {
    opts: Opts,
    depth: Arc<Mutex<usize>>,
    tree: Arc<Mutex<Tree<TreeData>>>,
    current_indexes: Arc<Mutex<HashMap<String, Vec<usize>>>>,
    chunks: Arc<Vec<Vec<String>>>,
    words: Vec<String>,
}

impl super::Runner for Recursive {
    async fn run(self) -> Result<()> {
        while *self.depth.lock() < self.opts.depth.unwrap_or(DEFAULT_DEPTH) {
            let previous_nodes = self.tree.lock().get_nodes_at_depth(*self.depth.lock());
            let root_progress = indicatif::MultiProgress::new();
            let mut progresses = HashMap::new();
            let mut rxs = Vec::new();
            let depth = self.depth.clone();
            for previous_node in &previous_nodes {
                let depth = depth.clone();
                let mut indexes = self.current_indexes.lock();
                let index = indexes
                    .entry(previous_node.lock().data.url.clone())
                    .or_insert_with(|| vec![0; self.chunks.len()]);

                let pb = root_progress
                    .add(indicatif::ProgressBar::new((self.words.len()) as u64))
                    .with_style(
                        indicatif::ProgressStyle::default_bar()
                            .template(PROGRESS_TEMPLATE)?
                            .progress_chars(PROGRESS_CHARS),
                    )
                    .with_message(format!(
                        "/{}",
                        previous_node.lock().data.path.trim_start_matches('/')
                    ))
                    .with_prefix(format!("d={}", *depth.lock()))
                    .with_position(index.iter().sum::<usize>() as u64);
                pb.enable_steady_tick(Duration::from_millis(100));
                progresses.insert(previous_node.lock().data.url.clone(), pb);

                let progress = progresses
                    .get(&previous_node.lock().data.url)
                    .ok_or(anyhow!(
                        "Couldn't find progress bar for {}",
                        previous_node.lock().data.url
                    ))?;

                let client = super::client::build(&self.opts)?;

                for (i, chunk) in self.chunks.iter().enumerate() {
                    let tree = self.tree.clone();
                    let previous_node = previous_node.clone();
                    let chunk = chunk.clone();
                    let client = client.clone();
                    let progress = progress.clone();
                    let indexes = self.current_indexes.clone();
                    let opts = self.opts.clone();
                    let depth = depth.clone();
                    let (tx, rx) = tokio::sync::mpsc::channel(1);
                    tokio::spawn(async move {
                        let res = Self::process_chunk(
                            chunk,
                            client,
                            progress,
                            tree,
                            opts,
                            depth,
                            previous_node,
                            indexes,
                            i,
                        )
                        .await;
                        tx.send(res).await.unwrap();
                    });
                    rxs.push(rx);
                }
            }

            for mut rx in rxs {
                let res = rx.recv().await.ok_or_else(|| {
                    anyhow::anyhow!("Failed to receive result from worker thread")
                })?;
                if res.is_err() {
                    return Err(res.err().unwrap());
                }
            }

            // Go to the next depth (/a/b/c -> /a/b/c/d)
            *depth.lock() += 1;
        }
        Ok(())
    }
}

impl Recursive {
    pub fn new(
        opts: Opts,
        depth: Arc<Mutex<usize>>,
        tree: Arc<Mutex<Tree<TreeData>>>,
        current_indexes: Arc<Mutex<HashMap<String, Vec<usize>>>>,
        chunks: Arc<Vec<Vec<String>>>,
        words: Vec<String>,
    ) -> Self {
        Self {
            opts,
            depth,
            tree,
            current_indexes,
            chunks,
            words,
        }
    }
    #[allow(clippy::too_many_arguments)]
    async fn process_chunk(
        chunk: Vec<String>,
        client: reqwest::Client,
        progress: indicatif::ProgressBar,
        tree: Arc<Mutex<Tree<TreeData>>>,
        opts: Opts,
        depth: Arc<Mutex<usize>>,
        previous_node: Arc<Mutex<TreeNode<TreeData>>>,
        indexes: Arc<Mutex<HashMap<String, Vec<usize>>>>,
        i: usize,
    ) -> Result<()> {
        while indexes
            .lock()
            .get_mut(&previous_node.lock().data.url)
            .ok_or(anyhow!("Couldn't find indexes for the previous node"))?[i]
            < chunk.len()
        {
            let index = indexes
                .lock()
                .get_mut(&previous_node.lock().data.url)
                .ok_or(anyhow!("Couldn't find indexes for the previous node"))?[i];

            let word = chunk[index].clone();
            let data = previous_node.lock().data.clone();

            let mut url = data.url.clone();
            match url.ends_with('/') {
                true => url.push_str(&word),
                false => url.push_str(&format!("/{}", word)),
            }

            let sender = super::client::get_sender(&opts, &url, &client);

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
                        Some(*depth.lock()),
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
                        // Check if this path is already in the tree
                        if !previous_node
                            .lock()
                            .children
                            .iter()
                            .any(|child| child.lock().data.path == *word)
                        {
                            tree.lock().insert(
                                TreeData {
                                    url: url.clone(),
                                    depth: data.depth + 1,
                                    path: word.clone(),
                                    status_code,
                                    extra: json!(additions),
                                },
                                Some(previous_node.clone()),
                            );
                        } else {
                            progress.println(format!(
                                "{} {} {}",
                                WARNING.to_string().yellow(),
                                "Already in tree".bold(),
                                url
                            ));
                        }
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
                        if !previous_node
                            .lock()
                            .children
                            .iter()
                            .any(|child| child.lock().data.path == *word)
                        {
                            tree.lock().insert(
                                TreeData {
                                    url: url.clone(),
                                    depth: data.depth + 1,
                                    path: word.clone(),
                                    status_code: 0,
                                    extra: json!([]),
                                },
                                Some(previous_node.clone()),
                            );
                        } else {
                            progress.println(format!(
                                "{} {} {}",
                                WARNING.to_string().yellow(),
                                "Already in tree".bold(),
                                url
                            ));
                        }
                    } else {
                        super::filters::print_error(&opts, &progress, &url, err);
                    }
                }
            }
            // Increase the index of the current chunk in the hashmap
            indexes
                .lock()
                .get_mut(&previous_node.lock().data.url)
                .ok_or(anyhow!("Couldn't find indexes for the previous node"))?[i] += 1;
            progress.inc(1);
        }

        Ok(())
    }
}
