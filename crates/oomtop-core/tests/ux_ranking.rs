//! UX §11 test 5: rows under the cursor never move; no row changes position more than once per 4 s without a
//! real score change (2 s refresh, ≥ 2 refreshes beating the margin). Also: profile reset returns ranking
//! to salience-only (test 7, core part), per-mode order and "why ranked here" goldens.

mod ux_common;

use oomtop_core::model::*;
use oomtop_core::modes::Mode;
use oomtop_core::ranking::*;
use std::collections::{HashMap, HashSet};
use ux_common::*;

const REFRESH_S: u64 = 2;
const MARGIN: f64 = DEFAULT_MOVE_MARGIN;

/// Deterministic xorshift noise in [-1, 1].
struct Noise(u64);
impl Noise {
    fn next(&mut self) -> f64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 % 20_001) as f64 / 10_000.0 - 1.0
    }
}

fn base_scores() -> Vec<(String, f64)> {
    let s = machine(true);
    let c = group_candidates(&s, Mode::Calm, &HashMap::new(), &HashSet::new());
    rank(&c, &RankWeights::default(), DEFAULT_DIVERSITY_LAMBDA)
        .into_iter()
        .map(|(id, p)| (id, p.total))
        .collect()
}

fn ids(rows: &[RankedRow]) -> Vec<String> {
    rows.iter().map(|r| r.id.clone()).collect()
}

struct Run {
    orders: Vec<Vec<String>>,
    scores: Vec<HashMap<String, f64>>,
    moved: Vec<HashSet<String>>,
}

fn simulate(
    refreshes: usize,
    mut scores_at: impl FnMut(usize) -> Vec<(String, f64)>,
    mut selected_at: impl FnMut(usize, &[String]) -> Option<String>,
) -> Run {
    let mut st = RankState::new();
    let mut run = Run {
        orders: Vec::new(),
        scores: Vec::new(),
        moved: Vec::new(),
    };
    for k in 0..refreshes {
        let sc = scores_at(k);
        let prev = run.orders.last().cloned().unwrap_or_default();
        let sel = selected_at(k, &prev);
        let rows = st.apply(&sc, sel.as_deref(), MARGIN);
        run.moved
            .push(rows.iter().filter(|r| r.moved).map(|r| r.id.clone()).collect());
        run.orders.push(ids(&rows));
        run.scores.push(sc.into_iter().collect());
    }
    run
}

fn pos(order: &[String], id: &str) -> usize {
    order.iter().position(|x| x == id).unwrap()
}

#[test]
fn test5_small_noise_never_reorders() {
    let base = base_scores();
    let mut n = Noise(0x9e3779b97f4a7c15);
    let run = simulate(
        150,
        |_| {
            base.iter()
                .map(|(id, s)| (id.clone(), s + n.next() * MARGIN / 2.0))
                .collect()
        },
        |_, _| None,
    );
    assert!(
        run.orders.windows(2).all(|w| w[0] == w[1]),
        "order changed under sub-margin noise"
    );
}

#[test]
fn test5_flip_flopping_rows_never_move() {
    let base = base_scores();
    let (a, b) = (base[2].0.clone(), base[3].0.clone());
    let run = simulate(
        100,
        |k| {
            base.iter()
                .map(|(id, s)| {
                    // a and b swap by a large margin every refresh.
                    let bump = if (id == &a) == (k % 2 == 0) && (id == &a || id == &b) {
                        1.0
                    } else {
                        0.0
                    };
                    (id.clone(), s + bump)
                })
                .collect()
        },
        |_, _| None,
    );
    let changes = run.orders.windows(2).filter(|w| w[0] != w[1]).count();
    assert_eq!(changes, 0, "a 2 s flip-flop must never reorder rows");
}

#[test]
fn test5_noisy_scores_move_only_on_sustained_wins_and_never_under_cursor() {
    let base = base_scores();
    let mut n = Noise(0xdeadbeefcafef00d);
    // Heavy noise (±0.3 ≫ margin) so moves do happen; the cursor sits on the 4th row, later the 7th.
    let sel_idx = |k: usize| if k < 150 { 3 } else { 6 };
    let run = simulate(
        300,
        |_| {
            base.iter()
                .map(|(id, s)| (id.clone(), s + n.next() * 0.3))
                .collect()
        },
        |k, prev| prev.get(sel_idx(k)).cloned(),
    );
    let selected = |k: usize| (k > 0).then(|| run.orders[k - 1][sel_idx(k)].clone());
    // The row the state machine compared `id` against at refresh k: its upper neighbour in the previous
    // order with the selected row taken out.
    let above = |k: usize, id: &str| -> String {
        let work: Vec<&String> = run.orders[k - 1]
            .iter()
            .filter(|x| Some((*x).clone()) != selected(k))
            .collect();
        let p = work.iter().position(|x| x.as_str() == id).unwrap();
        work[p - 1].clone()
    };
    let mut climbs = 0;
    let mut last_climb: HashMap<String, usize> = HashMap::new();
    for k in 1..run.orders.len() {
        let (before, after) = (&run.orders[k - 1], &run.orders[k]);
        // Rows under the cursor never move.
        let sel = selected(k).unwrap();
        assert_eq!(
            pos(before, &sel),
            pos(after, &sel),
            "refresh {k}: row under the cursor moved"
        );
        for id in &run.moved[k] {
            climbs += 1;
            assert!(k >= 2, "no climb without two refreshes of evidence");
            // Sustained win: beat the compared row by the margin on this and the previous refresh.
            let a = above(k, id);
            assert!(
                run.scores[k][id] > run.scores[k][&a] + MARGIN,
                "refresh {k}: {id}"
            );
            let a_prev = above(k - 1, id);
            assert!(
                run.scores[k - 1][id] > run.scores[k - 1][&a_prev] + MARGIN,
                "refresh {k}: {id} moved without a win on the previous refresh"
            );
            // A row climbs at most once per 4 s (2 refreshes at 2 s).
            if let Some(prev) = last_climb.insert(id.clone(), k) {
                assert!(
                    (k - prev) as u64 * REFRESH_S >= 4,
                    "{id} climbed at {prev} and {k}"
                );
            }
        }
        // Every reorder has a cause: some row climbed after a sustained win.
        if before != after {
            assert!(!run.moved[k].is_empty(), "refresh {k}: reorder without a climber");
        }
    }
    assert!(climbs > 0, "heavy noise should produce some sustained wins");
}
#[test]
fn test5_real_change_moves_once_within_4s_then_settles() {
    let base = base_scores();
    let s = machine(true);
    let chrome = s
        .groups
        .iter()
        .find(|g| g.label == "Google Chrome")
        .unwrap()
        .id
        .clone();
    let run = simulate(
        20,
        |k| {
            base.iter()
                .map(|(id, sc)| (id.clone(), if k >= 5 && id == &chrome { sc + 5.0 } else { *sc }))
                .collect()
        },
        |_, _| None,
    );
    assert_ne!(run.orders[4][0], chrome);
    assert_ne!(run.orders[5][0], chrome, "one refresh is not enough");
    assert_eq!(run.orders[6][0], chrome, "moves on the 2nd refresh (4 s)");
    assert!(run.moved[6].contains(&chrome));
    assert!(run.orders[6..].windows(2).all(|w| w[0] == w[1]), "then stable");
    assert!(run.moved[7].is_empty(), "the marker lasts one refresh");
}

#[test]
fn test7_reset_profile_is_salience_only() {
    let s = machine(true);
    let fp_aff: HashMap<String, f64> = [(FP_CHROME.to_string(), affinity_from_frecency(15.0))].into();
    let learned = group_candidates(&s, Mode::Calm, &fp_aff, &HashSet::new());
    let reset = group_candidates(&s, Mode::Calm, &HashMap::new(), &HashSet::new());
    assert!(learned.iter().any(|c| c.affinity > 0.9));
    for c in &reset {
        assert_eq!(c.affinity, 0.0);
        assert_eq!(c.prior, 0.0, "no priors unless the caller opts in");
        assert_eq!(c.query_match, 0.0);
    }
    let w = RankWeights::default();
    let order = |c: &[Candidate]| rank(c, &w, 0.0).into_iter().map(|r| r.0).collect::<Vec<_>>();
    let mut by_salience = reset.clone();
    by_salience.sort_by(|a, b| {
        let sa = a.salience + w.actionability * a.actionability - w.noise * a.noise + a.novelty;
        let sb = b.salience + w.actionability * b.actionability - w.noise * b.noise + b.novelty;
        sb.partial_cmp(&sa).unwrap().then(a.id.cmp(&b.id))
    });
    assert_eq!(
        order(&reset),
        by_salience.iter().map(|c| c.id.clone()).collect::<Vec<_>>()
    );
    assert_ne!(
        order(&learned),
        order(&reset),
        "affinity changed the order before the reset"
    );
}

fn render_ranking(
    s: &Snapshot,
    mode: Mode,
    ctx_aff: &HashMap<String, f64>,
    opens: &HashMap<&str, u32>,
) -> String {
    let muted = HashSet::new();
    let pinned: HashSet<String> = HashSet::new();
    let hist = oomtop_core::history::History::default();
    let ctx = RankContext {
        mode,
        affinity: Some(ctx_aff),
        muted: Some(&muted),
        pinned: Some(&pinned),
        history: Some(&hist),
        priors: ColdStartPriors::detect(s),
        ..Default::default()
    };
    let ranked = rank(
        &group_candidates_with(s, &ctx),
        &RankWeights::default(),
        DEFAULT_DIVERSITY_LAMBDA,
    );
    ranked
        .iter()
        .enumerate()
        .map(|(i, (id, parts))| {
            let g = s.group(id).unwrap();
            let mut facts = RankFacts::from_group(g, opens.get(g.fingerprint.as_str()).copied());
            facts.units = oomtop_core::units::UnitSystem::Si;
            format!(
                "{:>2}. {:<22} {:>6.3}  {}",
                i + 1,
                g.label,
                parts.total,
                explain_rank_for(i + 1, parts, &facts)
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn ranking_and_why_golden_per_mode() {
    let s = machine(true);
    // The user opened Chrome 9× and sd-server 5× this week.
    let fr_chrome = frecency(&[(86_400.0, 1.0); 9], DEFAULT_HALF_LIFE_S);
    let fr_sd = frecency(&[(2.0 * 86_400.0, 1.0); 5], DEFAULT_HALF_LIFE_S);
    let aff: HashMap<String, f64> = [
        (FP_CHROME.to_string(), affinity_from_frecency(fr_chrome)),
        (FP_SD.to_string(), affinity_from_frecency(fr_sd)),
    ]
    .into();
    let opens: HashMap<&str, u32> = [(FP_CHROME, 9), (FP_SD, 5)].into();
    let mut out = Vec::new();
    for mode in [
        Mode::Calm,
        Mode::Pressure,
        Mode::Throttle,
        Mode::Working,
        Mode::Leftovers,
    ] {
        out.push(format!(
            "== {} ==\n{}",
            mode.as_str(),
            render_ranking(&s, mode, &aff, &opens)
        ));
    }
    out.push(format!(
        "== cold start (no profile, priors) ==\n{}",
        render_ranking(&s, Mode::Calm, &HashMap::new(), &HashMap::new())
    ));
    insta::assert_snapshot!("ranking_why_per_mode", out.join("\n\n"));
}

#[test]
fn pressure_puts_idle_daemons_above_their_size_peers() {
    // UX §11 test 1 (core part): in Pressure the idle build daemons rank right after the biggest owner.
    let s = machine(true);
    let c = group_candidates(&s, Mode::Pressure, &HashMap::new(), &HashSet::new());
    let r = rank(&c, &RankWeights::default(), DEFAULT_DIVERSITY_LAMBDA);
    let labels: Vec<&str> = r
        .iter()
        .map(|(id, _)| s.group(id).unwrap().label.as_str())
        .collect();
    assert_eq!(labels[0], "sd-server");
    assert!(
        labels[1..3].contains(&"GradleDaemon") && labels[1..3].contains(&"KotlinCompileDaemon"),
        "{labels:?}"
    );
}

#[test]
fn ranking_is_fast() {
    // UX §5.4: ranking + layout ≤ 5 ms per frame (release). Debug gets a generous bound.
    let mut s = machine(true);
    for i in 0..300u32 {
        s.groups.push(Group {
            id: format!("app:helper-{i}"),
            kind: GroupKind::App,
            label: format!("Helper {}", i % 40),
            fingerprint: format!("fp-{i}"),
            totals: GroupTotals {
                footprint: Measured::exact(u64::from(i) * 1_000_000, "x"),
                ..Default::default()
            },
            ..Default::default()
        });
    }
    let aff = HashMap::new();
    let muted = HashSet::new();
    let mut st = RankState::new();
    let start = std::time::Instant::now();
    let n = 30;
    for k in 0..n {
        let c = group_candidates(&s, Mode::Calm, &aff, &muted);
        let r = rank(&c, &RankWeights::default(), DEFAULT_DIVERSITY_LAMBDA);
        let scored: Vec<(String, f64)> = r
            .into_iter()
            .map(|(id, p)| (id, p.total + (k % 3) as f64 * 0.01))
            .collect();
        std::hint::black_box(st.apply(&scored, None, MARGIN));
    }
    let per = start.elapsed() / n;
    eprintln!("rank+stabilize: {per:?} per frame over {} groups", s.groups.len());
    assert!(per.as_millis() < 50, "{per:?}");
}
