#![forbid(unsafe_code)]
//! Loads ONNX models with tract, checks them against onnxruntime reference outputs, and measures
//! latency and throughput with 1..N parallel workers (plan Step 0.3).
//!
//! Usage: tract-bench [--iters N] [--threads 1,2,3,4] models/a.onnx models/b.onnx …
//! Reference outputs come from tools/convert_models/make_pattern_goldens.py (models/golden/).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use tract_onnx::prelude::*;

type Runnable = Arc<TypedRunnableModel>;

/// The fixed input pattern shared with make_pattern_goldens.py.
fn pattern(shape: &[usize]) -> Result<Tensor> {
    let n: usize = shape.iter().product();
    let data: Vec<f32> = (0..n).map(|i| ((i * 31) % 256) as f32 / 255.0).collect();
    Ok(Tensor::from_shape(shape, &data)?)
}

fn load(path: &Path) -> Result<(Runnable, Vec<usize>, Duration)> {
    let started = Instant::now();
    let model = tract_onnx::onnx().model_for_path(path)?;
    let fact = model.input_fact(0)?.clone();
    let shape: Vec<usize> = fact
        .shape
        .as_concrete_finite()?
        .context("input shape is not fixed")?
        .to_vec();
    let runnable = model
        .with_input_fact(0, f32::fact(&shape).into())?
        .into_optimized()?
        .into_runnable()?;
    Ok((runnable, shape, started.elapsed()))
}

fn check(model: &Runnable, shape: &[usize], golden: &Path) -> Result<(f32, f32)> {
    let out = model.run(tvec!(pattern(shape)?.into()))?;
    let ours = out[0].to_plain_array_view::<f32>()?;
    let ours: Vec<f32> = ours.iter().copied().collect();
    let bytes = std::fs::read(golden).with_context(|| golden.display().to_string())?;
    let reference: Vec<f32> = bytes
        .chunks_exact(4)
        .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .collect();
    anyhow::ensure!(ours.len() == reference.len(), "output size {} vs {}", ours.len(), reference.len());
    let scale = reference.iter().fold(1f32, |m, v| m.max(v.abs()));
    let diff = ours.iter().zip(&reference).fold(0f32, |m, (a, b)| m.max((a - b).abs()));
    Ok((diff, diff / scale))
}

/// Runs `iters` inferences on each of `threads` threads sharing one model.
fn bench(model: &Runnable, shape: &[usize], threads: usize, iters: usize) -> Result<(f64, f64, f64)> {
    let input = pattern(shape)?;
    let started = Instant::now();
    let latencies: Vec<Vec<f64>> = std::thread::scope(|s| {
        let handles: Vec<_> = (0..threads)
            .map(|_| {
                let model = model.clone();
                let input = input.clone();
                s.spawn(move || -> Result<Vec<f64>> {
                    let mut times = Vec::with_capacity(iters);
                    for _ in 0..iters {
                        let t = Instant::now();
                        model.run(tvec!(input.clone().into()))?;
                        times.push(t.elapsed().as_secs_f64() * 1000.0);
                    }
                    Ok(times)
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().expect("thread")).collect::<Result<_>>()
    })?;
    let wall = started.elapsed().as_secs_f64();
    let mut all: Vec<f64> = latencies.into_iter().flatten().collect();
    all.sort_by(f64::total_cmp);
    let p50 = all[all.len() / 2];
    let p95 = all[(all.len() * 95 / 100).min(all.len() - 1)];
    Ok((p50, p95, (threads * iters) as f64 / wall))
}

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let mut iters = 20usize;
    let mut threads = vec![1usize, 2, 4];
    let mut models: Vec<PathBuf> = Vec::new();
    while let Some(a) = args.next() {
        match a.as_str() {
            "--iters" => iters = args.next().context("--iters N")?.parse()?,
            "--threads" => {
                threads = args
                    .next()
                    .context("--threads 1,2,4")?
                    .split(',')
                    .map(str::parse)
                    .collect::<Result<_, _>>()?
            }
            _ => models.push(PathBuf::from(a)),
        }
    }
    #[cfg(target_arch = "x86_64")]
    println!(
        "cpu: avx2={} fma={}",
        std::is_x86_feature_detected!("avx2"),
        std::is_x86_feature_detected!("fma")
    );
    println!("| Model | Load | max abs / rel diff vs ORT | Threads | p50 ms | p95 ms | inferences/s |");
    println!("|---|---|---|---|---|---|---|");
    for path in &models {
        let name = path.file_stem().and_then(|s| s.to_str()).unwrap_or("?");
        let (model, shape, load_time) = match load(path) {
            Ok(v) => v,
            Err(e) => {
                println!("| {name} | **failed to load**: {e:#} | | | | | |");
                continue;
            }
        };
        let golden = path.parent().unwrap_or(Path::new(".")).join("golden").join(format!("{name}.f32"));
        let diff = match check(&model, &shape, &golden) {
            Ok((abs, rel)) => format!("{abs:.1e} / {rel:.1e}"),
            Err(e) => format!("check failed: {e:#}"),
        };
        for &t in &threads {
            let (p50, p95, rate) = bench(&model, &shape, t, iters)?;
            println!(
                "| {name} | {:.1} s | {diff} | {t} | {p50:.1} | {p95:.1} | {rate:.1} |",
                load_time.as_secs_f64()
            );
        }
    }
    Ok(())
}
