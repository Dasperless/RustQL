//! Terminal UI (TUI) for interactive SQL querying, schema browsing, and result table exploration.
//!
//! Layout:
//! - Left sidebar: Loaded JSON tables, auto-detected JSON shapes, row count, cache status, and recursive schema tree.
//! - Top-right bar: SQL Input box (with query history Up/Down) and live Table Search/Filter box.
//! - Center-right pane: Scrollable, column-sortable, filterable Result Table (plus error banner when a query fails).
//! - Bottom bar: Status summary and context-aware keyboard shortcut hints.
//! - Modal overlay (`Enter` or `d` in Result Table): Pretty-printed full JSON record & selected cell inspector.

use crossterm::{
    event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{
        Block, Borders, Cell, Clear, List, ListItem, ListState, Paragraph, Row, Table, TableState,
        Wrap,
    },
    Frame, Terminal,
};
use serde_json::{Map, Value};
use std::cmp::Ordering;
use std::io;
use std::time::Duration;

use crate::cli::value_to_display_string;
use crate::engine::{compare_json_values, QueryEngine, QueryResult};

/// Active pane focus in the TUI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FocusPane {
    /// SQL query input bar
    SqlInput,
    /// Interactive result table
    ResultTable,
    /// Quick live text filter on the current result table
    FilterInput,
    /// Left sidebar with loaded tables, detected JSON shapes, and schema browser
    SchemaBrowser,
}

/// Column sort direction for interactive result table sorting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortDir {
    Asc,
    Desc,
}

/// Complete state of the Rustrix TUI application.
pub struct TuiApp {
    pub engine: QueryEngine,
    pub focus: FocusPane,
    /// Current SQL input string
    pub sql_input: String,
    /// Cursor position (character index) inside `sql_input`
    pub sql_cursor: usize,
    /// Executed SQL history
    pub query_history: Vec<String>,
    /// Current index in `query_history` when browsing with Up/Down (`None` = editing latest)
    pub history_index: Option<usize>,

    /// Live filter text applied to the displayed result rows
    pub filter_input: String,
    /// Latest successful query result
    pub current_result: Option<QueryResult>,
    /// Indices into `current_result.rows` after applying `filter_input` and interactive column sort
    pub visible_row_indices: Vec<usize>,
    /// Active interactive column sort `(col_idx, SortDir)`
    pub sort_state: Option<(usize, SortDir)>,

    /// Selected row index in `visible_row_indices`
    pub selected_row: usize,
    /// Selected column index in `current_result.columns`
    pub selected_col: usize,
    /// Horizontal column scroll offset so wide tables can be browsed smoothly
    pub col_scroll_offset: usize,

    /// Selected item index in the left Schema Browser list
    pub schema_selected: usize,
    /// Flattened selectable schema entries `(display_line, insert_snippet)`
    pub schema_entries: Vec<(Line<'static>, String)>,

    /// Latest SQL or evaluation error message (if the last query failed)
    pub error_message: Option<String>,
    /// Informational status message
    pub status_message: String,
    /// Whether the Row/Cell JSON Detail Inspector modal is open
    pub show_detail_modal: bool,
    /// Whether the Help overlay modal is open
    pub show_help_modal: bool,
    /// Whether the user requested to quit
    pub should_quit: bool,
}

impl TuiApp {
    /// Initialize the TUI state from a pre-loaded [`QueryEngine`].
    pub fn new(engine: QueryEngine) -> Self {
        let first_table_name = engine
            .tables()
            .first()
            .map(|t| t.name.clone())
            .unwrap_or_else(|| "data".to_string());
        let initial_sql = format!("SELECT * FROM {}", first_table_name);
        let sql_cursor = initial_sql.chars().count();
        let schema_entries = build_schema_entries(&engine);

        let mut app = Self {
            engine,
            focus: FocusPane::ResultTable,
            sql_input: initial_sql.clone(),
            sql_cursor,
            query_history: Vec::new(),
            history_index: None,
            filter_input: String::new(),
            current_result: None,
            visible_row_indices: Vec::new(),
            sort_state: None,
            selected_row: 0,
            selected_col: 0,
            col_scroll_offset: 0,
            schema_selected: 0,
            schema_entries,
            error_message: None,
            status_message: "Ready. Press '/' or 'e' to edit SQL, 's' to sort column, 'f' to filter rows, 'Enter' to inspect JSON, 'q' to quit.".to_string(),
            show_detail_modal: false,
            show_help_modal: false,
            should_quit: false,
        };

        app.execute_current_sql();
        app
    }

    /// Execute `self.sql_input` against `self.engine` and update result state.
    pub fn execute_current_sql(&mut self) {
        let query = self.sql_input.trim().to_string();
        if query.is_empty() {
            self.error_message = Some("SQL query cannot be empty.".to_string());
            return;
        }

        // Record in history if distinct from last entry
        if self.query_history.last().map(|s| s.as_str()) != Some(query.as_str()) {
            self.query_history.push(query.clone());
        }
        self.history_index = None;

        match self.engine.execute(&query) {
            Ok(res) => {
                let row_count = res.rows.len();
                let elapsed = res.elapsed_ms;
                self.current_result = Some(res);
                self.error_message = None;
                self.sort_state = None;
                self.selected_row = 0;
                self.selected_col = 0;
                self.col_scroll_offset = 0;
                self.recompute_visible_rows();
                self.status_message = format!(
                    "Executed in {:.2} ms — {} row(s) returned.",
                    elapsed, row_count
                );
            }
            Err(err) => {
                self.error_message = Some(err.to_string());
                self.status_message = "Query error — see error banner above.".to_string();
            }
        }
    }

    /// Re-filter and re-sort `self.visible_row_indices` based on `filter_input` and `sort_state`.
    pub fn recompute_visible_rows(&mut self) {
        let res = match &self.current_result {
            Some(r) => r,
            None => {
                self.visible_row_indices.clear();
                return;
            }
        };

        let needle = self.filter_input.trim().to_lowercase();
        let mut indices: Vec<usize> = (0..res.rows.len())
            .filter(|&r_idx| {
                if needle.is_empty() {
                    return true;
                }
                res.rows[r_idx].iter().any(|cell| {
                    value_to_display_string(cell)
                        .to_lowercase()
                        .contains(&needle)
                })
            })
            .collect();

        if let Some((col_idx, dir)) = self.sort_state {
            if col_idx < res.columns.len() {
                indices.sort_by(|&a, &b| {
                    let va = &res.rows[a][col_idx];
                    let vb = &res.rows[b][col_idx];
                    let ord = match (va.is_null(), vb.is_null()) {
                        (true, true) => Ordering::Equal,
                        (true, false) => Ordering::Greater,
                        (false, true) => Ordering::Less,
                        (false, false) => {
                            let c = compare_json_values(va, vb);
                            match dir {
                                SortDir::Asc => c,
                                SortDir::Desc => c.reverse(),
                            }
                        }
                    };
                    if ord == Ordering::Equal {
                        a.cmp(&b)
                    } else {
                        ord
                    }
                });
            }
        }

        self.visible_row_indices = indices;
        if self.selected_row >= self.visible_row_indices.len() {
            self.selected_row = self.visible_row_indices.len().saturating_sub(1);
        }
    }

    /// Toggle interactive sort on the currently selected column.
    pub fn toggle_sort_selected_column(&mut self) {
        let col_count = self
            .current_result
            .as_ref()
            .map(|r| r.columns.len())
            .unwrap_or(0);
        if col_count == 0 {
            return;
        }
        let col = self.selected_col.min(col_count - 1);
        self.sort_state = match self.sort_state {
            Some((c, SortDir::Asc)) if c == col => Some((col, SortDir::Desc)),
            Some((c, SortDir::Desc)) if c == col => None,
            _ => Some((col, SortDir::Asc)),
        };
        self.recompute_visible_rows();
    }

    /// Handle a single keyboard event.
    pub fn handle_key_event(&mut self, key: KeyEvent) {
        if key.kind != KeyEventKind::Press {
            return;
        }

        // Global Ctrl+C always exits
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.should_quit = true;
            return;
        }

        // If help modal or detail modal is open, any Esc/Enter/q closes it
        if self.show_help_modal {
            if matches!(
                key.code,
                KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q') | KeyCode::Char('?') | KeyCode::F(1)
            ) {
                self.show_help_modal = false;
            }
            return;
        }

        if self.show_detail_modal {
            if matches!(
                key.code,
                KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q') | KeyCode::Char('d')
            ) {
                self.show_detail_modal = false;
            }
            return;
        }

        // Global F1 for help
        if key.code == KeyCode::F(1) {
            self.show_help_modal = true;
            return;
        }

        // Tab / BackTab cycles focus across panes
        match key.code {
            KeyCode::Tab => {
                self.focus = match self.focus {
                    FocusPane::ResultTable => FocusPane::SqlInput,
                    FocusPane::SqlInput => FocusPane::FilterInput,
                    FocusPane::FilterInput => FocusPane::SchemaBrowser,
                    FocusPane::SchemaBrowser => FocusPane::ResultTable,
                };
                return;
            }
            KeyCode::BackTab => {
                self.focus = match self.focus {
                    FocusPane::ResultTable => FocusPane::SchemaBrowser,
                    FocusPane::SchemaBrowser => FocusPane::FilterInput,
                    FocusPane::FilterInput => FocusPane::SqlInput,
                    FocusPane::SqlInput => FocusPane::ResultTable,
                };
                return;
            }
            _ => {}
        }

        match self.focus {
            FocusPane::SqlInput => self.handle_sql_input_key(key),
            FocusPane::FilterInput => self.handle_filter_input_key(key),
            FocusPane::ResultTable => self.handle_result_table_key(key),
            FocusPane::SchemaBrowser => self.handle_schema_browser_key(key),
        }
    }

    fn handle_sql_input_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Enter => {
                self.execute_current_sql();
                if self.error_message.is_none() {
                    self.focus = FocusPane::ResultTable;
                }
            }
            KeyCode::Esc => {
                self.focus = FocusPane::ResultTable;
            }
            KeyCode::Left => {
                self.sql_cursor = self.sql_cursor.saturating_sub(1);
            }
            KeyCode::Right => {
                let len = self.sql_input.chars().count();
                if self.sql_cursor < len {
                    self.sql_cursor += 1;
                }
            }
            KeyCode::Home => {
                self.sql_cursor = 0;
            }
            KeyCode::End => {
                self.sql_cursor = self.sql_input.chars().count();
            }
            KeyCode::Backspace => {
                if self.sql_cursor > 0 {
                    let mut chars: Vec<char> = self.sql_input.chars().collect();
                    chars.remove(self.sql_cursor - 1);
                    self.sql_input = chars.into_iter().collect();
                    self.sql_cursor -= 1;
                }
            }
            KeyCode::Delete => {
                let mut chars: Vec<char> = self.sql_input.chars().collect();
                if self.sql_cursor < chars.len() {
                    chars.remove(self.sql_cursor);
                    self.sql_input = chars.into_iter().collect();
                }
            }
            KeyCode::Up => {
                if !self.query_history.is_empty() {
                    let next_idx = match self.history_index {
                        None => self.query_history.len() - 1,
                        Some(i) => i.saturating_sub(1),
                    };
                    self.history_index = Some(next_idx);
                    self.sql_input = self.query_history[next_idx].clone();
                    self.sql_cursor = self.sql_input.chars().count();
                }
            }
            KeyCode::Down => {
                if let Some(i) = self.history_index {
                    if i + 1 < self.query_history.len() {
                        self.history_index = Some(i + 1);
                        self.sql_input = self.query_history[i + 1].clone();
                        self.sql_cursor = self.sql_input.chars().count();
                    } else {
                        self.history_index = None;
                    }
                }
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.sql_input.clear();
                self.sql_cursor = 0;
            }
            KeyCode::Char(c) => {
                let mut chars: Vec<char> = self.sql_input.chars().collect();
                let pos = self.sql_cursor.min(chars.len());
                chars.insert(pos, c);
                self.sql_input = chars.into_iter().collect();
                self.sql_cursor = pos + 1;
            }
            _ => {}
        }
    }

    fn handle_filter_input_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Enter | KeyCode::Esc => {
                self.focus = FocusPane::ResultTable;
            }
            KeyCode::Backspace => {
                self.filter_input.pop();
                self.recompute_visible_rows();
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.filter_input.clear();
                self.recompute_visible_rows();
            }
            KeyCode::Char(c) => {
                self.filter_input.push(c);
                self.recompute_visible_rows();
            }
            _ => {}
        }
    }

    fn handle_result_table_key(&mut self, key: KeyEvent) {
        let row_count = self.visible_row_indices.len();
        let col_count = self
            .current_result
            .as_ref()
            .map(|r| r.columns.len())
            .unwrap_or(0);

        match key.code {
            KeyCode::Char('q') => {
                self.should_quit = true;
            }
            KeyCode::Char('?') => {
                self.show_help_modal = true;
            }
            KeyCode::Char('/') | KeyCode::Char('e') => {
                self.focus = FocusPane::SqlInput;
            }
            KeyCode::Char('f') => {
                self.focus = FocusPane::FilterInput;
            }
            KeyCode::Char('c') => {
                // Clear active filter and sort
                self.filter_input.clear();
                self.sort_state = None;
                self.recompute_visible_rows();
                self.status_message = "Cleared active row filter and column sort.".to_string();
            }
            KeyCode::Char('s') => {
                self.toggle_sort_selected_column();
            }
            KeyCode::Enter | KeyCode::Char('d') => {
                if row_count > 0 {
                    self.show_detail_modal = true;
                }
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if row_count > 0 && self.selected_row + 1 < row_count {
                    self.selected_row += 1;
                }
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.selected_row = self.selected_row.saturating_sub(1);
            }
            KeyCode::PageDown => {
                if row_count > 0 {
                    self.selected_row = (self.selected_row + 15).min(row_count - 1);
                }
            }
            KeyCode::PageUp => {
                self.selected_row = self.selected_row.saturating_sub(15);
            }
            KeyCode::Home | KeyCode::Char('g') => {
                self.selected_row = 0;
            }
            KeyCode::End | KeyCode::Char('G') => {
                if row_count > 0 {
                    self.selected_row = row_count - 1;
                }
            }
            KeyCode::Right | KeyCode::Char('l') => {
                if col_count > 0 && self.selected_col + 1 < col_count {
                    self.selected_col += 1;
                    if self.selected_col >= self.col_scroll_offset + 6 {
                        self.col_scroll_offset = self.selected_col.saturating_sub(5);
                    }
                }
            }
            KeyCode::Left | KeyCode::Char('h') => {
                self.selected_col = self.selected_col.saturating_sub(1);
                if self.selected_col < self.col_scroll_offset {
                    self.col_scroll_offset = self.selected_col;
                }
            }
            _ => {}
        }
    }

    fn handle_schema_browser_key(&mut self, key: KeyEvent) {
        let count = self.schema_entries.len();
        match key.code {
            KeyCode::Char('q') => {
                self.should_quit = true;
            }
            KeyCode::Char('?') => {
                self.show_help_modal = true;
            }
            KeyCode::Char('/') | KeyCode::Char('e') => {
                self.focus = FocusPane::SqlInput;
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if count > 0 && self.schema_selected + 1 < count {
                    self.schema_selected += 1;
                }
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.schema_selected = self.schema_selected.saturating_sub(1);
            }
            KeyCode::Enter => {
                // Insert selected table name or field path into SQL query or run `SELECT * FROM table`
                if let Some((_, snippet)) = self.schema_entries.get(self.schema_selected) {
                    if snippet.starts_with("SELECT ") {
                        self.sql_input = snippet.clone();
                        self.sql_cursor = self.sql_input.chars().count();
                        self.execute_current_sql();
                    } else {
                        if !self.sql_input.ends_with(' ') && !self.sql_input.is_empty() {
                            self.sql_input.push(' ');
                        }
                        self.sql_input.push_str(snippet);
                        self.sql_cursor = self.sql_input.chars().count();
                        self.focus = FocusPane::SqlInput;
                    }
                }
            }
            _ => {}
        }
    }
}

fn build_schema_entries(engine: &QueryEngine) -> Vec<(Line<'static>, String)> {
    let mut items = Vec::new();
    for table in engine.tables() {
        let cache_tag = if table.loaded_from_cache {
            " [cached]"
        } else {
            ""
        };
        let header_line = Line::from(vec![
            Span::styled(
                format!("▸ {} ", table.name),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("({} rows{})", table.rows.len(), cache_tag),
                Style::default().fg(Color::DarkGray),
            ),
        ]);
        items.push((header_line, format!("SELECT * FROM {}", table.name)));

        let shape_line = Line::from(vec![
            Span::raw("  Shape: "),
            Span::styled(
                table.shape.description(),
                Style::default().fg(Color::Yellow),
            ),
        ]);
        items.push((shape_line, table.name.clone()));

        for field in &table.schema_fields {
            let indent = "  ".repeat(field.depth + 1);
            let field_line = Line::from(vec![
                Span::raw(format!("{}• ", indent)),
                Span::styled(field.path.clone(), Style::default().fg(Color::White)),
                Span::styled(
                    format!(" : {}", field.data_type),
                    Style::default().fg(Color::Green),
                ),
            ]);
            items.push((field_line, field.path.clone()));
        }
    }
    items
}

/// Render the TUI frame.
pub fn draw_tui(frame: &mut Frame, app: &TuiApp) {
    let root_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(10),
            Constraint::Length(1), // Bottom status bar
        ])
        .split(frame.area());

    let main_chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(27), // Left sidebar: Schema & Detected JSON Shapes
            Constraint::Percentage(73), // Right area: SQL bar + Filter bar + Result Table
        ])
        .split(root_chunks[0]);

    draw_schema_sidebar(frame, app, main_chunks[0]);

    let has_error = app.error_message.is_some();
    let right_constraints = if has_error {
        vec![
            Constraint::Length(3), // SQL input + Filter input
            Constraint::Length(3), // Error banner
            Constraint::Min(5),    // Result table
        ]
    } else {
        vec![
            Constraint::Length(3), // SQL input + Filter input
            Constraint::Min(5),    // Result table
        ]
    };

    let right_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(right_constraints)
        .split(main_chunks[1]);

    draw_top_bars(frame, app, right_chunks[0]);

    if has_error {
        draw_error_banner(frame, app, right_chunks[1]);
        draw_result_table(frame, app, right_chunks[2]);
    } else {
        draw_result_table(frame, app, right_chunks[1]);
    }

    draw_status_bar(frame, app, root_chunks[1]);

    if app.show_detail_modal {
        draw_detail_modal(frame, app);
    }
    if app.show_help_modal {
        draw_help_modal(frame);
    }
}

fn draw_schema_sidebar(frame: &mut Frame, app: &TuiApp, area: Rect) {
    let focused = app.focus == FocusPane::SchemaBrowser;
    let border_style = if focused {
        Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::DarkGray)
    };

    let list_items: Vec<ListItem> = app
        .schema_entries
        .iter()
        .map(|(line, _)| ListItem::new(line.clone()))
        .collect();

    let list = List::new(list_items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(border_style)
                .title(" Tables, Detected Shapes & Schema "),
        )
        .highlight_style(
            Style::default()
                .bg(Color::Rgb(40, 60, 90))
                .add_modifier(Modifier::BOLD),
        );

    let mut state = ListState::default();
    if !app.schema_entries.is_empty() {
        state.select(Some(app.schema_selected));
    }
    frame.render_stateful_widget(list, area, &mut state);
}

fn draw_top_bars(frame: &mut Frame, app: &TuiApp, area: Rect) {
    let top_chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(72), // SQL Input
            Constraint::Percentage(28), // Quick Filter Input
        ])
        .split(area);

    // 1. SQL Input box
    let sql_focused = app.focus == FocusPane::SqlInput;
    let sql_border = if sql_focused {
        Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::DarkGray)
    };

    let hist_badge = match app.history_index {
        Some(idx) => format!(
            " SQL Query [History {}/{}] (Enter=Run, Up/Down=History) ",
            idx + 1,
            app.query_history.len()
        ),
        None => " SQL Query (Enter=Run, '/'=Focus, Up/Down=History) ".to_string(),
    };

    let sql_inner_w = top_chunks[0].width.saturating_sub(2) as usize;
    let sql_scroll_x = if sql_inner_w > 0 && app.sql_cursor >= sql_inner_w {
        (app.sql_cursor - sql_inner_w + 1) as u16
    } else {
        0
    };

    let sql_para = Paragraph::new(app.sql_input.as_str())
        .scroll((0, sql_scroll_x))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(sql_border)
                .title(hist_badge),
        );
    frame.render_widget(sql_para, top_chunks[0]);

    if sql_focused {
        let visible_cursor_offset = app.sql_cursor.saturating_sub(sql_scroll_x as usize);
        let cx = top_chunks[0].x + 1 + (visible_cursor_offset.min(sql_inner_w) as u16);
        let cy = top_chunks[0].y + 1;
        frame.set_cursor_position((cx, cy));
    }

    // 2. Quick Filter Input box
    let filter_focused = app.focus == FocusPane::FilterInput;
    let filter_border = if filter_focused {
        Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::DarkGray)
    };

    let filter_inner_w = top_chunks[1].width.saturating_sub(2) as usize;
    let filter_len = app.filter_input.chars().count();
    let filter_scroll_x = if filter_inner_w > 0 && filter_len >= filter_inner_w {
        (filter_len - filter_inner_w + 1) as u16
    } else {
        0
    };

    let filter_para = Paragraph::new(app.filter_input.as_str())
        .scroll((0, filter_scroll_x))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(filter_border)
                .title(" Row Filter ('f') "),
        );
    frame.render_widget(filter_para, top_chunks[1]);

    if filter_focused {
        let visible_offset = filter_len.saturating_sub(filter_scroll_x as usize);
        let cx = top_chunks[1].x + 1 + (visible_offset.min(filter_inner_w) as u16);
        let cy = top_chunks[1].y + 1;
        frame.set_cursor_position((cx, cy));
    }
}

fn draw_error_banner(frame: &mut Frame, app: &TuiApp, area: Rect) {
    let msg = app.error_message.as_deref().unwrap_or("");
    let err_para = Paragraph::new(Span::styled(
        format!(" ⚠ {}", msg),
        Style::default().fg(Color::LightRed).add_modifier(Modifier::BOLD),
    ))
    .block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Red))
            .title(" Error "),
    );
    frame.render_widget(err_para, area);
}

fn draw_result_table(frame: &mut Frame, app: &TuiApp, area: Rect) {
    let focused = app.focus == FocusPane::ResultTable;
    let border_style = if focused {
        Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::DarkGray)
    };

    let res = match &app.current_result {
        Some(r) if !r.columns.is_empty() => r,
        _ => {
            let empty_para = Paragraph::new("No rows or columns to display.")
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_style(border_style)
                        .title(" Results "),
                );
            frame.render_widget(empty_para, area);
            return;
        }
    };

    let total_cols = res.columns.len();
    let start_col = app.col_scroll_offset.min(total_cols.saturating_sub(1));
    let visible_cols_slice = &res.columns[start_col..];

    let header_cells = visible_cols_slice.iter().enumerate().map(|(offset, col_name)| {
        let actual_col = start_col + offset;
        let sort_indicator = match app.sort_state {
            Some((sc, SortDir::Asc)) if sc == actual_col => " ▲",
            Some((sc, SortDir::Desc)) if sc == actual_col => " ▼",
            _ => "",
        };
        let is_selected_col = actual_col == app.selected_col;
        let style = if is_selected_col {
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        };
        Cell::from(format!("{}{}", col_name, sort_indicator)).style(style)
    });

    let header = Row::new(header_cells).height(1).bottom_margin(1);

    let rows = app.visible_row_indices.iter().map(|&r_idx| {
        let row_data = &res.rows[r_idx];
        let cells = (start_col..total_cols).map(|c_idx| {
            let val = row_data.get(c_idx).unwrap_or(&Value::Null);
            let text = value_to_display_string(val);
            let style = if val.is_null() {
                Style::default().fg(Color::DarkGray)
            } else if c_idx == app.selected_col {
                Style::default().fg(Color::Yellow)
            } else {
                Style::default().fg(Color::White)
            };
            Cell::from(text).style(style)
        });
        Row::new(cells).height(1)
    });

    // Compute dynamic column widths
    let num_vis_cols = visible_cols_slice.len().max(1);
    let mut constraints = Vec::with_capacity(num_vis_cols);
    for (offset, col_name) in visible_cols_slice.iter().enumerate() {
        let actual_col = start_col + offset;
        let mut max_w = col_name.chars().count() + 3;
        for &r_idx in app.visible_row_indices.iter().take(50) {
            if let Some(val) = res.rows[r_idx].get(actual_col) {
                let len = value_to_display_string(val).chars().count();
                if len > max_w {
                    max_w = len;
                }
            }
        }
        constraints.push(Constraint::Length(max_w.clamp(8, 36) as u16));
    }

    let filter_badge = if app.filter_input.is_empty() {
        String::new()
    } else {
        format!(" [filtered: '{}']", app.filter_input)
    };

    let title = format!(
        " Results ({}/{} rows, col {}/{}){} — Arrows/hjkl:Navigate  s:SortCol  f:Filter  Enter:Inspect ",
        app.visible_row_indices.len(),
        res.rows.len(),
        (app.selected_col + 1).min(total_cols),
        total_cols,
        filter_badge
    );

    let table = Table::new(rows, constraints)
        .header(header)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(border_style)
                .title(title),
        )
        .row_highlight_style(
            Style::default()
                .bg(Color::Rgb(35, 50, 75))
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("▶ ");

    let mut table_state = TableState::default();
    if !app.visible_row_indices.is_empty() {
        table_state.select(Some(app.selected_row));
    }
    frame.render_stateful_widget(table, area, &mut table_state);
}

fn draw_status_bar(frame: &mut Frame, app: &TuiApp, area: Rect) {
    let focus_name = match app.focus {
        FocusPane::ResultTable => "TABLE",
        FocusPane::SqlInput => "SQL",
        FocusPane::FilterInput => "FILTER",
        FocusPane::SchemaBrowser => "SCHEMA",
    };

    let line = Line::from(vec![
        Span::styled(
            format!(" [{}] ", focus_name),
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
        Span::styled(&app.status_message, Style::default().fg(Color::Gray)),
        Span::raw("  |  "),
        Span::styled(
            "Tab:Pane  /:SQL  f:Filter  s:Sort  c:Reset  Enter:JSON  ?:Help  q:Quit",
            Style::default().fg(Color::Yellow),
        ),
    ]);

    frame.render_widget(Paragraph::new(line), area);
}

fn draw_detail_modal(frame: &mut Frame, app: &TuiApp) {
    let res = match &app.current_result {
        Some(r) => r,
        None => return,
    };
    let &actual_row_idx = match app.visible_row_indices.get(app.selected_row) {
        Some(i) => i,
        None => return,
    };
    let row = &res.rows[actual_row_idx];
    let mut row_obj = Map::new();
    for (col, val) in res.columns.iter().zip(row.iter()) {
        row_obj.insert(col.clone(), val.clone());
    }

    let selected_col_name = res
        .columns
        .get(app.selected_col)
        .cloned()
        .unwrap_or_default();
    let selected_val = row.get(app.selected_col).cloned().unwrap_or(Value::Null);

    let pretty_cell = serde_json::to_string_pretty(&selected_val).unwrap_or_default();
    let pretty_row = serde_json::to_string_pretty(&Value::Object(row_obj)).unwrap_or_default();

    let content = format!(
        "Selected Cell [{}]:\n{}\n\nFull Row JSON (Row #{}):\n{}",
        selected_col_name,
        pretty_cell,
        actual_row_idx + 1,
        pretty_row
    );

    let area = centered_rect(70, 75, frame.area());
    frame.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD))
        .title(" Row & Cell JSON Inspector (Press Esc or Enter to close) ");
    let para = Paragraph::new(content)
        .block(block)
        .wrap(Wrap { trim: false });
    frame.render_widget(para, area);
}

fn draw_help_modal(frame: &mut Frame) {
    let help_text = "\
Keyboard Shortcuts:
────────────────────────────────────────────────────────────
  Tab / Shift+Tab   Cycle focus: Result Table → SQL → Filter → Schema
  / or e            Focus SQL Query Input bar
  Enter (in SQL)    Execute SQL query
  Up / Down (SQL)   Navigate SQL query history
  Ctrl+U (in SQL)   Clear input line

  Arrows / hjkl     Navigate rows and columns in Result Table
  PageUp / PageDown Scroll 15 rows up or down
  Home/g / End/G    Jump to first / last row
  s                 Sort Result Table by selected column (Asc → Desc → Off)
  f                 Focus live Row Filter input
  c                 Clear active row filter and column sort
  Enter or d        Open JSON Detail Inspector for current row & cell
  Enter (in Schema) Run SELECT * on table or append field path to SQL
  ? or F1           Toggle this Help modal
  q or Ctrl+C       Quit Rustrix";

    let area = centered_rect(68, 68, frame.area());
    frame.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD))
        .title(" Rustrix Help (Press Esc, Enter, or ? to close) ");
    let para = Paragraph::new(help_text).block(block);
    frame.render_widget(para, area);
}

fn centered_rect(percent_x: u16, percent_y: u16, r: Rect) -> Rect {
    let popup_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(r);

    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(popup_layout[1])[1]
}

/// Launch the interactive TUI in the user's terminal.
pub fn run_tui(engine: QueryEngine) -> Result<(), Box<dyn std::error::Error>> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut app = TuiApp::new(engine);
    let res = run_event_loop(&mut terminal, &mut app);

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    res?;
    Ok(())
}

fn run_event_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut TuiApp,
) -> io::Result<()> {
    loop {
        terminal.draw(|f| draw_tui(f, app))?;

        if event::poll(Duration::from_millis(100))? {
            if let Event::Key(key) = event::read()? {
                app.handle_key_event(key);
            }
        }

        if app.should_quit {
            break;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::loader::{load_json_file, parse_file_spec};
    use ratatui::backend::TestBackend;
    use tempfile::TempDir;

    #[test]
    fn test_tui_filter_sort_scroll_and_render() {
        let tmp = TempDir::new().unwrap();
        let mut engine = QueryEngine::new();
        let (name, path) = parse_file_spec("examples/users.json");
        let table = load_json_file(&name, &path, Some(tmp.path()), false).unwrap();
        engine.register_table(table);

        let mut app = TuiApp::new(engine);
        assert_eq!(app.visible_row_indices.len(), 5);

        // Render into a headless ratatui TestBackend to verify layout & drawing
        let backend = TestBackend::new(120, 35);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| draw_tui(f, &app)).unwrap();

        // Scroll down & right
        app.handle_key_event(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(app.selected_row, 1);
        app.handle_key_event(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
        assert_eq!(app.selected_col, 1); // 'name' column

        // Sort by 'name' ascending then descending
        app.handle_key_event(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE));
        assert_eq!(app.sort_state, Some((1, SortDir::Asc)));
        app.handle_key_event(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE));
        assert_eq!(app.sort_state, Some((1, SortDir::Desc)));

        // Filter rows for 'Seattle'
        app.handle_key_event(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::NONE));
        assert_eq!(app.focus, FocusPane::FilterInput);
        for ch in "Seattle".chars() {
            app.handle_key_event(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE));
        }
        assert_eq!(app.visible_row_indices.len(), 2); // Alice and David live in Seattle

        // Return to table and open JSON detail inspector
        app.handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(app.focus, FocusPane::ResultTable);
        app.handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(app.show_detail_modal);
        terminal.draw(|f| draw_tui(f, &app)).unwrap();
    }
}
