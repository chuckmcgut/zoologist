#![forbid(unsafe_code)]

use std::process::ExitCode;

use clap::Parser;
use zoologist_core::config::CameraKind;
use zoologist_core::{Config, redact_url};
use zoologist_server::cli::{Cli, Command};
use zoologist_server::{demo, healthcheck, pipeline, tools};

fn main() -> ExitCode {
    tracing_subscriber::fmt()
        // Plain text under Docker and systemd; colours only on a terminal.
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stdout()))
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let cli = Cli::parse();
    match cli.command {
        Command::Run {
            config,
            fast_files,
            allow_slow_cpu,
        } => with_runtime(&config, |cfg| {
            let opts = pipeline::RunOptions {
                fast_files,
                allow_slow_cpu,
            };
            pipeline::run(cfg, opts)
        }),
        Command::CheckConfig { config } => check_config(&config),
        Command::Probe {
            config,
            camera,
            seconds,
        } => with_runtime(&config, |cfg| async move {
            tools::probe(&cfg, &camera, seconds).await
        }),
        Command::Detect {
            config,
            model,
            images,
        } => with_config(&config, |cfg| {
            tools::detect(&cfg, model.as_deref(), &images)
        }),
        Command::Bench {
            config,
            model,
            iterations,
            threads,
        } => with_config(&config, |cfg| {
            tools::bench(&cfg, model.as_deref(), iterations, &threads)
        }),
        Command::HubTest {
            config,
            hub,
            out,
            no_download,
            channel,
            days,
            snap,
        } => with_config(&config, |cfg| {
            zoologist_server::hub_test::hub_test(
                &cfg,
                &hub,
                &out,
                !no_download,
                channel,
                days,
                snap,
            )
        }),
        Command::Onvif { url, user } => match tools::onvif(&url, &user) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("error: {e:#}");
                ExitCode::FAILURE
            }
        },
        Command::Replay {
            config,
            camera,
            min_movement,
            species,
            main,
            clips,
        } => with_config(&config, |cfg| {
            zoologist_server::replay::replay(cfg, &camera, &clips, min_movement, species, main)
        }),
        Command::Prune {
            config,
            cameras,
            labels,
            before,
            yes,
        } => with_config(&config, |cfg| prune(&cfg, cameras, &labels, before, yes)),
        Command::Reclassify {
            config,
            cameras,
            since,
            all,
            yes,
            server,
        } => with_config(&config, |cfg| {
            let filter = zoologist_server::reclassify::ReclassifyFilter {
                cameras,
                since,
                all,
            };
            zoologist_server::reclassify::reclassify(cfg, &filter, yes, server)
        }),
        Command::Healthcheck { url } => healthcheck::run(&url),
        Command::Janitor { config, dry_run } => with_config(&config, |cfg| janitor(&cfg, dry_run)),
        Command::SeedDemo { config, fixtures } => {
            with_config(&config, |cfg| demo::seed_demo(&cfg, &fixtures))
        }
        Command::Classify { config, images } => {
            with_config(&config, |cfg| tools::classify(&cfg, &images))
        }
        Command::Capture {
            config,
            camera,
            stream,
            seconds,
            out,
        } => with_runtime(&config, |cfg| async move {
            tools::capture(&cfg, &camera, stream, seconds, &out).await
        }),
    }
}

/// Loads the config and runs a synchronous tool.
fn with_config(
    path: &std::path::Path,
    tool: impl FnOnce(Config) -> anyhow::Result<()>,
) -> ExitCode {
    let config = match Config::load(path) {
        Ok(config) => config,
        Err(err) => {
            eprintln!("{err}");
            return ExitCode::FAILURE;
        }
    };
    match tool(config) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("error: {err:#}");
            ExitCode::FAILURE
        }
    }
}

/// Loads the config and runs an async tool on a fresh tokio runtime.
fn with_runtime<F, Fut>(path: &std::path::Path, tool: F) -> ExitCode
where
    F: FnOnce(Config) -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<()>>,
{
    let config = match Config::load(path) {
        Ok(config) => config,
        Err(err) => {
            eprintln!("{err}");
            return ExitCode::FAILURE;
        }
    };
    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
    match runtime.block_on(tool(config)) {
        Ok(()) => {
            runtime.shutdown_timeout(std::time::Duration::from_secs(2));
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("error: {err:#}");
            runtime.shutdown_timeout(std::time::Duration::from_secs(2));
            ExitCode::FAILURE
        }
    }
}

fn prune(
    config: &Config,
    cameras: Vec<String>,
    labels: &[String],
    before: chrono::NaiveDate,
    yes: bool,
) -> anyhow::Result<()> {
    let labels = labels
        .iter()
        .map(|l| l.parse().map_err(|e| anyhow::anyhow!("label {l:?}: {e}")))
        .collect::<anyhow::Result<Vec<_>>>()?;
    let filter = zoologist_server::prune::PruneFilter {
        cameras,
        labels,
        before,
    };
    let report = zoologist_server::prune::prune(config, &filter, yes)?;
    let verb = if yes { "deleted" } else { "would delete" };
    println!(
        "{verb} {} events and {} files ({:.1} GB)",
        report.events,
        report.files,
        report.bytes as f64 / 1e9
    );
    if !yes {
        println!("run again with --yes to delete them");
    }
    Ok(())
}

fn janitor(config: &Config, dry_run: bool) -> anyhow::Result<()> {
    let dir = &config.server.data_dir;
    let store = zoologist_store::Store::open(&dir.join("zoologist.redb"), config.station.timezone)?;
    let report =
        zoologist_server::janitor::run_once(&store, config, dir, chrono::Utc::now(), dry_run)?;
    for action in &report.actions {
        println!("{action}");
    }
    println!(
        "{}{} segments, {} expired events, {} trimmed clips, {} empty directories, {} MB",
        if dry_run {
            "would remove: "
        } else {
            "removed: "
        },
        report.segments_deleted,
        report.events_purged,
        report.clips_trimmed,
        report.dirs_removed,
        report.bytes_freed / (1024 * 1024)
    );
    Ok(())
}

fn check_config(path: &std::path::Path) -> ExitCode {
    match Config::load(path) {
        Ok(config) => {
            let enabled = config.enabled_cameras().count();
            println!(
                "OK: {} cameras ({} enabled), {} Reolink hub(s)",
                config.cameras.len(),
                enabled,
                config.reolink_hubs.len()
            );
            for cam in &config.cameras {
                let source = match cam.kind {
                    CameraKind::Stream => format!(
                        "{:?} {}",
                        cam.transport.expect("validated"),
                        redact_url(cam.detect_url.as_deref().unwrap_or_default())
                    ),
                    CameraKind::HubClips => format!(
                        "hub {} channel {}",
                        cam.hub.as_deref().unwrap_or_default(),
                        cam.channel.unwrap_or_default()
                    ),
                };
                println!("  {:<16} {}", cam.id, source);
            }
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("{err}");
            ExitCode::FAILURE
        }
    }
}
