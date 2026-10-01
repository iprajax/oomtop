//! Where the per-sample CPU goes (SPEC §14 budget: ≤ 1 % of one core at the 2 s refresh = ≤ 20 ms per
//! 2 s). Times the collector (`Sampler::sample_once`) and the CLI's enrichment (`Engine::enrich`: attribution,
//! idle, adapters, history, forecast, lineage) separately, on this machine, read-only.
//!
//! ```console
//! $ cargo run --release -p oomtop-cli --example pipeline_cost -- 10     # 10 samples, 2 s apart
//! ```

use oomtop_cli::engine::Engine;
use oomtop_collect::{Sampler, SamplerOptions};
use oomtop_config::Config;
use std::time::{Duration, Instant};

fn cpu_ms() -> f64 {
    // SAFETY: getrusage fills a plain-data struct for RUSAGE_SELF.
    let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
    unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut ru) };
    let tv = |t: libc::timeval| t.tv_sec as f64 * 1000.0 + t.tv_usec as f64 / 1000.0;
    tv(ru.ru_utime) + tv(ru.ru_stime)
}

fn main() {
    let n: usize = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(10);
    let mut cfg = Config::default();
    cfg.adapters.enabled = false; // --offline: no HTTP
    let opts = SamplerOptions {
        marker_allowlist: cfg.privacy.marker_allowlist.clone(),
        ..Default::default()
    };
    let mut sampler = Sampler::platform_default(&opts);
    let mut engine = Engine::live(&cfg, None, false, true);
    let (mut sample_cpu, mut enrich_cpu, mut json_cpu) = (Vec::new(), Vec::new(), Vec::new());
    let mut procs = 0;
    let (mut cpu_after_first, mut wall_after_first) = (0.0, Instant::now());
    for i in 0..n {
        let started = Instant::now();
        let c0 = cpu_ms();
        let raw = sampler.sample_once();
        let c1 = cpu_ms();
        let s = engine.enrich(raw);
        let c2 = cpu_ms();
        let bytes = serde_json::to_vec(&oomtop_core::redact::redact_snapshot(&s))
            .map(|v| v.len())
            .unwrap_or(0);
        let c3 = cpu_ms();
        procs = s.processes.len();
        if i == 0 {
            cpu_after_first = cpu_ms();
            wall_after_first = Instant::now();
        }
        if i > 0 {
            // the first sample pays one-time costs (process list, lineage load, profile)
            sample_cpu.push(c1 - c0);
            enrich_cpu.push(c2 - c1);
            json_cpu.push(c3 - c2);
        }
        eprintln!(
            "sample {i:>2}: collect {:>6.1} ms · enrich {:>6.1} ms · json {:>6.1} ms ({} KB) · {procs} procs",
            c1 - c0,
            c2 - c1,
            c3 - c2,
            bytes / 1024
        );
        if i + 1 < n {
            std::thread::sleep(Duration::from_secs(2).saturating_sub(started.elapsed()));
        }
    }
    // Whole process (every thread, including sleeps) after the first sample: catches background work the
    // per-sample windows above can't see.
    let whole_ms = cpu_ms() - cpu_after_first;
    let wall_ms = wall_after_first.elapsed().as_secs_f64() * 1000.0;
    let json_total: f64 = json_cpu.iter().sum();
    println!(
        "whole process after the first sample: {:.1} ms CPU over {:.1} s → {:.2} % of one core (without JSON)",
        whole_ms - json_total,
        wall_ms / 1000.0,
        (whole_ms - json_total) / wall_ms.max(1.0) * 100.0
    );
    let avg = |v: &[f64]| v.iter().sum::<f64>() / v.len().max(1) as f64;
    let (a, b, c) = (avg(&sample_cpu), avg(&enrich_cpu), avg(&json_cpu));
    println!(
        "steady state per 2 s sample ({procs} processes): collect {a:.1} ms + enrich {b:.1} ms = {:.1} ms CPU \
         → {:.2} % of one core (budget 1 % = 20 ms); ndjson adds {c:.1} ms of JSON",
        a + b,
        (a + b) / 2000.0 * 100.0
    );
}
