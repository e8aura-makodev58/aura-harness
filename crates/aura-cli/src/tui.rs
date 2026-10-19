//! Live dashboard. A client only: it reads status from the daemon and sends the
//! same requests the CLI does. Closing it changes nothing about running work.

use crate::client::Client;
use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use crossterm::ExecutableCommand;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::{Frame, Terminal};
use serde_json::{json, Value};
use std::io::stdout;
use std::time::{Duration, Instant};

const REFRESH: Duration = Duration::from_secs(1);

struct App {
    status: Value,
    selected: ListState,
    message: String,
}

impl App {
    fn tasks(&self) -> &[Value] {
        self.status["tasks"]
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    fn selected_task(&self) -> Option<&Value> {
        self.selected.selected().and_then(|i| self.tasks().get(i))
    }

    fn move_selection(&mut self, delta: i64) {
        let len = self.tasks().len();
        if len == 0 {
            self.selected.select(None);
            return;
        }
        let current = self.selected.selected().unwrap_or(0) as i64;
        let next = (current + delta).rem_euclid(len as i64) as usize;
        self.selected.select(Some(next));
    }
}

pub fn run(client: &Client) -> Result<()> {
    enable_raw_mode()?;
    stdout().execute(EnterAlternateScreen)?;
    let result = event_loop(client);
    disable_raw_mode()?;
    stdout().execute(LeaveAlternateScreen)?;
    result
}

fn event_loop(client: &Client) -> Result<()> {
    let mut terminal = Terminal::new(CrosstermBackend::new(stdout()))?;
    let mut app = App {
        status: client.call("status", json!({}))?,
        selected: ListState::default(),
        message: String::new(),
    };
    if !app.tasks().is_empty() {
        app.selected.select(Some(0));
    }
    let mut last_refresh = Instant::now();

    loop {
        terminal.draw(|frame| draw(frame, &mut app))?;

        if event::poll(Duration::from_millis(200))? {
            if let Event::Key(key) = event::read()? {
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                let outcome = match key.code {
                    KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
                    KeyCode::Down | KeyCode::Char('j') => {
                        app.move_selection(1);
                        None
                    }
                    KeyCode::Up | KeyCode::Char('k') => {
                        app.move_selection(-1);
                        None
                    }
                    KeyCode::Char('p') => {
                        let paused = app.status["paused"].as_bool().unwrap_or(false);
                        Some(client.call(if paused { "resume" } else { "pause" }, json!({})))
                    }
                    KeyCode::Char('a') => app
                        .selected_task()
                        .map(|t| t["id"].as_str().unwrap_or_default().to_string())
                        .map(|id| client.call("approve", json!({ "id": id }))),
                    KeyCode::Char('c') => app
                        .selected_task()
                        .map(|t| t["id"].as_str().unwrap_or_default().to_string())
                        .map(|id| client.call("cancel", json!({ "id": id }))),
                    KeyCode::Char('r') => {
                        last_refresh = Instant::now() - REFRESH;
                        None
                    }
                    _ => None,
                };
                if let Some(result) = outcome {
                    app.message = match result {
                        Ok(v) => v
                            .as_str()
                            .map(str::to_string)
                            .unwrap_or_else(|| "ok".into()),
                        Err(e) => format!("error: {e}"),
                    };
                    last_refresh = Instant::now() - REFRESH;
                }
            }
        }

        if last_refresh.elapsed() >= REFRESH {
            match client.call("status", json!({})) {
                Ok(status) => {
                    app.status = status;
                    let len = app.tasks().len();
                    if len == 0 {
                        app.selected.select(None);
                    } else if app.selected.selected().is_none_or(|i| i >= len) {
                        app.selected.select(Some(0));
                    }
                }
                Err(e) => app.message = format!("daemon unreachable: {e}"),
            }
            last_refresh = Instant::now();
        }
    }
}

fn draw(frame: &mut Frame, app: &mut App) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(8),
            Constraint::Length(9),
            Constraint::Length(1),
        ])
        .split(frame.area());

    let paused = app.status["paused"].as_bool().unwrap_or(false);
    let header = Line::from(vec![
        Span::styled("aura ", Style::default().add_modifier(Modifier::BOLD)),
        Span::styled(
            if paused { "PAUSED" } else { "running" },
            Style::default().fg(if paused { Color::Yellow } else { Color::Green }),
        ),
        Span::raw(format!(
            "   repo {}   main {}   merging {}",
            app.status["repo"].as_str().unwrap_or("?"),
            app.status["main_branch"].as_str().unwrap_or("?"),
            app.status["merge_in_progress"].as_str().unwrap_or("-")
        )),
    ]);
    frame.render_widget(
        Paragraph::new(header).block(Block::default().borders(Borders::ALL)),
        rows[0],
    );

    let panes = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(45), Constraint::Percentage(55)])
        .split(rows[1]);

    let items: Vec<ListItem> = app
        .tasks()
        .iter()
        .map(|t| {
            let state = t["state"].as_str().unwrap_or("?");
            ListItem::new(Line::from(vec![
                Span::styled(
                    format!("{:<9}", t["id"].as_str().unwrap_or("?")),
                    Style::default().fg(Color::Cyan),
                ),
                Span::styled(
                    format!("{:<13}", state),
                    Style::default().fg(state_color(state)),
                ),
                Span::raw(t["title"].as_str().unwrap_or("").to_string()),
            ]))
        })
        .collect();
    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title(" tasks "))
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED));
    frame.render_stateful_widget(list, panes[0], &mut app.selected);

    let detail = match app.selected_task() {
        Some(t) => {
            let mut lines = vec![
                Line::from(Span::styled(
                    format!(
                        "{}  {}",
                        t["id"].as_str().unwrap_or(""),
                        t["title"].as_str().unwrap_or("")
                    ),
                    Style::default().add_modifier(Modifier::BOLD),
                )),
                Line::from(format!(
                    "state {}   runs {}/{}",
                    t["state"].as_str().unwrap_or("?"),
                    t["runs_used"],
                    t["max_runs"]
                )),
                Line::from(format!("scope {}", join_strings(&t["scope"]["write"]))),
                Line::from(""),
                Line::from(t["brief"].as_str().unwrap_or("").to_string()),
            ];
            if let Some(notes) = t["notes"].as_array().filter(|n| !n.is_empty()) {
                lines.push(Line::from(""));
                lines.push(Line::from(Span::styled(
                    "notes",
                    Style::default().fg(Color::Yellow),
                )));
                for n in notes {
                    lines.push(Line::from(format!("- {}", n.as_str().unwrap_or(""))));
                }
            }
            lines
        }
        None => vec![Line::from(
            "No tasks yet. Give the planner a goal: aura directive \"...\"",
        )],
    };
    frame.render_widget(
        Paragraph::new(detail)
            .wrap(Wrap { trim: false })
            .block(Block::default().borders(Borders::ALL).title(" detail ")),
        panes[1],
    );

    let mut lower = Vec::new();
    for q in app.status["quotas"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or(&[])
    {
        let state = q["state"].as_str().unwrap_or("?");
        let when = q["retry_at"]
            .as_str()
            .filter(|_| state != "available")
            .unwrap_or("");
        lower.push(Line::from(format!(
            "{:<12} {:<18} running {}  {}",
            q["provider"].as_str().unwrap_or("?"),
            state,
            q["running"],
            when
        )));
    }
    for r in app.status["active_runs"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or(&[])
    {
        lower.push(Line::from(format!(
            "run {} {} on {} for {}",
            r["id"].as_str().unwrap_or(""),
            r["role"].as_str().unwrap_or(""),
            r["provider"].as_str().unwrap_or(""),
            r["task_id"].as_str().unwrap_or("-")
        )));
    }
    for q in app.status["open_questions"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or(&[])
    {
        lower.push(Line::from(Span::styled(
            format!(
                "question {} ({}): {}",
                q["id"].as_str().unwrap_or(""),
                q["task_id"].as_str().unwrap_or(""),
                q["text"].as_str().unwrap_or("")
            ),
            Style::default().fg(Color::Yellow),
        )));
    }
    frame.render_widget(
        Paragraph::new(lower).block(
            Block::default()
                .borders(Borders::ALL)
                .title(" quota, runs, questions "),
        ),
        rows[2],
    );

    let footer = if app.message.is_empty() {
        "j/k select  a approve  c cancel  p pause/resume  r refresh  q quit".to_string()
    } else {
        app.message.clone()
    };
    frame.render_widget(Paragraph::new(footer), rows[3]);
}

fn state_color(state: &str) -> Color {
    match state {
        "done" => Color::Green,
        "running" | "review" | "merge-queued" => Color::Cyan,
        "blocked" => Color::Red,
        "proposed" | "gated" => Color::Yellow,
        "cancelled" => Color::DarkGray,
        _ => Color::White,
    }
}

fn join_strings(v: &Value) -> String {
    v.as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default()
}
