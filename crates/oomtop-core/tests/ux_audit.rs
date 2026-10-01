//! Regression tests for defects found in the UX-logic audit (query understanding, headline, ranking).
//! Each test names the input that used to go wrong.

mod ux_common;

use oomtop_core::headline::{format_amount, input_from, render, HeadlineInput};
use oomtop_core::headroom::{compute, HeadroomConfig};
use oomtop_core::model::*;
use oomtop_core::modes::Mode;
use oomtop_core::query::*;
use oomtop_core::ranking::{anomaly_score, rank, Candidate, RankWeights};
use oomtop_core::units::{UnitSystem, GIB, MIB};
use std::collections::{HashMap, HashSet};
use ux_common::*;

fn vocab() -> Vocabulary {
    Vocabulary::from_snapshot(&machine(true), &HashMap::new(), &HashMap::new(), &HashSet::new())
}

fn name<'a>(v: &'a Vocabulary, id: &str) -> &'a str {
    &v.entities.iter().find(|e| e.id == id).unwrap().name
}

// --- typo correction ------------------------------------------------------------------------------------

#[test]
fn typing_a_prefix_of_an_entity_is_never_typo_fixed() {
    // Was: "clan" (on the way to "clang") → corrected to "clean" → Reclaim intent.
    let v = vocab();
    let u = understand("clan", &v);
    assert!(u.corrections.is_empty(), "{:?}", u.corrections);
    assert_eq!(u.intent, Intent::Find);
    assert_eq!(name(&v, u.top_target().unwrap()), "clang");
}

#[test]
fn four_letter_words_are_only_fixed_by_transposition() {
    let v = vocab();
    // Was: "zoom" → "room" (headroom), "heap" → "heat" (explain).
    for w in ["zoom", "heap"] {
        let u = understand(w, &v);
        assert!(u.corrections.is_empty(), "{w}: {:?}", u.corrections);
        assert_ne!(u.intent, Intent::Headroom, "{w}");
    }
    // A transposition is still a typo: "idel" → "idle".
    let u = understand("idel", &v);
    assert_eq!(
        u.corrections,
        vec![Correction {
            from: "idel".into(),
            to: "idle".into()
        }]
    );
    assert_eq!(u.intent, Intent::Reclaim);
    // Longer words keep full Damerau-Levenshtein ≤ 1.
    assert_eq!(understand("memroy hogs", &v).intent, Intent::RankBy(Metric::Mem));
    assert_eq!(understand("thrtotle", &v).intent, Intent::Explain);
}

// --- entity linking -------------------------------------------------------------------------------------

#[test]
fn camel_case_humps_are_name_tokens() {
    assert_eq!(name_tokens("GradleDaemon"), ["gradledaemon", "gradle", "daemon"]);
    assert_eq!(name_tokens("HTTPServer"), ["httpserver", "http", "server"]);
    assert_eq!(name_tokens("sd-server"), ["sd", "server"]);
    assert_eq!(name_tokens("Claude Code"), ["claude", "code"]);
    assert!(name_tokens("").is_empty());
    // Was: "gradel" linked nothing (the typo sits inside one camelCase word).
    let v = vocab();
    let u = understand("gradel", &v);
    assert_eq!(name(&v, u.top_target().unwrap()), "GradleDaemon");
    assert_eq!(u.intent, Intent::Find);
}

#[test]
fn strong_intent_with_a_target_is_not_ambiguous() {
    // Was: Explain 0.90 tied with "go to sd-server" 0.90 → confidence capped at 0.45 → palette choices.
    let v = vocab();
    let u = understand("why is sd-server slow", &v);
    assert_eq!(u.intent, Intent::Explain);
    assert!(!u.is_ambiguous(), "confidence {}", u.confidence);
    assert_eq!(name(&v, u.top_target().unwrap()), "sd-server");
    assert_eq!(u.interpretations[0].label, "explain slowdown · sd-server");
    assert_eq!(u.interpretations[1].label, "go to sd-server");

    let u = understand("stop chrome", &v);
    assert_eq!(u.intent, Intent::Reclaim);
    assert!(!u.is_ambiguous());
    assert_eq!(u.interpretations[0].targets[0].0, "app:google-chrome");

    // A weak intent word still leaves a clear entity query as Find.
    let u = understand("chrome memory", &v);
    assert_eq!(u.intent, Intent::Find);
    assert_eq!(u.top_target(), Some("app:google-chrome"));
}

// --- units ----------------------------------------------------------------------------------------------

#[test]
fn m_means_mib_when_talking_about_room() {
    let v = vocab();
    // Was: "room for 500m" read as 500 minutes (because of "for") → no need_bytes.
    let u = understand("room for 500m", &v);
    assert_eq!(u.intent, Intent::Headroom);
    assert_eq!(u.need_bytes, Some(500 * MIB));
    assert_eq!(u.interpretations[0].label, "can 500 MiB fit?");
    // Time words still win: idle for 30m is a duration.
    let u = understand("idle for 30m", &v);
    assert_eq!(u.intent, Intent::Reclaim);
    assert_eq!(u.filter.unwrap().to_string(), "idle>=30m");
    assert_eq!(u.need_bytes, None);
}

#[test]
fn headroom_command_with_a_bare_number_asks_for_a_unit() {
    let v = vocab();
    let u = understand(":headroom 13", &v);
    assert_eq!(u.intent, Intent::Headroom);
    assert_eq!(u.need_bytes, None);
    assert_eq!(u.error.as_deref(), Some("add a unit to 13, e.g. 13G or 13GB"));
    let u = understand(":headroom 13G", &v);
    assert_eq!(u.need_bytes, Some(13 * GIB));
    assert_eq!(u.error, None);
}

// --- grammar: quoting -----------------------------------------------------------------------------------

#[test]
fn quoted_literals_are_words_and_quoted_commas_do_not_split() {
    // Was: `"a:b"` → UnknownKey("a"); `owner:"Smith, John"` → values ["Smith", "John"].
    assert_eq!(
        parse_filter(r#""a:b""#).unwrap(),
        Expr::Term(Term::Word("a:b".into()))
    );
    assert_eq!(
        parse_filter(r#"owner:"Smith, John""#).unwrap(),
        Expr::Term(Term::Field {
            key: FieldKey::Owner,
            values: vec!["Smith, John".into()]
        })
    );
    assert_eq!(
        parse_filter(r#"owner: "Smith, John",bob"#).unwrap(),
        Expr::Term(Term::Field {
            key: FieldKey::Owner,
            values: vec!["Smith, John".into(), "bob".into()]
        })
    );
    assert_eq!(
        parse_filter(r#""or""#).unwrap(),
        Expr::Term(Term::Word("or".into()))
    );
    assert_eq!(
        parse_filter(r#"-"a b""#).unwrap(),
        Expr::Not(Box::new(Expr::Term(Term::Word("a b".into()))))
    );
    assert_eq!(
        parse_filter(r#"!"mem>2G""#).unwrap(),
        Expr::Not(Box::new(Expr::Term(Term::Word("mem>2G".into()))))
    );
}

#[test]
fn display_round_trips_through_the_parser() {
    let exprs = [
        Expr::Term(Term::Word("a:b".into())),
        Expr::Term(Term::Word("x>1".into())),
        Expr::Term(Term::Word("!bang".into())),
        Expr::Term(Term::Word("||".into())),
        Expr::Term(Term::Field {
            key: FieldKey::Owner,
            values: vec!["Smith, John".into(), "Claude Code".into()],
        }),
        Expr::Not(Box::new(Expr::Term(Term::Word("a b".into())))),
        Expr::And(vec![
            Expr::Or(vec![
                Expr::Term(Term::Word("chrome".into())),
                Expr::Term(Term::Word("x,y".into())),
            ]),
            Expr::Term(Term::Cmp {
                metric: Metric::Mem,
                op: CmpOp::Ge,
                value: (3 * GIB / 2) as f64,
            }),
        ]),
    ];
    for e in exprs {
        let text = e.to_string();
        assert_eq!(parse_filter(&text).as_ref(), Ok(&e), "{text}");
    }
    for text in [
        "kind:model,agent or gpu>1G -muted",
        r#"owner:"Claude Code" -(kind:app or idle>=2h)"#,
        "state:runtime:docker",
    ] {
        let e = parse_filter(text).unwrap();
        assert_eq!(parse_filter(&e.to_string()).unwrap(), e, "{text}");
    }
}

#[test]
fn a_quoted_phrase_filters_literally_and_links_its_entity() {
    let v = vocab();
    let s = machine(true);
    let u = understand(r#""Claude Code""#, &v);
    assert_eq!(u.intent, Intent::Find);
    assert_eq!(u.filter.as_ref().unwrap().to_string(), r#""Claude Code""#);
    assert_eq!(name(&v, u.top_target().unwrap()), "Claude Code");
    let f = u.filter.unwrap();
    let hits = s
        .groups
        .iter()
        .filter(|g| eval(&f, &EntityView::from_group(g, &s)))
        .count();
    assert_eq!(
        hits, 4,
        "the four Claude Code sessions, not the Claude desktop VM"
    );
}

#[test]
fn state_values_accept_globs() {
    let mut e = EntityView {
        name: "x".into(),
        ..Default::default()
    };
    e.add_state("orphan");
    e.add_state("runtime:docker");
    assert!(eval(&parse_filter("state:orph*").unwrap(), &e));
    assert!(eval(&parse_filter("state:runtime:*").unwrap(), &e));
    assert!(!eval(&parse_filter("state:idle*").unwrap(), &e));
    assert!(eval(&parse_filter("state:orphan").unwrap(), &e));
}

// --- headline -------------------------------------------------------------------------------------------

#[test]
fn amounts_promote_at_the_unit_boundary() {
    // Was: "1000 MB" and "1024 MiB".
    assert_eq!(format_amount(999_960_000, UnitSystem::Si), "1.0 GB");
    assert_eq!(format_amount(999_400_000, UnitSystem::Si), "999 MB");
    assert_eq!(format_amount(1_073_717_248, UnitSystem::Iec), "1.0 GiB");
    assert_eq!(format_amount(999, UnitSystem::Si), "999 B");
    assert_eq!(format_amount(1_000, UnitSystem::Si), "1.0 KB");
    assert_eq!(format_amount(u64::MAX, UnitSystem::Si), "18446744 TB");
}

#[test]
fn throttle_lead_never_contradicts_the_factor() {
    // Was: "Running at 95% speed — thermal pressure heavy." (the factor is not a slowdown).
    let h = render(&HeadlineInput {
        mode: Mode::Throttle,
        throttle_factor: Some(0.95),
        thermal: Some(ThermalPressure::Heavy),
        ..Default::default()
    });
    assert_eq!(h.text, "Thermal pressure heavy. Pause heavy work or let it cool.");
    // Non-finite numbers are never rendered ("NaN% speed", "on battery (NaN%)").
    let h = render(&HeadlineInput {
        mode: Mode::Throttle,
        throttle_factor: Some(f64::NAN),
        on_battery_pct: Some(f64::NAN),
        low_power_mode: true,
        ..Default::default()
    });
    assert_eq!(h.text, "Low Power Mode on. Turn it off or pause heavy work.");
    assert!(!h.text.contains("NaN"));
    let h = render(&HeadlineInput {
        mode: Mode::Throttle,
        throttle_factor: Some(0.5),
        on_battery_pct: Some(140.0),
        ..Default::default()
    });
    assert_eq!(
        h.text,
        "Running at 50% speed — on battery (100%). Plug in or pause heavy work."
    );
}

#[test]
fn working_headline_shows_only_a_model_basename() {
    let mut s = machine(false);
    s.model_servers[0].group_id = None;
    s.model_servers[0].busy = Measured::exact(true, "adapter");
    s.model_servers[0].models = vec![LoadedModel {
        name: "/Users/someone/models/flux-q8.gguf".into(),
        ..Default::default()
    }];
    let h = compute(&s, &HeadroomConfig::default());
    let input = input_from(&s, &h, Mode::Working);
    assert_eq!(input.working.as_deref(), Some("flux-q8.gguf generating"));
    assert!(!render(&input).text.contains("/Users/"));
}

// --- ranking --------------------------------------------------------------------------------------------

#[test]
fn ranking_never_panics_on_non_finite_features() {
    let c = |id: &str, s: f64| Candidate {
        id: id.into(),
        salience: s,
        cluster: Some("same".into()),
        ..Default::default()
    };
    let r = rank(
        &[c("a", f64::NAN), c("b", f64::INFINITY), c("c", 0.5)],
        &RankWeights::default(),
        0.15,
    );
    assert_eq!(r.len(), 3);
    assert!(r.iter().all(|(_, p)| p.total.is_finite()));
    assert_eq!(anomaly_score(&[]), 0.0);
    assert_eq!(anomaly_score(&[u64::MAX; 8]), 0.0);
}

#[test]
fn same_mode_context_affinity_adds_to_affinity_and_is_explained() {
    use oomtop_core::ranking::{
        explain_rank_for, group_candidates_with, score, ColdStartPriors, RankContext, RankFacts,
        CONTEXT_AFFINITY_WEIGHT,
    };
    // UX §5.4 "+ context match: same mode": the user opens GradleDaemon during Pressure.
    let s = machine(true);
    let aff: HashMap<String, f64> = [(FP_GRADLE.to_string(), 0.4)].into();
    let in_pressure: HashMap<String, f64> = [(FP_GRADLE.to_string(), 1.0), (FP_SD.to_string(), 0.5)].into();
    let base = RankContext {
        mode: Mode::Pressure,
        affinity: Some(&aff),
        priors: ColdStartPriors::detect(&s),
        ..Default::default()
    };
    let with_ctx = RankContext {
        mode_affinity: Some(&in_pressure),
        ..base
    };
    let find = |c: &[Candidate], id: &str| c.iter().find(|c| c.id == id).unwrap().clone();
    let a = group_candidates_with(&s, &base);
    let b = group_candidates_with(&s, &with_ctx);
    let g0 = find(&a, "daemon:gradledaemon");
    let g1 = find(&b, "daemon:gradledaemon");
    assert!((g1.affinity - (0.4 + CONTEXT_AFFINITY_WEIGHT)).abs() < 1e-9);
    assert!(g1.affinity > g0.affinity);
    // A context signal alone counts as learned: sd-server loses its cold-start prior, gains context affinity.
    assert!(find(&a, "model:sd-server").prior > 0.0);
    let sd = find(&b, "model:sd-server");
    assert_eq!(sd.prior, 0.0);
    assert!((sd.affinity - 0.5 * CONTEXT_AFFINITY_WEIGHT).abs() < 1e-9);
    // Capped at 1.
    let full: HashMap<String, f64> = [(FP_GRADLE.to_string(), 1.0)].into();
    let c = group_candidates_with(
        &s,
        &RankContext {
            affinity: Some(&full),
            ..with_ctx
        },
    );
    assert_eq!(find(&c, "daemon:gradledaemon").affinity, 1.0);

    let g = s.group("daemon:gradledaemon").unwrap();
    let mut facts = RankFacts::from_group(g, Some(6));
    facts.opens_in_mode = Some((Mode::Pressure, 4));
    facts.units = UnitSystem::Si;
    let parts = score(
        &Candidate {
            affinity: 1.0,
            salience: 0.1,
            ..g1
        },
        &RankWeights::default(),
    );
    assert_eq!(
        explain_rank_for(2, &parts, &facts),
        "2nd: you opened it 6× this week, 4× in pressure mode (affinity) · idle 3h40m, could free 2.9 GB \
         (actionability) · holds 2.9 GB (salience)."
    );
}

#[test]
fn oom_forecast_is_the_top_insight_in_every_mode() {
    // UX §5.5 "top insight, else mode summary": the ETA shows before the Pressure latch (10 s) catches up.
    for mode in Mode::ALL {
        let h = render(&HeadlineInput {
            mode,
            forecast_eta_s: Some(360),
            forecast_target: Some(ForecastTarget::SwapExhaustion),
            top_owner: Some(("sd-server".into(), 9_900_000_000)),
            best_action: Some(("2 idle build daemons".into(), 5_900_000_000)),
            free: Some(11_000_000_000),
            working: Some("sd-server generating 3/6".into()),
            ..Default::default()
        });
        assert_eq!(
            h.text,
            "Swap full in ~6 min at this rate — sd-server holds 9.9 GB; 2 idle build daemons could free 5.9 GB.",
            "{mode:?}"
        );
        assert_eq!(h.action_key, Some('r'));
        assert_eq!(h.mode, mode, "the header still shows the latched mode");
    }
}
