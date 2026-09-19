//! `zoologist hub-test --hub <id>` (plan Steps 0.5 and 7.2): logs in to a Reolink Hub, lists its
//! cameras and today's recordings, and downloads the newest recording (sub and main stream).
//!
//! Every JSON answer is saved to `--out` with the token removed, and so are the two
//! downloaded files. They become the fixtures for the Hub importer's tests.

use std::path::Path;

use anyhow::{Context, Result, bail};
use chrono::{TimeZone, Utc};
use zoologist_core::Config;
use zoologist_core::config::CameraKind;
use zoologist_video::mp4r::read_mp4_index;
use zoologist_video::reolink_hub::{HubClient, HubFile};

/// What a downloaded recording contains, in one line.
fn describe(path: &Path) -> String {
    match read_mp4_index(path) {
        Ok((info, samples)) => {
            let secs: f64 = samples
                .iter()
                .map(|s| f64::from(s.duration_90k))
                .sum::<f64>()
                / 90_000.0;
            let keys = samples.iter().filter(|s| s.is_key).count();
            format!(
                "{:?} {}×{}, {} frames, {:.1} s, {:.1} fps, {} keyframes",
                info.codec,
                info.width,
                info.height,
                samples.len(),
                secs,
                samples.len() as f64 / secs.max(0.001),
                keys
            )
        }
        Err(e) => format!("not readable as an H.264 MP4 by Zoologist: {e}"),
    }
}

fn show(file: &HubFile, tz: chrono_tz::Tz) -> String {
    format!(
        "{} → {} ({} s, {:.1} MB, {}×{}) {}",
        file.start.with_timezone(&tz).format("%m-%d %H:%M:%S"),
        file.end.with_timezone(&tz).format("%H:%M:%S"),
        (file.end - file.start).num_seconds(),
        file.size as f64 / 1_048_576.0,
        file.width,
        file.height,
        file.name
    )
}

/// Recordings that ended less than this long ago may still be written by the Hub.
const SETTLE: chrono::Duration = chrono::Duration::seconds(60);

pub fn hub_test(
    config: &Config,
    hub_id: &str,
    out: &Path,
    download: bool,
    only_channel: Option<u8>,
    days: u32,
) -> Result<()> {
    let hub = config
        .reolink_hubs
        .iter()
        .find(|h| h.id == hub_id)
        .with_context(|| format!("no [[reolink_hubs]] with id {hub_id:?} in the config"))?;
    if !hub.url.starts_with("http://") {
        bail!(
            "the Hub url must be http:// (Zoologist has no TLS client): {}",
            hub.url
        );
    }
    let tz = config.station.timezone;
    let mut client = HubClient::new(&hub.url, &hub.user, &hub.password);
    client.record_to(out)?;

    println!("Hub {hub_id} at {}", hub.url);
    client
        .login()
        .context("login failed (check user and password in the config)")?;
    println!("  login OK as {:?}", hub.user);
    match client.device_info() {
        Ok(info) => {
            let field = |k: &str| {
                info.get(k)
                    .map(|v| v.to_string().trim_matches('"').to_string())
            };
            println!(
                "  model {}, firmware {}, {} channels",
                field("model").unwrap_or_default(),
                field("firmVer").unwrap_or_default(),
                field("channelNum").unwrap_or_default()
            );
        }
        Err(e) => println!("  GetDevInfo failed: {e}"),
    }

    let channels = client.channels().context("GetChannelstatus failed")?;
    let configured: Vec<(u8, &str)> = config
        .cameras
        .iter()
        .filter(|c| c.kind == CameraKind::HubClips && c.hub.as_deref() == Some(hub_id))
        .filter_map(|c| Some((c.channel?, c.id.as_str())))
        .collect();
    println!("\nChannels:");
    for c in &channels {
        let camera = configured
            .iter()
            .find(|(ch, _)| *ch == c.channel)
            .map_or("not in the config".to_string(), |(_, id)| {
                format!("camera {id:?}")
            });
        println!(
            "  {:>2}  {:<20} {:<8} {:<8} {camera}",
            c.channel,
            c.name,
            if c.online { "online" } else { "offline" },
            if c.sleeping { "asleep" } else { "awake" },
        );
    }

    let now = Utc::now();
    let local_midnight = now
        .with_timezone(&tz)
        .date_naive()
        .and_hms_opt(0, 0, 0)
        .expect("midnight");
    let today = tz
        .from_local_datetime(&local_midnight)
        .earliest()
        .map_or(now - chrono::Duration::hours(24), |t| t.with_timezone(&Utc));
    let since = today - chrono::Duration::days(i64::from(days));
    println!(
        "\nRecordings since {} (station time zone {tz}):",
        since.with_timezone(&tz).format("%Y-%m-%d")
    );
    let mut newest: Option<(u8, HubFile)> = None;
    let wanted = channels
        .iter()
        .filter(|c| c.online && only_channel.is_none_or(|ch| ch == c.channel));
    for c in wanted {
        for stream in ["sub", "main"] {
            match client.search(c.channel, stream, since, now, tz) {
                Ok(files) => {
                    println!("  channel {} {stream}: {} files", c.channel, files.len());
                    for f in files.iter().rev().take(3) {
                        println!("      {}", show(f, tz));
                    }
                    let finished = files.iter().rev().find(|f| f.end < now - SETTLE);
                    if stream == "sub"
                        && let Some(last) = finished
                        && newest.as_ref().is_none_or(|(_, n)| last.start > n.start)
                    {
                        newest = Some((c.channel, last.clone()));
                    }
                }
                Err(e) => println!("  channel {} {stream}: search failed: {e}", c.channel),
            }
        }
    }

    if download && let Some((channel, sub)) = newest {
        println!(
            "\nDownloading the newest finished recording (channel {channel}, {}):",
            show(&sub, tz)
        );
        let dest = out.join(format!("ch{channel}-sub.mp4"));
        let started = std::time::Instant::now();
        match client.download(&sub, &dest) {
            Ok(bytes) => println!(
                "  sub:  {:.1} MB in {:.1} s → {}\n        {}",
                bytes as f64 / 1_048_576.0,
                started.elapsed().as_secs_f64(),
                dest.display(),
                describe(&dest)
            ),
            Err(e) => println!("  sub: download failed: {e}"),
        }
        let window = (
            sub.start - chrono::Duration::seconds(5),
            sub.end + chrono::Duration::seconds(5),
        );
        let main = client
            .search(channel, "main", window.0, window.1, tz)
            .unwrap_or_default()
            .into_iter()
            .min_by_key(|f| (f.start - sub.start).num_seconds().abs());
        match main {
            Some(main) if (main.start - sub.start).num_seconds().abs() <= 2 => {
                let dest = out.join(format!("ch{channel}-main.mp4"));
                let started = std::time::Instant::now();
                match client.download(&main, &dest) {
                    Ok(bytes) => println!(
                        "  main: {:.1} MB in {:.1} s → {}\n        {}",
                        bytes as f64 / 1_048_576.0,
                        started.elapsed().as_secs_f64(),
                        dest.display(),
                        describe(&dest)
                    ),
                    Err(e) => println!("  main: download failed: {e}"),
                }
            }
            _ => println!("  main: no main-stream file starts within 2 s of the sub file"),
        }
    }

    client.logout();
    println!(
        "\nSaved the Hub's answers (without the token) and any downloads to {}",
        out.display()
    );
    Ok(())
}
