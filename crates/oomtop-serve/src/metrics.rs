//! Prometheus text exposition (format 0.0.4), pure.
//!
//! Host series plus per-group series aggregated by `(kind, label)` — never per pid (cardinality, SPEC §12.2).
//! Only the largest [`MAX_GROUP_SERIES`] `(kind, label)` pairs by footprint get their own series; the rest are
//! folded into `label="(other)"` per kind. Unavailable values are omitted, never exported as zero.

use oomtop_core::headroom::Headroom;
use oomtop_core::redact::redact_text;
use oomtop_core::{Measured, PressureLevel, Snapshot, SourceStatus, ThermalPressure};
use std::collections::BTreeMap;

/// Upper bound on distinct `(kind, label)` group series.
pub const MAX_GROUP_SERIES: usize = 50;
/// Label used for groups folded together beyond [`MAX_GROUP_SERIES`].
pub const OTHER_LABEL: &str = "(other)";

/// Escapes a label value (backslash, double quote, newline).
pub fn escape_label(v: &str) -> String {
    let mut o = String::with_capacity(v.len());
    for c in v.chars() {
        match c {
            '\\' => o.push_str("\\\\"),
            '"' => o.push_str("\\\""),
            '\n' => o.push_str("\\n"),
            c => o.push(c),
        }
    }
    o
}

/// Escapes HELP text (backslash and newline).
fn escape_help(v: &str) -> String {
    v.replace('\\', "\\\\").replace('\n', "\\n")
}

fn fmt_value(v: f64) -> String {
    if v.is_nan() {
        "NaN".into()
    } else if v == f64::INFINITY {
        "+Inf".into()
    } else if v == f64::NEG_INFINITY {
        "-Inf".into()
    } else {
        format!("{v}")
    }
}

fn labels(pairs: &[(&str, &str)]) -> String {
    if pairs.is_empty() {
        return String::new();
    }
    let inner: Vec<String> = pairs
        .iter()
        .map(|(k, v)| format!("{k}=\"{}\"", escape_label(v)))
        .collect();
    format!("{{{}}}", inner.join(","))
}

struct Writer {
    out: String,
}

impl Writer {
    /// Writes one metric family; skipped entirely when it has no samples.
    fn family(&mut self, name: &str, kind: &str, help: &str, samples: Vec<(String, f64)>) {
        if samples.is_empty() {
            return;
        }
        self.out.push_str(&format!(
            "# HELP {name} {}\n# TYPE {name} {kind}\n",
            escape_help(help)
        ));
        for (l, v) in samples {
            self.out.push_str(&format!("{name}{l} {}\n", fmt_value(v)));
        }
    }

    fn gauge(&mut self, name: &str, help: &str, samples: Vec<(String, f64)>) {
        self.family(name, "gauge", help, samples)
    }

    fn one(&mut self, name: &str, help: &str, v: Option<f64>) {
        self.gauge(
            name,
            help,
            v.map(|v| vec![(String::new(), v)]).unwrap_or_default(),
        )
    }
}

fn avail<T: Copy>(m: &Measured<T>) -> Option<T> {
    m.value.filter(|_| m.quality.is_available())
}

fn bytes(m: &Measured<u64>) -> Option<f64> {
    avail(m).map(|v| v as f64)
}

fn flag(m: &Measured<bool>) -> Option<f64> {
    avail(m).map(|b| if b { 1.0 } else { 0.0 })
}

fn pressure_level(p: PressureLevel) -> f64 {
    match p {
        PressureLevel::Normal => 0.0,
        PressureLevel::Warn => 1.0,
        PressureLevel::Critical => 2.0,
    }
}

fn thermal_level(p: ThermalPressure) -> f64 {
    match p {
        ThermalPressure::Nominal => 0.0,
        ThermalPressure::Moderate => 1.0,
        ThermalPressure::Heavy => 2.0,
        ThermalPressure::Trapping => 3.0,
        ThermalPressure::Sleeping => 4.0,
    }
}

fn snake<T: serde::Serialize>(v: &T) -> String {
    serde_json::to_value(v)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_else(|| "other".into())
}

#[derive(Default, Clone, Copy)]
struct Agg {
    groups: u32,
    processes: u64,
    footprint: Option<f64>,
    resident: Option<f64>,
    gpu: Option<f64>,
    swapped: Option<f64>,
    cpu: Option<f64>,
    reclaim: Option<f64>,
    idle: u32,
}

fn add(a: &mut Option<f64>, v: Option<f64>) {
    if let Some(v) = v {
        *a = Some(a.unwrap_or(0.0) + v);
    }
}

impl Agg {
    fn merge(&mut self, o: &Agg) {
        self.groups += o.groups;
        self.processes += o.processes;
        add(&mut self.footprint, o.footprint);
        add(&mut self.resident, o.resident);
        add(&mut self.gpu, o.gpu);
        add(&mut self.swapped, o.swapped);
        add(&mut self.cpu, o.cpu);
        add(&mut self.reclaim, o.reclaim);
        self.idle += o.idle;
    }
}

/// Aggregates groups by `(kind, label)`, keeping the largest [`MAX_GROUP_SERIES`] and folding the rest.
fn group_series(s: &Snapshot) -> BTreeMap<(String, String), Agg> {
    let mut by: BTreeMap<(String, String), Agg> = BTreeMap::new();
    for g in &s.groups {
        let e = by
            .entry((g.kind.as_str().to_string(), redact_text(&g.label)))
            .or_default();
        e.merge(&Agg {
            groups: 1,
            processes: if g.totals.process_count > 0 {
                g.totals.process_count as u64
            } else {
                g.members.len() as u64
            },
            footprint: bytes(&g.totals.footprint),
            resident: bytes(&g.totals.resident),
            gpu: bytes(&g.totals.gpu),
            swapped: bytes(&g.totals.swapped),
            cpu: avail(&g.totals.cpu_pct),
            reclaim: bytes(&g.reclaim_gain),
            idle: g.idle as u32,
        });
    }
    if by.len() <= MAX_GROUP_SERIES {
        return by;
    }
    let mut ranked: Vec<((String, String), Agg)> = by.into_iter().collect();
    ranked.sort_by(|a, b| {
        b.1.footprint
            .unwrap_or(0.0)
            .partial_cmp(&a.1.footprint.unwrap_or(0.0))
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.0.cmp(&b.0))
    });
    let mut out: BTreeMap<(String, String), Agg> = BTreeMap::new();
    for (i, (key, agg)) in ranked.into_iter().enumerate() {
        let key = if i < MAX_GROUP_SERIES - 1 {
            key
        } else {
            (key.0, OTHER_LABEL.to_string())
        };
        out.entry(key).or_default().merge(&agg);
    }
    out
}

/// Prometheus text exposition for one snapshot (pure).
pub fn prometheus_text(s: &Snapshot, h: &Headroom) -> String {
    let mut w = Writer { out: String::new() };
    let m = &s.memory;

    w.gauge(
        "oomtop_info",
        "oomtop build and host facts (value is always 1).",
        vec![(
            labels(&[
                ("version", oomtop_core::VERSION),
                ("os", &snake(&s.host.os)),
                ("arch", &s.host.arch),
                ("schema_version", &s.schema_version.to_string()),
            ]),
            1.0,
        )],
    );
    w.one(
        "oomtop_snapshot_timestamp_seconds",
        "When the snapshot was taken (Unix time).",
        (s.taken_at_ms > 0).then(|| s.taken_at_ms as f64 / 1000.0),
    );

    // Memory (SPEC §5, §8.1).
    let total = bytes(&m.total).or((s.host.mem_total > 0).then_some(s.host.mem_total as f64));
    w.one("oomtop_memory_total_bytes", "Physical memory.", total);
    w.one(
        "oomtop_memory_available_bytes",
        "Available memory (headroom input, SPEC 8.1).",
        bytes(&m.available),
    );
    w.one("oomtop_memory_free_bytes", "Free memory.", bytes(&m.free));
    w.one("oomtop_memory_cached_bytes", "File cache.", bytes(&m.cached));
    w.one(
        "oomtop_memory_wired_bytes",
        "Wired / unevictable memory.",
        bytes(&m.wired),
    );
    w.one(
        "oomtop_memory_compressed_bytes",
        "Physical memory held by the compressor (zswap/zram on Linux).",
        bytes(&m.compressed),
    );
    w.one(
        "oomtop_memory_compressed_logical_bytes",
        "Uncompressed bytes stored in the compressor.",
        bytes(&m.compressed_logical),
    );
    w.one("oomtop_memory_app_bytes", "Anonymous app memory.", bytes(&m.app));
    w.one("oomtop_swap_used_bytes", "Swap used.", bytes(&m.swap_used));
    w.one("oomtop_swap_total_bytes", "Swap total.", bytes(&m.swap_total));
    w.one(
        "oomtop_swap_in_bytes_per_minute",
        "Swap-in rate.",
        bytes(&m.swap_in_per_min),
    );
    w.one(
        "oomtop_swap_out_bytes_per_minute",
        "Swap-out rate.",
        bytes(&m.swap_out_per_min),
    );
    w.one(
        "oomtop_memory_pressure_level",
        "Memory pressure: 0 normal, 1 warn, 2 critical.",
        avail(&m.pressure).map(pressure_level),
    );
    if let Some(psi) = avail(&m.psi) {
        w.gauge(
            "oomtop_memory_psi_percent",
            "Linux memory PSI (percent of time stalled).",
            vec![
                (labels(&[("kind", "some"), ("window", "10s")]), psi.some_avg10),
                (labels(&[("kind", "some"), ("window", "60s")]), psi.some_avg60),
                (labels(&[("kind", "some"), ("window", "300s")]), psi.some_avg300),
                (labels(&[("kind", "full"), ("window", "10s")]), psi.full_avg10),
                (labels(&[("kind", "full"), ("window", "60s")]), psi.full_avg60),
                (labels(&[("kind", "full"), ("window", "300s")]), psi.full_avg300),
            ],
        );
    }

    // Headroom (SPEC §8.1).
    w.one(
        "oomtop_headroom_bytes",
        "Available memory minus the safety margin (may be negative).",
        h.headroom.map(|x| x as f64),
    );
    w.one(
        "oomtop_safety_margin_bytes",
        "Safety margin (boosted under pressure or growing swap).",
        Some(h.safety_margin as f64),
    );
    w.one(
        "oomtop_reclaimable_bytes",
        "Estimated RAM freed by stopping idle build daemons, orphans and idle model servers.",
        bytes(&h.reclaimable),
    );
    w.one(
        "oomtop_swap_growing",
        "1 when swap use is growing (absent when swap is not measured).",
        // A trend derived from nothing is not a 0: omit it when swap use itself is unavailable.
        bytes(&m.swap_used).map(|_| if h.swap_growing { 1.0 } else { 0.0 }),
    );
    w.gauge(
        "oomtop_gpu_headroom_bytes",
        "GPU/Metal budget minus GPU memory in use and margin, per accelerator.",
        h.gpu
            .iter()
            .filter_map(|g| {
                g.headroom
                    .map(|x| (labels(&[("gpu", &g.accelerator_id)]), x as f64))
            })
            .collect(),
    );

    // OOM forecast (SPEC §8.3).
    if let Some(f) = &s.oom.forecast {
        let killer = f.killer.map(|k| snake(&k)).unwrap_or_else(|| "unknown".into());
        let l = labels(&[("target", &snake(&f.target)), ("killer", &killer)]);
        w.gauge(
            "oomtop_oom_eta_seconds",
            "Forecast time until the OOM target is reached.",
            vec![(l.clone(), f.eta_s as f64)],
        );
        w.gauge(
            "oomtop_oom_forecast_confidence",
            "R squared of the forecast fit (0..1).",
            vec![(l, f.confidence)],
        );
    }
    w.one(
        "oomtop_oom_recent_kills",
        "OOM kills seen recently (kernel, oomd, earlyoom, jetsam).",
        Some(s.oom.recent_kills.len() as f64),
    );

    // CPU.
    w.one(
        "oomtop_cpu_utilization_percent",
        "Whole-machine CPU utilization (100 = all cores busy).",
        avail(&s.cpu.total_pct),
    );
    w.gauge(
        "oomtop_load_average",
        "Load average.",
        [
            ("1m", &s.cpu.load_avg_1),
            ("5m", &s.cpu.load_avg_5),
            ("15m", &s.cpu.load_avg_15),
        ]
        .into_iter()
        .filter_map(|(k, v)| avail(v).map(|x| (labels(&[("window", k)]), x)))
        .collect(),
    );

    // Thermal & power (SPEC §9).
    let t = &s.thermal;
    w.one(
        "oomtop_thermal_pressure_level",
        "Thermal pressure: 0 nominal, 1 moderate, 2 heavy, 3 trapping, 4 sleeping.",
        avail(&t.pressure).map(thermal_level),
    );
    w.one(
        "oomtop_throttle_factor",
        "Observed / max frequency of busy units (0..1); absent while idle.",
        avail(&t.throttle_factor),
    );
    w.one(
        "oomtop_low_power_mode",
        "1 when Low Power Mode is on.",
        flag(&t.low_power_mode),
    );
    w.one(
        "oomtop_on_battery",
        "1 when running on battery.",
        flag(&t.on_battery),
    );
    w.one("oomtop_battery_percent", "Battery charge.", avail(&t.battery_pct));
    w.one(
        "oomtop_package_power_watts",
        "CPU+GPU package power.",
        avail(&t.package_power_w),
    );

    // Accelerators (bounded: one series per device).
    let acc = |f: &dyn Fn(&oomtop_core::Accelerator) -> Option<f64>| -> Vec<(String, f64)> {
        s.accelerators
            .iter()
            .filter_map(|a| f(a).map(|v| (labels(&[("gpu", &a.id), ("name", &a.name)]), v)))
            .collect()
    };
    w.gauge(
        "oomtop_gpu_memory_used_bytes",
        "GPU memory in use.",
        acc(&|a| bytes(&a.mem_used)),
    );
    w.gauge(
        "oomtop_gpu_memory_total_bytes",
        "GPU memory (VRAM) total.",
        acc(&|a| bytes(&a.mem_total)),
    );
    w.gauge(
        "oomtop_gpu_budget_bytes",
        "GPU working-set budget (Metal recommendedMaxWorkingSetSize / iogpu.wired_limit_mb).",
        acc(&|a| bytes(&a.gpu_budget)),
    );
    w.gauge(
        "oomtop_gpu_utilization_percent",
        "GPU utilization.",
        acc(&|a| avail(&a.util_pct)),
    );
    w.gauge(
        "oomtop_gpu_power_watts",
        "GPU power.",
        acc(&|a| avail(&a.power_w)),
    );
    w.gauge(
        "oomtop_gpu_temperature_celsius",
        "GPU temperature.",
        acc(&|a| avail(&a.temp_c)),
    );

    // Inventory.
    w.one(
        "oomtop_processes",
        "Processes in the snapshot.",
        Some(s.processes.len() as f64),
    );
    w.one("oomtop_groups", "Attributed groups.", Some(s.groups.len() as f64));
    w.one(
        "oomtop_model_servers",
        "Detected model servers.",
        Some(s.model_servers.len() as f64),
    );
    w.one(
        "oomtop_sandboxes",
        "Detected sandboxes.",
        Some(s.sandboxes.len() as f64),
    );
    w.gauge(
        "oomtop_source_status",
        "Data source status (1 for the current status of each source).",
        s.source_status
            .iter()
            .map(|(name, st)| {
                let st = match st {
                    SourceStatus::Available => "available",
                    SourceStatus::Partial(_) => "partial",
                    SourceStatus::Unavailable(_) => "unavailable",
                };
                (labels(&[("source", name), ("status", st)]), 1.0)
            })
            .collect(),
    );

    // Per-group series by (kind, label).
    let agg = group_series(s);
    let series = |f: &dyn Fn(&Agg) -> Option<f64>| -> Vec<(String, f64)> {
        agg.iter()
            .filter_map(|((k, l), a)| f(a).map(|v| (labels(&[("kind", k), ("label", l)]), v)))
            .collect()
    };
    w.gauge(
        "oomtop_group_footprint_bytes",
        "Group memory: footprint (macOS) / PSS (Linux), summed by kind and label.",
        series(&|a| a.footprint),
    );
    w.gauge(
        "oomtop_group_resident_bytes",
        "Group resident memory.",
        series(&|a| a.resident),
    );
    w.gauge("oomtop_group_gpu_bytes", "Group GPU memory.", series(&|a| a.gpu));
    w.gauge(
        "oomtop_group_swapped_bytes",
        "Group swapped memory.",
        series(&|a| a.swapped),
    );
    w.gauge(
        "oomtop_group_cpu_percent",
        "Group CPU (per-core percent).",
        series(&|a| a.cpu),
    );
    w.gauge(
        "oomtop_group_reclaim_gain_bytes",
        "Estimated RAM freed if the group were stopped.",
        series(&|a| a.reclaim),
    );
    w.gauge(
        "oomtop_group_processes",
        "Processes in the group(s).",
        series(&|a| Some(a.processes as f64)),
    );
    w.gauge(
        "oomtop_group_count",
        "Groups sharing this kind and label.",
        series(&|a| Some(a.groups as f64)),
    );
    w.gauge(
        "oomtop_group_idle_count",
        "Idle groups sharing this kind and label.",
        series(&|a| Some(a.idle as f64)),
    );
    w.out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_and_values() {
        assert_eq!(escape_label("a\"b\\c\nd"), "a\\\"b\\\\c\\nd");
        assert_eq!(fmt_value(1.0), "1");
        assert_eq!(fmt_value(0.25), "0.25");
        assert_eq!(fmt_value(f64::NAN), "NaN");
        assert_eq!(fmt_value(f64::INFINITY), "+Inf");
        assert_eq!(fmt_value(1e21), "1000000000000000000000");
    }
}
