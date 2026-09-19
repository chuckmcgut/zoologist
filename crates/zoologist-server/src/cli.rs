//! Command-line interface definition.

use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};

/// Watches cameras and detects people, vehicles, animals (with species) and motion.
#[derive(Debug, Parser)]
#[command(name = "zoologist", version, about)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

/// Default config location inside the Docker image.
const DEFAULT_CONFIG: &str = "/config/zoologist.toml";

/// Every subcommand. Most are filled in by later steps of `IMPLEMENTATION_PLAN.md`.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Run the detection pipeline, recorder and web server.
    Run {
        #[arg(long, default_value = DEFAULT_CONFIG)]
        config: PathBuf,
        /// Analyse `file://` sources as fast as possible and exit when they end (offline tests).
        #[arg(long)]
        fast_files: bool,
        /// Start even if the CPU lacks AVX2/FMA (detection will be several times slower).
        #[arg(long)]
        allow_slow_cpu: bool,
    },
    /// Parse and validate a config file, then print a summary.
    CheckConfig {
        #[arg(long, default_value = DEFAULT_CONFIG)]
        config: PathBuf,
    },
    /// Save a camera stream's raw H.264 for offline tests and decoder checks (plan Step 0.4).
    /// Writes `<out>/<camera>-<stream>.h264` plus `.units.jsonl` and `.info.json`.
    Capture {
        #[arg(long, default_value = DEFAULT_CONFIG)]
        config: PathBuf,
        #[arg(long)]
        camera: String,
        /// Which stream: `detect` (substream) or `record` (main stream).
        #[arg(long, default_value = "detect")]
        stream: StreamArg,
        #[arg(long, default_value_t = 30)]
        seconds: u64,
        #[arg(long, default_value = "tools/fixtures/owner")]
        out: PathBuf,
    },
    /// Connect to one camera for a few seconds, decode its substream and report what it sends
    /// (plan Step 2.4). Writes `probe_<camera>.jpg`.
    Probe {
        #[arg(long, default_value = DEFAULT_CONFIG)]
        config: PathBuf,
        #[arg(long)]
        camera: String,
        #[arg(long, default_value_t = 10)]
        seconds: u64,
    },
    /// Run the object detector on photos: prints detections as JSON and writes `<photo>.det.jpg`
    /// with the boxes drawn (plan Step 4.2).
    Detect {
        #[arg(long, default_value = DEFAULT_CONFIG)]
        config: PathBuf,
        /// Key of a `[models.*]` section; default: `inference.detector`.
        #[arg(long)]
        model: Option<String>,
        images: Vec<PathBuf>,
    },
    /// Run the species classifier on photos of one animal (ideally cropped to it) and print the
    /// top 5 plus the final answer after geofence and roll-up rules (plan Step 8.2).
    Classify {
        #[arg(long, default_value = DEFAULT_CONFIG)]
        config: PathBuf,
        images: Vec<PathBuf>,
    },
    /// Measure detector speed with 1..N parallel workers (plan Steps 0.3 and 4.2).
    Bench {
        #[arg(long, default_value = DEFAULT_CONFIG)]
        config: PathBuf,
        /// Key of a `[models.*]` section; default: `inference.detector`.
        #[arg(long)]
        model: Option<String>,
        #[arg(long, default_value_t = 30)]
        iterations: usize,
        /// Worker counts to try, e.g. `1,2,3,4`.
        #[arg(long, value_delimiter = ',', default_values_t = [1, 2, 3, 4])]
        threads: Vec<usize>,
    },
    /// Run one retention pass now (plan Step 11.1). `zoologist run` does this every 10 minutes.
    Janitor {
        #[arg(long, default_value = DEFAULT_CONFIG)]
        config: PathBuf,
        /// Only list what would be deleted.
        #[arg(long)]
        dry_run: bool,
    },
    /// Log in to a Reolink Hub, list its cameras and today's recordings, and download the newest
    /// one (plan Step 7.2). Saves the Hub's answers, without the token, to `--out`.
    HubTest {
        #[arg(long, default_value = DEFAULT_CONFIG)]
        config: PathBuf,
        #[arg(long)]
        hub: String,
        #[arg(long, default_value = "tools/fixtures/owner/hub")]
        out: PathBuf,
        /// Only list, do not download.
        #[arg(long)]
        no_download: bool,
        /// Only this Hub channel (0-based).
        #[arg(long)]
        channel: Option<u8>,
        /// Also search this many days before today.
        #[arg(long, default_value_t = 0)]
        days: u32,
    },
    /// List an ONVIF camera's streams: each profile's codec, resolution, frame rate, bitrate and
    /// RTSP address, and the resolutions its encoder allows. Asks for the password (hidden) unless
    /// ONVIF_PASSWORD is set; the password is never printed.
    Onvif {
        /// Device service address, or just `host:port` (e.g. `192.168.1.40:9520`).
        url: String,
        #[arg(long, default_value = "admin")]
        user: String,
    },
    /// Insert fake events so the UI can be checked without cameras (plan Step 10.2).
    /// Refuses to touch a database that already has events.
    SeedDemo {
        #[arg(long, default_value = DEFAULT_CONFIG)]
        config: PathBuf,
        /// Where the demo pictures and the fox clip come from.
        #[arg(long, default_value = "tools/fixtures")]
        fixtures: PathBuf,
    },
    /// Exit 0 if the local server answers its health endpoint (Docker HEALTHCHECK).
    Healthcheck {
        #[arg(long, default_value = "http://127.0.0.1:8090/api/v1/health")]
        url: String,
    },
}

/// A camera's stream, as named on the command line.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum StreamArg {
    Detect,
    Record,
}
