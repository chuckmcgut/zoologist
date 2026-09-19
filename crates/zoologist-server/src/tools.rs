//! One-off command-line tools: `probe` and `capture`.

use std::path::Path;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use zoologist_core::config::{CameraConfig, CameraKind, DecoderKind};
use zoologist_core::{Config, redact_url};
use zoologist_video::capture::CaptureWriter;
use zoologist_video::decode::{DecodeStats, DecoderChoice, spawn_decode_worker};
use zoologist_video::snapshot::write_frame_jpeg;
use zoologist_video::source::{SharedStatus, StreamRole, spawn_source};
use zoologist_video::stream::StreamItem;

use crate::cli::StreamArg;

fn stream_camera<'a>(config: &'a Config, id: &str) -> Result<&'a CameraConfig> {
    let camera = config
        .cameras
        .iter()
        .find(|c| c.id == id)
        .with_context(|| format!("no camera with id {id:?} in the config"))?;
    if camera.kind != CameraKind::Stream {
        bail!("camera {id:?} is a Hub camera; it has no live stream (use hub-test instead)");
    }
    Ok(camera)
}

/// The decoder chosen in the config.
pub fn decoder_choice(config: &Config) -> DecoderChoice {
    match config.video.decoder {
        DecoderKind::Rust => DecoderChoice::Rust,
        DecoderKind::Ffmpeg => DecoderChoice::Ffmpeg(config.video.ffmpeg_path.clone()),
    }
}

/// `zoologist probe`: decode a camera's substream for a while and print what arrived.
pub async fn probe(config: &Config, camera_id: &str, seconds: u64) -> Result<()> {
    let camera = stream_camera(config, camera_id)?;
    println!(
        "probing {camera_id} ({}) for {seconds} s …",
        redact_url(camera.detect_url.as_deref().unwrap_or_default())
    );
    let cancel = CancellationToken::new();
    let status = SharedStatus::default();
    let (item_tx, mut item_rx) = mpsc::channel::<StreamItem>(64);
    let (decode_tx, decode_rx) = mpsc::channel::<StreamItem>(64);
    let (frame_tx, mut frame_rx) = mpsc::channel(8);
    let stats = Arc::new(RwLock::new(DecodeStats::default()));
    spawn_source(
        camera,
        StreamRole::Detect,
        item_tx,
        status.clone(),
        cancel.clone(),
    )
    .context("camera has no detect_url")?;
    spawn_decode_worker(
        camera.id.clone(),
        decoder_choice(config),
        camera.detect_fps,
        decode_rx,
        frame_tx,
        stats.clone(),
        false,
        config.video.analysis_max_width,
    )?;

    let mut info = None;
    let mut units = 0u64;
    let mut keyframes = 0u64;
    let mut last_frame = None;
    let mut frames = 0u64;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(seconds);
    let started = Instant::now();
    loop {
        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => break,
            item = item_rx.recv() => {
                let Some(item) = item else { break };
                match &item {
                    StreamItem::Info(i) => info = Some(i.clone()),
                    StreamItem::Unit(u) => {
                        units += 1;
                        keyframes += u64::from(u.is_keyframe);
                    }
                }
                let _ = decode_tx.send(item).await;
            }
            frame = frame_rx.recv() => {
                if let Some(frame) = frame {
                    frames += 1;
                    last_frame = Some(frame);
                }
            }
        }
    }
    cancel.cancel();
    let elapsed = started.elapsed().as_secs_f64();
    let status = status.read().unwrap_or_else(|e| e.into_inner()).clone();
    let stats = stats.read().unwrap_or_else(|e| e.into_inner()).clone();

    match &info {
        Some(i) => println!("stream:     {:?} {}×{}", i.codec, i.width, i.height),
        None => println!("stream:     no stream information received"),
    }
    println!(
        "state:      {:?} (reconnects {})",
        status.state, status.reconnects
    );
    if let Some(err) = &status.last_error {
        println!("last error: {err}");
    }
    println!(
        "received:   {units} frames ({:.1} fps), {keyframes} keyframes, {:.0} kbit/s",
        units as f64 / elapsed,
        status.bitrate_kbps
    );
    if keyframes > 1 {
        println!("keyframe:   every {:.1} s", elapsed / keyframes as f64);
    }
    println!(
        "decoded:    {} frames with the {:?} decoder, {:.2} ms each, {} errors",
        stats.decoded, config.video.decoder, stats.mean_decode_ms, stats.errors
    );
    println!(
        "analysed:   {frames} frames ({:.1} fps, target {})",
        frames as f64 / elapsed,
        camera.detect_fps
    );
    if let Some(frame) = last_frame {
        let path = format!("probe_{camera_id}.jpg");
        write_frame_jpeg(&frame, Path::new(&path), 85)?;
        println!("snapshot:   {path} ({}×{})", frame.width, frame.height);
        Ok(())
    } else {
        bail!("no frames were decoded")
    }
}

/// `zoologist capture`: save a camera stream to disk for offline tests.
pub async fn capture(
    config: &Config,
    camera_id: &str,
    stream: StreamArg,
    seconds: u64,
    out: &Path,
) -> Result<()> {
    let camera = stream_camera(config, camera_id)?;
    let (role, name) = match stream {
        StreamArg::Detect => (StreamRole::Detect, "detect"),
        StreamArg::Record => (StreamRole::Record, "record"),
    };
    let base = out.join(format!("{camera_id}-{name}"));
    let cancel = CancellationToken::new();
    let status = SharedStatus::default();
    let (tx, mut rx) = mpsc::channel::<StreamItem>(256);
    spawn_source(camera, role, tx, status.clone(), cancel.clone())
        .context("camera has no URL for that stream")?;
    let mut writer = CaptureWriter::create(&base)
        .with_context(|| format!("cannot create {}", base.display()))?;
    println!(
        "capturing {camera_id} {name} stream for {seconds} s into {}.h264 …",
        base.display()
    );
    let deadline = tokio::time::Instant::now() + Duration::from_secs(seconds);
    loop {
        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => break,
            item = rx.recv() => match item {
                Some(item) => writer.push(&item)?,
                None => break,
            },
        }
    }
    cancel.cancel();
    let written = writer.units_written;
    writer.finish()?;
    if written == 0 {
        let status = status.read().unwrap_or_else(|e| e.into_inner()).clone();
        bail!(
            "nothing captured (state {:?}, last error {:?})",
            status.state,
            status.last_error
        );
    }
    println!("captured {written} frames");
    Ok(())
}

fn model_config<'a>(
    config: &'a Config,
    key: Option<&str>,
) -> Result<(&'a str, &'a zoologist_core::config::ModelConfig)> {
    let key = key.unwrap_or(&config.inference.detector);
    let (key, model) = config
        .models
        .get_key_value(key)
        .with_context(|| format!("no [models.{key}] section in the config"))?;
    Ok((key.as_str(), model))
}

/// `zoologist detect`: runs the detector on whole photos (stretched to the model's input size).
pub fn detect(config: &Config, key: Option<&str>, images: &[std::path::PathBuf]) -> Result<()> {
    use zoologist_vision::detector::{DetectorModel, resize_rgb};
    let (key, model_cfg) = model_config(config, key)?;
    let detector =
        DetectorModel::load(model_cfg).with_context(|| format!("loading model {key}"))?;
    let size = detector.input_size();
    for path in images {
        let img = image::open(path)
            .with_context(|| format!("cannot open {}", path.display()))?
            .to_rgb8();
        let (w, h) = img.dimensions();
        let input = resize_rgb(img.as_raw(), w, h, size)?;
        let started = Instant::now();
        let detections = detector.detect(&input)?;
        let ms = started.elapsed().as_secs_f64() * 1000.0;
        let json: Vec<serde_json::Value> = detections
            .iter()
            .map(|d| {
                serde_json::json!({
                    "label": d.label, "class": d.raw_class, "score": (d.score * 1000.0).round() / 1000.0,
                    "bbox": [d.bbox.x1, d.bbox.y1, d.bbox.x2, d.bbox.y2],
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::json!({ "image": path.display().to_string(), "model": key, "ms": (ms * 10.0).round() / 10.0, "detections": json })
        );
        let mut rgb = img.into_raw();
        for d in &detections {
            zoologist_video::clips::draw_box(&mut rgb, w, h, &d.bbox, [255, 196, 0]);
        }
        let out = path.with_extension("det.jpg");
        let jpeg = zoologist_video::snapshot::encode_jpeg(&rgb, w, h, 85)?;
        std::fs::write(&out, jpeg)?;
    }
    Ok(())
}

/// `zoologist bench`: detector latency and throughput with several worker counts.
pub fn bench(
    config: &Config,
    key: Option<&str>,
    iterations: usize,
    threads: &[usize],
) -> Result<()> {
    use zoologist_vision::detector::DetectorModel;
    let (key, model_cfg) = model_config(config, key)?;
    #[cfg(target_arch = "x86_64")]
    println!(
        "cpu: avx2={} fma={}",
        std::is_x86_feature_detected!("avx2"),
        std::is_x86_feature_detected!("fma")
    );
    let started = Instant::now();
    let detector = Arc::new(DetectorModel::load(model_cfg)?);
    println!(
        "model {key} ({}) loaded in {:.1} s",
        detector.id(),
        started.elapsed().as_secs_f64()
    );
    let size = detector.input_size() as usize;
    let input: Vec<u8> = (0..size * size * 3)
        .map(|i| ((i * 31) % 256) as u8)
        .collect();
    detector.detect(&input)?; // warm up
    println!("| workers | p50 ms | p95 ms | detections/s |");
    println!("|---|---|---|---|");
    for &n in threads {
        let wall = Instant::now();
        let mut times: Vec<f64> = std::thread::scope(|s| {
            let handles: Vec<_> = (0..n.max(1))
                .map(|_| {
                    let (detector, input) = (detector.clone(), &input);
                    s.spawn(move || {
                        (0..iterations)
                            .map(|_| {
                                let t = Instant::now();
                                detector
                                    .detect(input)
                                    .map(|_| t.elapsed().as_secs_f64() * 1000.0)
                            })
                            .collect::<Result<Vec<f64>, _>>()
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|h| h.join().expect("bench thread"))
                .collect::<Result<Vec<Vec<f64>>, _>>()
        })?
        .into_iter()
        .flatten()
        .collect();
        let rate = times.len() as f64 / wall.elapsed().as_secs_f64();
        times.sort_by(f64::total_cmp);
        let p = |q: usize| times[(times.len() * q / 100).min(times.len() - 1)];
        println!("| {n} | {:.1} | {:.1} | {rate:.1} |", p(50), p(95));
    }
    Ok(())
}

/// `zoologist classify`: SpeciesNet on whole photos (each stretched to 480×480).
pub fn classify(config: &Config, images: &[std::path::PathBuf]) -> Result<()> {
    use zoologist_vision::detector::resize_rgb;
    use zoologist_vision::species::{INPUT_SIZE, SpeciesModel};
    let model = SpeciesModel::load(&config.species, &config.station)?;
    for path in images {
        let img = image::open(path)
            .with_context(|| format!("cannot open {}", path.display()))?
            .to_rgb8();
        let input = resize_rgb(img.as_raw(), img.width(), img.height(), INPUT_SIZE)?;
        let started = Instant::now();
        let probs = model.classify(&input)?;
        let ms = started.elapsed().as_secs_f64() * 1000.0;
        let mut top: Vec<(usize, f32)> = probs.iter().copied().enumerate().collect();
        top.sort_by(|a, b| b.1.total_cmp(&a.1));
        println!("{} ({ms:.0} ms)", path.display());
        for (i, p) in top.iter().take(5) {
            let label = &model.rules.labels[*i];
            println!(
                "  {:>5.1} %  {} ({})",
                p * 100.0,
                label.display_name(),
                label.scientific_name()
            );
        }
        match model.rules.decide(&probs, 0.9) {
            Some(g) => println!(
                "  answer: {} ({}) {:.0} %",
                g.common_name,
                g.scientific_name,
                g.score * 100.0
            ),
            None => println!("  answer: unidentified animal"),
        }
    }
    Ok(())
}

/// `zoologist onvif`: prints a camera's ONVIF media profiles and stream addresses.
pub fn onvif(url: &str, user: &str) -> Result<()> {
    use zoologist_video::onvif::OnvifClient;
    let device_url = if url.contains("://") {
        url.to_string()
    } else {
        format!("http://{url}/onvif/device_service")
    };
    let password = match std::env::var("ONVIF_PASSWORD") {
        Ok(p) => p,
        Err(_) => rpassword::prompt_password(format!("Password for {user} at {device_url}: "))
            .context("cannot read the password")?,
    };
    let mut client = OnvifClient::new(&device_url, user, &password);
    drop(password);
    let camera_time = client.sync_clock().context("GetSystemDateAndTime")?;
    println!("camera clock: {camera_time}");
    match client.device_information() {
        Ok(info) => {
            for (k, v) in info {
                println!("{k}: {v}");
            }
        }
        Err(e) => println!("device information: {e}"),
    }
    let media = client.media_url().context("finding the media service")?;
    let profiles = match client.profiles(&media) {
        Ok(p) => p,
        // Some cameras check the token's time against a clock that is off: try local time.
        Err(first) => {
            client.use_local_clock();
            client
                .profiles(&media)
                .map_err(|_| first)
                .context("GetProfiles")?
        }
    };
    println!("\n{} profiles:", profiles.len());
    for p in &profiles {
        println!(
            "\n  {} (token {}, source {})\n    {} {}x{}{}{}",
            p.name,
            p.token,
            p.source,
            p.encoding,
            p.width,
            p.height,
            p.fps.map_or(String::new(), |f| format!(", {f} fps")),
            p.bitrate_kbps
                .map_or(String::new(), |b| format!(", max {b} kbit/s")),
        );
        match client.stream_uri(&media, &p.token) {
            Ok(uri) => println!("    stream: {}", redact_url(&uri)),
            Err(e) => println!("    stream: {e}"),
        }
        match client.allowed_resolutions(&media, p) {
            Ok(list) => {
                for (codec, sizes) in list {
                    let sizes: Vec<String> =
                        sizes.iter().map(|(w, h)| format!("{w}x{h}")).collect();
                    println!("    allowed {codec}: {}", sizes.join(", "));
                }
            }
            Err(e) => println!("    allowed resolutions: {e}"),
        }
    }
    Ok(())
}
