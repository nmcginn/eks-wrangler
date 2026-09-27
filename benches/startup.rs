//! Startup-budget benchmarks — CLAUDE.md's "under 50ms to first paint" as
//! something measured rather than merely claimed.
//!
//! Two groups, matching the two costs on the critical path before a user sees
//! anything:
//!
//! - `kubeconfig_parse` isolates [`KubeConfig::parse`] alone, so a regression
//!   there cannot hide behind the render cost below it.
//! - `first_paint` walks the same sequence `main::dashboard` does up to its
//!   first `terminal.draw`: parse, build the sidebar's [`ClusterView`]s,
//!   construct the [`App`], and render one frame. It uses [`TestBackend`]
//!   rather than a real terminal for the same reason the UI's own tests do —
//!   this crate has no `unsafe`, so there is no raw-mode escape-sequence cost
//!   to measure honestly here, only the computation this tool controls.
//!
//! Neither benchmark touches disk or network: `KubeConfig::parse` takes a
//! `&str`, exactly as `kubeconfig.rs`'s own unit tests exercise it, so this
//! file measures the same pure functions the architecture already isolates
//! rather than a second, I/O-shaped version of them.
//!
//! The process around that computation — exec, the dynamic linker, the real
//! binary's own startup — is timed separately by `scripts/bench-startup.sh`
//! with `hyperfine` (`make bench-process`), against the same fixture.

#![allow(clippy::unwrap_used)]

use criterion::{Criterion, criterion_group, criterion_main};
use eks::commands::contexts;
use eks::kubeconfig::KubeConfig;
use eks::theme::Theme;
use eks::ui::App;
use ratatui::Terminal;
use ratatui::backend::TestBackend;

/// A kubeconfig with 50 EKS contexts, shaped like the ones this tool actually
/// reads: one AWS-generated ARN per cluster and context, an `exec` block per
/// user, and a `current-context` naming the last one — the same fields
/// `k8s::client` and `cluster::ClusterView` read out of a real file.
/// Multi-account, multi-region operators are exactly the users for whom
/// kubeconfig parsing time is not academic, so the fixture is sized like
/// their config rather than the two-cluster sample the unit tests use. Fifty
/// is large enough to separate a linear-in-clusters regression from noise,
/// without making the benchmark itself slow to iterate.
///
/// It is a committed file rather than generated here because
/// `scripts/bench-startup.sh` points the real binary's `KUBECONFIG` at the
/// same bytes, so the in-process and wall-clock numbers describe one input.
const KUBECONFIG: &str = include_str!("fixtures/kubeconfig-50.yaml");

fn kubeconfig_parse(c: &mut Criterion) {
    c.bench_function("kubeconfig_parse", |b| {
        b.iter(|| KubeConfig::parse(std::hint::black_box(KUBECONFIG)).unwrap());
    });
}

fn first_paint(c: &mut Criterion) {
    // 120x40 covers a normal terminal window without leaning on `--wide`'s
    // extra columns, matching the size the dashboard's own rendering tests
    // use elsewhere in `ui::mod`.
    let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();

    c.bench_function("first_paint", |b| {
        b.iter(|| {
            let config = KubeConfig::parse(std::hint::black_box(KUBECONFIG)).unwrap();
            let views = contexts::views(&config);
            let mut app = App::new(views);
            app.set_theme(Theme::dark());
            terminal.draw(|frame| eks::ui::draw(frame, &app)).unwrap();
        });
    });
}

criterion_group!(benches, kubeconfig_parse, first_paint);
criterion_main!(benches);
