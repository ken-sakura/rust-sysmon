use color_eyre::Result;
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    symbols,
    text::{Line, Span},
    widgets::{
        Axis, Block, Borders, Chart, Dataset, Gauge, GraphType, Paragraph, Row, Table,
    },
    Terminal,
};
use sysinfo::{CpuRefreshKind, Disks, Networks, RefreshKind, System};
use std::{
    collections::VecDeque,
    io,
    time::{Duration, Instant},
};

#[derive(PartialEq)]
enum CurrentScreen {
    ResourceMonitor,
    ProcessList,
}

#[derive(PartialEq)]
enum InputMode {
    Normal,
    Command,
}

struct App {
    current_screen: CurrentScreen,
    input_mode: InputMode,
    command_input: String,
    sys: System,
    disks: Disks,
    networks: Networks,
    network_in_history: VecDeque<(f64, f64)>,
    network_out_history: VecDeque<(f64, f64)>,
    tick_count: u64,
}

impl App {
    fn new() -> Self {
        let mut sys = System::new_with_specifics(
            RefreshKind::new()
                .with_cpu(CpuRefreshKind::everything())
                // 修正箇所: 引数を追加
                .with_memory(sysinfo::MemoryRefreshKind::everything())
                .with_processes(sysinfo::ProcessRefreshKind::everything()),
        );
        sys.refresh_all();
        let disks = Disks::new_with_refreshed_list();
        let networks = Networks::new_with_refreshed_list();

        Self {
            current_screen: CurrentScreen::ResourceMonitor,
            input_mode: InputMode::Normal,
            command_input: String::new(),
            sys,
            disks,
            networks,
            network_in_history: VecDeque::from(vec![(0.0, 0.0); 40]),
            network_out_history: VecDeque::from(vec![(0.0, 0.0); 40]),
            tick_count: 0,
        }
    }

    fn on_tick(&mut self) {
        self.sys.refresh_cpu();
        self.sys.refresh_memory();
        self.sys.refresh_processes();
        self.disks.refresh();
        self.networks.refresh();

        self.tick_count += 1;
        let x = self.tick_count as f64;

        let mut total_rx = 0;
        let mut total_tx = 0;
        for (_interface_name, network) in &self.networks {
            total_rx += network.received();
            total_tx += network.transmitted();
        }

        if self.network_in_history.len() >= 40 {
            self.network_in_history.pop_front();
        }
        self.network_in_history.push_back((x, total_rx as f64 / 1024.0)); // KB

        if self.network_out_history.len() >= 40 {
            self.network_out_history.pop_front();
        }
        self.network_out_history.push_back((x, total_tx as f64 / 1024.0)); // KB
    }
}

fn main() -> Result<()> {
    color_eyre::install()?;
    
    // Terminal setup
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut app = App::new();
    let tick_rate = Duration::from_millis(500);
    let mut last_tick = Instant::now();

    loop {
        terminal.draw(|f| ui(f, &mut app))?;

        let timeout = tick_rate
            .checked_sub(last_tick.elapsed())
            .unwrap_or_else(|| Duration::from_secs(0));

        if crossterm::event::poll(timeout)? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press {
                    match app.input_mode {
                        InputMode::Normal => match key.code {
                            KeyCode::Char(':') => {
                                app.input_mode = InputMode::Command;
                                app.command_input.clear();
                                app.command_input.push(':');
                            }
                            _ => {}
                        },
                        InputMode::Command => match key.code {
                            KeyCode::Enter => {
                                let cmd = app.command_input.trim();
                                match cmd {
                                    ":re" => app.current_screen = CurrentScreen::ResourceMonitor,
                                    ":ps" => app.current_screen = CurrentScreen::ProcessList,
                                    ":q" => break,
                                    _ => {}
                                }
                                app.input_mode = InputMode::Normal;
                                app.command_input.clear();
                            }
                            KeyCode::Esc => {
                                app.input_mode = InputMode::Normal;
                                app.command_input.clear();
                            }
                            KeyCode::Char(c) => {
                                app.command_input.push(c);
                            }
                            KeyCode::Backspace => {
                                app.command_input.pop();
                                if app.command_input.is_empty() {
                                    app.input_mode = InputMode::Normal;
                                }
                            }
                            _ => {}
                        },
                    }
                }
            }
        }

        if last_tick.elapsed() >= tick_rate {
            app.on_tick();
            last_tick = Instant::now();
        }
    }

    // Restore terminal
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    Ok(())
}

fn ui(f: &mut ratatui::Frame, app: &mut App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(3)])
        .split(f.size());

    match app.current_screen {
        CurrentScreen::ResourceMonitor => render_resource_monitor(f, app, chunks[0]),
        CurrentScreen::ProcessList => render_process_list(f, app, chunks[0]),
    }

    render_command_bar(f, app, chunks[1]);
}

fn render_resource_monitor(f: &mut ratatui::Frame, app: &App, area: Rect) {
    let grid = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(area);

    let top_row = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(grid[0]);

    let bottom_row = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(grid[1]);

    // 1. CPU Usage
    let cpus = app.sys.cpus();
    let cpu_lines: Vec<Line> = cpus
        .iter()
        .enumerate()
        .take(8) // Limit to first 8 cores for layout
        .map(|(i, cpu)| {
            let usage = cpu.cpu_usage();
            let bars = (usage / 5.0) as usize;
            let bar_str = "█".repeat(bars) + &"░".repeat(20 - bars.min(20));
            Line::from(vec![
                Span::styled(format!("Core {:2}: ", i), Style::default().fg(Color::Cyan)),
                Span::styled(format!("[{:>5.1}%] ", usage), Style::default().fg(Color::Yellow)),
                Span::styled(bar_str, Style::default().fg(Color::Green)),
            ])
        })
        .collect();

    let cpu_block = Paragraph::new(cpu_lines).block(
        Block::default()
            .title(" 1. CPU Usage (Per Core) ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Cyan)),
    );
    f.render_widget(cpu_block, top_row[0]);

    // 2. Memory Usage
    let total_mem = app.sys.total_memory() as f64 / 1024.0 / 1024.0; // GB
    let used_mem = app.sys.used_memory() as f64 / 1024.0 / 1024.0; // GB
    let mem_ratio = if total_mem > 0.0 { used_mem / total_mem } else { 0.0 };

    let gauge = Gauge::default()
        .block(
            Block::default()
                .title(" 2. Memory Usage ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Magenta)),
        )
        .gauge_style(
            Style::default()
                .fg(Color::Magenta)
                .bg(Color::Black)
                .add_modifier(Modifier::BOLD),
        )
        .ratio(mem_ratio.clamp(0.0, 1.0))
        .label(format!("{:.2} GB / {:.2} GB ({:.1}%)", used_mem, total_mem, mem_ratio * 100.0));

    f.render_widget(gauge, top_row[1]);

    // 3. Disk Space
    let disk_lines: Vec<Line> = app
        .disks
        .iter()
        .map(|disk| {
            let total = disk.total_space() as f64 / 1024.0 / 1024.0 / 1024.0;
            let available = disk.available_space() as f64 / 1024.0 / 1024.0 / 1024.0;
            let used = total - available;
            let pct = if total > 0.0 { (used / total) * 100.0 } else { 0.0 };
            Line::from(vec![
                Span::styled(
                    format!("{}: ", disk.mount_point().to_string_lossy()),
                    Style::default().fg(Color::Yellow),
                ),
                Span::raw(format!("{:.1} GB / {:.1} GB ({:.0}%)", used, total, pct)),
            ])
        })
        .collect();

    let disk_block = Paragraph::new(disk_lines).block(
        Block::default()
            .title(" 3. Disk Space ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Yellow)),
    );
    f.render_widget(disk_block, bottom_row[0]);

    // 4. Network Traffic
    let in_data: Vec<(f64, f64)> = app.network_in_history.iter().copied().collect();
    let out_data: Vec<(f64, f64)> = app.network_out_history.iter().copied().collect();

    let min_x = app.tick_count.saturating_sub(40) as f64;
    let max_x = app.tick_count as f64;

    let datasets = vec![
        Dataset::default()
            .name("IN (KB)")
            .marker(symbols::Marker::Braille)
            .graph_type(GraphType::Line)
            .style(Style::default().fg(Color::Green))
            .data(&in_data),
        Dataset::default()
            .name("OUT (KB)")
            .marker(symbols::Marker::Braille)
            .graph_type(GraphType::Line)
            .style(Style::default().fg(Color::LightRed))
            .data(&out_data),
    ];

    let chart = Chart::new(datasets)
        .block(
            Block::default()
                .title(" 4. Network Traffic ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Green)),
        )
        .x_axis(Axis::default().bounds([min_x, max_x]))
        .y_axis(Axis::default().bounds([0.0, 500.0]));

    f.render_widget(chart, bottom_row[1]);
}

fn render_process_list(f: &mut ratatui::Frame, app: &App, area: Rect) {
    let mut processes: Vec<_> = app.sys.processes().values().collect();
    processes.sort_by(|a, b| b.cpu_usage().partial_cmp(&a.cpu_usage()).unwrap_or(std::cmp::Ordering::Equal));

    let rows: Vec<Row> = processes
        .iter()
        .take(25)
        .map(|p| {
            let pid = p.pid().to_string();
            let name = p.name().to_string();
            let cpu = format!("{:.1}%", p.cpu_usage());
            let mem = format!("{:.1} MB", p.memory() as f64 / 1024.0 / 1024.0);

            Row::new(vec![pid, name, cpu, mem]).style(Style::default().fg(Color::White))
        })
        .collect();

    let table = Table::new(
        rows,
        [
            Constraint::Length(10),
            Constraint::Percentage(40),
            Constraint::Percentage(25),
            Constraint::Percentage(25),
        ],
    )
    .header(
        Row::new(vec!["PID", "Name", "CPU %", "Memory"])
            .style(Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
    )
    .block(
        Block::default()
            .title(" Process List (Sorted by CPU Usage) ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Cyan)),
    );

    f.render_widget(table, area);
}

fn render_command_bar(f: &mut ratatui::Frame, app: &App, area: Rect) {
    let text = match app.input_mode {
        InputMode::Normal => Line::from(vec![
            Span::styled("NORMAL MODE ", Style::default().bg(Color::Blue).fg(Color::Black).add_modifier(Modifier::BOLD)),
            Span::raw(" Press "),
            Span::styled(":", Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
            Span::raw(" to enter command mode (Commands: :re, :ps, :q)"),
        ]),
        InputMode::Command => Line::from(vec![
            Span::styled("COMMAND MODE ", Style::default().bg(Color::Yellow).fg(Color::Black).add_modifier(Modifier::BOLD)),
            Span::raw(" "),
            Span::styled(&app.command_input, Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)),
        ]),
    };

    let p = Paragraph::new(text).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::DarkGray)),
    );
    f.render_widget(p, area);
}
