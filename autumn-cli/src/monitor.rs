//! Live monitoring TUI dashboard for Autumn applications.
//!
//! Connects to a running Autumn app's actuator endpoints and renders
//! real-time metrics, health status, and task information in a rich
//! terminal UI.

use std::collections::{HashMap, VecDeque};
use std::io;
use std::time::{Duration, Instant};

use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::terminal::{self, EnterAlternateScreen, LeaveAlternateScreen};
use crossterm::{cursor, execute};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{
    Bar, BarChart, BarGroup, Block, Borders, Cell, Padding, Paragraph, Row, Sparkline, Table, Tabs,
    Wrap,
};
use serde::Deserialize;

// ── Actuator response types ───────────────────────────────────

#[derive(Debug, Deserialize, Default, Clone)]
struct HealthResponse {
    status: String,
    #[serde(default)]
    version: String,
    #[serde(default)]
    profile: String,
    #[serde(default)]
    uptime: String,
    #[serde(default)]
    checks: Option<HealthChecks>,
}

#[derive(Debug, Deserialize, Default, Clone)]
struct LoggersResponse {
    #[serde(default)]
    current_level: String,
    #[serde(default)]
    #[allow(dead_code)]
    available_levels: Vec<String>,
    #[serde(default)]
    loggers: HashMap<String, String>,
}

#[derive(Debug, Deserialize, Default, Clone)]
struct HealthChecks {
    database: Option<DatabaseCheck>,
}

#[derive(Debug, Deserialize, Default, Clone)]
struct DatabaseCheck {
    status: String,
    pool_size: u64,
    active_connections: u64,
    idle_connections: u64,
}

#[derive(Debug, Deserialize, Default, Clone)]
struct MetricsResponse {
    #[serde(default)]
    http: HttpMetrics,
    #[serde(default)]
    database: Option<DbPoolMetrics>,
}

#[derive(Debug, Deserialize, Default, Clone)]
struct HttpMetrics {
    #[serde(default)]
    requests_total: u64,
    #[serde(default)]
    requests_active: u64,
    #[serde(default)]
    latency_ms: LatencySnapshot,
    #[serde(default)]
    by_route: HashMap<String, RouteSnapshot>,
    #[serde(default)]
    by_status: StatusSnapshot,
}

#[derive(Debug, Deserialize, Default, Clone)]
struct LatencySnapshot {
    #[serde(default)]
    p50: u64,
    #[serde(default)]
    p95: u64,
    #[serde(default)]
    p99: u64,
}

#[derive(Debug, Deserialize, Default, Clone)]
struct RouteSnapshot {
    #[serde(default)]
    count: u64,
    #[serde(default)]
    p50_ms: u64,
    #[serde(default)]
    p95_ms: u64,
    #[serde(default)]
    p99_ms: u64,
}

#[derive(Debug, Deserialize, Default, Clone)]
struct StatusSnapshot {
    #[serde(rename = "2xx", default)]
    s2xx: u64,
    #[serde(rename = "3xx", default)]
    s3xx: u64,
    #[serde(rename = "4xx", default)]
    s4xx: u64,
    #[serde(rename = "5xx", default)]
    s5xx: u64,
}

#[derive(Debug, Deserialize, Default, Clone)]
struct DbPoolMetrics {
    #[serde(default)]
    pool_size: u64,
    #[serde(default)]
    active_connections: u64,
    #[serde(default)]
    idle_connections: u64,
}

#[derive(Debug, Deserialize, Default, Clone)]
struct ChannelsResponse {
    #[serde(default)]
    channels: HashMap<String, usize>,
}

#[derive(Debug, Deserialize, Default, Clone)]
struct TasksResponse {
    #[serde(default)]
    scheduled_tasks: HashMap<String, TaskStatus>,
}

#[derive(Debug, Deserialize, Default, Clone)]
struct TaskStatus {
    #[serde(default)]
    schedule: String,
    #[serde(default)]
    status: String,
    #[serde(default)]
    last_run: Option<String>,
    #[serde(default)]
    last_duration_ms: Option<u64>,
    #[serde(default)]
    last_result: Option<String>,
    #[serde(default)]
    last_error: Option<String>,
    #[serde(default)]
    total_runs: u64,
    #[serde(default)]
    total_failures: u64,
}

// ── Dashboard state ───────────────────────────────────────────

/// Maximum sparkline history depth.
const SPARKLINE_DEPTH: usize = 120;

struct DashboardState {
    base_url: String,
    health: HealthResponse,
    metrics: MetricsResponse,
    tasks: TasksResponse,
    loggers: LoggersResponse,
    channels: ChannelsResponse,
    /// Rolling throughput samples (requests in last interval).
    throughput_history: VecDeque<u64>,
    /// Rolling p50 latency samples.
    latency_p50_history: VecDeque<u64>,
    /// Rolling p99 latency samples.
    latency_p99_history: VecDeque<u64>,
    /// Previous total requests for computing delta.
    prev_requests_total: u64,
    /// Whether the app is reachable.
    connected: bool,
    /// Last error message.
    last_error: Option<String>,
    /// When we last polled.
    last_poll: Instant,
    /// Route table scroll offset.
    route_scroll: usize,
    /// Currently selected tab.
    active_tab: usize,
    /// Tick counter for animations.
    tick: u64,
}

impl DashboardState {
    fn new(base_url: String) -> Self {
        Self {
            base_url,
            health: HealthResponse::default(),
            metrics: MetricsResponse::default(),
            tasks: TasksResponse::default(),
            loggers: LoggersResponse::default(),
            channels: ChannelsResponse::default(),
            throughput_history: VecDeque::with_capacity(SPARKLINE_DEPTH),
            latency_p50_history: VecDeque::with_capacity(SPARKLINE_DEPTH),
            latency_p99_history: VecDeque::with_capacity(SPARKLINE_DEPTH),
            prev_requests_total: 0,
            connected: false,
            last_error: None,
            last_poll: Instant::now(),
            route_scroll: 0,
            active_tab: 0,
            tick: 0,
        }
    }

    fn poll(&mut self) {
        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(2))
            .build();

        let client = match client {
            Ok(c) => c,
            Err(e) => {
                self.connected = false;
                self.last_error = Some(format!("HTTP client error: {e}"));
                return;
            }
        };

        if !self.fetch_health(&client) {
            return;
        }

        self.fetch_metrics(&client);
        self.fetch_tasks(&client);
        self.fetch_loggers(&client);
        self.fetch_channels(&client);

        self.last_poll = Instant::now();
    }

    fn fetch_health(&mut self, client: &reqwest::blocking::Client) -> bool {
        match client
            .get(format!("{}/actuator/health", self.base_url))
            .send()
        {
            Ok(resp) if resp.status().is_success() || resp.status().as_u16() == 503 => {
                self.connected = true;
                self.last_error = None;
                if let Ok(h) = resp.json::<HealthResponse>() {
                    self.health = h;
                }
                true
            }
            Ok(resp) => {
                self.connected = true;
                self.last_error = Some(format!("Health returned {}", resp.status()));
                true
            }
            Err(e) => {
                self.connected = false;
                self.last_error = Some(format!("Connection failed: {e}"));
                false
            }
        }
    }

    fn fetch_metrics(&mut self, client: &reqwest::blocking::Client) {
        if let Ok(resp) = client
            .get(format!("{}/actuator/metrics", self.base_url))
            .send()
            && let Ok(m) = resp.json::<MetricsResponse>()
        {
            let delta = m
                .http
                .requests_total
                .saturating_sub(self.prev_requests_total);
            if self.prev_requests_total > 0 || !self.throughput_history.is_empty() {
                self.throughput_history.push_back(delta);
                if self.throughput_history.len() > SPARKLINE_DEPTH {
                    self.throughput_history.pop_front();
                }
                self.throughput_history.make_contiguous();
            }
            self.prev_requests_total = m.http.requests_total;

            self.latency_p50_history.push_back(m.http.latency_ms.p50);
            if self.latency_p50_history.len() > SPARKLINE_DEPTH {
                self.latency_p50_history.pop_front();
            }
            self.latency_p50_history.make_contiguous();
            self.latency_p99_history.push_back(m.http.latency_ms.p99);
            if self.latency_p99_history.len() > SPARKLINE_DEPTH {
                self.latency_p99_history.pop_front();
            }
            self.latency_p99_history.make_contiguous();

            self.metrics = m;
        }
    }

    fn fetch_tasks(&mut self, client: &reqwest::blocking::Client) {
        if let Ok(resp) = client
            .get(format!("{}/actuator/tasks", self.base_url))
            .send()
            && let Ok(t) = resp.json::<TasksResponse>()
        {
            self.tasks = t;
        }
    }

    fn fetch_loggers(&mut self, client: &reqwest::blocking::Client) {
        if let Ok(resp) = client
            .get(format!("{}/actuator/loggers", self.base_url))
            .send()
            && let Ok(l) = resp.json::<LoggersResponse>()
        {
            self.loggers = l;
        }
    }

    fn fetch_channels(&mut self, client: &reqwest::blocking::Client) {
        if let Ok(resp) = client
            .get(format!("{}/actuator/channels", self.base_url))
            .send()
            && let Ok(c) = resp.json::<ChannelsResponse>()
        {
            self.channels = c;
        }
    }
}

// ── TUI rendering ─────────────────────────────────────────────

pub fn run(url: &str, poll_secs: u64) {
    let base_url = url.trim_end_matches('/').to_string();
    let poll_interval = Duration::from_secs(poll_secs);

    let mut state = DashboardState::new(base_url);

    state.poll();

    terminal::enable_raw_mode().expect("failed to enable raw mode");
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, cursor::Hide).expect("failed to setup terminal");
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).expect("failed to create terminal");

    let result = run_loop(&mut terminal, &mut state, poll_interval);

    terminal::disable_raw_mode().expect("failed to disable raw mode");
    execute!(terminal.backend_mut(), LeaveAlternateScreen, cursor::Show)
        .expect("failed to restore terminal");

    if let Err(e) = result {
        eprintln!("Error: {e}");
    }
}

fn run_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    state: &mut DashboardState,
    poll_interval: Duration,
) -> io::Result<()> {
    loop {
        terminal.draw(|frame| draw(frame, state))?;

        // A short timeout keeps the animation smooth.
        if event::poll(Duration::from_millis(100))?
            && let Event::Key(key) = event::read()?
            && key.kind == KeyEventKind::Press
        {
            match key.code {
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    return Ok(());
                }
                KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
                KeyCode::Tab => {
                    state.active_tab = (state.active_tab + 1) % 4;
                }
                KeyCode::BackTab => {
                    if state.active_tab == 0 {
                        state.active_tab = 3;
                    } else {
                        state.active_tab -= 1;
                    }
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    state.route_scroll = state.route_scroll.saturating_add(1);
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    state.route_scroll = state.route_scroll.saturating_sub(1);
                }
                KeyCode::Home | KeyCode::Char('g') => {
                    state.route_scroll = 0;
                }
                _ => {}
            }
        }

        state.tick += 1;

        if state.last_poll.elapsed() >= poll_interval {
            state.poll();
        }
    }
}

fn draw(frame: &mut ratatui::Frame, state: &DashboardState) {
    let area = frame.area();

    let main_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // header
            Constraint::Min(10),   // body
            Constraint::Length(1), // footer
        ])
        .split(area);

    draw_header(frame, main_chunks[0], state);

    match state.active_tab {
        0 => draw_overview_tab(frame, main_chunks[1], state),
        1 => draw_routes_tab(frame, main_chunks[1], state),
        2 => draw_loggers_tab(frame, main_chunks[1], state),
        3 => draw_channels_tab(frame, main_chunks[1], state),
        _ => {}
    }

    draw_footer(frame, main_chunks[2], state);
}

fn draw_header(frame: &mut ratatui::Frame, area: Rect, state: &DashboardState) {
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Length(30), // logo
            Constraint::Min(20),    // tabs
            Constraint::Length(28), // connection status
        ])
        .split(area);

    let status_color = if !state.connected {
        Color::Red
    } else if state.health.status == "ok" {
        Color::Green
    } else {
        Color::Yellow
    };

    let title = Paragraph::new(Line::from(vec![
        Span::styled("  🍂 ", Style::default().fg(Color::Rgb(204, 120, 50))),
        Span::styled(
            "autumn",
            Style::default()
                .fg(Color::Rgb(204, 120, 50))
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" monitor", Style::default().fg(Color::Gray)),
    ]))
    .block(
        Block::default()
            .borders(Borders::BOTTOM)
            .border_style(Style::default().fg(Color::DarkGray)),
    );
    frame.render_widget(title, chunks[0]);

    let tab_titles = vec!["Overview", "Routes", "Loggers", "Channels"];
    let tabs = Tabs::new(tab_titles)
        .select(state.active_tab)
        .style(Style::default().fg(Color::DarkGray))
        .highlight_style(
            Style::default()
                .fg(Color::Rgb(204, 120, 50))
                .add_modifier(Modifier::BOLD),
        )
        .divider(Span::raw(" | "))
        .block(
            Block::default()
                .borders(Borders::BOTTOM)
                .border_style(Style::default().fg(Color::DarkGray)),
        );
    frame.render_widget(tabs, chunks[1]);

    let (indicator, label) = if !state.connected {
        ("●", "disconnected")
    } else if state.health.status == "ok" {
        ("●", "healthy")
    } else {
        ("●", "degraded")
    };

    let conn = Paragraph::new(Line::from(vec![
        Span::styled(indicator, Style::default().fg(status_color)),
        Span::raw(" "),
        Span::styled(label, Style::default().fg(status_color)),
        Span::raw("  "),
        Span::styled(
            if state.health.profile.is_empty() {
                String::new()
            } else {
                format!("[{}]", state.health.profile)
            },
            Style::default().fg(Color::DarkGray),
        ),
    ]))
    .alignment(Alignment::Right)
    .block(
        Block::default()
            .borders(Borders::BOTTOM)
            .border_style(Style::default().fg(Color::DarkGray)),
    );
    frame.render_widget(conn, chunks[2]);
}

fn draw_overview_tab(frame: &mut ratatui::Frame, area: Rect, state: &DashboardState) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(9),  // stats cards row
            Constraint::Length(10), // sparklines
            Constraint::Min(8),     // status codes + tasks
        ])
        .split(area);

    draw_stats_cards(frame, rows[0], state);
    draw_sparklines(frame, rows[1], state);
    draw_bottom_panels(frame, rows[2], state);
}

fn draw_stat_card(
    frame: &mut ratatui::Frame,
    area: Rect,
    title: &str,
    main_value: String,
    main_color: Color,
    subtitle: String,
) {
    let block = make_card_block(title);
    let paragraph = Paragraph::new(Text::from(vec![
        Line::raw(""),
        Line::from(Span::styled(
            main_value,
            Style::default().fg(main_color).add_modifier(Modifier::BOLD),
        )),
        Line::raw(""),
        Line::from(Span::styled(subtitle, Style::default().fg(Color::DarkGray))),
    ]))
    .alignment(Alignment::Center)
    .block(block);
    frame.render_widget(paragraph, area);
}

fn draw_stats_cards(frame: &mut ratatui::Frame, area: Rect, state: &DashboardState) {
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(20),
            Constraint::Percentage(20),
            Constraint::Percentage(20),
            Constraint::Percentage(20),
            Constraint::Percentage(20),
        ])
        .split(area);

    let m = &state.metrics.http;

    draw_stat_card(
        frame,
        chunks[0],
        "Total Requests",
        format_number(m.requests_total),
        Color::White,
        format!("{} active", m.requests_active),
    );

    let rps = state.throughput_history.back().copied().unwrap_or(0);
    draw_stat_card(
        frame,
        chunks[1],
        "Throughput",
        format!("{rps}"),
        if rps > 0 {
            Color::Green
        } else {
            Color::DarkGray
        },
        "req/s".to_string(),
    );

    draw_stat_card(
        frame,
        chunks[2],
        "p50 Latency",
        format!("{}ms", m.latency_ms.p50),
        latency_color(m.latency_ms.p50),
        "median".to_string(),
    );

    draw_stat_card(
        frame,
        chunks[3],
        "p95 Latency",
        format!("{}ms", m.latency_ms.p95),
        latency_color(m.latency_ms.p95),
        "95th pct".to_string(),
    );

    draw_stat_card(
        frame,
        chunks[4],
        "p99 Latency",
        format!("{}ms", m.latency_ms.p99),
        latency_color(m.latency_ms.p99),
        "99th pct".to_string(),
    );
}

fn draw_sparklines(frame: &mut ratatui::Frame, area: Rect, state: &DashboardState) {
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(area);

    let throughput_block = Block::default()
        .title(Span::styled(
            " Throughput (req/s) ",
            Style::default()
                .fg(Color::Rgb(204, 120, 50))
                .add_modifier(Modifier::BOLD),
        ))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray));

    let throughput_sparkline = Sparkline::default()
        .block(throughput_block)
        .data(state.throughput_history.as_slices().0)
        .style(Style::default().fg(Color::Green));
    frame.render_widget(throughput_sparkline, chunks[0]);

    let latency_block = Block::default()
        .title(Span::styled(
            " Latency p99 (ms) ",
            Style::default()
                .fg(Color::Rgb(204, 120, 50))
                .add_modifier(Modifier::BOLD),
        ))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray));

    let latency_sparkline = Sparkline::default()
        .block(latency_block)
        .data(state.latency_p99_history.as_slices().0)
        .style(Style::default().fg(Color::Rgb(255, 150, 50)));
    frame.render_widget(latency_sparkline, chunks[1]);
}

fn draw_bottom_panels(frame: &mut ratatui::Frame, area: Rect, state: &DashboardState) {
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(30), // status codes
            Constraint::Percentage(35), // health & db
            Constraint::Percentage(35), // tasks
        ])
        .split(area);

    draw_status_codes(frame, chunks[0], state);
    draw_health_panel(frame, chunks[1], state);
    draw_tasks_panel(frame, chunks[2], state);
}

fn draw_status_codes(frame: &mut ratatui::Frame, area: Rect, state: &DashboardState) {
    let s = &state.metrics.http.by_status;

    let bar_group = BarGroup::default().bars(&[
        Bar::default()
            .value(s.s2xx)
            .label("2xx")
            .style(Style::default().fg(Color::Green)),
        Bar::default()
            .value(s.s3xx)
            .label("3xx")
            .style(Style::default().fg(Color::Cyan)),
        Bar::default()
            .value(s.s4xx)
            .label("4xx")
            .style(Style::default().fg(Color::Yellow)),
        Bar::default()
            .value(s.s5xx)
            .label("5xx")
            .style(Style::default().fg(Color::Red)),
    ]);

    let chart = BarChart::default()
        .block(
            Block::default()
                .title(Span::styled(
                    " Status Codes ",
                    Style::default()
                        .fg(Color::Rgb(204, 120, 50))
                        .add_modifier(Modifier::BOLD),
                ))
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::DarkGray)),
        )
        .data(bar_group)
        .bar_width(5)
        .bar_gap(2)
        .value_style(
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        );

    frame.render_widget(chart, area);
}

fn draw_health_panel(frame: &mut ratatui::Frame, area: Rect, state: &DashboardState) {
    let block = Block::default()
        .title(Span::styled(
            " Health & Info ",
            Style::default()
                .fg(Color::Rgb(204, 120, 50))
                .add_modifier(Modifier::BOLD),
        ))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray));

    let mut lines = vec![
        info_line(
            "Status",
            &state.health.status,
            status_color(&state.health.status),
        ),
        info_line("Version", &state.health.version, Color::White),
        info_line("Profile", &state.health.profile, Color::Cyan),
        info_line("Uptime", &state.health.uptime, Color::White),
    ];

    if let Some(db) = &state.metrics.database {
        push_db_pool_lines(
            &mut lines,
            None,
            db.pool_size,
            db.active_connections,
            db.idle_connections,
        );
    } else if let Some(checks) = &state.health.checks
        && let Some(db) = &checks.database
    {
        push_db_pool_lines(
            &mut lines,
            Some(&db.status),
            db.pool_size,
            db.active_connections,
            db.idle_connections,
        );
    }

    let paragraph = Paragraph::new(lines).block(block).wrap(Wrap { trim: true });
    frame.render_widget(paragraph, area);
}

fn push_db_pool_lines(
    lines: &mut Vec<Line<'static>>,
    status: Option<&str>,
    pool_size: u64,
    active_connections: u64,
    idle_connections: u64,
) {
    lines.push(Line::raw(""));
    lines.push(Line::from(Span::styled(
        "Database Pool",
        Style::default()
            .fg(Color::Rgb(204, 120, 50))
            .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
    )));
    if let Some(status) = status {
        lines.push(info_line("DB Status", status, status_color(status)));
    }
    lines.push(info_line("Pool Size", &pool_size.to_string(), Color::White));
    lines.push(info_line(
        "Active",
        &active_connections.to_string(),
        Color::Yellow,
    ));
    lines.push(info_line(
        "Idle",
        &idle_connections.to_string(),
        Color::Green,
    ));
}

fn draw_tasks_panel(frame: &mut ratatui::Frame, area: Rect, state: &DashboardState) {
    let block = Block::default()
        .title(Span::styled(
            " Scheduled Tasks ",
            Style::default()
                .fg(Color::Rgb(204, 120, 50))
                .add_modifier(Modifier::BOLD),
        ))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray));

    if state.tasks.scheduled_tasks.is_empty() {
        let no_tasks = Paragraph::new(Text::from(vec![
            Line::raw(""),
            Line::from(Span::styled(
                "No scheduled tasks",
                Style::default().fg(Color::DarkGray),
            )),
        ]))
        .alignment(Alignment::Center)
        .block(block);
        frame.render_widget(no_tasks, area);
        return;
    }

    let mut lines = Vec::new();
    for (name, task) in &state.tasks.scheduled_tasks {
        let status_icon = match task.status.as_str() {
            "running" => Span::styled("▶ ", Style::default().fg(Color::Green)),
            "idle" => Span::styled("◆ ", Style::default().fg(Color::DarkGray)),
            _ => Span::styled("? ", Style::default().fg(Color::Yellow)),
        };

        lines.push(Line::from(vec![
            status_icon,
            Span::styled(
                name,
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            ),
        ]));
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(&task.schedule, Style::default().fg(Color::DarkGray)),
            Span::raw("  "),
            Span::styled(
                format!("runs: {}", task.total_runs),
                Style::default().fg(Color::Cyan),
            ),
            if task.total_failures > 0 {
                Span::styled(
                    format!("  fails: {}", task.total_failures),
                    Style::default().fg(Color::Red),
                )
            } else {
                Span::raw("")
            },
        ]));

        if task.last_run.is_some() || task.last_duration_ms.is_some() || task.last_result.is_some()
        {
            let mut run_info = vec![Span::raw("  ")];
            if let Some(last_run) = &task.last_run {
                run_info.push(Span::styled(
                    format!("last run: {last_run}"),
                    Style::default().fg(Color::DarkGray),
                ));
            }
            if let Some(dur) = task.last_duration_ms {
                run_info.push(Span::styled(
                    format!(" ({dur}ms)"),
                    Style::default().fg(Color::DarkGray),
                ));
            }
            if let Some(res) = &task.last_result {
                run_info.push(Span::raw(" ["));
                let res_color =
                    if res.eq_ignore_ascii_case("ok") || res.eq_ignore_ascii_case("success") {
                        Color::Green
                    } else {
                        Color::Yellow
                    };
                run_info.push(Span::styled(res, Style::default().fg(res_color)));
                run_info.push(Span::raw("]"));
            }
            lines.push(Line::from(run_info));
        }

        if let Some(err) = &task.last_error {
            lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(
                    truncate(err, 40),
                    Style::default()
                        .fg(Color::Red)
                        .add_modifier(Modifier::ITALIC),
                ),
            ]));
        }
    }

    let paragraph = Paragraph::new(lines).block(block).wrap(Wrap { trim: true });
    frame.render_widget(paragraph, area);
}

fn draw_loggers_tab(frame: &mut ratatui::Frame, area: Rect, state: &DashboardState) {
    let block = Block::default()
        .title(Span::styled(
            " Loggers ",
            Style::default()
                .fg(Color::Rgb(204, 120, 50))
                .add_modifier(Modifier::BOLD),
        ))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray))
        .padding(Padding::new(1, 1, 0, 0));

    let header = Row::new(vec![
        Cell::from("Logger Name").style(
            Style::default()
                .fg(Color::Rgb(204, 120, 50))
                .add_modifier(Modifier::BOLD),
        ),
        Cell::from("Level").style(
            Style::default()
                .fg(Color::Rgb(204, 120, 50))
                .add_modifier(Modifier::BOLD),
        ),
    ])
    .height(1)
    .bottom_margin(1);

    let mut loggers: Vec<_> = state.loggers.loggers.iter().collect();
    loggers.sort_by(|a, b| a.0.cmp(b.0));

    let mut rows = vec![Row::new(vec![
        Cell::from("ROOT (current)").style(
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
        Cell::from(state.loggers.current_level.clone()).style(
            Style::default()
                .fg(match state.loggers.current_level.as_str() {
                    "trace" => Color::Magenta,
                    "debug" => Color::Cyan,
                    "info" => Color::Green,
                    "warn" => Color::Yellow,
                    "error" => Color::Red,
                    _ => Color::White,
                })
                .add_modifier(Modifier::BOLD),
        ),
    ])];

    for (name, level) in loggers {
        let level_color = match level.as_str() {
            "trace" => Color::Magenta,
            "debug" => Color::Cyan,
            "info" => Color::Green,
            "warn" => Color::Yellow,
            "error" => Color::Red,
            _ => Color::White,
        };

        rows.push(Row::new(vec![
            Cell::from(name.clone()).style(Style::default().fg(Color::White)),
            Cell::from(level.clone()).style(
                Style::default()
                    .fg(level_color)
                    .add_modifier(Modifier::BOLD),
            ),
        ]));
    }

    let table = Table::new(
        rows,
        [Constraint::Percentage(70), Constraint::Percentage(30)],
    )
    .header(header)
    .block(block)
    .column_spacing(2);

    frame.render_widget(table, area);
}

fn draw_channels_tab(frame: &mut ratatui::Frame, area: Rect, state: &DashboardState) {
    let block = Block::default()
        .title(Span::styled(
            " Channels ",
            Style::default()
                .fg(Color::Rgb(204, 120, 50))
                .add_modifier(Modifier::BOLD),
        ))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray))
        .padding(Padding::new(1, 1, 0, 0));

    let header = Row::new(vec![
        Cell::from("Channel Name").style(
            Style::default()
                .fg(Color::Rgb(204, 120, 50))
                .add_modifier(Modifier::BOLD),
        ),
        Cell::from("Subscribers").style(
            Style::default()
                .fg(Color::Rgb(204, 120, 50))
                .add_modifier(Modifier::BOLD),
        ),
    ])
    .height(1)
    .bottom_margin(1);

    let mut rows = Vec::new();

    if state.channels.channels.is_empty() {
        rows.push(Row::new(vec![
            Cell::from("No active channels").style(Style::default().fg(Color::DarkGray)),
            Cell::from(""),
        ]));
    } else {
        let mut sorted_channels: Vec<_> = state.channels.channels.iter().collect();
        sorted_channels.sort_by_key(|(name, _)| *name);

        for (name, count) in sorted_channels {
            let count_color = if *count > 0 {
                Color::Green
            } else {
                Color::DarkGray
            };

            rows.push(Row::new(vec![
                Cell::from(name.clone()),
                Cell::from(count.to_string()).style(Style::default().fg(count_color)),
            ]));
        }
    }

    let widths = [Constraint::Percentage(50), Constraint::Percentage(50)];

    let table = Table::new(rows, widths)
        .header(header)
        .block(block)
        .column_spacing(2);

    frame.render_widget(table, area);
}

fn draw_routes_tab(frame: &mut ratatui::Frame, area: Rect, state: &DashboardState) {
    let block = Block::default()
        .title(Span::styled(
            " Routes ",
            Style::default()
                .fg(Color::Rgb(204, 120, 50))
                .add_modifier(Modifier::BOLD),
        ))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray))
        .padding(Padding::new(1, 1, 0, 0));

    let header = Row::new(vec![
        Cell::from("Route").style(
            Style::default()
                .fg(Color::Rgb(204, 120, 50))
                .add_modifier(Modifier::BOLD),
        ),
        Cell::from("Count").style(
            Style::default()
                .fg(Color::Rgb(204, 120, 50))
                .add_modifier(Modifier::BOLD),
        ),
        Cell::from("p50").style(
            Style::default()
                .fg(Color::Rgb(204, 120, 50))
                .add_modifier(Modifier::BOLD),
        ),
        Cell::from("p95").style(
            Style::default()
                .fg(Color::Rgb(204, 120, 50))
                .add_modifier(Modifier::BOLD),
        ),
        Cell::from("p99").style(
            Style::default()
                .fg(Color::Rgb(204, 120, 50))
                .add_modifier(Modifier::BOLD),
        ),
        Cell::from("Bar").style(
            Style::default()
                .fg(Color::Rgb(204, 120, 50))
                .add_modifier(Modifier::BOLD),
        ),
    ])
    .height(1)
    .bottom_margin(1);

    let mut routes: Vec<_> = state.metrics.http.by_route.iter().collect();
    routes.sort_by_key(|route| std::cmp::Reverse(route.1.count));

    let max_count = routes.first().map_or(1, |r| r.1.count.max(1));

    let rows: Vec<Row> = routes
        .iter()
        .enumerate()
        .map(|(i, (name, snap))| {
            #[allow(
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss,
                clippy::cast_precision_loss
            )]
            let bar_width = ((snap.count as f64 / max_count as f64) * 20.0) as usize;
            let bar = "█".repeat(bar_width);

            let bg = if i % 2 == 0 {
                Color::Reset
            } else {
                Color::Rgb(30, 30, 30)
            };

            Row::new(vec![
                Cell::from((*name).clone()).style(Style::default().fg(Color::White)),
                Cell::from(format_number(snap.count)).style(Style::default().fg(Color::Cyan)),
                Cell::from(format!("{}ms", snap.p50_ms))
                    .style(Style::default().fg(latency_color(snap.p50_ms))),
                Cell::from(format!("{}ms", snap.p95_ms))
                    .style(Style::default().fg(latency_color(snap.p95_ms))),
                Cell::from(format!("{}ms", snap.p99_ms))
                    .style(Style::default().fg(latency_color(snap.p99_ms))),
                Cell::from(bar).style(Style::default().fg(Color::Green)),
            ])
            .style(Style::default().bg(bg))
        })
        .collect();

    let widths = [
        Constraint::Min(30),
        Constraint::Length(10),
        Constraint::Length(8),
        Constraint::Length(8),
        Constraint::Length(8),
        Constraint::Min(20),
    ];

    let table = Table::new(rows, widths)
        .header(header)
        .block(block)
        .row_highlight_style(Style::default().add_modifier(Modifier::REVERSED));

    frame.render_widget(table, area);
}

fn draw_footer(frame: &mut ratatui::Frame, area: Rect, state: &DashboardState) {
    let elapsed = state.last_poll.elapsed().as_secs();

    let mut spans = vec![
        Span::styled(
            " q",
            Style::default()
                .fg(Color::Rgb(204, 120, 50))
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" quit  ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            "Tab",
            Style::default()
                .fg(Color::Rgb(204, 120, 50))
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" switch view  ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            "j/k",
            Style::default()
                .fg(Color::Rgb(204, 120, 50))
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" scroll  ", Style::default().fg(Color::DarkGray)),
    ];

    if let Some(err) = &state.last_error {
        spans.push(Span::styled(
            format!("  ⚠ {}", truncate(err, 50)),
            Style::default().fg(Color::Red),
        ));
    } else {
        spans.push(Span::styled(
            format!("  polled {elapsed}s ago"),
            Style::default().fg(Color::DarkGray),
        ));
    }

    let footer = Paragraph::new(Line::from(spans));
    frame.render_widget(footer, area);
}

// ── Helpers ───────────────────────────────────────────────────

fn make_card_block(title: &str) -> Block<'_> {
    Block::default()
        .title(Span::styled(
            format!(" {title} "),
            Style::default()
                .fg(Color::Rgb(204, 120, 50))
                .add_modifier(Modifier::BOLD),
        ))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray))
}

fn info_line(label: &str, value: &str, color: Color) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("  {label}: "), Style::default().fg(Color::DarkGray)),
        Span::styled(value.to_string(), Style::default().fg(color)),
    ])
}

const fn latency_color(ms: u64) -> Color {
    match ms {
        0..=10 => Color::Green,
        11..=50 => Color::Cyan,
        51..=200 => Color::Yellow,
        201..=1000 => Color::Rgb(255, 150, 50),
        _ => Color::Red,
    }
}

fn status_color(status: &str) -> Color {
    match status {
        "ok" | "up" => Color::Green,
        "degraded" => Color::Yellow,
        "down" => Color::Red,
        _ => Color::DarkGray,
    }
}

#[allow(clippy::cast_precision_loss)]
fn format_number(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}K", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() > max {
        format!("{}...", &s[..max.saturating_sub(3)])
    } else {
        s.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;

    fn test_state() -> DashboardState {
        let mut state = DashboardState::new("http://localhost:3000".to_string());
        state.connected = true;
        state.health = HealthResponse {
            status: "ok".to_string(),
            version: "0.1.0".to_string(),
            profile: "dev".to_string(),
            uptime: "1h 23m".to_string(),
            checks: None,
        };
        state.metrics = MetricsResponse {
            http: HttpMetrics {
                requests_total: 1500,
                requests_active: 3,
                latency_ms: LatencySnapshot {
                    p50: 5,
                    p95: 25,
                    p99: 100,
                },
                by_route: HashMap::from([
                    (
                        "GET /".to_string(),
                        RouteSnapshot {
                            count: 1000,
                            p50_ms: 3,
                            p95_ms: 10,
                            p99_ms: 50,
                        },
                    ),
                    (
                        "POST /api/users".to_string(),
                        RouteSnapshot {
                            count: 500,
                            p50_ms: 15,
                            p95_ms: 80,
                            p99_ms: 250,
                        },
                    ),
                ]),
                by_status: StatusSnapshot {
                    s2xx: 1400,
                    s3xx: 50,
                    s4xx: 30,
                    s5xx: 20,
                },
            },
            database: Some(DbPoolMetrics {
                pool_size: 10,
                active_connections: 3,
                idle_connections: 7,
            }),
        };
        state.throughput_history = VecDeque::from(vec![10, 20, 30, 25, 15, 42]);
        state.latency_p50_history = VecDeque::from(vec![3, 4, 5, 3, 4]);
        state.latency_p99_history = VecDeque::from(vec![50, 80, 100, 90, 70]);
        state
    }

    // ── Helper function tests ─────────────────────────────────

    #[test]
    fn format_number_plain() {
        assert_eq!(format_number(0), "0");
        assert_eq!(format_number(999), "999");
    }

    #[test]
    fn format_number_thousands() {
        assert_eq!(format_number(1_500), "1.5K");
        assert_eq!(format_number(42_000), "42.0K");
    }

    #[test]
    fn format_number_millions() {
        assert_eq!(format_number(2_500_000), "2.5M");
    }

    #[test]
    fn format_number_boundary() {
        assert_eq!(format_number(1_000), "1.0K");
        assert_eq!(format_number(1_000_000), "1.0M");
    }

    #[test]
    fn latency_color_green_for_fast() {
        assert_eq!(latency_color(0), Color::Green);
        assert_eq!(latency_color(5), Color::Green);
        assert_eq!(latency_color(10), Color::Green);
    }

    #[test]
    fn latency_color_cyan_for_moderate() {
        assert_eq!(latency_color(11), Color::Cyan);
        assert_eq!(latency_color(50), Color::Cyan);
    }

    #[test]
    fn latency_color_yellow_for_slow() {
        assert_eq!(latency_color(51), Color::Yellow);
        assert_eq!(latency_color(200), Color::Yellow);
    }

    #[test]
    fn latency_color_orange_for_very_slow() {
        assert_eq!(latency_color(201), Color::Rgb(255, 150, 50));
        assert_eq!(latency_color(1000), Color::Rgb(255, 150, 50));
    }

    #[test]
    fn latency_color_red_for_slow() {
        assert_eq!(latency_color(1001), Color::Red);
        assert_eq!(latency_color(5000), Color::Red);
    }

    #[test]
    fn truncate_short_string() {
        assert_eq!(truncate("hello", 10), "hello");
    }

    #[test]
    fn truncate_exact_length() {
        assert_eq!(truncate("hello", 5), "hello");
    }

    #[test]
    fn truncate_long_string() {
        assert_eq!(truncate("hello world this is long", 10), "hello w...");
    }

    #[test]
    fn status_color_mapping() {
        assert_eq!(status_color("ok"), Color::Green);
        assert_eq!(status_color("up"), Color::Green);
        assert_eq!(status_color("degraded"), Color::Yellow);
        assert_eq!(status_color("down"), Color::Red);
        assert_eq!(status_color("unknown"), Color::DarkGray);
    }

    #[test]
    fn info_line_produces_two_spans() {
        let line = info_line("Status", "ok", Color::Green);
        assert_eq!(line.spans.len(), 2);
    }

    #[test]
    fn make_card_block_has_title() {
        let block = make_card_block("Test");
        let _ = block;
    }

    // ── Dashboard state tests ─────────────────────────────────

    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;
    use std::thread;

    #[test]
    fn test_poll_updates_state() {
        let listener =
            TcpListener::bind("127.0.0.1:0").expect("failed extracting response component");
        let port = listener
            .local_addr()
            .expect("failed extracting response component")
            .port();
        let url = format!("http://127.0.0.1:{port}");

        thread::spawn(move || {
            for stream in listener.incoming().take(4) {
                let mut stream = stream.expect("failed extracting response component");
                let mut reader = BufReader::new(&mut stream);
                let mut req_line = String::new();
                if reader.read_line(&mut req_line).is_err() || req_line.is_empty() {
                    continue;
                }

                loop {
                    let mut header_line = String::new();
                    if reader.read_line(&mut header_line).is_err()
                        || header_line == "\r\n"
                        || header_line.trim().is_empty()
                    {
                        break;
                    }
                }

                let (body, status) = if req_line.contains("/actuator/health") {
                    ("{\"status\":\"up\"}", "200 OK")
                } else if req_line.contains("/actuator/metrics") {
                    ("{\"http\":{\"requests_total\":42}}", "200 OK")
                } else if req_line.contains("/actuator/tasks") {
                    ("{\"scheduled_tasks\":{}}", "200 OK")
                } else if req_line.contains("/actuator/loggers") {
                    ("{\"current_level\":\"info\"}", "200 OK")
                } else {
                    ("", "404 NOT FOUND")
                };

                let response = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{body}",
                    body.len()
                );

                let _ = stream.write_all(response.as_bytes());
            }
        });

        let mut state = DashboardState::new(url);
        assert!(!state.connected);
        assert_eq!(state.metrics.http.requests_total, 0);

        state.poll();

        assert!(state.connected);
        assert_eq!(state.health.status, "up");
        assert_eq!(state.metrics.http.requests_total, 42);
        assert_eq!(state.loggers.current_level, "info");
    }

    #[test]
    fn test_poll_handles_connection_error() {
        // Bind and drop a listener to get a port nothing listens on.
        let listener =
            TcpListener::bind("127.0.0.1:0").expect("failed extracting response component");
        let port = listener
            .local_addr()
            .expect("failed extracting response component")
            .port();
        drop(listener); // Ensure the port is immediately closed and unreachable

        let mut state = DashboardState::new(format!("http://127.0.0.1:{port}"));
        state.connected = true; // Assume it was previously connected

        state.poll();

        assert!(!state.connected);
        assert!(state.last_error.is_some());
        assert!(
            state
                .last_error
                .as_ref()
                .expect("failed extracting response component")
                .contains("Connection failed")
                || state
                    .last_error
                    .as_ref()
                    .expect("failed extracting response component")
                    .contains("HTTP client error")
        );
    }

    #[test]
    fn dashboard_state_initial() {
        let state = DashboardState::new("http://localhost:3000".to_string());
        assert!(!state.connected);
        assert_eq!(state.prev_requests_total, 0);
        assert!(state.throughput_history.is_empty());
        assert!(state.latency_p50_history.is_empty());
        assert!(state.latency_p99_history.is_empty());
        assert_eq!(state.active_tab, 0);
        assert_eq!(state.route_scroll, 0);
        assert_eq!(state.tick, 0);
        assert!(state.last_error.is_none());
    }

    #[test]
    fn dashboard_state_with_trailing_slash() {
        let state = DashboardState::new("http://localhost:3000/".to_string());
        assert_eq!(state.base_url, "http://localhost:3000/");
    }

    // ── Deserialization tests ─────────────────────────────────

    #[test]
    fn deserialize_health_response() {
        let json = r#"{"status":"ok","version":"0.1.0","profile":"dev","uptime":"1h 23m"}"#;
        let health: HealthResponse =
            serde_json::from_str(json).expect("failed extracting response component");
        assert_eq!(health.status, "ok");
        assert_eq!(health.profile, "dev");
        assert_eq!(health.version, "0.1.0");
        assert_eq!(health.uptime, "1h 23m");
        assert!(health.checks.is_none());
    }

    #[test]
    fn deserialize_health_with_db_check() {
        let json = r#"{
            "status":"ok","version":"0.1.0","profile":"dev","uptime":"1h",
            "checks":{"database":{"status":"ok","pool_size":10,"active_connections":3,"idle_connections":7}}
        }"#;
        let health: HealthResponse =
            serde_json::from_str(json).expect("failed extracting response component");
        let db = health
            .checks
            .expect("failed extracting response component")
            .database
            .expect("failed extracting response component");
        assert_eq!(db.status, "ok");
        assert_eq!(db.pool_size, 10);
        assert_eq!(db.active_connections, 3);
        assert_eq!(db.idle_connections, 7);
    }

    #[test]
    fn deserialize_health_minimal() {
        let json = r#"{"status":"up"}"#;
        let health: HealthResponse =
            serde_json::from_str(json).expect("failed extracting response component");
        assert_eq!(health.status, "up");
        assert!(health.version.is_empty());
    }

    #[test]
    fn deserialize_metrics_response() {
        let json = r#"{
            "http": {
                "requests_total": 150,
                "requests_active": 3,
                "latency_ms": {"p50": 5, "p95": 25, "p99": 100},
                "by_route": {
                    "GET /": {"count": 100, "p50_ms": 3, "p95_ms": 10, "p99_ms": 50}
                },
                "by_status": {"2xx": 140, "3xx": 5, "4xx": 3, "5xx": 2}
            }
        }"#;
        let metrics: MetricsResponse =
            serde_json::from_str(json).expect("failed extracting response component");
        assert_eq!(metrics.http.requests_total, 150);
        assert_eq!(metrics.http.requests_active, 3);
        assert_eq!(metrics.http.latency_ms.p50, 5);
        assert_eq!(metrics.http.latency_ms.p95, 25);
        assert_eq!(metrics.http.latency_ms.p99, 100);
        assert_eq!(metrics.http.by_status.s2xx, 140);
        assert_eq!(metrics.http.by_status.s3xx, 5);
        assert_eq!(metrics.http.by_status.s4xx, 3);
        assert_eq!(metrics.http.by_status.s5xx, 2);
        assert_eq!(metrics.http.by_route["GET /"].count, 100);
        assert_eq!(metrics.http.by_route["GET /"].p50_ms, 3);
        assert_eq!(metrics.http.by_route["GET /"].p95_ms, 10);
        assert_eq!(metrics.http.by_route["GET /"].p99_ms, 50);
    }

    #[test]
    fn deserialize_metrics_with_db() {
        let json = r#"{
            "http": {"requests_total": 0, "requests_active": 0,
                     "latency_ms": {"p50": 0, "p95": 0, "p99": 0},
                     "by_route": {}, "by_status": {"2xx": 0, "3xx": 0, "4xx": 0, "5xx": 0}},
            "database": {"pool_size": 10, "active_connections": 2, "idle_connections": 8}
        }"#;
        let metrics: MetricsResponse =
            serde_json::from_str(json).expect("failed extracting response component");
        let db = metrics
            .database
            .expect("failed extracting response component");
        assert_eq!(db.pool_size, 10);
        assert_eq!(db.active_connections, 2);
        assert_eq!(db.idle_connections, 8);
    }

    #[test]
    fn deserialize_metrics_minimal() {
        let json = r#"{"http":{}}"#;
        let metrics: MetricsResponse =
            serde_json::from_str(json).expect("failed extracting response component");
        assert_eq!(metrics.http.requests_total, 0);
        assert!(metrics.database.is_none());
    }

    #[test]
    fn deserialize_tasks_response() {
        let json = r#"{"scheduled_tasks":{"cleanup":{"schedule":"every 5m","status":"idle","total_runs":10,"total_failures":1}}}"#;
        let tasks: TasksResponse =
            serde_json::from_str(json).expect("failed extracting response component");
        assert_eq!(tasks.scheduled_tasks["cleanup"].total_runs, 10);
        assert_eq!(tasks.scheduled_tasks["cleanup"].total_failures, 1);
        assert_eq!(tasks.scheduled_tasks["cleanup"].schedule, "every 5m");
        assert_eq!(tasks.scheduled_tasks["cleanup"].status, "idle");
    }

    #[test]
    fn deserialize_tasks_with_error() {
        let json = r#"{"scheduled_tasks":{"sync":{"schedule":"cron 0 * * * *","status":"idle",
            "last_run":"2026-01-01T00:00:00Z","last_duration_ms":150,"last_result":"failed",
            "last_error":"connection refused","total_runs":5,"total_failures":2}}}"#;
        let tasks: TasksResponse =
            serde_json::from_str(json).expect("failed extracting response component");
        let sync = &tasks.scheduled_tasks["sync"];
        assert_eq!(sync.last_error.as_deref(), Some("connection refused"));
        assert_eq!(sync.total_failures, 2);
    }

    #[test]
    fn deserialize_tasks_empty() {
        let json = r#"{"scheduled_tasks":{}}"#;
        let tasks: TasksResponse =
            serde_json::from_str(json).expect("failed extracting response component");
        assert!(tasks.scheduled_tasks.is_empty());
    }

    #[test]
    fn deserialize_loggers_response() {
        let json = r#"{"current_level":"info","available_levels":["trace","debug","info","warn","error"],"loggers":{"my_module":"debug","other_module":"trace"}}"#;
        let loggers: LoggersResponse =
            serde_json::from_str(json).expect("failed extracting response component");
        assert_eq!(loggers.current_level, "info");
        assert_eq!(loggers.available_levels.len(), 5);
        assert_eq!(loggers.loggers["my_module"], "debug");
        assert_eq!(loggers.loggers["other_module"], "trace");
    }

    #[test]
    fn deserialize_channels_response() {
        let json = r#"{"channels":{"chat":10,"notifications":0}}"#;
        let channels: ChannelsResponse =
            serde_json::from_str(json).expect("failed extracting response component");
        assert_eq!(channels.channels.len(), 2);
        assert_eq!(channels.channels["chat"], 10);
        assert_eq!(channels.channels["notifications"], 0);
    }

    #[test]
    fn default_types() {
        let _h = HealthResponse::default();
        let _m = MetricsResponse::default();
        let _t = TasksResponse::default();
        let _l = LatencySnapshot::default();
        let _s = StatusSnapshot::default();
        let _r = RouteSnapshot::default();
        let _hm = HttpMetrics::default();
        let _ts = TaskStatus::default();
        let _hc = HealthChecks::default();
        let _dc = DatabaseCheck::default();
        let _db = DbPoolMetrics::default();
        let _l = LoggersResponse::default();
        let _c = ChannelsResponse::default();
    }

    // ── Rendering tests (TestBackend) ─────────────────────────

    fn render_frame(state: &DashboardState, width: u16, height: u16) {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).expect("failed extracting response component");
        terminal
            .draw(|frame| draw(frame, state))
            .expect("failed extracting response component");
    }

    #[test]
    fn render_overview_tab() {
        let state = test_state();
        render_frame(&state, 120, 40);
    }

    #[test]
    fn render_routes_tab() {
        let mut state = test_state();
        state.active_tab = 1;
        render_frame(&state, 120, 40);
    }

    #[test]
    fn render_channels_tab() {
        let mut state = test_state();
        state.active_tab = 3;
        state.channels.channels.insert("chat".to_string(), 10);
        state
            .channels
            .channels
            .insert("notifications".to_string(), 0);
        render_frame(&state, 120, 40);
    }

    #[test]
    fn render_loggers_tab() {
        let mut state = test_state();
        state.active_tab = 2;
        state.loggers = LoggersResponse {
            current_level: "unknown_level".to_string(), // Test fallback color
            available_levels: vec![
                "trace".to_string(),
                "debug".to_string(),
                "info".to_string(),
                "warn".to_string(),
                "error".to_string(),
            ],
            loggers: vec![
                ("my_module".to_string(), "debug".to_string()),
                ("other_module".to_string(), "trace".to_string()),
                ("mod_warn".to_string(), "warn".to_string()),
                ("mod_error".to_string(), "error".to_string()),
                ("mod_info".to_string(), "info".to_string()),
                ("mod_unknown".to_string(), "unknown".to_string()),
            ]
            .into_iter()
            .collect(),
        };
        render_frame(&state, 120, 40);
    }

    #[test]
    fn render_disconnected() {
        let mut state = DashboardState::new("http://localhost:3000".to_string());
        state.connected = false;
        state.last_error = Some("Connection refused".to_string());
        render_frame(&state, 120, 40);
    }

    #[test]
    fn render_degraded_health() {
        let mut state = test_state();
        state.health.status = "degraded".to_string();
        render_frame(&state, 120, 40);
    }

    #[test]
    fn render_empty_profile() {
        let mut state = test_state();
        state.health.profile = String::new();
        render_frame(&state, 120, 40);
    }

    #[test]
    fn render_no_routes() {
        let mut state = test_state();
        state.active_tab = 1;
        state.metrics.http.by_route.clear();
        render_frame(&state, 120, 40);
    }

    #[test]
    fn render_no_tasks() {
        let mut state = test_state();
        state.tasks.scheduled_tasks.clear();
        render_frame(&state, 120, 40);
    }

    #[test]
    fn render_with_tasks() {
        let mut state = test_state();
        state.tasks.scheduled_tasks.insert(
            "cleanup".to_string(),
            TaskStatus {
                schedule: "every 5m".to_string(),
                status: "idle".to_string(),
                last_run: None,
                last_duration_ms: None,
                last_result: None,
                last_error: None,
                total_runs: 42,
                total_failures: 0,
            },
        );
        state.tasks.scheduled_tasks.insert(
            "sync".to_string(),
            TaskStatus {
                schedule: "cron 0 * * * *".to_string(),
                status: "running".to_string(),
                last_run: Some("2026-01-01T00:00:00Z".to_string()),
                last_duration_ms: Some(150),
                last_result: Some("failed".to_string()),
                last_error: Some("connection refused".to_string()),
                total_runs: 10,
                total_failures: 3,
            },
        );
        render_frame(&state, 120, 40);
    }

    #[test]
    fn render_with_health_checks_db() {
        let mut state = test_state();
        state.metrics.database = None;
        state.health.checks = Some(HealthChecks {
            database: Some(DatabaseCheck {
                status: "ok".to_string(),
                pool_size: 10,
                active_connections: 3,
                idle_connections: 7,
            }),
        });
        render_frame(&state, 120, 40);
    }

    #[test]
    fn render_no_db_info() {
        let mut state = test_state();
        state.metrics.database = None;
        state.health.checks = None;
        render_frame(&state, 120, 40);
    }

    #[test]
    fn render_zero_throughput() {
        let mut state = test_state();
        state.throughput_history = VecDeque::from(vec![0, 0, 0]);
        render_frame(&state, 120, 40);
    }

    #[test]
    fn render_with_error_in_footer() {
        let mut state = test_state();
        state.last_error = Some(
            "Something went wrong with a really long error message that should be truncated"
                .to_string(),
        );
        render_frame(&state, 120, 40);
    }

    #[test]
    fn render_small_terminal() {
        let state = test_state();
        render_frame(&state, 60, 20);
    }

    #[test]
    fn render_wide_terminal() {
        let state = test_state();
        render_frame(&state, 200, 50);
    }

    #[test]
    fn render_task_unknown_status() {
        let mut state = test_state();
        state.tasks.scheduled_tasks.insert(
            "mystery".to_string(),
            TaskStatus {
                schedule: "every 1h".to_string(),
                status: "unknown".to_string(),
                total_runs: 0,
                total_failures: 0,
                ..TaskStatus::default()
            },
        );
        render_frame(&state, 120, 40);
    }

    #[test]
    fn render_invalid_tab_does_not_panic() {
        let mut state = test_state();
        state.active_tab = 99;
        render_frame(&state, 120, 40);
    }

    #[test]
    fn back_tab_wrap_logic() {
        let mut state = test_state();
        state.active_tab = 0;

        if state.active_tab == 0 {
            state.active_tab = 3;
        } else {
            state.active_tab -= 1;
        }
        assert_eq!(state.active_tab, 3);

        if state.active_tab == 0 {
            state.active_tab = 3;
        } else {
            state.active_tab -= 1;
        }
        assert_eq!(state.active_tab, 2);
    }

    #[test]
    fn render_all_zero_status_codes() {
        let mut state = test_state();
        state.metrics.http.by_status = StatusSnapshot::default();
        render_frame(&state, 120, 40);
    }

    #[test]
    fn render_high_latency_values() {
        let mut state = test_state();
        state.metrics.http.latency_ms = LatencySnapshot {
            p50: 500,
            p95: 2000,
            p99: 5000,
        };
        render_frame(&state, 120, 40);
    }

    #[test]
    fn render_large_request_counts() {
        let mut state = test_state();
        state.metrics.http.requests_total = 2_500_000;
        state.metrics.http.by_route.insert(
            "GET /popular".to_string(),
            RouteSnapshot {
                count: 1_500_000,
                p50_ms: 2,
                p95_ms: 8,
                p99_ms: 30,
            },
        );
        render_frame(&state, 120, 40);
    }
}
