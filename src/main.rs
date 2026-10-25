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
        Axis, Block, Borders, Chart, Clear, Dataset, Gauge, GraphType, Paragraph, Row, Table,
        TableState,
    },
    Terminal,
};
use std::{
    collections::VecDeque,
    io,
    panic,
    time::{Duration, Instant},
};
use sysinfo::{
    CpuRefreshKind, DiskRefreshKind, Disks, MemoryRefreshKind, Networks, ProcessRefreshKind,
    ProcessesToUpdate, RefreshKind, System,
};

const HISTORY_LEN: usize = 40;
const MAX_COMMAND_LEN: usize = 64;
const BYTES_PER_GB: f64 = 1024.0 * 1024.0 * 1024.0;
const BYTES_PER_MB: f64 = 1024.0 * 1024.0;
const BYTES_PER_KB: f64 = 1024.0;

#[derive(PartialEq, Clone, Copy, Debug)]
enum CurrentScreen {
    ResourceMonitor,
    ProcessList,
}

#[derive(PartialEq, Clone, Copy, Debug)]
enum InputMode {
    Normal,
    Command,
    Filter,
}

#[derive(PartialEq, Clone, Copy, Debug)]
enum SortColumn {
    Cpu,
    Memory,
    Pid,
    Name,
}

impl SortColumn {
    fn next(self) -> Self {
        match self {
            Self::Cpu => Self::Memory,
            Self::Memory => Self::Pid,
            Self::Pid => Self::Name,
            Self::Name => Self::Cpu,
        }
    }
}

/// RAII ガード: パニック時や早期リターン時にも必ずターミナル状態を復元する
struct TerminalGuard;

impl TerminalGuard {
    fn init() -> Result<(Self, Terminal<CrosstermBackend<io::Stdout>>)> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        if let Err(e) = execute!(stdout, EnterAlternateScreen) {
            let _ = disable_raw_mode();
            return Err(e.into());
        }
        let backend = CrosstermBackend::new(stdout);
        match Terminal::new(backend) {
            Ok(terminal) => Ok((Self, terminal)),
            Err(e) => {
                let _ = execute!(io::stdout(), LeaveAlternateScreen);
                let _ = disable_raw_mode();
                Err(e.into())
            }
        }
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
    }
}

fn install_hooks() -> Result<()> {
    let (panic_hook, eyre_hook) = color_eyre::config::HookBuilder::default().into_hooks();
    eyre_hook.install()?;
    panic::set_hook(Box::new(move |panic_info| {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
        eprintln!("{}", panic_hook.panic_report(panic_info));
    }));
    Ok(())
}

struct ProcessItem {
    pid: u32,
    name: String,
    cpu_usage: f32,
    memory_bytes: u64,
}

struct App {
    current_screen: CurrentScreen,
    input_mode: InputMode,
    command_input: String,
    filter_query: String,
    status_message: Option<String>,
    show_help: bool,
    sort_column: SortColumn,
    sort_ascending: bool,
    table_state: TableState,
    sys: System,
    disks: Disks,
    networks: Networks,
    network_in_history: VecDeque<(f64, f64)>,
    network_out_history: VecDeque<(f64, f64)>,
    last_rx_kb: f64,
    last_tx_kb: f64,
    tick_count: u64,
}

impl App {
    fn new() -> Self {
        let mut sys = System::new_with_specifics(
            RefreshKind::nothing()
                .with_cpu(CpuRefreshKind::everything())
                .with_memory(MemoryRefreshKind::everything())
                .with_processes(ProcessRefreshKind::everything()),
        );
        sys.refresh_all();
        let disks = Disks::new_with_refreshed_list();
        let networks = Networks::new_with_refreshed_list();

        let mut table_state = TableState::default();
        table_state.select(Some(0));

        Self {
            current_screen: CurrentScreen::ResourceMonitor,
            input_mode: InputMode::Normal,
            command_input: String::new(),
            filter_query: String::new(),
            status_message: None,
            show_help: false,
            sort_column: SortColumn::Cpu,
            sort_ascending: false,
            table_state,
            sys,
            disks,
            networks,
            network_in_history: VecDeque::from(vec![(0.0, 0.0); HISTORY_LEN]),
            network_out_history: VecDeque::from(vec![(0.0, 0.0); HISTORY_LEN]),
            last_rx_kb: 0.0,
            last_tx_kb: 0.0,
            tick_count: 0,
        }
    }

    fn on_tick(&mut self) {
        self.sys.refresh_cpu_usage();
        self.sys.refresh_memory();

        // プロセス一覧画面のとき、または4ティックに1回だけプロセスを更新して負荷を抑える
        if self.current_screen == CurrentScreen::ProcessList || self.tick_count.is_multiple_of(4) {
            self.sys.refresh_processes_specifics(
                ProcessesToUpdate::All,
                true,
                ProcessRefreshKind::nothing().with_cpu().with_memory(),
            );
        }

        // ディスク情報の更新頻度を抑制 (4ティック=2秒に1回)
        if self.tick_count.is_multiple_of(4) {
            self.disks
                .refresh_specifics(true, DiskRefreshKind::nothing().with_storage());
        }

        self.networks.refresh(true);

        self.tick_count = self.tick_count.saturating_add(1);
        let x = self.tick_count as f64;

        let mut total_rx: u64 = 0;
        let mut total_tx: u64 = 0;
        for (_interface_name, network) in &self.networks {
            total_rx = total_rx.saturating_add(network.received());
            total_tx = total_tx.saturating_add(network.transmitted());
        }

        self.last_rx_kb = total_rx as f64 / BYTES_PER_KB;
        self.last_tx_kb = total_tx as f64 / BYTES_PER_KB;

        if self.network_in_history.len() >= HISTORY_LEN {
            self.network_in_history.pop_front();
        }
        self.network_in_history.push_back((x, self.last_rx_kb));

        if self.network_out_history.len() >= HISTORY_LEN {
            self.network_out_history.pop_front();
        }
        self.network_out_history.push_back((x, self.last_tx_kb));
    }

    fn filtered_sorted_processes(&self) -> Vec<ProcessItem> {
        let query = self.filter_query.trim().to_lowercase();
        let mut items: Vec<ProcessItem> = self
            .sys
            .processes()
            .values()
            .filter_map(|p| {
                let pid = p.pid().as_u32();
                let name = p.name().to_string_lossy().into_owned();
                if !query.is_empty()
                    && !name.to_lowercase().contains(&query)
                    && !pid.to_string().contains(&query)
                {
                    return None;
                }
                let cpu_usage = if p.cpu_usage().is_finite() {
                    p.cpu_usage().max(0.0)
                } else {
                    0.0
                };
                Some(ProcessItem {
                    pid,
                    name,
                    cpu_usage,
                    memory_bytes: p.memory(),
                })
            })
            .collect();

        items.sort_by(|a, b| {
            let cmp = match self.sort_column {
                SortColumn::Cpu => a
                    .cpu_usage
                    .partial_cmp(&b.cpu_usage)
                    .unwrap_or(std::cmp::Ordering::Equal),
                SortColumn::Memory => a.memory_bytes.cmp(&b.memory_bytes),
                SortColumn::Pid => a.pid.cmp(&b.pid),
                SortColumn::Name => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
            };
            if self.sort_ascending {
                cmp
            } else {
                cmp.reverse()
            }
        });

        items
    }

    fn move_selection(&mut self, delta: isize, total: usize) {
        if total == 0 {
            self.table_state.select(None);
            return;
        }
        let current = self.table_state.selected().unwrap_or(0);
        let next = if delta < 0 {
            current.saturating_sub(delta.unsigned_abs())
        } else {
            current.saturating_add(delta as usize)
        }
        .min(total.saturating_sub(1));
        self.table_state.select(Some(next));
    }

    fn execute_command(&mut self) -> bool {
        let cmd = self.command_input.trim();
        self.status_message = None;

        if cmd == ":re" || cmd == ":resource" {
            self.current_screen = CurrentScreen::ResourceMonitor;
        } else if cmd == ":ps" || cmd == ":proc" {
            self.current_screen = CurrentScreen::ProcessList;
            self.sys.refresh_processes_specifics(
                ProcessesToUpdate::All,
                true,
                ProcessRefreshKind::nothing().with_cpu().with_memory(),
            );
        } else if cmd == ":q" || cmd == ":quit" {
            return true;
        } else if cmd == ":help" || cmd == ":h" {
            self.show_help = !self.show_help;
        } else if cmd == ":clear" {
            self.filter_query.clear();
            self.table_state.select(Some(0));
            self.status_message = Some("Filter cleared".to_string());
        } else if let Some(rest) = cmd.strip_prefix(":sort ") {
            match rest.trim() {
                "cpu" => {
                    self.sort_column = SortColumn::Cpu;
                    self.sort_ascending = false;
                }
                "mem" | "memory" => {
                    self.sort_column = SortColumn::Memory;
                    self.sort_ascending = false;
                }
                "pid" => {
                    self.sort_column = SortColumn::Pid;
                    self.sort_ascending = true;
                }
                "name" => {
                    self.sort_column = SortColumn::Name;
                    self.sort_ascending = true;
                }
                other => {
                    self.status_message =
                        Some(format!("Unknown sort key '{other}' (use cpu|mem|pid|name)"));
                }
            }
        } else if let Some(rest) = cmd.strip_prefix(":filter ") {
            self.filter_query = rest.trim().to_string();
            self.current_screen = CurrentScreen::ProcessList;
            self.table_state.select(Some(0));
        } else {
            self.status_message = Some(format!(
                "Unknown command: {cmd} (Try :re, :ps, :sort <key>, :filter <str>, :help, :q)"
            ));
        }
        false
    }
}

fn main() -> Result<()> {
    install_hooks()?;
    let (_guard, mut terminal) = TerminalGuard::init()?;

    let mut app = App::new();
    let tick_rate = Duration::from_millis(500);
    let mut last_tick = Instant::now();

    loop {
        terminal.draw(|f| ui(f, &mut app))?;

        let timeout = tick_rate
            .checked_sub(last_tick.elapsed())
            .unwrap_or(Duration::ZERO);

        if crossterm::event::poll(timeout)? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press && handle_key_event(&mut app, key.code) {
                    break;
                }
            }
        }

        if last_tick.elapsed() >= tick_rate {
            app.on_tick();
            last_tick = Instant::now();
        }
    }

    terminal.show_cursor()?;
    Ok(())
}

fn handle_key_event(app: &mut App, code: KeyCode) -> bool {
    match app.input_mode {
        InputMode::Normal => {
            if app.show_help {
                match code {
                    KeyCode::Esc | KeyCode::Char('?') | KeyCode::Enter | KeyCode::Char('q') => {
                        app.show_help = false;
                    }
                    _ => {}
                }
                return false;
            }

            match code {
                KeyCode::Char(':') => {
                    app.input_mode = InputMode::Command;
                    app.command_input.clear();
                    app.command_input.push(':');
                    app.status_message = None;
                }
                KeyCode::Char('/') => {
                    app.current_screen = CurrentScreen::ProcessList;
                    app.input_mode = InputMode::Filter;
                    app.status_message = None;
                }
                KeyCode::Char('q') => return true,
                KeyCode::Char('?') => app.show_help = !app.show_help,
                KeyCode::Tab => {
                    app.current_screen = match app.current_screen {
                        CurrentScreen::ResourceMonitor => {
                            app.sys.refresh_processes_specifics(
                                ProcessesToUpdate::All,
                                true,
                                ProcessRefreshKind::nothing().with_cpu().with_memory(),
                            );
                            CurrentScreen::ProcessList
                        }
                        CurrentScreen::ProcessList => CurrentScreen::ResourceMonitor,
                    };
                }
                KeyCode::Char('1') => app.current_screen = CurrentScreen::ResourceMonitor,
                KeyCode::Char('2') => {
                    app.current_screen = CurrentScreen::ProcessList;
                    app.sys.refresh_processes_specifics(
                        ProcessesToUpdate::All,
                        true,
                        ProcessRefreshKind::nothing().with_cpu().with_memory(),
                    );
                }
                KeyCode::Down | KeyCode::Char('j')
                    if app.current_screen == CurrentScreen::ProcessList =>
                {
                    let len = app.filtered_sorted_processes().len();
                    app.move_selection(1, len);
                }
                KeyCode::Up | KeyCode::Char('k')
                    if app.current_screen == CurrentScreen::ProcessList =>
                {
                    let len = app.filtered_sorted_processes().len();
                    app.move_selection(-1, len);
                }
                KeyCode::PageDown if app.current_screen == CurrentScreen::ProcessList => {
                    let len = app.filtered_sorted_processes().len();
                    app.move_selection(10, len);
                }
                KeyCode::PageUp if app.current_screen == CurrentScreen::ProcessList => {
                    let len = app.filtered_sorted_processes().len();
                    app.move_selection(-10, len);
                }
                KeyCode::Home | KeyCode::Char('g')
                    if app.current_screen == CurrentScreen::ProcessList =>
                {
                    let len = app.filtered_sorted_processes().len();
                    if len > 0 {
                        app.table_state.select(Some(0));
                    }
                }
                KeyCode::End | KeyCode::Char('G')
                    if app.current_screen == CurrentScreen::ProcessList =>
                {
                    let len = app.filtered_sorted_processes().len();
                    if len > 0 {
                        app.table_state.select(Some(len - 1));
                    }
                }
                KeyCode::Char('s') if app.current_screen == CurrentScreen::ProcessList => {
                    app.sort_column = app.sort_column.next();
                }
                KeyCode::Char('c') if app.current_screen == CurrentScreen::ProcessList => {
                    app.sort_column = SortColumn::Cpu;
                    app.sort_ascending = false;
                }
                KeyCode::Char('m') if app.current_screen == CurrentScreen::ProcessList => {
                    app.sort_column = SortColumn::Memory;
                    app.sort_ascending = false;
                }
                KeyCode::Char('p') if app.current_screen == CurrentScreen::ProcessList => {
                    app.sort_column = SortColumn::Pid;
                    app.sort_ascending = true;
                }
                KeyCode::Char('n') if app.current_screen == CurrentScreen::ProcessList => {
                    app.sort_column = SortColumn::Name;
                    app.sort_ascending = true;
                }
                KeyCode::Char('r') if app.current_screen == CurrentScreen::ProcessList => {
                    app.sort_ascending = !app.sort_ascending;
                }
                KeyCode::Esc if !app.filter_query.is_empty() => {
                    app.filter_query.clear();
                    app.table_state.select(Some(0));
                }
                _ => {}
            }
        }
        InputMode::Command => match code {
            KeyCode::Enter => {
                let should_quit = app.execute_command();
                app.input_mode = InputMode::Normal;
                app.command_input.clear();
                if should_quit {
                    return true;
                }
            }
            KeyCode::Esc => {
                app.input_mode = InputMode::Normal;
                app.command_input.clear();
            }
            KeyCode::Char(c) => {
                if app.command_input.chars().count() < MAX_COMMAND_LEN {
                    app.command_input.push(c);
                }
            }
            KeyCode::Backspace => {
                app.command_input.pop();
                if app.command_input.is_empty() {
                    app.input_mode = InputMode::Normal;
                }
            }
            _ => {}
        },
        InputMode::Filter => match code {
            KeyCode::Enter | KeyCode::Esc => {
                app.input_mode = InputMode::Normal;
            }
            KeyCode::Char(c) => {
                if app.filter_query.chars().count() < MAX_COMMAND_LEN {
                    app.filter_query.push(c);
                    app.table_state.select(Some(0));
                }
            }
            KeyCode::Backspace => {
                app.filter_query.pop();
                app.table_state.select(Some(0));
            }
            _ => {}
        },
    }
    false
}

fn format_cpu_bar(usage: f32, width: usize) -> String {
    let clamped = if usage.is_finite() {
        usage.clamp(0.0, 100.0)
    } else {
        0.0
    };
    let filled = ((clamped / 100.0) * width as f32).round() as usize;
    let filled = filled.min(width);
    "█".repeat(filled) + &"░".repeat(width - filled)
}

fn ui(f: &mut ratatui::Frame, app: &mut App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(3)])
        .split(f.area());

    match app.current_screen {
        CurrentScreen::ResourceMonitor => render_resource_monitor(f, app, chunks[0]),
        CurrentScreen::ProcessList => render_process_list(f, app, chunks[0]),
    }

    render_command_bar(f, app, chunks[1]);

    if app.show_help {
        render_help_popup(f, f.area());
    }
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

    // 1. CPU Usage (全コア平均 + マルチカラム対応)
    let cpus = app.sys.cpus();
    let global_cpu = app.sys.global_cpu_usage();
    let max_rows = top_row[0].height.saturating_sub(2) as usize;
    let per_col = max_rows.max(1);

    let cpu_lines: Vec<Line> = (0..per_col.min(cpus.len()))
        .map(|row_idx| {
            let mut spans = Vec::new();
            let cols = cpus.len().div_ceil(per_col);
            let bar_width = if cols > 1 { 10 } else { 20 };

            for col_idx in 0..cols {
                let cpu_idx = col_idx * per_col + row_idx;
                if let Some(cpu) = cpus.get(cpu_idx) {
                    if col_idx > 0 {
                        spans.push(Span::raw("  "));
                    }
                    let usage = if cpu.cpu_usage().is_finite() {
                        cpu.cpu_usage().clamp(0.0, 100.0)
                    } else {
                        0.0
                    };
                    let bar_color = if usage >= 80.0 {
                        Color::Red
                    } else if usage >= 50.0 {
                        Color::Yellow
                    } else {
                        Color::Green
                    };
                    spans.push(Span::styled(
                        format!("C{:<2}:", cpu_idx),
                        Style::default().fg(Color::Cyan),
                    ));
                    spans.push(Span::styled(
                        format!("{:>5.1}% ", usage),
                        Style::default().fg(Color::Yellow),
                    ));
                    spans.push(Span::styled(
                        format_cpu_bar(usage, bar_width),
                        Style::default().fg(bar_color),
                    ));
                }
            }
            Line::from(spans)
        })
        .collect();

    let cpu_block = Paragraph::new(cpu_lines).block(
        Block::default()
            .title(format!(
                " 1. CPU Usage ({} Cores | Avg: {:.1}%) ",
                cpus.len(),
                global_cpu
            ))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Cyan)),
    );
    f.render_widget(cpu_block, top_row[0]);

    // 2. Memory & Swap Usage (バイトからGBへ正確に換算)
    let mem_block = Block::default()
        .title(" 2. Memory & Swap Usage ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Magenta));
    let mem_inner = mem_block.inner(top_row[1]);
    f.render_widget(mem_block, top_row[1]);

    let mem_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(mem_inner);

    let total_mem = app.sys.total_memory() as f64 / BYTES_PER_GB;
    let used_mem = app.sys.used_memory() as f64 / BYTES_PER_GB;
    let mem_ratio = if total_mem > 0.0 {
        (used_mem / total_mem).clamp(0.0, 1.0)
    } else {
        0.0
    };

    let ram_gauge = Gauge::default()
        .block(Block::default().title("RAM"))
        .gauge_style(
            Style::default()
                .fg(Color::Magenta)
                .bg(Color::Black)
                .add_modifier(Modifier::BOLD),
        )
        .ratio(mem_ratio)
        .label(format!(
            "{:.2} GB / {:.2} GB ({:.1}%)",
            used_mem,
            total_mem,
            mem_ratio * 100.0
        ));
    f.render_widget(ram_gauge, mem_chunks[0]);

    let total_swap = app.sys.total_swap() as f64 / BYTES_PER_GB;
    let used_swap = app.sys.used_swap() as f64 / BYTES_PER_GB;
    let swap_ratio = if total_swap > 0.0 {
        (used_swap / total_swap).clamp(0.0, 1.0)
    } else {
        0.0
    };

    let swap_gauge = Gauge::default()
        .block(Block::default().title("Swap"))
        .gauge_style(
            Style::default()
                .fg(Color::LightBlue)
                .bg(Color::Black)
                .add_modifier(Modifier::BOLD),
        )
        .ratio(swap_ratio)
        .label(format!(
            "{:.2} GB / {:.2} GB ({:.1}%)",
            used_swap,
            total_swap,
            swap_ratio * 100.0
        ));
    f.render_widget(swap_gauge, mem_chunks[1]);

    // 3. Disk Space
    let disk_lines: Vec<Line> = app
        .disks
        .iter()
        .map(|disk| {
            let total = disk.total_space() as f64 / BYTES_PER_GB;
            let available = disk.available_space() as f64 / BYTES_PER_GB;
            let used = (total - available).max(0.0);
            let pct = if total > 0.0 {
                ((used / total) * 100.0).clamp(0.0, 100.0)
            } else {
                0.0
            };
            let bar = format_cpu_bar(pct as f32, 10);
            Line::from(vec![
                Span::styled(
                    format!("{:<12} ", disk.mount_point().to_string_lossy()),
                    Style::default().fg(Color::Yellow),
                ),
                Span::styled(format!("{bar} "), Style::default().fg(Color::Yellow)),
                Span::raw(format!("{:.1} / {:.1} GB ({:.0}%)", used, total, pct)),
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

    // 4. Network Traffic (動的Y軸スケール)
    let in_data: Vec<(f64, f64)> = app.network_in_history.iter().copied().collect();
    let out_data: Vec<(f64, f64)> = app.network_out_history.iter().copied().collect();

    let min_x = app.tick_count.saturating_sub(HISTORY_LEN as u64) as f64;
    let max_x = app.tick_count.max(1) as f64;

    let max_observed = in_data
        .iter()
        .chain(out_data.iter())
        .map(|&(_, y)| y)
        .fold(0.0_f64, f64::max);
    let max_y = (max_observed * 1.2).max(10.0);

    let datasets = vec![
        Dataset::default()
            .name(format!("IN: {:.1} KB", app.last_rx_kb))
            .marker(symbols::Marker::Braille)
            .graph_type(GraphType::Line)
            .style(Style::default().fg(Color::Green))
            .data(&in_data),
        Dataset::default()
            .name(format!("OUT: {:.1} KB", app.last_tx_kb))
            .marker(symbols::Marker::Braille)
            .graph_type(GraphType::Line)
            .style(Style::default().fg(Color::LightRed))
            .data(&out_data),
    ];

    let chart = Chart::new(datasets)
        .block(
            Block::default()
                .title(format!(
                    " 4. Network Traffic (Max scale: {:.0} KB) ",
                    max_y
                ))
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Green)),
        )
        .x_axis(Axis::default().bounds([min_x, max_x]))
        .y_axis(
            Axis::default()
                .bounds([0.0, max_y])
                .labels(vec![
                    Span::raw("0"),
                    Span::raw(format!("{:.0}KB", max_y / 2.0)),
                    Span::raw(format!("{:.0}KB", max_y)),
                ]),
        );

    f.render_widget(chart, bottom_row[1]);
}

fn render_process_list(f: &mut ratatui::Frame, app: &mut App, area: Rect) {
    let processes = app.filtered_sorted_processes();
    let total_count = processes.len();

    if total_count == 0 {
        app.table_state.select(None);
    } else if app
        .table_state
        .selected()
        .is_none_or(|sel| sel >= total_count)
    {
        app.table_state.select(Some(total_count.saturating_sub(1)));
    }

    let arrow = if app.sort_ascending { "▲" } else { "▼" };
    let header_label = |col: SortColumn, name: &str| -> String {
        if app.sort_column == col {
            format!("{name} {arrow}")
        } else {
            name.to_string()
        }
    };

    let rows: Vec<Row> = processes
        .iter()
        .map(|p| {
            let pid = p.pid.to_string();
            let name = p.name.clone();
            let cpu = format!("{:.1}%", p.cpu_usage);
            let mem = format!("{:.1} MB", p.memory_bytes as f64 / BYTES_PER_MB);

            Row::new(vec![pid, name, cpu, mem]).style(Style::default().fg(Color::White))
        })
        .collect();

    let filter_info = if app.filter_query.is_empty() {
        String::new()
    } else {
        format!(" | Filter: \"{}\" ", app.filter_query)
    };

    let title = format!(
        " Process List ({total_count} procs{filter_info} | j/k: Move, s/c/m/p/n: Sort, r: Reverse, /: Filter) "
    );

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
        Row::new(vec![
            header_label(SortColumn::Pid, "PID"),
            header_label(SortColumn::Name, "Name"),
            header_label(SortColumn::Cpu, "CPU %"),
            header_label(SortColumn::Memory, "Memory"),
        ])
        .style(
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
    )
    .row_highlight_style(
        Style::default()
            .bg(Color::DarkGray)
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD),
    )
    .highlight_symbol("▶ ")
    .block(
        Block::default()
            .title(title)
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Cyan)),
    );

    f.render_stateful_widget(table, area, &mut app.table_state);
}

fn render_command_bar(f: &mut ratatui::Frame, app: &App, area: Rect) {
    let text = match app.input_mode {
        InputMode::Normal => {
            let mut spans = vec![
                Span::styled(
                    " NORMAL ",
                    Style::default()
                        .bg(Color::Blue)
                        .fg(Color::Black)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(" "),
                Span::styled("Tab/1/2", Style::default().fg(Color::Cyan)),
                Span::raw(":Screen  "),
                Span::styled(":", Style::default().fg(Color::Yellow)),
                Span::raw(":Cmd(:re/:ps/:q)  "),
                Span::styled("/", Style::default().fg(Color::Green)),
                Span::raw(":Filter  "),
                Span::styled("?", Style::default().fg(Color::Magenta)),
                Span::raw(":Help  "),
                Span::styled("q", Style::default().fg(Color::Red)),
                Span::raw(":Quit"),
            ];
            if let Some(msg) = &app.status_message {
                spans.push(Span::raw(" | "));
                spans.push(Span::styled(msg, Style::default().fg(Color::LightRed)));
            }
            Line::from(spans)
        }
        InputMode::Command => Line::from(vec![
            Span::styled(
                " COMMAND ",
                Style::default()
                    .bg(Color::Yellow)
                    .fg(Color::Black)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" "),
            Span::styled(
                &app.command_input,
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
        InputMode::Filter => Line::from(vec![
            Span::styled(
                " FILTER ",
                Style::default()
                    .bg(Color::Green)
                    .fg(Color::Black)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" /"),
            Span::styled(
                &app.filter_query,
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" (Enter/Esc to finish)"),
        ]),
    };

    let p = Paragraph::new(text).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::DarkGray)),
    );
    f.render_widget(p, area);
}

fn render_help_popup(f: &mut ratatui::Frame, area: Rect) {
    let popup_width = area.width.min(64);
    let popup_height = area.height.min(18);
    let popup_area = Rect {
        x: area.x + (area.width.saturating_sub(popup_width)) / 2,
        y: area.y + (area.height.saturating_sub(popup_height)) / 2,
        width: popup_width,
        height: popup_height,
    };

    let help_lines = vec![
        Line::from(Span::styled(
            " Keybindings & Commands ",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from("  Tab / 1 / 2   : Switch Resource Monitor / Process List"),
        Line::from("  q or :q       : Quit application"),
        Line::from("  :re / :ps     : Switch screen via command mode"),
        Line::from("  / or :filter  : Filter processes by name or PID"),
        Line::from("  Esc           : Clear active filter / cancel input"),
        Line::from("  j / k / ↑ / ↓ : Move process selection up / down"),
        Line::from("  PgUp / PgDn   : Page up / down in process list"),
        Line::from("  g / G         : Jump to top / bottom of process list"),
        Line::from("  c / m / p / n : Sort by CPU / Memory / PID / Name"),
        Line::from("  s / r         : Cycle sort column / Reverse sort order"),
        Line::from("  ? or :help    : Toggle this help dialog"),
    ];

    f.render_widget(Clear, popup_area);
    let block = Paragraph::new(help_lines).block(
        Block::default()
            .title(" Help (Press ? or Esc to close) ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Cyan)),
    );
    f.render_widget(block, popup_area);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_cpu_bar_bounds() {
        assert_eq!(format_cpu_bar(0.0, 20).chars().count(), 20);
        assert_eq!(format_cpu_bar(100.0, 20).chars().count(), 20);
        // Should not panic or exceed 20 chars when usage > 100% or negative or NaN
        assert_eq!(format_cpu_bar(250.0, 20).chars().count(), 20);
        assert_eq!(format_cpu_bar(-10.0, 20).chars().count(), 20);
        assert_eq!(format_cpu_bar(f32::NAN, 20).chars().count(), 20);
    }

    #[test]
    fn test_command_execution_and_navigation() {
        let mut app = App::new();
        assert_eq!(app.current_screen, CurrentScreen::ResourceMonitor);

        handle_key_event(&mut app, KeyCode::Char(':'));
        assert_eq!(app.input_mode, InputMode::Command);
        for c in "ps".chars() {
            handle_key_event(&mut app, KeyCode::Char(c));
        }
        let quit = handle_key_event(&mut app, KeyCode::Enter);
        assert!(!quit);
        assert_eq!(app.current_screen, CurrentScreen::ProcessList);
        assert_eq!(app.input_mode, InputMode::Normal);

        // Sort key shortcuts
        handle_key_event(&mut app, KeyCode::Char('m'));
        assert_eq!(app.sort_column, SortColumn::Memory);
        handle_key_event(&mut app, KeyCode::Char('r'));
        assert!(app.sort_ascending);

        // Quit command
        handle_key_event(&mut app, KeyCode::Char(':'));
        handle_key_event(&mut app, KeyCode::Char('q'));
        assert!(handle_key_event(&mut app, KeyCode::Enter));
    }
}
