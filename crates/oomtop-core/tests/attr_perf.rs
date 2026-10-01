//! Attribution cost on a large synthetic process table (SPEC §14: the whole UX layer gets ≤ 5 ms/frame;
//! attribution runs once per process sample). Prints the timing; the bound is generous for debug builds.

mod attr_support;

use attr_support::*;
use oomtop_core::actions::ProtectContext;
use oomtop_core::attribution::{attribute, AttributionContext};
use std::collections::BTreeMap;
use std::time::Instant;

#[test]
fn attributes_3000_processes_quickly() {
    let mut v = Vec::new();
    // 300 top-level roots, each with a 9-process tree (depth 3), plus marker carriers and app helpers
    for r in 0..300u32 {
        let root = 10_000 + r * 10;
        let exe = match r % 5 {
            0 => "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome".to_string(),
            1 => "/usr/bin/java".to_string(),
            2 => "/opt/homebrew/bin/node".to_string(),
            3 => format!("/usr/local/bin/tool{r}"),
            _ => "/System/Library/CoreServices/daemon".to_string(),
        };
        let mut p = P::new(root, 1, 500, &exe).mem(100 * MIB, 90 * MIB).cpu(1.0);
        if r % 5 == 1 {
            p = p.argv(&[
                "java",
                "-cp",
                "x.jar",
                "org.gradle.launcher.daemon.bootstrap.GradleDaemon",
                "9.8.0",
            ]);
        }
        v.push(p.build());
        for c in 1..10u32 {
            let parent = if c <= 3 { root } else { root + (c - 1) / 3 };
            let mut child = P::new(root + c, parent, 400, "/opt/homebrew/bin/node").mem(10 * MIB, 9 * MIB);
            if r % 7 == 0 {
                child = child.markers(claude_markers(SESSIONS[(r % 4) as usize]));
            }
            v.push(child.build());
        }
    }
    let s = oomtop_core::Snapshot {
        taken_at_ms: NOW_MS,
        processes: v,
        ..Default::default()
    };
    assert_eq!(s.processes.len(), 3000);
    let r = rules();
    let lineage = BTreeMap::new();
    let protect = ProtectContext::default();
    let ctx = AttributionContext {
        rules: &r,
        lineage: &lineage,
        protect: &protect,
    };
    let _ = attribute(&s, &ctx); // warm-up (regex/glob caches)
    let t = Instant::now();
    let runs = 5;
    let mut n = 0;
    for _ in 0..runs {
        n = attribute(&s, &ctx).len();
    }
    let per = t.elapsed() / runs;
    println!("attribute(3000 processes) = {per:?} per run, {n} groups");
    assert!(n > 0);
    assert!(per.as_millis() < 1500, "attribution too slow: {per:?}");
}
