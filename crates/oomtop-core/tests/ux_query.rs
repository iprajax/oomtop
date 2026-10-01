//! UX §11 test 3 (`cl` selects the user's Claude Code group, not `clang`) and test 4 (`gpu hogs`,
//! `what's eating memory` and `mem>2G` produce the expected intent/filters), plus golden outputs for the
//! UX §5.2 examples.

mod ux_common;

use oomtop_core::query::*;
use oomtop_core::ranking::{query_scores, RankState};
use oomtop_core::units::GIB;
use std::collections::{HashMap, HashSet};
use ux_common::*;

fn vocab_with(frecency: &[(&str, f64)]) -> Vocabulary {
    let s = machine(true);
    let fr: HashMap<String, f64> = frecency.iter().map(|(k, v)| (k.to_string(), *v)).collect();
    Vocabulary::from_snapshot(&s, &fr, &HashMap::new(), &HashSet::new())
}

fn label_of<'a>(v: &'a Vocabulary, id: &str) -> &'a str {
    &v.entities.iter().find(|e| e.id == id).unwrap().name
}

#[test]
fn test3_cl_selects_users_claude_code_not_clang() {
    // The user has opened Claude Code many times this week; never clang.
    let v = vocab_with(&[(FP_CLAUDE, 9.0)]);
    let u = understand("cl", &v);
    assert_eq!(u.intent, Intent::Find);
    assert!(!u.is_ambiguous(), "confidence {}", u.confidence);
    let top = u.top_target().unwrap();
    assert_eq!(label_of(&v, top), "Claude Code");
    assert!(
        u.targets.iter().any(|(id, _)| label_of(&v, id) == "clang"),
        "clang is still reachable"
    );

    // Serving: the query match puts a Claude Code session first in the ranked list.
    let s = machine(true);
    let q = query_scores(&u, &s);
    let best = q
        .iter()
        .max_by(|a, b| a.1.partial_cmp(b.1).unwrap().then(b.0.cmp(a.0)))
        .unwrap();
    assert_eq!(s.group(best.0).unwrap().label, "Claude Code");

    // Without history the two are close; typing more disambiguates without frecency.
    let cold = vocab_with(&[]);
    let u = understand("clau", &cold);
    assert_eq!(label_of(&cold, u.top_target().unwrap()), "Claude Code");
    let u = understand("clang", &cold);
    assert_eq!(label_of(&cold, u.top_target().unwrap()), "clang");
}

#[test]
fn test3_frecency_flips_the_winner() {
    // A user who compiles all day and never opens Claude Code gets clang for `cl`.
    let v = vocab_with(&[(FP_CLANG, 0.0)]);
    let mut v2 = v.clone();
    for e in &mut v2.entities {
        if e.name == "clang" {
            e.frecency = 20.0;
        }
    }
    let u = understand("cl", &v2);
    assert_eq!(label_of(&v2, u.top_target().unwrap()), "clang");
}

#[test]
fn test4_intents_and_filters() {
    let v = vocab_with(&[]);
    let u = understand("gpu hogs", &v);
    assert_eq!(u.intent, Intent::RankBy(Metric::Gpu));
    assert_eq!(u.sort, Some(Metric::Gpu));
    assert_eq!(u.filter, None);
    assert!(u.confidence >= 0.8);

    let u = understand("what's eating memory", &v);
    assert_eq!(u.intent, Intent::RankBy(Metric::Mem));
    assert_eq!(u.sort, Some(Metric::Mem));
    assert_eq!(u.filter, None);
    assert!(u.confidence >= 0.8);

    let u = understand("mem>2G", &v);
    assert_eq!(u.intent, Intent::Find);
    assert_eq!(
        u.filter,
        Some(Expr::Term(Term::Cmp {
            metric: Metric::Mem,
            op: CmpOp::Gt,
            value: (2 * GIB) as f64,
        }))
    );
    assert_eq!(u.sort, Some(Metric::Mem));
    assert_eq!(u.confidence, 1.0);

    // mem>2G on the motivating machine: sd-server, the daemons and Chrome.
    let s = machine(true);
    let f = u.filter.unwrap();
    let mut hits: Vec<String> = s
        .groups
        .iter()
        .filter(|g| eval(&f, &EntityView::from_group(g, &s)))
        .map(|g| g.label.clone())
        .collect();
    hits.sort();
    assert_eq!(
        hits,
        [
            "Google Chrome",
            "GradleDaemon",
            "KotlinCompileDaemon",
            "sd-server"
        ]
    );
}

fn golden(u: &Understanding) -> serde_json::Value {
    serde_json::json!({
        "intent": u.intent,
        "filter": u.filter.as_ref().map(|f| f.to_string()),
        "sort": u.sort,
        "need_bytes": u.need_bytes,
        "command": u.command,
        "confidence": (u.confidence * 100.0).round() / 100.0,
        "ambiguous": u.is_ambiguous(),
        "corrections": u.corrections,
        "normalized": u.normalized,
        "error": u.error,
        "interpretations": u.interpretations.iter().map(|i| format!("{} ({:.2})", i.label, i.score)).collect::<Vec<_>>(),
    })
}

#[test]
fn test4_golden_understandings() {
    let v = vocab_with(&[(FP_CLAUDE, 9.0), (FP_SD, 4.0)]);
    let inputs = [
        "gpu hogs",
        "what's eating memory",
        "mem>2G",
        "mem>2G kind:daemon idle>30m",
        "gpu>1G",
        r#"owner:"Claude Code""#,
        "sandbox:*",
        "kind:model,agent or gpu>1G -muted",
        "claude",
        "why slow",
        "idle stuff",
        "models",
        "can I load 13g",
        "can i load 13 gb",
        ":reclaim",
        ":headroom 13G",
        ":why",
        ":pin sd-server",
        ":mode pressure",
        "throtle",
        "gpu hogs kind:model",
        "memroy hogs",
        "kind:modle",
        "(kind:app",
        "frobnicate",
    ];
    let out: Vec<serde_json::Value> = inputs
        .iter()
        .map(|i| serde_json::json!({ "input": i, "understood": golden(&understand(i, &v)) }))
        .collect();
    insta::assert_json_snapshot!("ux_5_2_understandings", out);
}

#[test]
fn low_confidence_offers_top_three() {
    let v = vocab_with(&[]);
    let u = understand("frobnicate", &v);
    assert!(u.is_ambiguous());
    assert_eq!(u.interpretations.len(), 3);
    assert_eq!(u.alternatives.len(), 3);
    // Two similarly named entities with no history: ambiguous, both offered.
    let vocab = Vocabulary {
        entities: vec![
            VocabEntity {
                id: "a".into(),
                name: "node server".into(),
                ..Default::default()
            },
            VocabEntity {
                id: "b".into(),
                name: "node worker".into(),
                ..Default::default()
            },
        ],
    };
    let u = understand("node", &vocab);
    assert!(u.is_ambiguous(), "confidence {}", u.confidence);
    assert!(u.interpretations.len() >= 2);
}

#[test]
fn query_match_dominates_ranking() {
    use oomtop_core::modes::Mode;
    use oomtop_core::ranking::{group_candidates_with, rank, RankContext, RankWeights};
    let s = machine(true);
    let v = vocab_with(&[]);
    let u = understand("kind:agent", &v);
    let q = query_scores(&u, &s);
    let ctx = RankContext {
        mode: Mode::Calm,
        query: Some(&q),
        ..Default::default()
    };
    let ranked = rank(&group_candidates_with(&s, &ctx), &RankWeights::default(), 0.15);
    let top4: Vec<&str> = ranked
        .iter()
        .take(4)
        .map(|(id, _)| s.group(id).unwrap().label.as_str())
        .collect();
    assert_eq!(top4, ["Claude Code"; 4]);
    // Served through the stability layer the first frame is plain score order.
    let scored: Vec<(String, f64)> = ranked.iter().map(|(id, p)| (id.clone(), p.total)).collect();
    let rows = RankState::new().apply(&scored, None, 0.05);
    assert_eq!(s.group(&rows[0].id).unwrap().label, "Claude Code");
}

#[test]
fn understanding_is_fast() {
    // UX §5.2: < 1 ms per query (release). Debug builds get a generous bound; this guards against
    // accidental quadratic blowups, not absolute speed.
    let mut v = vocab_with(&[(FP_CLAUDE, 9.0)]);
    for i in 0..300 {
        v.entities.push(VocabEntity {
            id: format!("pid:{i}"),
            name: format!("helper-process-{i} renderer"),
            ..Default::default()
        });
    }
    let queries = [
        "cl",
        "what's eating memory",
        "kind:model,agent or gpu>1G -muted",
        "gradel daemons",
    ];
    let start = std::time::Instant::now();
    let n = 20;
    for _ in 0..n {
        for q in queries {
            std::hint::black_box(understand(q, &v));
        }
    }
    let per = start.elapsed() / (n * queries.len() as u32);
    eprintln!(
        "understand(): {per:?} per query over {} entities",
        v.entities.len()
    );
    assert!(per.as_millis() < 50, "{per:?}");
}
