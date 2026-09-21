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

#![allow(clippy::unwrap_used)]

use std::fmt::Write as _;

use criterion::{Criterion, criterion_group, criterion_main};
use eks::commands::contexts;
use eks::kubeconfig::KubeConfig;
use eks::theme::Theme;
use eks::ui::App;
use ratatui::Terminal;
use ratatui::backend::TestBackend;

/// A kubeconfig with `clusters` EKS contexts, shaped like the ones this tool
/// actually reads: one AWS-generated ARN per cluster and context, an `exec`
/// block per user, and a `current-context` naming the last one — the same
/// fields `k8s::client` and `cluster::ClusterView` read out of a real file.
/// Multi-account, multi-region operators are exactly the users for whom
/// kubeconfig parsing time is not academic, so the fixture is sized like
/// their config rather than the two-cluster sample the unit tests use.
fn synthetic_kubeconfig(clusters: usize) -> String {
    let mut out = String::from("apiVersion: v1\nkind: Config\n");

    let arn = |i: usize| format!("arn:aws:eks:us-east-1:111122223333:cluster/cluster-{i:03}");

    let _ = writeln!(out, "current-context: {}", arn(clusters.saturating_sub(1)));

    out.push_str("clusters:\n");
    for i in 0..clusters {
        let name = arn(i);
        let _ = write!(
            out,
            "  - name: {name}\n    cluster:\n      server: https://{i:04X}.gr7.us-east-1.eks.amazonaws.com\n      certificate-authority-data: Zm9vYmFyYmF6cXV1eA==\n"
        );
    }

    out.push_str("contexts:\n");
    for i in 0..clusters {
        let name = arn(i);
        let _ = write!(
            out,
            "  - name: {name}\n    context:\n      cluster: {name}\n      user: {name}\n      namespace: default\n"
        );
    }

    out.push_str("users:\n");
    for i in 0..clusters {
        let name = arn(i);
        let _ = write!(
            out,
            "  - name: {name}\n    user:\n      exec:\n        apiVersion: client.authentication.k8s.io/v1beta1\n        command: aws\n        args: [\"eks\", \"get-token\", \"--cluster-name\", \"cluster-{i:03}\"]\n"
        );
    }

    out
}

/// The number of contexts the fixture carries — large enough to separate a
/// linear-in-clusters regression from noise, without making the benchmark
/// itself slow to iterate.
const CLUSTERS: usize = 50;

fn kubeconfig_parse(c: &mut Criterion) {
    let yaml = synthetic_kubeconfig(CLUSTERS);
    c.bench_function("kubeconfig_parse", |b| {
        b.iter(|| KubeConfig::parse(std::hint::black_box(&yaml)).unwrap());
    });
}

fn first_paint(c: &mut Criterion) {
    let yaml = synthetic_kubeconfig(CLUSTERS);
    // 120x40 covers a normal terminal window without leaning on `--wide`'s
    // extra columns, matching the size the dashboard's own rendering tests
    // use elsewhere in `ui::mod`.
    let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();

    c.bench_function("first_paint", |b| {
        b.iter(|| {
            let config = KubeConfig::parse(std::hint::black_box(&yaml)).unwrap();
            let views = contexts::views(&config);
            let mut app = App::new(views);
            app.set_theme(Theme::dark());
            terminal.draw(|frame| eks::ui::draw(frame, &app)).unwrap();
        });
    });
}

criterion_group!(benches, kubeconfig_parse, first_paint);
criterion_main!(benches);
