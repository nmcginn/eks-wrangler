//! The interactive dashboard.
//!
//! State and input handling live in [`App`], deliberately free of any terminal
//! I/O, so navigation can be tested by feeding it key events. Only [`run`]
//! touches the real terminal.

use anyhow::Result;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::{Frame, Terminal};
use std::time::Duration;

use crate::cluster::ClusterView;
use crate::theme::Theme;

/// How long to wait for input before waking up to redraw. Short enough that
/// live data will feel immediate once it exists, long enough to stay at
/// effectively zero CPU while idle.
const TICK: Duration = Duration::from_millis(250);

/// What the event loop should do after handling an input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flow {
    /// Stay in the loop.
    Continue,
    /// Tear down and exit.
    Quit,
}

/// Dashboard state.
#[derive(Debug, Clone)]
pub struct App {
    clusters: Vec<ClusterView>,
    selected: usize,
    theme: Theme,
}

impl App {
    /// Create an app over the clusters found in the kubeconfig, starting with
    /// the active one selected.
    #[must_use]
    pub fn new(clusters: Vec<ClusterView>) -> Self {
        let selected = clusters.iter().position(|c| c.is_current).unwrap_or(0);
        Self {
            clusters,
            selected,
            theme: Theme::dark(),
        }
    }

    /// The clusters shown in the sidebar.
    #[must_use]
    pub fn clusters(&self) -> &[ClusterView] {
        &self.clusters
    }

    /// Index of the highlighted row.
    #[must_use]
    pub fn selected_index(&self) -> usize {
        self.selected
    }

    /// The highlighted cluster, if there is one.
    #[must_use]
    pub fn selected_cluster(&self) -> Option<&ClusterView> {
        self.clusters.get(self.selected)
    }

    /// Highlight the cluster with this context name.
    ///
    /// Returns `false` when no such cluster is loaded, leaving the selection
    /// untouched.
    pub fn select_context(&mut self, context_name: &str) -> bool {
        match self
            .clusters
            .iter()
            .position(|c| c.context_name == context_name)
        {
            Some(index) => {
                self.selected = index;
                true
            }
            None => false,
        }
    }

    /// Move the highlight down, wrapping at the end.
    pub fn select_next(&mut self) {
        if self.clusters.is_empty() {
            return;
        }
        self.selected = (self.selected + 1) % self.clusters.len();
    }

    /// Move the highlight up, wrapping at the start.
    pub fn select_previous(&mut self) {
        if self.clusters.is_empty() {
            return;
        }
        self.selected = self
            .selected
            .checked_sub(1)
            .unwrap_or(self.clusters.len() - 1);
    }

    /// Handle a key press.
    ///
    /// Supports both arrow keys and vim-style `j`/`k`, because the people who
    /// live in this kind of tool expect the latter.
    pub fn on_key(&mut self, key: KeyEvent) -> Flow {
        // Key *release* events arrive on Windows and modern terminals; acting on
        // both would move the selection twice per press.
        if key.kind == KeyEventKind::Release {
            return Flow::Continue;
        }

        if key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('c')) {
            return Flow::Quit;
        }

        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => return Flow::Quit,
            KeyCode::Char('j') | KeyCode::Down => self.select_next(),
            KeyCode::Char('k') | KeyCode::Up => self.select_previous(),
            KeyCode::Home => self.selected = 0,
            KeyCode::End => self.selected = self.clusters.len().saturating_sub(1),
            _ => {}
        }
        Flow::Continue
    }
}

/// Run the dashboard against the real terminal.
///
/// Terminal setup and teardown are handled by `ratatui`, which installs a panic
/// hook so a crash cannot leave the user staring at a wedged shell.
pub fn run(app: App) -> Result<()> {
    let mut terminal = ratatui::init();
    let result = event_loop(&mut terminal, app);
    ratatui::restore();
    result
}

fn event_loop<B>(terminal: &mut Terminal<B>, mut app: App) -> Result<()>
where
    B: ratatui::backend::Backend,
    B::Error: std::error::Error + Send + Sync + 'static,
{
    loop {
        terminal.draw(|frame| draw(frame, &app))?;

        // Only block for input; on timeout we fall through and redraw, which is
        // where live cluster data will be picked up.
        if !event::poll(TICK)? {
            continue;
        }

        if let Event::Key(key) = event::read()?
            && app.on_key(key) == Flow::Quit
        {
            return Ok(());
        }
    }
}

/// Draw one frame.
pub fn draw(frame: &mut Frame, app: &App) {
    let theme = app.theme;
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // header
            Constraint::Min(0),    // body
            Constraint::Length(1), // footer
        ])
        .split(frame.area());

    draw_header(frame, chunks[0], app);
    draw_body(frame, chunks[1], app);
    draw_footer(frame, chunks[2], theme);
}

fn draw_header(frame: &mut Frame, area: Rect, app: &App) {
    let theme = app.theme;
    let cluster = app
        .selected_cluster()
        .map_or_else(|| "no cluster".to_owned(), ClusterView::label);

    let line = Line::from(vec![
        Span::styled(" eks ", theme.heading()),
        Span::styled("│ ", theme.dim()),
        Span::styled(cluster, theme.body().bold()),
    ]);

    frame.render_widget(Paragraph::new(line), area);
}

fn draw_body(frame: &mut Frame, area: Rect, app: &App) {
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(32), Constraint::Min(0)])
        .split(area);

    draw_cluster_list(frame, columns[0], app);
    draw_detail(frame, columns[1], app);
}

fn draw_cluster_list(frame: &mut Frame, area: Rect, app: &App) {
    let theme = app.theme;

    let items: Vec<ListItem> = app
        .clusters()
        .iter()
        .map(|cluster| {
            let marker = if cluster.is_current { "● " } else { "  " };
            ListItem::new(Line::from(vec![
                Span::styled(marker, theme.severity(crate::theme::Severity::Ok)),
                Span::styled(cluster.display_name.clone(), theme.body()),
                Span::raw(" "),
                Span::styled(cluster.region.clone().unwrap_or_default(), theme.dim()),
            ]))
        })
        .collect();

    let block = Block::bordered()
        .title(" Clusters ")
        .border_style(theme.pane_border(true))
        .title_style(theme.heading());

    let mut state = ListState::default().with_selected(Some(app.selected_index()));
    frame.render_stateful_widget(
        List::new(items)
            .block(block)
            .highlight_style(theme.selected()),
        area,
        &mut state,
    );
}

fn draw_detail(frame: &mut Frame, area: Rect, app: &App) {
    let theme = app.theme;

    let body = match app.selected_cluster() {
        Some(cluster) => {
            let mut lines = vec![
                detail_row("Context", &cluster.context_name, theme),
                detail_row("Namespace", &cluster.namespace, theme),
            ];
            if let Some(region) = &cluster.region {
                lines.push(detail_row("Region", region, theme));
            }
            if let Some(account) = &cluster.account_id {
                lines.push(detail_row("Account", account, theme));
            }
            lines.push(Line::raw(""));
            lines.push(Line::styled(
                "Node and pod views land here next — see docs/ROADMAP.md.",
                theme.dim(),
            ));
            lines
        }
        None => vec![Line::styled(
            "No clusters in your kubeconfig. Run `aws eks update-kubeconfig --name <cluster>`.",
            theme.dim(),
        )],
    };

    let block = Block::bordered()
        .title(" Overview ")
        .border_style(theme.pane_border(false))
        .title_style(theme.heading());

    frame.render_widget(
        Paragraph::new(body).block(block).wrap(Wrap { trim: true }),
        area,
    );
}

fn detail_row<'a>(label: &'a str, value: &'a str, theme: Theme) -> Line<'a> {
    Line::from(vec![
        Span::styled(format!("{label:<10}"), theme.dim()),
        Span::styled(value, theme.body()),
    ])
}

fn draw_footer(frame: &mut Frame, area: Rect, theme: Theme) {
    let hints = [("j/k", "move"), ("enter", "open"), ("q", "quit")];

    let mut spans = vec![Span::raw(" ")];
    for (key, action) in hints {
        spans.push(Span::styled(key, theme.heading()));
        spans.push(Span::styled(format!(" {action}   "), theme.dim()));
    }

    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn cluster(name: &str, is_current: bool) -> ClusterView {
        ClusterView {
            context_name: format!("arn:aws:eks:us-east-1:1234:cluster/{name}"),
            display_name: name.to_owned(),
            region: Some("us-east-1".to_owned()),
            account_id: Some("1234".to_owned()),
            namespace: "default".to_owned(),
            is_current,
        }
    }

    fn app() -> App {
        App::new(vec![
            cluster("alpha", false),
            cluster("beta", true),
            cluster("gamma", false),
        ])
    }

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn selection_starts_on_the_active_cluster() {
        assert_eq!(app().selected_cluster().unwrap().display_name, "beta");
    }

    #[test]
    fn selection_starts_at_the_top_when_nothing_is_active() {
        let app = App::new(vec![cluster("alpha", false), cluster("beta", false)]);
        assert_eq!(app.selected_index(), 0);
    }

    #[test]
    fn select_context_targets_a_named_cluster() {
        let mut app = app();

        assert!(app.select_context("arn:aws:eks:us-east-1:1234:cluster/gamma"));
        assert_eq!(app.selected_cluster().unwrap().display_name, "gamma");

        assert!(!app.select_context("nope"));
        assert_eq!(
            app.selected_cluster().unwrap().display_name,
            "gamma",
            "a failed lookup must not move the selection"
        );
    }

    #[test]
    fn j_and_k_move_the_selection() {
        let mut app = app();

        app.on_key(press(KeyCode::Char('j')));
        assert_eq!(app.selected_cluster().unwrap().display_name, "gamma");

        app.on_key(press(KeyCode::Char('k')));
        assert_eq!(app.selected_cluster().unwrap().display_name, "beta");
    }

    #[test]
    fn arrow_keys_match_vim_keys() {
        let mut arrows = app();
        let mut vim = app();

        arrows.on_key(press(KeyCode::Down));
        vim.on_key(press(KeyCode::Char('j')));

        assert_eq!(arrows.selected_index(), vim.selected_index());
    }

    #[test]
    fn selection_wraps_at_both_ends() {
        let mut app = app();

        app.on_key(press(KeyCode::Home));
        app.on_key(press(KeyCode::Char('k')));
        assert_eq!(app.selected_cluster().unwrap().display_name, "gamma");

        app.on_key(press(KeyCode::Char('j')));
        assert_eq!(app.selected_cluster().unwrap().display_name, "alpha");
    }

    #[test]
    fn home_and_end_jump_to_the_edges() {
        let mut app = app();

        app.on_key(press(KeyCode::End));
        assert_eq!(app.selected_index(), 2);

        app.on_key(press(KeyCode::Home));
        assert_eq!(app.selected_index(), 0);
    }

    #[test]
    fn q_esc_and_ctrl_c_quit() {
        assert_eq!(app().on_key(press(KeyCode::Char('q'))), Flow::Quit);
        assert_eq!(app().on_key(press(KeyCode::Esc)), Flow::Quit);
        assert_eq!(
            app().on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            Flow::Quit
        );
    }

    #[test]
    fn key_releases_are_ignored() {
        let mut app = app();
        let mut release = press(KeyCode::Char('j'));
        release.kind = KeyEventKind::Release;

        app.on_key(release);

        assert_eq!(app.selected_index(), 1, "release events must not navigate");
    }

    #[test]
    fn navigating_an_empty_cluster_list_is_harmless() {
        let mut app = App::new(Vec::new());

        assert_eq!(app.on_key(press(KeyCode::Char('j'))), Flow::Continue);
        assert_eq!(app.on_key(press(KeyCode::Char('k'))), Flow::Continue);
        assert_eq!(app.on_key(press(KeyCode::End)), Flow::Continue);
        assert!(app.selected_cluster().is_none());
    }

    #[test]
    fn a_frame_renders_the_selected_cluster() {
        let mut terminal = Terminal::new(TestBackend::new(90, 20)).unwrap();
        let app = app();

        terminal.draw(|frame| draw(frame, &app)).unwrap();

        let rendered = terminal.backend().to_string();
        assert!(rendered.contains("Clusters"), "{rendered}");
        assert!(rendered.contains("beta"), "{rendered}");
        assert!(rendered.contains("us-east-1"), "{rendered}");
        assert!(rendered.contains("quit"), "{rendered}");
    }

    #[test]
    fn rendering_survives_a_tiny_terminal() {
        // Users do resize their terminals to absurd sizes; a panic here would
        // leave the shell in raw mode.
        for (width, height) in [(1, 1), (8, 3), (20, 2), (200, 60)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal.draw(|frame| draw(frame, &app())).unwrap();
        }
    }

    #[test]
    fn rendering_an_empty_cluster_list_explains_itself() {
        let mut terminal = Terminal::new(TestBackend::new(90, 20)).unwrap();
        let app = App::new(Vec::new());

        terminal.draw(|frame| draw(frame, &app)).unwrap();

        assert!(terminal.backend().to_string().contains("update-kubeconfig"));
    }
}
