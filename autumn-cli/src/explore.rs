use std::io;
use std::process::Command;
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use crossterm::terminal::{self, EnterAlternateScreen, LeaveAlternateScreen};
use crossterm::{cursor, execute};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState, Wrap};

use crate::routes::{RouteInfo, RoutesOptions, compile_binary, find_binary};

struct ExploreState {
    routes: Vec<RouteInfo>,
    filtered_routes: Vec<RouteInfo>,
    table_state: TableState,
    search_query: String,
}

impl ExploreState {
    fn new(routes: Vec<RouteInfo>) -> Self {
        let mut state = Self {
            filtered_routes: routes.clone(),
            routes,
            table_state: TableState::default(),
            search_query: String::new(),
        };
        state.table_state.select(Some(0));
        state
    }

    fn apply_filter(&mut self) {
        if self.search_query.is_empty() {
            self.filtered_routes = self.routes.clone();
        } else {
            let query = self.search_query.to_lowercase();
            self.filtered_routes = self
                .routes
                .iter()
                .filter(|r| {
                    r.path.to_lowercase().contains(&query)
                        || r.method.to_lowercase().contains(&query)
                        || r.handler.to_lowercase().contains(&query)
                        || r.source.to_lowercase().contains(&query)
                        || r.middleware
                            .iter()
                            .any(|m| m.to_lowercase().contains(&query))
                })
                .cloned()
                .collect();
        }

        // Adjust selection if it's now out of bounds
        if let Some(selected) = self.table_state.selected() {
            if self.filtered_routes.is_empty() {
                self.table_state.select(None);
            } else if selected >= self.filtered_routes.len() {
                self.table_state
                    .select(Some(self.filtered_routes.len() - 1));
            }
        } else if !self.filtered_routes.is_empty() {
            self.table_state.select(Some(0));
        }
    }

    fn next(&mut self) {
        if self.filtered_routes.is_empty() {
            return;
        }
        let i = match self.table_state.selected() {
            Some(i) => {
                if i >= self.filtered_routes.len() - 1 {
                    0
                } else {
                    i + 1
                }
            }
            None => 0,
        };
        self.table_state.select(Some(i));
    }

    fn previous(&mut self) {
        if self.filtered_routes.is_empty() {
            return;
        }
        let i = match self.table_state.selected() {
            Some(i) => {
                if i == 0 {
                    self.filtered_routes.len() - 1
                } else {
                    i - 1
                }
            }
            None => 0,
        };
        self.table_state.select(Some(i));
    }
}

pub fn run(opts: &RoutesOptions<'_>) {
    eprintln!("🍁 autumn explore\n");
    compile_binary(opts.package, opts.bin);
    let binary = find_binary(opts.package, opts.bin);

    let output = Command::new(&binary)
        .env("AUTUMN_DUMP_ROUTES", "1")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit())
        .output()
        .unwrap_or_else(|e| {
            eprintln!("❌ Failed to run {}: {e}", binary.display());
            std::process::exit(1);
        });

    if !output.status.success() {
        eprintln!(
            "❌ Binary exited with status {} while dumping routes",
            output.status
        );
        std::process::exit(output.status.code().unwrap_or(1));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let routes: Vec<RouteInfo> = serde_json::from_str(&stdout).unwrap_or_else(|e| {
        eprintln!("❌ Failed to parse route listing JSON: {e}");
        eprintln!("Raw output: {stdout}");
        std::process::exit(1);
    });

    let mut state = ExploreState::new(routes);

    // Setup terminal
    terminal::enable_raw_mode().expect("failed to enable raw mode");
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, cursor::Hide).expect("failed to setup terminal");
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).expect("failed to create terminal");

    let result = run_loop(&mut terminal, &mut state);

    // Restore terminal
    terminal::disable_raw_mode().expect("failed to disable raw mode");
    execute!(terminal.backend_mut(), LeaveAlternateScreen, cursor::Show)
        .expect("failed to restore terminal");

    if let Err(e) = result {
        eprintln!("Error: {e}");
    }
}

fn run_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    state: &mut ExploreState,
) -> io::Result<()> {
    loop {
        terminal.draw(|frame| draw(frame, state))?;

        if event::poll(Duration::from_millis(100))? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press {
                    match key.code {
                        KeyCode::Esc => return Ok(()),
                        KeyCode::Down => state.next(),
                        KeyCode::Up => state.previous(),
                        KeyCode::Backspace => {
                            state.search_query.pop();
                            state.apply_filter();
                        }
                        KeyCode::Char(c) => {
                            state.search_query.push(c);
                            state.apply_filter();
                        }
                        _ => {}
                    }
                }
            }
        }
    }
}

fn draw(frame: &mut ratatui::Frame, state: &mut ExploreState) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // Search bar
            Constraint::Min(5),    // Table
            Constraint::Length(8), // Details panel
        ])
        .split(frame.area());

    draw_search_bar(frame, chunks[0], state);
    draw_table(frame, chunks[1], state);
    draw_details_panel(frame, chunks[2], state);
}

fn draw_search_bar(frame: &mut ratatui::Frame, area: Rect, state: &ExploreState) {
    let search_text = format!(" Search: {}_", state.search_query);
    let p = Paragraph::new(search_text).block(
        Block::default()
            .borders(Borders::ALL)
            .title(Span::styled(
                " Route Explorer ",
                Style::default()
                    .fg(Color::Rgb(204, 120, 50))
                    .add_modifier(Modifier::BOLD),
            ))
            .border_style(Style::default().fg(Color::DarkGray)),
    );
    frame.render_widget(p, area);
}

fn draw_table(frame: &mut ratatui::Frame, area: Rect, state: &mut ExploreState) {
    let header_style = Style::default()
        .fg(Color::Rgb(204, 120, 50))
        .add_modifier(Modifier::BOLD);
    let selected_style = Style::default()
        .bg(Color::DarkGray)
        .add_modifier(Modifier::BOLD);

    let header = Row::new(vec![
        Cell::from("Method").style(header_style),
        Cell::from("Path").style(header_style),
        Cell::from("Handler").style(header_style),
    ])
    .height(1)
    .bottom_margin(1);

    let rows: Vec<Row<'_>> = state
        .filtered_routes
        .iter()
        .map(|route| {
            let method_color = match route.method.as_str() {
                "GET" => Color::Green,
                "POST" => Color::Blue,
                "PUT" | "PATCH" => Color::Yellow,
                "DELETE" => Color::Red,
                _ => Color::White,
            };

            Row::new(vec![
                Cell::from(route.method.clone()).style(
                    Style::default()
                        .fg(method_color)
                        .add_modifier(Modifier::BOLD),
                ),
                Cell::from(route.path.clone()),
                Cell::from(route.handler.clone()).style(Style::default().fg(Color::DarkGray)),
            ])
        })
        .collect();

    let widths = [
        Constraint::Length(10),
        Constraint::Percentage(50),
        Constraint::Percentage(40),
    ];

    let table = Table::new(rows, widths)
        .header(header)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::DarkGray)),
        )
        .row_highlight_style(selected_style);

    frame.render_stateful_widget(table, area, &mut state.table_state);
}

fn draw_details_panel(frame: &mut ratatui::Frame, area: Rect, state: &ExploreState) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray))
        .title(Span::styled(
            " Details ",
            Style::default()
                .fg(Color::Rgb(204, 120, 50))
                .add_modifier(Modifier::BOLD),
        ));

    let mut lines = Vec::new();

    if let Some(selected_idx) = state.table_state.selected() {
        if let Some(route) = state.filtered_routes.get(selected_idx) {
            lines = build_detail_lines(route);
        }
    } else {
        lines.push(Line::from(Span::styled(
            "No route selected",
            Style::default().fg(Color::DarkGray),
        )));
    }

    // Add help text at the bottom of the details panel
    lines.push(Line::raw(""));
    lines.push(Line::from(vec![
        Span::styled(
            "Esc",
            Style::default()
                .fg(Color::Rgb(204, 120, 50))
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" quit  ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            "↑/↓",
            Style::default()
                .fg(Color::Rgb(204, 120, 50))
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" scroll  ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            "Type",
            Style::default()
                .fg(Color::Rgb(204, 120, 50))
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" to search", Style::default().fg(Color::DarkGray)),
    ]));

    let p = Paragraph::new(lines).block(block).wrap(Wrap { trim: true });
    frame.render_widget(p, area);
}

pub fn build_detail_lines(route: &RouteInfo) -> Vec<Line<'static>> {
    let mut lines = Vec::new();

    lines.push(Line::from(vec![
        Span::styled("Source: ", Style::default().fg(Color::DarkGray)),
        Span::raw(route.source.clone()),
    ]));

    let middleware_str = if route.middleware.is_empty() {
        "None".to_string()
    } else {
        route.middleware.join(", ")
    };

    lines.push(Line::from(vec![
        Span::styled("Middleware: ", Style::default().fg(Color::DarkGray)),
        Span::styled(middleware_str, Style::default().fg(Color::Cyan)),
    ]));

    if let Some(ver) = &route.api_version {
        lines.push(Line::from(vec![
            Span::styled("API Version: ", Style::default().fg(Color::DarkGray)),
            Span::styled(ver.clone(), Style::default().fg(Color::Magenta)),
        ]));
    }

    if let Some(status) = &route.status {
        let status_color = if status == "sunset" {
            Color::Red
        } else {
            Color::Yellow
        };
        lines.push(Line::from(vec![
            Span::styled("Status: ", Style::default().fg(Color::DarkGray)),
            Span::styled(status.clone(), Style::default().fg(status_color)),
        ]));

        if let Some(opt_out) = route.sunset_opt_out {
            if opt_out {
                lines.push(Line::from(vec![
                    Span::styled("Sunset Opt-Out: ", Style::default().fg(Color::DarkGray)),
                    Span::styled("true", Style::default().fg(Color::Green)),
                ]));
            }
        }
    }

    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_detail_lines() {
        let route = RouteInfo {
            method: "GET".to_string(),
            path: "/users/{id}".to_string(),
            handler: "get_user".to_string(),
            source: "src/users.rs".to_string(),
            middleware: vec!["auth".to_string()],
            api_version: None,
            status: None,
            sunset_opt_out: None,
        };

        let lines = build_detail_lines(&route);
        assert!(!lines.is_empty(), "Details should not be empty");

        // Convert the spans back to a searchable string to assert on contents
        let source_line = lines[0]
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect::<String>();
        assert!(source_line.contains("Source: src/users.rs"));

        let middleware_line = lines[1]
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect::<String>();
        assert!(middleware_line.contains("Middleware: auth"));
    }

    #[test]
    fn test_apply_filter() {
        let route1 = RouteInfo {
            method: "GET".to_string(),
            path: "/users".to_string(),
            handler: "list_users".to_string(),
            source: "src/users.rs".to_string(),
            middleware: vec![],
            api_version: None,
            status: None,
            sunset_opt_out: None,
        };
        let route2 = RouteInfo {
            method: "POST".to_string(),
            path: "/posts".to_string(),
            handler: "create_post".to_string(),
            source: "src/posts.rs".to_string(),
            middleware: vec!["auth".to_string()],
            api_version: None,
            status: None,
            sunset_opt_out: None,
        };

        let mut state = ExploreState::new(vec![route1, route2]);
        assert_eq!(state.filtered_routes.len(), 2);

        state.search_query = "post".to_string();
        state.apply_filter();
        assert_eq!(state.filtered_routes.len(), 1);
        assert_eq!(state.filtered_routes[0].path, "/posts");

        state.search_query = "auth".to_string();
        state.apply_filter();
        assert_eq!(state.filtered_routes.len(), 1);
        assert_eq!(state.filtered_routes[0].path, "/posts");
    }
}
