use super::{setting_search_score, settings_meta};
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use miette::{IntoDiagnostic, miette};
use ratatui::{
    Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap},
};
use std::io::{self, IsTerminal};

struct ConfigTui {
    settings: Vec<&'static settings_meta::SettingMeta>,
    filtered: Vec<usize>,
    selected: usize,
    query: String,
    searching: bool,
}

impl ConfigTui {
    fn new() -> Self {
        let settings = settings_meta::all().iter().collect::<Vec<_>>();
        let filtered = (0..settings.len()).collect::<Vec<_>>();
        Self {
            settings,
            filtered,
            selected: 0,
            query: String::new(),
            searching: false,
        }
    }

    fn selected(&self) -> Option<&'static settings_meta::SettingMeta> {
        self.filtered
            .get(self.selected)
            .and_then(|idx| self.settings.get(*idx))
            .copied()
    }

    fn apply_filter(&mut self) {
        let terms = self
            .query
            .split_whitespace()
            .map(|q| q.to_ascii_lowercase())
            .collect::<Vec<_>>();
        self.filtered = if terms.is_empty() {
            (0..self.settings.len()).collect()
        } else {
            self.settings
                .iter()
                .enumerate()
                .filter_map(|(idx, meta)| (setting_search_score(meta, &terms) > 0).then_some(idx))
                .collect()
        };
        self.selected = self.selected.min(self.filtered.len().saturating_sub(1));
    }

    fn move_selection(&mut self, delta: isize) {
        if self.filtered.is_empty() {
            self.selected = 0;
            return;
        }
        self.selected = self
            .selected
            .saturating_add_signed(delta)
            .min(self.filtered.len() - 1);
    }
}

pub fn run() -> miette::Result<()> {
    if !io::stdout().is_terminal() {
        return Err(miette!(
            "`aube config tui` requires an interactive terminal"
        ));
    }

    enable_raw_mode().into_diagnostic()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen).into_diagnostic()?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).into_diagnostic()?;

    let result = run_tui(&mut terminal);
    let restore = disable_raw_mode()
        .into_diagnostic()
        .and_then(|_| execute!(terminal.backend_mut(), LeaveAlternateScreen).into_diagnostic())
        .and_then(|_| terminal.show_cursor().into_diagnostic());

    result.and(restore)
}

fn run_tui(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>) -> miette::Result<()> {
    let mut app = ConfigTui::new();
    loop {
        terminal
            .draw(|frame| draw_tui(frame, &mut app))
            .into_diagnostic()?;

        let Event::Key(key) = event::read().into_diagnostic()? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }

        if app.searching {
            match key.code {
                KeyCode::Esc | KeyCode::Enter => app.searching = false,
                KeyCode::Backspace => {
                    app.query.pop();
                    app.apply_filter();
                }
                KeyCode::Char(c) => {
                    app.query.push(c);
                    app.apply_filter();
                }
                _ => {}
            }
            continue;
        }

        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
            KeyCode::Char('/') => app.searching = true,
            KeyCode::Down | KeyCode::Char('j') => app.move_selection(1),
            KeyCode::Up | KeyCode::Char('k') => app.move_selection(-1),
            KeyCode::PageDown => app.move_selection(10),
            KeyCode::PageUp => app.move_selection(-10),
            KeyCode::Home => app.selected = 0,
            KeyCode::End => app.selected = app.filtered.len().saturating_sub(1),
            _ => {}
        }
    }
}

fn draw_tui(frame: &mut ratatui::Frame<'_>, app: &mut ConfigTui) {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(0),
            Constraint::Length(1),
        ])
        .split(frame.area());

    let search_title = if app.searching {
        "Search (Enter/Esc to finish)"
    } else {
        "Search (/ to edit)"
    };
    let search = Paragraph::new(app.query.as_str())
        .block(Block::default().borders(Borders::ALL).title(search_title));
    frame.render_widget(search, vertical[0]);

    let horizontal = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(42), Constraint::Percentage(58)])
        .split(vertical[1]);

    let items = app
        .filtered
        .iter()
        .map(|idx| {
            let meta = app.settings[*idx];
            ListItem::new(Line::from(vec![
                Span::styled(
                    meta.name,
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(format!("  {}", meta.description)),
            ]))
        })
        .collect::<Vec<_>>();
    let mut state = ListState::default();
    if !app.filtered.is_empty() {
        state.select(Some(app.selected));
    }
    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title("Settings"))
        .highlight_style(Style::default().fg(Color::Black).bg(Color::Cyan))
        .highlight_symbol("> ");
    frame.render_stateful_widget(list, horizontal[0], &mut state);

    let details = app
        .selected()
        .map(setting_detail_lines)
        .unwrap_or_else(|| vec![Line::from("No settings match the search.")]);
    let details = Paragraph::new(details)
        .block(Block::default().borders(Borders::ALL).title("Details"))
        .wrap(Wrap { trim: false });
    frame.render_widget(details, horizontal[1]);

    let help = Paragraph::new("q quit  / search  arrows/jk move  PgUp/PgDn jump");
    frame.render_widget(help, vertical[2]);
}

fn setting_detail_lines(meta: &settings_meta::SettingMeta) -> Vec<Line<'static>> {
    let mut lines = vec![
        Line::from(Span::styled(
            meta.name.to_string(),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(format!("Type: {}", meta.type_)),
        Line::from(format!("Default: {}", meta.default)),
        Line::from(format!("Description: {}", meta.description)),
    ];

    detail_source_line(&mut lines, "CLI flags", meta.cli_flags);
    detail_source_line(&mut lines, "Environment", meta.env_vars);
    detail_source_line(&mut lines, ".npmrc keys", meta.npmrc_keys);
    detail_source_line(&mut lines, "Workspace YAML keys", meta.workspace_yaml_keys);

    let docs = meta.docs.trim();
    if !docs.is_empty() {
        lines.push(Line::from(""));
        lines.extend(docs.lines().map(|line| Line::from(line.to_string())));
    }

    if !meta.examples.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::from("Examples:"));
        lines.extend(
            meta.examples
                .iter()
                .map(|example| Line::from(format!("  {example}"))),
        );
    }

    lines
}

fn detail_source_line(lines: &mut Vec<Line<'static>>, label: &str, values: &[&str]) {
    if !values.is_empty() {
        lines.push(Line::from(format!("{label}: {}", values.join(", "))));
    }
}
