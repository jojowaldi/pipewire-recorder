mod config;
mod index;
mod ipc;
mod mpris;
mod pipewire;
mod recorder;
mod tagger;

use std::path::{Path, PathBuf};

use anyhow::Result;
use clap::{Parser, Subcommand, ValueEnum};

use crate::config::{Config, ConfigFormat};
use crate::index::RecordedIndex;
use crate::mpris::{find_active_player_bus, list_mpris_players, query_player_snapshot};
use crate::pipewire::{
    CAPTURE_SINK_ROLE, find_matching_node, find_sink_by_name, list_audio_output_streams,
    list_audio_sinks, list_loopback_playback_nodes,
};

#[derive(Parser, Debug)]
#[command(
    name = "pipewire-recorder",
    version,
    about = "Automatic PipeWire stream recorder with MPRIS metadata, cover art, completeness validation, and deduplication"
)]
struct Cli {
    /// Explicit path to config.yaml or config.json
    #[arg(long, global = true)]
    config: Option<PathBuf>,

    /// Override output directory from config
    #[arg(long, global = true)]
    output_dir: Option<String>,

    /// Override target MPRIS player name (e.g. spotify, firefox, vlc)
    #[arg(long, global = true)]
    player: Option<String>,

    /// Override target PipeWire stream matcher (app name, binary, node.name, or serial)
    #[arg(long, global = true)]
    pipewire_target: Option<String>,

    /// Override virtual PipeWire capture sink + playback loopback (`--virtual-sink` or `--virtual-sink=false`)
    #[arg(long, global = true, num_args = 0..=1, default_missing_value = "true")]
    virtual_sink: Option<bool>,

    /// Override 100% unity volume enforcement (`--enforce-unity-volume=true|false`)
    #[arg(long, global = true, num_args = 0..=1, default_missing_value = "true")]
    enforce_unity_volume: Option<bool>,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum CliConfigFormat {
    Yaml,
    Json,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Start the background recorder daemon (default)
    Run {
        /// Enable NDJSON IPC protocol on stdout/stdin (logs go to stderr)
        #[arg(long)]
        ipc: bool,
        /// Start with recording paused (can be enabled later via IPC)
        #[arg(long)]
        paused: bool,
    },
    /// List active MPRIS players and PipeWire Stream/Output/Audio nodes
    ListStreams,
    /// Write a default configuration file to ~/.config/pipewire-recorder/
    InitConfig {
        /// File format to generate
        #[arg(long, value_enum, default_value_t = CliConfigFormat::Yaml)]
        format: CliConfigFormat,
        /// Target file path (defaults to ~/.config/pipewire-recorder/config.{yaml,json})
        #[arg(long)]
        output: Option<PathBuf>,
        /// Overwrite existing file
        #[arg(long)]
        force: bool,
    },
    /// Show loaded configuration, active stream match status, and index statistics
    Status,
    /// List recorded tracks from the deduplication index
    List,
    /// Remove stale entries from recorded_index.json where files no longer exist on disk
    VerifyIndex,
}

#[tokio::main]
async fn main() -> Result<()> {
    let max_level = match std::env::var("RUST_LOG")
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "trace" => tracing::Level::TRACE,
        "debug" => tracing::Level::DEBUG,
        "warn" | "warning" => tracing::Level::WARN,
        "error" => tracing::Level::ERROR,
        _ => tracing::Level::INFO,
    };
    let cli = Cli::parse();

    let is_ipc = matches!(cli.command, Some(Commands::Run { ipc: true, .. }));
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(max_level)
        .with_target(false);
    if is_ipc {
        subscriber.with_writer(std::io::stderr).init();
    } else {
        subscriber.init();
    }

    match &cli.command {
        Some(Commands::InitConfig {
            format,
            output,
            force,
        }) => {
            let fmt = match format {
                CliConfigFormat::Yaml => ConfigFormat::Yaml,
                CliConfigFormat::Json => ConfigFormat::Json,
            };
            let target_path = match output {
                Some(p) => p.clone(),
                None => {
                    let ext = match fmt {
                        ConfigFormat::Yaml => "yaml",
                        ConfigFormat::Json => "json",
                    };
                    Config::default_config_dir().join(format!("config.{ext}"))
                }
            };
            Config::write_default_config(&target_path, fmt, *force)?;
            println!("Wrote default config to {}", target_path.display());
            return Ok(());
        }
        Some(Commands::ListStreams) => return cmd_list_streams().await,
        _ => {}
    }

    let (mut cfg, cfg_path) = Config::load_or_init(cli.config.as_deref())?;
    if let Some(out) = cli.output_dir {
        cfg.output_dir = out;
    }
    if let Some(player) = cli.player {
        cfg.stream.mpris_player = player;
    }
    if let Some(pw) = cli.pipewire_target {
        cfg.stream.pipewire_target = pw;
    }
    if let Some(vs) = cli.virtual_sink {
        cfg.stream.virtual_sink.enabled = vs;
    }
    if let Some(unity) = cli.enforce_unity_volume {
        cfg.stream.enforce_unity_volume = unity;
    }

    match cli.command.unwrap_or(Commands::Run {
        ipc: false,
        paused: false,
    }) {
        Commands::Run { ipc: true, paused } => {
            recorder::run_daemon_ipc(cfg, cfg_path, !paused).await
        }
        Commands::Run { ipc: false, .. } => recorder::run_daemon(cfg, cfg_path).await,
        Commands::ListStreams | Commands::InitConfig { .. } => unreachable!(),
        Commands::Status => cmd_status(&cfg, &cfg_path).await,
        Commands::List => cmd_list(&cfg),
        Commands::VerifyIndex => cmd_verify_index(&cfg),
    }
}

async fn cmd_list_streams() -> Result<()> {
    println!("=== Active MPRIS Players (D-Bus) ===");
    match zbus::Connection::session().await {
        Ok(conn) => {
            let players = list_mpris_players(&conn).await?;
            if players.is_empty() {
                println!("  (none active)");
            } else {
                for bus in players {
                    let short = bus.strip_prefix("org.mpris.MediaPlayer2.").unwrap_or(&bus);
                    let status = match query_player_snapshot(&conn, &bus).await {
                        Ok(snap) => {
                            let track = snap
                                .metadata
                                .as_ref()
                                .map(|m| format!("{} - {}", m.primary_artist(), m.title))
                                .unwrap_or_else(|| "no track".to_string());
                            format!("{:?} | {}", snap.status, track)
                        }
                        Err(_) => "unreachable".to_string(),
                    };
                    println!("  - mpris_player: \"{short}\" ({bus}) -> {status}");
                }
            }
        }
        Err(err) => {
            println!("  (unable to connect to session D-Bus: {err})");
        }
    }

    println!("\n=== Active PipeWire Output Streams (pw-dump) ===");
    match list_audio_output_streams().await {
        Ok(nodes) => {
            if nodes.is_empty() {
                println!("  (none active)");
            } else {
                for node in nodes {
                    println!(
                        "  - serial: {:<5} id: {:<4} app: {:<18} binary: {:<14} node: {:<18} media: {:<16} vol: {}",
                        node.serial,
                        node.id,
                        node.app_name,
                        node.binary_name,
                        node.node_name,
                        node.media_name,
                        node.display_volume()
                    );
                }
            }
        }
        Err(err) => {
            println!("  (unable to query pw-dump: {err})");
        }
    }

    let virtual_sinks: Vec<_> = list_audio_sinks()
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(|s| s.recorder_role.as_deref() == Some(CAPTURE_SINK_ROLE))
        .collect();
    let loopbacks = list_loopback_playback_nodes().await.unwrap_or_default();
    if !virtual_sinks.is_empty() || !loopbacks.is_empty() {
        println!("\n=== Active PipeWire Recorder Virtual Sink / Loopback ===");
        for sink in virtual_sinks {
            println!(
                "  - capture sink  : serial: {:<5} id: {:<4} node: {:<28} desc: \"{}\" vol: {}",
                sink.serial,
                sink.id,
                sink.node_name,
                sink.description,
                sink.display_volume()
            );
        }
        for lb in loopbacks {
            println!(
                "  - playback mixer: serial: {:<5} id: {:<4} node: {:<28} app: \"{}\" vol: {}",
                lb.serial,
                lb.id,
                lb.node_name,
                lb.app_name,
                lb.display_volume()
            );
        }
    }

    Ok(())
}

async fn cmd_status(cfg: &Config, cfg_path: &Path) -> Result<()> {
    let index_path = cfg.resolved_index_path();
    let index = RecordedIndex::load(&index_path)?;

    println!("Config path         : {}", cfg_path.display());
    println!(
        "Output directory    : {}",
        cfg.resolved_output_dir().display()
    );
    println!("Path template       : {}", cfg.path_template);
    println!(
        "Index path          : {} ({} tracks)",
        index_path.display(),
        index.entries.len()
    );
    println!("Target MPRIS player : {}", cfg.stream.mpris_player);
    println!("Target PW stream    : {}", cfg.stream.pipewire_target);
    println!("Trim silence        : {}", cfg.audio.trim_silence);
    println!("Enforce 100% volume : {}", cfg.stream.enforce_unity_volume);
    if cfg.stream.virtual_sink.enabled {
        println!(
            "Virtual sink        : enabled (sink=\"{}\", playback=\"{}\", auto_route={})",
            cfg.stream.virtual_sink.sink_name,
            cfg.stream.virtual_sink.playback_name,
            cfg.stream.virtual_sink.auto_route
        );
    } else {
        println!("Virtual sink        : disabled");
    }

    if let Ok(conn) = zbus::Connection::session().await {
        match find_active_player_bus(&conn, &cfg.stream.mpris_player).await? {
            Some(bus) => println!("Matched MPRIS bus   : {bus}"),
            None => println!("Matched MPRIS bus   : (not currently running)"),
        }
    } else {
        println!("Matched MPRIS bus   : (no D-Bus session available)");
    }

    if let Ok(nodes) = list_audio_output_streams().await {
        match find_matching_node(&nodes, &cfg.stream.pipewire_target) {
            Some(node) => println!(
                "Matched PW stream   : id={} serial={} app=\"{}\" vol={}",
                node.id,
                node.serial,
                node.app_name,
                node.display_volume()
            ),
            None => println!("Matched PW stream   : (no active PipeWire stream found)"),
        }
    } else {
        println!("Matched PW stream   : (pw-dump unavailable)");
    }

    if cfg.stream.virtual_sink.enabled {
        match find_sink_by_name(&cfg.stream.virtual_sink.sink_name).await {
            Ok(Some(sink)) => println!(
                "Active virtual sink : id={} serial={} node=\"{}\" vol={}",
                sink.id,
                sink.serial,
                sink.node_name,
                sink.display_volume()
            ),
            Ok(None) => println!("Active virtual sink : (not currently active)"),
            Err(_) => println!("Active virtual sink : (pw-dump unavailable)"),
        }
        if let Ok(playbacks) = list_loopback_playback_nodes().await {
            let expected_node = cfg.stream.virtual_sink.playback_node_name();
            if let Some(lb) = playbacks.into_iter().find(|n| n.node_name == expected_node) {
                println!(
                    "Loopback playback   : id={} serial={} mixer=\"{}\" vol={}",
                    lb.id,
                    lb.serial,
                    lb.app_name,
                    lb.display_volume()
                );
            } else {
                println!("Loopback playback   : (not currently active)");
            }
        }
    }

    Ok(())
}

fn cmd_list(cfg: &Config) -> Result<()> {
    let index_path = cfg.resolved_index_path();
    let index = RecordedIndex::load(&index_path)?;
    if index.entries.is_empty() {
        println!("No recorded tracks in {}", index_path.display());
        return Ok(());
    }
    for (i, entry) in index.entries.iter().enumerate() {
        println!(
            "{:4}. {} - {} [{}] ({}s) -> {}",
            i + 1,
            entry.artist,
            entry.title,
            entry.album,
            entry.duration_ms / 1000,
            entry.file_path.display()
        );
    }
    Ok(())
}

fn cmd_verify_index(cfg: &Config) -> Result<()> {
    let index_path = cfg.resolved_index_path();
    let mut index = RecordedIndex::load(&index_path)?;
    let removed = index.prune_missing();
    index.save(&index_path)?;
    println!(
        "Pruned {removed} missing/duplicate entries; {} entries remain in {}",
        index.entries.len(),
        index_path.display()
    );
    Ok(())
}
