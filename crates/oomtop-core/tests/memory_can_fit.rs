//! Golden tests for `can_fit` decisions (SPEC §8.2, §17 acceptance #6: `oomtop headroom --need 13G`
//! gives the correct yes / yes-after-reclaim / no answer and exit code).

mod memory_common;

use memory_common::*;
use oomtop_core::can_fit::*;
use oomtop_core::headroom::{compute, HeadroomConfig};
use oomtop_core::model_estimate::{estimate_diffusion, estimate_llm, parse_gguf_header, KvParams};
use oomtop_core::units::{format_bytes_short, parse_bytes, GIB, MIB};
use oomtop_core::*;
use serde::Serialize;

#[derive(Serialize)]
struct Decision {
    scenario: &'static str,
    available: String,
    safety_margin: String,
    headroom: Option<String>,
    answer: &'static str,
    exit_code: i32,
    reclaim: Vec<String>,
    gain: Option<String>,
    shortfall: Option<String>,
    reason: String,
}

fn signed(v: i64) -> String {
    if v < 0 {
        format!("-{}", format_bytes_short(v.unsigned_abs()))
    } else {
        format_bytes_short(v as u64)
    }
}

fn decide(scenario: &'static str, s: &Snapshot, need: &Need) -> (Decision, CanFitAnswer) {
    let h = compute(s, &HeadroomConfig::default());
    // oomtop's own group (and the caller's session) never appear as candidates.
    let a = can_fit_snapshot(need, s, &HeadroomConfig::default(), Some("agent:77b1c0de0042"));
    let (answer, reclaim, gain, shortfall) = match &a.fit {
        Fit::Yes => ("yes", vec![], None, None),
        Fit::YesAfterReclaim { reclaim, gain } => (
            "yes_after_reclaim",
            reclaim
                .iter()
                .map(|c| format!("{} ≈{}", c.group_id, format_bytes_short(c.gain)))
                .collect(),
            Some(format_bytes_short(*gain)),
            None,
        ),
        Fit::No { shortfall } => ("no", vec![], None, Some(format_bytes_short(*shortfall))),
    };
    (
        Decision {
            scenario,
            available: h
                .available_now
                .value
                .map(format_bytes_short)
                .unwrap_or_else(|| "n/a".into()),
            safety_margin: format_bytes_short(h.safety_margin),
            headroom: h.headroom.map(signed),
            answer,
            exit_code: answer_exit_code(&a),
            reclaim,
            gain,
            shortfall,
            reason: a.reason.clone(),
        },
        a,
    )
}

fn settings() -> insta::Settings {
    let mut s = insta::Settings::clone_current();
    s.set_snapshot_path("memory_snapshots");
    s.set_prepend_module_to_snapshot(false);
    s
}

#[test]
fn acceptance_6_need_13g() {
    let need = Need {
        label: Some("13G".into()),
        ..Need::bytes(parse_bytes("13G").unwrap())
    };
    let mut out = Vec::new();

    // Plenty free (studio stopped, daemons already gone).
    let (d, a) = decide("plenty free", &m5_air(16 * GIB), &need);
    assert_eq!(a.fit, Fit::Yes);
    assert_eq!(answer_exit_code(&a), EXIT_YES);
    out.push(d);

    // Just short: stopping the idle Gradle daemon is enough (the prune keeps Kotlin running).
    let (d, a) = decide("short by 1.9G", &m5_air(13 * GIB), &need);
    assert_eq!(answer_exit_code(&a), EXIT_YES_AFTER_RECLAIM);
    match &a.fit {
        Fit::YesAfterReclaim { reclaim, .. } => {
            assert_eq!(reclaim.len(), 1);
            assert_eq!(reclaim[0].group_id, "daemon:gradle");
        }
        other => panic!("{other:?}"),
    }
    out.push(d);

    // Needs both daemons.
    let (d, a) = decide("needs both daemons", &m5_air(10 * GIB), &need);
    assert_eq!(answer_exit_code(&a), EXIT_YES_AFTER_RECLAIM);
    match &a.fit {
        Fit::YesAfterReclaim { reclaim, .. } => assert_eq!(reclaim.len(), 2),
        other => panic!("{other:?}"),
    }
    out.push(d);

    // As captured (≈ 6.9 GB available): not even after reclaim.
    let (d, a) = decide("as captured", &m5_air(420_320 * PAGE), &need);
    assert_eq!(answer_exit_code(&a), EXIT_NO);
    out.push(d);

    // Swap growing: +50 % margin turns a plain yes into yes-after-reclaim.
    let mut s = m5_air(15 * GIB);
    let (_, plain) = decide("", &s, &need);
    assert_eq!(plain.fit, Fit::Yes);
    s.memory.swap_out_per_min = Measured::exact(600 * MIB, "vm_statistics64.swapouts Δ");
    let (d, a) = decide("swap growing", &s, &need);
    assert_eq!(answer_exit_code(&a), EXIT_YES_AFTER_RECLAIM);
    out.push(d);

    // Gradle busy compiling: not offered; Kotlin alone is not enough → no.
    let mut s = m5_air(10 * GIB);
    s.groups[0].idle = false;
    s.groups[0].totals.cpu_pct = Measured::exact(180.0, "Σ cpu");
    let (d, a) = decide("gradle busy", &s, &need);
    assert_eq!(answer_exit_code(&a), EXIT_NO);
    out.push(d);

    // No memory data: error, exit 1.
    let mut s = m5_air(0);
    s.memory.available = Measured::unavailable("vm_statistics64", "host_statistics64 failed");
    s.memory.memorystatus_level = Measured::unavailable("kern.memorystatus_level", "unreadable");
    let (d, a) = decide("no memory data", &s, &need);
    assert_eq!(answer_exit_code(&a), EXIT_ERROR);
    out.push(d);

    settings().bind(|| {
        insta::assert_yaml_snapshot!("can_fit_acceptance_6", out);
    });
}

#[test]
fn answer_json_schema() {
    let (_, a) = decide("short by 1.9G", &m5_air(13 * GIB), &Need::bytes(13 * GIB));
    assert_eq!(a.valid_for_s, 10);
    assert_eq!(a.expires_at_ms, a.as_of_ms + 10_000);
    settings().bind(|| {
        insta::assert_json_snapshot!("can_fit_answer_json", a);
    });
}

#[test]
fn model_aware_needs() {
    let mut out = Vec::new();
    // Qwen3-VL-8B Q4_K_M on Metal (M5 acceptance #6 with --model): 5.03 GB file.
    let meta = parse_gguf_header(&qwen3_8b_gguf()).unwrap();
    let est = estimate_llm(5_027_784_800, Some(&meta), &KvParams::default());
    let need = Need::from_estimate(&est, true, "Qwen3VL-8B-Instruct-Q4_K_M.gguf");
    let (d, a) = decide("8B GGUF on Metal, as captured", &m5_air(420_320 * PAGE), &need);
    assert_eq!(a.gpu_device.as_deref(), Some("gpu0"));
    assert!(a.gpu_checked);
    assert_eq!(answer_exit_code(&a), EXIT_YES_AFTER_RECLAIM);
    out.push(d);

    // The whole Qwen-Image studio (Q4_K_M diffusion + VAE + text encoder + mmproj + turbo LoRA), fashion edit.
    let studio = estimate_diffusion(
        &[
            4_604_558_112,
            675_509_688,
            5_027_784_800,
            1_159_029_824,
            679_604_800,
        ],
        448,
        608,
    );
    let need = Need::from_estimate(&studio, true, "qwen-image-studio (448×608)");
    let (d, a) = decide("studio on Metal, 18G free", &m5_air(18 * GIB), &need);
    assert_eq!(answer_exit_code(&a), EXIT_YES);
    out.push(d);
    let (d, _) = decide("studio on Metal, 10G free", &m5_air(10 * GIB), &need);
    out.push(d);

    // Discrete GPU: host RAM is fine, VRAM is not, unless the idle Ollama unloads (GPU-resident gain).
    let s = linux_box(40 * GIB);
    let need = Need {
        bytes: 14 * GIB,
        gpu_bytes: Some(14 * GIB),
        label: Some("14G on CUDA".into()),
    };
    let h = compute(&s, &HeadroomConfig::default());
    let cands = reclaim_candidates(&s);
    assert_eq!(cands[0].gpu_gain, Some(9 * GIB));
    assert_eq!(cands[0].swap_gain, Some(256 * MIB));
    let a = can_fit(&need, &h, &cands);
    assert_eq!(answer_exit_code(&a), EXIT_YES_AFTER_RECLAIM);
    let a_no = can_fit(&need, &h, &[]);
    assert!(a_no.gpu_shortfall.is_some());
    let (d, _) = decide("14G on a 24G RTX 4090 with 10G in use", &s, &need);
    out.push(d);

    settings().bind(|| {
        insta::assert_yaml_snapshot!("can_fit_models", out);
    });
}
