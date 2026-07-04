use std::io;
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use crossterm::terminal::{self, EnterAlternateScreen, LeaveAlternateScreen};
use crossterm::{cursor, execute};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::Terminal;

use crate::task::{TaskListing, TaskOptions, get_task_list};

enum TuiState {
    Browsing,
    Running,
}

enum BackgroundEvent {
    Stdout(String),
    Stderr(String),
    Exit(i32),
}

struct App<'a> {
    state: TuiState,
    tasks: Vec<TaskListing>,
    list_state: ListState,
    output: Vec<Line<'a>>,
    binary: &'a Path,
    opts: &'a TaskOptions<'a>,
    rx: Option<mpsc::Receiver<BackgroundEvent>>,
    scroll_offset: u16,
}

impl<'a> App<'a> {
    fn new(tasks: Vec<TaskListing>, binary: &'a Path, opts: &'a TaskOptions<'a>) -> Self {
        let mut list_state = ListState::default();
        if !tasks.is_empty() {
            list_state.select(Some(0));
        }
        Self {
            state: TuiState::Browsing,
            tasks,
            list_state,
            output: Vec::new(),
            binary,
            opts,
            rx: None,
            scroll_offset: 0,
        }
    }

    fn next(&mut self) {
        if self.tasks.is_empty() {
            return;
        }
        let i = match self.list_state.selected() {
            Some(i) => {
                if i >= self.tasks.len() - 1 {
                    0
                } else {
                    i + 1
                }
            }
            None => 0,
        };
        self.list_state.select(Some(i));
    }

    fn previous(&mut self) {
        if self.tasks.is_empty() {
            return;
        }
        let i = match self.list_state.selected() {
            Some(i) => {
                if i == 0 {
                    self.tasks.len() - 1
                } else {
                    i - 1
                }
            }
            None => 0,
        };
        self.list_state.select(Some(i));
    }

    fn run_selected_task(&mut self) {
        let Some(selected) = self.list_state.selected() else {
            return;
        };
        let task = &self.tasks[selected];

        self.state = TuiState::Running;
        self.output.clear();
        self.output.push(Line::from(Span::styled(
            format!("Starting task: {}...", task.name),
            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        )));
        self.output.push(Line::raw(""));
        self.scroll_offset = 0;

        let mut command = Command::new(self.binary);

        let mut args_json = "[]".to_string();
        if !self.opts.args.is_empty() {
             args_json = serde_json::to_string(self.opts.args).unwrap_or_else(|_| "[]".to_string());
        }

        command
            .env("AUTUMN_RUN_TASK", &task.name)
            .env("AUTUMN_TASK_ARGS", args_json)
            .env("AUTUMN_ENV", self.opts.profile)
            .env("AUTUMN_PROFILE", self.opts.profile)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        command.env_remove(crate::serve::MANAGED_PG_ATTACH_URL_ENV);
        if let Some(env) = crate::serve::managed_pg_env(self.opts.package) {
            command.env(crate::serve::MANAGED_PG_DATA_DIR_ENV, &env.data_dir);
            if let Some(url) = env.attach_url {
                command.env(crate::serve::MANAGED_PG_ATTACH_URL_ENV, url);
            }
        }

        match command.spawn() {
            Ok(mut child) => {
                let (tx, rx) = mpsc::channel();
                self.rx = Some(rx);

                let stdout = child.stdout.take().expect("Failed to capture stdout");
                let stderr = child.stderr.take().expect("Failed to capture stderr");

                let tx_out = tx.clone();
                thread::spawn(move || {
                    let reader = BufReader::new(stdout);
                    for line in reader.lines().flatten() {
                        let _ = tx_out.send(BackgroundEvent::Stdout(line));
                    }
                });

                let tx_err = tx.clone();
                thread::spawn(move || {
                    let reader = BufReader::new(stderr);
                    for line in reader.lines().flatten() {
                        let _ = tx_err.send(BackgroundEvent::Stderr(line));
                    }
                });

                thread::spawn(move || {
                    let status = child.wait().expect("Failed to wait on child");
                    let _ = tx.send(BackgroundEvent::Exit(status.code().unwrap_or(-1)));
                });
            }
            Err(e) => {
                self.output.push(Line::from(Span::styled(
                    format!("Failed to spawn task: {e}"),
                    Style::default().fg(Color::Red),
                )));
                self.state = TuiState::Browsing;
            }
        }
    }
}

pub fn run(binary: &Path, opts: &TaskOptions<'_>) {
    let tasks = get_task_list(binary, opts);
    let mut app = App::new(tasks, binary, opts);

    terminal::enable_raw_mode().expect("failed to enable raw mode");
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, cursor::Hide).expect("failed to setup terminal");
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).expect("failed to create terminal");

    let res = run_app(&mut terminal, &mut app);

    terminal::disable_raw_mode().expect("failed to disable raw mode");
    execute!(terminal.backend_mut(), LeaveAlternateScreen, cursor::Show)
        .expect("failed to restore terminal");

    if let Err(err) = res {
        eprintln!("{err:?}");
    }
}

fn run_app(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App,
) -> io::Result<()> {
    loop {
        terminal.draw(|f| ui(f, app))?;

        if let TuiState::Running = app.state {
            if let Some(rx) = &app.rx {
                while let Ok(event) = rx.try_recv() {
                    match event {
                        BackgroundEvent::Stdout(line) => {
                            app.output.push(Line::raw(line));
                        }
                        BackgroundEvent::Stderr(line) => {
                            app.output.push(Line::from(Span::styled(line, Style::default().fg(Color::Yellow))));
                        }
                        BackgroundEvent::Exit(code) => {
                            app.output.push(Line::raw(""));
                            let color = if code == 0 { Color::Green } else { Color::Red };
                            app.output.push(Line::from(Span::styled(
                                format!("Task exited with code {code}"),
                                Style::default().fg(color).add_modifier(Modifier::BOLD),
                            )));
                            app.output.push(Line::from(Span::styled(
                                "Press 'Esc' to return to task list",
                                Style::default().fg(Color::DarkGray),
                            )));
                        }
                    }
                }
            }
        }

        if event::poll(Duration::from_millis(50))? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press {
                    match app.state {
                        TuiState::Browsing => match key.code {
                            KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
                            KeyCode::Down | KeyCode::Char('j') => app.next(),
                            KeyCode::Up | KeyCode::Char('k') => app.previous(),
                            KeyCode::Enter => app.run_selected_task(),
                            _ => {}
                        },
                        TuiState::Running => match key.code {
                            KeyCode::Esc => {
                                app.state = TuiState::Browsing;
                                app.rx = None; // detach from background output
                            }
                            KeyCode::Char('q') => return Ok(()),
                            KeyCode::Down | KeyCode::Char('j') => {
                                app.scroll_offset = app.scroll_offset.saturating_add(1);
                            }
                            KeyCode::Up | KeyCode::Char('k') => {
                                app.scroll_offset = app.scroll_offset.saturating_sub(1);
                            }
                            _ => {}
                        },
                    }
                }
            }
        }
    }
}

fn ui(f: &mut ratatui::Frame, app: &mut App) {
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(30), Constraint::Percentage(70)])
        .split(f.area());

    let items: Vec<ListItem> = app
        .tasks
        .iter()
        .map(|t| ListItem::new(Line::from(t.name.as_str())))
        .collect();

    let list_title = Span::styled(
        " Tasks ",
        Style::default()
            .fg(Color::Rgb(204, 120, 50))
            .add_modifier(Modifier::BOLD),
    );
    let list_block = Block::default()
        .title(list_title)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray));

    let tasks_list = List::new(items)
        .block(list_block)
        .highlight_style(
            Style::default()
                .bg(Color::Rgb(204, 120, 50))
                .fg(Color::Black)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol(">> ");

    f.render_stateful_widget(tasks_list, chunks[0], &mut app.list_state);

    let main_pane = match app.state {
        TuiState::Browsing => {
            let mut info_lines = vec![Line::raw("")];
            if let Some(selected) = app.list_state.selected() {
                if let Some(task) = app.tasks.get(selected) {
                    info_lines.push(Line::from(Span::styled(
                        format!("Task: {}", task.name),
                        Style::default()
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::BOLD),
                    )));
                    info_lines.push(Line::raw(""));
                    info_lines.push(Line::from(Span::raw(&task.description)));
                }
            } else {
                info_lines.push(Line::from(Span::raw("No task selected.")));
            }

            info_lines.push(Line::raw(""));
            info_lines.push(Line::from(Span::styled(
                "Press 'Enter' to run the selected task.",
                Style::default().fg(Color::DarkGray),
            )));
            info_lines.push(Line::from(Span::styled(
                "Press 'q' or 'Esc' to exit.",
                Style::default().fg(Color::DarkGray),
            )));

            Paragraph::new(info_lines).block(
                Block::default()
                    .title(Span::styled(
                        " Info ",
                        Style::default()
                            .fg(Color::Rgb(204, 120, 50))
                            .add_modifier(Modifier::BOLD),
                    ))
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::DarkGray)),
            ).wrap(Wrap { trim: true })
        }
        TuiState::Running => Paragraph::new(app.output.clone())
            .block(
                Block::default()
                    .title(Span::styled(
                        " Output ",
                        Style::default()
                            .fg(Color::Rgb(204, 120, 50))
                            .add_modifier(Modifier::BOLD),
                    ))
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::DarkGray)),
            )
            .wrap(Wrap { trim: false })
            .scroll((app.scroll_offset, 0)),
    };

    f.render_widget(main_pane, chunks[1]);
}
