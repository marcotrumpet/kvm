//! Test harness for the core: connects two machines without any OS input integration.
//!
//!   kvm-cli id
//!   kvm-cli run --peer <ID of the other machine> --synthetic 5 --bench 10
//!
//!   kvm-cli discover [--accept]       list KVM machines on this network
//!   kvm-cli pair --peer <ID>          ask a machine to pair
//!   kvm-cli host                      act as a computer for remote controls (iPhone, iPad):
//!                                     accept every pairing, print what arrives
//!
//! Use `--state <dir>` (and `--name`) to run two instances on one machine.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use clap::{Parser, Subcommand};
use kvm_core::{EndpointId, Event, Link, Node, NodeEvent, VideoParams};

#[derive(Parser)]
struct Cli {
    /// Directory holding this machine's identity [default: ~/.kvm-cli].
    #[arg(long)]
    state: Option<PathBuf>,
    /// Name shown to other machines [default: hostname].
    #[arg(long)]
    name: Option<String>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Print this machine's id (share it with the peer).
    Id,
    /// Connect to the peer and print what arrives.
    Run {
        #[arg(long)]
        peer: EndpointId,
        /// Take control of the peer and send 1 kHz synthetic motion for this many seconds.
        #[arg(long)]
        synthetic: Option<u64>,
        /// Ping at 100 Hz for this many seconds and report round-trip latency.
        #[arg(long)]
        bench: Option<u64>,
        /// View the peer's (synthetic) screen for this many seconds and report frame stats.
        #[arg(long)]
        view: Option<u64>,
        /// Bitrate to ask for when viewing, in kbit/s.
        #[arg(long, default_value_t = 8000)]
        kbps: u32,
        /// Send these files or folders once connected.
        #[arg(long, num_args = 1..)]
        send: Vec<PathBuf>,
        /// Send this text as clipboard contents once connected.
        #[arg(long)]
        clip: Option<String>,
        /// Send `--send` files as clipboard contents (to the peer's clipboard cache).
        #[arg(long)]
        clip_files: bool,
        /// Where received files go [default: ~/Downloads].
        #[arg(long)]
        downloads: Option<PathBuf>,
    },
    /// Print KVM machines discovered on the local network and pairing requests.
    Discover {
        /// Accept every pairing request.
        #[arg(long)]
        accept: bool,
    },
    /// Ask a machine to pair, and wait for its answer.
    Pair {
        #[arg(long)]
        peer: EndpointId,
    },
    /// Accept remote controls (pairing included) and print the input they send.
    Host {
        /// Remote controls already paired.
        #[arg(long, num_args = 1..)]
        allow: Vec<EndpointId>,
    },
}

#[derive(Default)]
struct Stats {
    moves: u64,
    dx: i64,
    dy: i64,
    keys: u64,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "warn,kvm_core=info".into()),
        )
        .with_writer(std::io::stderr)
        .init();
    let cli = Cli::parse();
    let state = cli.state.unwrap_or_else(|| {
        PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".kvm-cli")
    });
    let host = cli.name.unwrap_or_else(|| {
        std::process::Command::new("hostname")
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_else(|_| "kvm-cli".into())
    });
    let node = Node::bind(&state, host).await?;
    let (peer, synthetic, bench, view, kbps, send, clip, clip_files) = match cli.command {
        Command::Run { peer, synthetic, bench, view, kbps, send, clip, clip_files, downloads } => {
            if let Some(dir) = downloads {
                kvm_core::set_clipboard_dir(dir.join("clipboard"));
                kvm_core::set_download_dir(dir);
            }
            (peer, synthetic, bench, view, kbps, send, clip, clip_files)
        }
        Command::Id => {
            println!("{}", node.id());
            node.close().await;
            return Ok(());
        }
        Command::Discover { accept } => return watch_node(node, None, accept).await,
        Command::Pair { peer } => return watch_node(node, Some(peer), false).await,
        Command::Host { allow } => return host_remotes(node, allow).await,
    };
    println!("me:   {}\npeer: {peer}", node.id());

    let (link, mut events) = node.link(peer);
    let link = Arc::new(link);
    let mut started_jobs = false;
    let mut latencies = Vec::new();
    let mut bench_until = None;
    let mut stats = Stats::default();
    let mut host_video: Option<tokio::task::JoinHandle<()>> = None;
    let host_kbps = Arc::new(std::sync::atomic::AtomicU32::new(0));
    let mut view_until: Option<Instant> = None;
    let (mut frames, mut frame_bytes, mut keyframes, mut keyframe_requests) = (0u64, 0u64, 0u64, 0u64);
    let mut window = Stats::default();
    let mut tick = tokio::time::interval(Duration::from_secs(1));

    loop {
        tokio::select! {
            Some(event) = events.recv() => match event {
                Event::Move { dx, dy } => {
                    for s in [&mut stats, &mut window] {
                        s.moves += 1;
                        s.dx += dx;
                        s.dy += dy;
                    }
                }
                Event::Latency(rtt) => {
                    if bench_until.is_some_and(|t| Instant::now() < t) {
                        latencies.push(rtt);
                    }
                }
                Event::Key { code, down } => {
                    stats.keys += 1;
                    println!("key 0x{code:02x} {}", if down { "down" } else { "up" });
                }
                Event::Left => {
                    println!("left: {} motion updates, total dx={} dy={}, {} key events",
                        stats.moves, stats.dx, stats.dy, stats.keys);
                    stats = Stats::default();
                }
                Event::VideoRequested(params) => {
                    println!("VideoRequested({params:?}): sending synthetic frames");
                    host_video = Some(tokio::spawn(synthetic_video(link.clone(), params, host_kbps.clone())));
                }
                Event::VideoStopped => {
                    println!("VideoStopped");
                    if let Some(task) = host_video.take() {
                        task.abort();
                    }
                }
                Event::VideoBitrate(kbps) => {
                    println!("  host: bitrate -> {kbps} kbit/s");
                    host_kbps.store(kbps, std::sync::atomic::Ordering::Relaxed);
                }
                Event::Clipboard(bytes) => {
                    println!("Clipboard: {} bytes: {:?}", bytes.len(), String::from_utf8_lossy(&bytes[..bytes.len().min(60)]));
                }
                Event::FileSendProgress { .. } | Event::FileReceiveProgress { .. } => {}
                Event::KeyframeRequested => {
                    keyframe_requests += 1;
                }
                Event::VideoFrame { payload, keyframe } => {
                    frames += 1;
                    frame_bytes += payload.len() as u64;
                    keyframes += keyframe as u64;
                }
                Event::Connected { .. } => {
                    println!("{event:?}");
                    if !started_jobs {
                        started_jobs = true;
                        if let Some(text) = clip.clone() {
                            link.send_clipboard(text.into_bytes());
                        }
                        if !send.is_empty() {
                            println!("sending files: transfer {:?}", link.send_files(send.clone(), clip_files));
                        }
                        if let Some(secs) = view {
                            view_until = Some(Instant::now() + Duration::from_secs(secs));
                            link.request_video(VideoParams { max_width: 1920, max_height: 1080, fps: 60, bitrate_kbps: kbps, prefer: 0, display: 0, fit_host: 0, codecs: 0, audio: false, text_size: 0 });
                        }
                        if let Some(secs) = synthetic {
                            tokio::spawn(synthetic_input(link.clone(), secs));
                        }
                        if let Some(secs) = bench {
                            bench_until = Some(Instant::now() + Duration::from_secs(secs));
                            tokio::spawn(bench_pings(link.clone(), secs));
                        }
                    }
                }
                other => println!("{other:?}"),
            },
            _ = tick.tick() => {
                if window.moves > 0 {
                    println!("  {} motion updates/s (dx={} dy={})", window.moves, window.dx, window.dy);
                    window = Stats::default();
                }
                if view_until.is_some() && frames > 0 {
                    println!("  video: {frames} frames/s, {:.1} Mbit/s, {keyframes} keyframes, {keyframe_requests} keyframe requests",
                        frame_bytes as f64 * 8.0 / 1e6);
                    (frames, frame_bytes, keyframes) = (0, 0, 0);
                }
                if view_until.is_some_and(|t| Instant::now() >= t) {
                    view_until = None;
                    link.stop_video();
                    println!("view: stopped");
                }
                if bench_until.is_some_and(|t| Instant::now() >= t) {
                    bench_until = None;
                    report_latency(&mut latencies);
                }
            }
            _ = tokio::signal::ctrl_c() => break,
        }
    }
    drop(link);
    node.close().await;
    Ok(())
}

/// Accepts every remote control that asks to pair, and prints what they send.
async fn host_remotes(node: Node, allow: Vec<EndpointId>) -> Result<()> {
    println!("me: {}", node.id());
    node.set_discoverable(true);
    let mut allowed: std::collections::HashSet<_> = allow.into_iter().collect();
    let mut node_events = node.events().expect("events taken once");
    let (mut link, mut events) = node.accept_remotes(allowed.clone());
    loop {
        tokio::select! {
            Some(event) = node_events.recv() => {
                println!("{event:?}");
                if let NodeEvent::PairRequest { id, remote: true, .. } = event {
                    println!("accepting: {}", node.respond_pairing(id, true));
                    allowed.insert(id);
                    drop(link);
                    (link, events) = node.accept_remotes(allowed.clone());
                }
            }
            Some(event) = events.recv() => match event {
                Event::Connected { .. } => {
                    println!("{event:?}");
                    link.send_clipboard(b"\x00\x01\x00\x0atext/plain\x00\x00\x00\x05hello".to_vec());
                }
                Event::Latency(_) => {}
                other => println!("{other:?}"),
            },
            _ = tokio::signal::ctrl_c() => break,
        }
    }
    drop(link);
    node.close().await;
    Ok(())
}

/// Prints machine-level events; with `pair`, exits once that machine has answered.
async fn watch_node(node: Node, pair: Option<EndpointId>, accept: bool) -> Result<()> {
    println!("me: {}", node.id());
    node.set_discoverable(pair.is_none());
    let mut events = node.events().expect("events taken once");
    if let Some(peer) = pair {
        node.request_pairing(peer, false);
    }
    loop {
        tokio::select! {
            Some(event) = events.recv() => {
                println!("{event:?}");
                match event {
                    NodeEvent::PairRequest { id, .. } if accept => {
                        println!("accepting: {}", node.respond_pairing(id, true));
                    }
                    NodeEvent::PairResult { id, .. } if Some(id) == pair => break,
                    _ => {}
                }
            }
            _ = tokio::signal::ctrl_c() => break,
        }
    }
    node.close().await;
    Ok(())
}

async fn synthetic_input(link: Arc<Link>, secs: u64) {
    tokio::time::sleep(Duration::from_millis(500)).await;
    link.enter();
    link.send_key(0xE1, true); // left shift, held for the whole run
    let mut tick = tokio::time::interval(Duration::from_millis(1));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Burst);
    let end = Instant::now() + Duration::from_secs(secs);
    let mut sent = 0i64;
    while Instant::now() < end {
        tick.tick().await;
        link.send_motion(1, 0, 0, 0);
        sent += 1;
    }
    link.send_key(0x04, true); // 'a'
    link.send_key(0x04, false);
    // Shift is still held: the peer must release it when control leaves.
    link.leave();
    println!("synthetic: sent {sent} motion updates (expect peer dx={sent}), 'a' tap, shift held then leave");
}

/// Stands in for screen capture + encoding: frames sized for the requested bitrate, with a
/// keyframe (4x larger) every second or whenever a frame was refused or requested.
async fn synthetic_video(link: Arc<Link>, params: VideoParams, kbps: Arc<std::sync::atomic::AtomicU32>) {
    let fps = params.fps.max(1) as u64;
    let mut tick = tokio::time::interval(Duration::from_micros(1_000_000 / fps));
    let (mut n, mut refused, mut force_key) = (0u64, 0u64, true);
    loop {
        tick.tick().await;
        let rate = match kbps.load(std::sync::atomic::Ordering::Relaxed) {
            0 => params.bitrate_kbps,
            k => k,
        };
        let frame_bytes = (rate as u64 * 1000 / 8 / fps) as usize;
        let keyframe = force_key || n % fps == 0;
        let size = if keyframe { frame_bytes * 4 } else { frame_bytes };
        if link.send_video_frame(vec![0xAB; size], keyframe) {
            force_key = false;
        } else {
            refused += 1;
            force_key = true;
            if refused % 30 == 1 {
                println!("  host: {refused} frames refused so far (link behind)");
            }
        }
        n += 1;
    }
}

async fn bench_pings(link: Arc<Link>, secs: u64) {
    let end = Instant::now() + Duration::from_secs(secs);
    let mut tick = tokio::time::interval(Duration::from_millis(10));
    while Instant::now() < end {
        tick.tick().await;
        link.ping();
    }
}

fn report_latency(samples: &mut Vec<Duration>) {
    if samples.is_empty() {
        println!("bench: no samples");
        return;
    }
    samples.sort();
    let pct = |p: f64| samples[((samples.len() - 1) as f64 * p) as usize];
    println!(
        "bench: {} samples, rtt p50={:?} p95={:?} p99={:?} max={:?}",
        samples.len(),
        pct(0.50),
        pct(0.95),
        pct(0.99),
        samples[samples.len() - 1]
    );
    samples.clear();
}
