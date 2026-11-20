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
use std::collections::BTreeSet;
use std::io;
use std::time::Duration;

use crate::cli::{sanitize_terminal_text, value_to_display_string};
use crate::engine::{compare_json_values, QueryEngine, QueryResult};
use crate::sql_helper::{
    build_cell_quick_actions, build_sql_templates, collect_field_sample_values,
    compute_sql_suggestions, diagnose_sql_error, format_sql_literal, highlight_sql_line,
    merge_where_condition, validate_sql_live, QuickFilterAction, SqlSuggestion, SqlTemplateRecipe,
    SqlValidationStatus,
};

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

/// Complete state of the RustQL TUI application.
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

    /// Live SQL syntax & schema validation status
    pub sql_validation: SqlValidationStatus,
    /// Context-aware autocomplete suggestions for the current cursor position
    pub sql_suggestions: Vec<SqlSuggestion>,
    /// Selected index in `sql_suggestions`
    pub suggestion_selected: usize,
    /// Whether the autocomplete dropdown popup is currently visible
    pub show_suggestions: bool,

    /// Whether the cell Quick-Filter modal (`w`) is open
    pub show_quick_filter_modal: bool,
    /// Generated quick-filter actions for the currently selected table cell
    pub quick_filter_actions: Vec<QuickFilterAction>,
    /// Selected index in `quick_filter_actions`
    pub quick_filter_selected: usize,

    /// Whether the SQL Recipe & Template Picker modal (`t` / `Ctrl+T`) is open
    pub show_template_modal: bool,
    /// Schema-tailored SQL recipes
    pub sql_templates: Vec<SqlTemplateRecipe>,
    /// Selected index in `sql_templates`
    pub template_selected: usize,

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
    /// Vertical row scroll offset so huge datasets render instantly with virtual windowing
    pub row_scroll_offset: usize,
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
    /// Vertical scroll offset (in visual lines) inside the Row/Cell JSON Detail Inspector modal
    pub detail_scroll_offset: usize,
    /// Selected line cursor index inside the Row/Cell JSON Detail Inspector modal
    pub detail_cursor_line: usize,
    /// Set of collapsed JSON node paths inside the Row/Cell JSON Detail Inspector modal
    pub detail_collapsed_paths: BTreeSet<String>,
    /// Whether the Help overlay modal is open
    pub show_help_modal: bool,
    /// Whether the user requested to quit
    pub should_quit: bool,
}

impl TuiApp {
    /// Initialize the TUI state from a pre-loaded [`QueryEngine`].
    pub fn new(engine: QueryEngine) -> Self {
        let tables = engine.tables();
        let (first_table_name, table_row_count) = if let Some(first_table) = tables.first() {
            (first_table.name.clone(), first_table.rows.len())
        } else {
            ("data".to_string(), 0)
        };
        let initial_sql = if table_row_count > 1000 {
            format!("SELECT * FROM {} LIMIT 1000", first_table_name)
        } else {
            format!("SELECT * FROM {}", first_table_name)
        };
        let sql_cursor = initial_sql.chars().count();
        let schema_entries = build_schema_entries(&engine);
        let initial_validation = validate_sql_live(&initial_sql, &engine);
        let sql_templates = build_sql_templates(&engine);

        let mut app = Self {
            engine,
            focus: FocusPane::ResultTable,
            sql_input: initial_sql.clone(),
            sql_cursor,
            query_history: Vec::new(),
            history_index: None,
            sql_validation: initial_validation,
            sql_suggestions: Vec::new(),
            suggestion_selected: 0,
            show_suggestions: false,
            show_quick_filter_modal: false,
            quick_filter_actions: Vec::new(),
            quick_filter_selected: 0,
            show_template_modal: false,
            sql_templates,
            template_selected: 0,
            filter_input: String::new(),
            current_result: None,
            visible_row_indices: Vec::new(),
            sort_state: None,
            selected_row: 0,
            row_scroll_offset: 0,
            selected_col: 0,
            col_scroll_offset: 0,
            schema_selected: 0,
            schema_entries,
            error_message: None,
            status_message: "Ready. Press '/' to edit SQL (with live autocomplete), 'w' on any cell to filter, 't' for SQL templates, '?' for help.".to_string(),
            show_detail_modal: false,
            detail_scroll_offset: 0,
            detail_cursor_line: 0,
            detail_collapsed_paths: BTreeSet::new(),
            show_help_modal: false,
            should_quit: false,
        };

        app.execute_current_sql();
        app
    }

    /// Recompute live SQL validation and context-aware autocomplete suggestions.
    pub fn refresh_sql_assistant(&mut self, open_popup: bool) {
        self.sql_validation = validate_sql_live(&self.sql_input, &self.engine);
        self.sql_suggestions =
            compute_sql_suggestions(&self.sql_input, self.sql_cursor, &self.engine);
        if self.suggestion_selected >= self.sql_suggestions.len() {
            self.suggestion_selected = 0;
        }
        self.show_suggestions = open_popup && !self.sql_suggestions.is_empty();
    }

    /// Accept the currently highlighted autocomplete suggestion in `self.sql_suggestions`.
    pub fn accept_selected_suggestion(&mut self) -> bool {
        if !self.show_suggestions || self.sql_suggestions.is_empty() {
            return false;
        }
        let idx = self
            .suggestion_selected
            .min(self.sql_suggestions.len().saturating_sub(1));
        let sug = self.sql_suggestions[idx].clone();

        let chars: Vec<char> = self.sql_input.chars().collect();
        let pos = self.sql_cursor.min(chars.len());
        let replace_start = pos.saturating_sub(sug.replace_prefix_len);

        let mut new_sql: String = chars[..replace_start].iter().collect();
        new_sql.push_str(&sug.insert_text);
        let new_cursor = new_sql.chars().count();

        // Skip duplicating a leading space if the character immediately after cursor is already whitespace
        let mut tail_slice = &chars[pos..];
        if sug.insert_text.ends_with(' ') && tail_slice.first() == Some(&' ') {
            tail_slice = &tail_slice[1..];
        }
        let tail: String = tail_slice.iter().collect();
        new_sql.push_str(&tail);

        self.sql_input = new_sql;
        self.sql_cursor = new_cursor;
        self.suggestion_selected = 0;
        self.refresh_sql_assistant(true);
        true
    }

    /// Open the one-key Quick Filter & Action menu (`w`) for the currently selected cell in the Result Table.
    pub fn open_quick_filter_for_selected_cell(&mut self) {
        let res = match &self.current_result {
            Some(r) if !r.columns.is_empty() && !self.visible_row_indices.is_empty() => r,
            _ => {
                self.status_message =
                    "No active table cell to filter — press 't' to open SQL Templates instead."
                        .to_string();
                return;
            }
        };

        let col_idx = self.selected_col.min(res.columns.len() - 1);
        let col_name = res.columns[col_idx].clone();
        let actual_row_idx = self.visible_row_indices[self.selected_row];
        let cell_val = res.rows[actual_row_idx]
            .get(col_idx)
            .cloned()
            .unwrap_or(Value::Null);

        let first_table = self.engine.tables().first().copied();
        let table_name = first_table
            .map(|t| t.name.clone())
            .unwrap_or_else(|| "data".to_string());
        let table_rows = first_table.map(|t| t.rows.len()).unwrap_or(0);

        self.quick_filter_actions = build_cell_quick_actions(
            &self.sql_input,
            &table_name,
            &col_name,
            &cell_val,
            table_rows,
        );
        self.quick_filter_selected = 0;
        self.show_quick_filter_modal = !self.quick_filter_actions.is_empty();
    }

    /// Open the interactive SQL Template & Recipe Picker modal (`t` or `Ctrl+T`).
    pub fn open_template_picker(&mut self) {
        self.sql_templates = build_sql_templates(&self.engine);
        if self.template_selected >= self.sql_templates.len() {
            self.template_selected = 0;
        }
        self.show_template_modal = true;
    }

    /// Execute `self.sql_input` against `self.engine` and update result state.
    pub fn execute_current_sql(&mut self) {
        self.show_suggestions = false;
        let query = self.sql_input.trim().to_string();
        if query.is_empty() {
            self.error_message = Some("SQL query cannot be empty.".to_string());
            return;
        }

        self.sql_validation = validate_sql_live(&query, &self.engine);

        // Record in history if distinct from last entry (capped at 500 entries to bound memory)
        if self.query_history.last().map(|s| s.as_str()) != Some(query.as_str()) {
            if self.query_history.len() >= 500 {
                self.query_history.remove(0);
            }
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
                self.row_scroll_offset = 0;
                self.selected_col = 0;
                self.col_scroll_offset = 0;
                self.detail_scroll_offset = 0;
                self.detail_cursor_line = 0;
                self.detail_collapsed_paths.clear();
                self.recompute_visible_rows();
                if let SqlValidationStatus::Warning { ref message } = self.sql_validation {
                    self.status_message = format!(
                        "Executed in {:.2} ms — {} row(s). {}",
                        elapsed, row_count, message
                    );
                } else {
                    self.status_message = format!(
                        "Executed in {:.2} ms — {} row(s) returned. (Press 'w' on any cell to filter, 't' for SQL templates)",
                        elapsed, row_count
                    );
                }
            }
            Err(err) => {
                let friendly = diagnose_sql_error(&query, &err.to_string(), &self.engine);
                self.error_message = Some(friendly);
                self.status_message =
                    "Query error — see hint above, or press Ctrl+T for SQL templates.".to_string();
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
        let mut indices: Vec<usize> = if needle.is_empty() {
            (0..res.rows.len()).collect()
        } else {
            (0..res.rows.len())
                .filter(|&r_idx| {
                    res.rows[r_idx]
                        .iter()
                        .any(|cell| value_matches_filter(cell, &needle))
                })
                .collect()
        };

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
        if self.selected_row < self.row_scroll_offset {
            self.row_scroll_offset = self.selected_row;
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

        // Global Ctrl+T opens SQL Template & Recipe Picker
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('t') {
            self.open_template_picker();
            return;
        }

        // If help modal or detail modal is open, handle closing it
        if self.show_help_modal {
            if matches!(
                key.code,
                KeyCode::Esc
                    | KeyCode::Enter
                    | KeyCode::Char('q')
                    | KeyCode::Char('?')
                    | KeyCode::F(1)
            ) {
                self.show_help_modal = false;
            }
            return;
        }

        if self.show_detail_modal {
            self.handle_detail_modal_key(key);
            return;
        }

        if self.show_quick_filter_modal {
            self.handle_quick_filter_modal_key(key);
            return;
        }

        if self.show_template_modal {
            self.handle_template_modal_key(key);
            return;
        }

        // Global F1 for help
        if key.code == KeyCode::F(1) {
            self.show_help_modal = true;
            return;
        }

        // Tab in SQL input accepts autocomplete suggestion when dropdown is open
        if key.code == KeyCode::Tab
            && self.focus == FocusPane::SqlInput
            && self.show_suggestions
            && self.accept_selected_suggestion()
        {
            return;
        }

        // Tab / BackTab cycles focus across panes
        match key.code {
            KeyCode::Tab => {
                self.show_suggestions = false;
                self.focus = match self.focus {
                    FocusPane::ResultTable => {
                        self.refresh_sql_assistant(false);
                        FocusPane::SqlInput
                    }
                    FocusPane::SqlInput => FocusPane::FilterInput,
                    FocusPane::FilterInput => FocusPane::SchemaBrowser,
                    FocusPane::SchemaBrowser => FocusPane::ResultTable,
                };
                return;
            }
            KeyCode::BackTab => {
                self.show_suggestions = false;
                self.focus = match self.focus {
                    FocusPane::ResultTable => FocusPane::SchemaBrowser,
                    FocusPane::SchemaBrowser => FocusPane::FilterInput,
                    FocusPane::FilterInput => {
                        self.refresh_sql_assistant(false);
                        FocusPane::SqlInput
                    }
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

    fn handle_detail_modal_key(&mut self, key: KeyEvent) {
        let col_count = self
            .current_result
            .as_ref()
            .map(|r| r.columns.len())
            .unwrap_or(0);
        let entries = build_detail_modal_entries(self);
        let line_count = entries.len();

        match key.code {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('d') => {
                self.show_detail_modal = false;
            }
            KeyCode::Char(' ') | KeyCode::Enter | KeyCode::Char('c') | KeyCode::Char('z') => {
                self.toggle_detail_collapse_at_cursor();
            }
            KeyCode::Char('a') | KeyCode::Char('A') => {
                self.toggle_detail_collapse_all();
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if line_count > 0 {
                    if self.detail_cursor_line + 1 < line_count {
                        self.detail_cursor_line += 1;
                    } else {
                        self.detail_scroll_offset = self.detail_scroll_offset.saturating_add(1);
                    }
                }
            }
            KeyCode::Up | KeyCode::Char('k') => {
                if self.detail_cursor_line > 0 {
                    self.detail_cursor_line -= 1;
                } else {
                    self.detail_scroll_offset = self.detail_scroll_offset.saturating_sub(1);
                }
            }
            KeyCode::PageDown => {
                if line_count > 0 {
                    let prev_cursor = self.detail_cursor_line;
                    self.detail_cursor_line = (self.detail_cursor_line + 15).min(line_count - 1);
                    if self.detail_cursor_line == prev_cursor {
                        self.detail_scroll_offset = self.detail_scroll_offset.saturating_add(15);
                    }
                }
            }
            KeyCode::PageUp => {
                if self.detail_cursor_line > 0 {
                    self.detail_cursor_line = self.detail_cursor_line.saturating_sub(15);
                } else {
                    self.detail_scroll_offset = self.detail_scroll_offset.saturating_sub(15);
                }
            }
            KeyCode::Home | KeyCode::Char('g') => {
                self.detail_cursor_line = 0;
                self.detail_scroll_offset = 0;
            }
            KeyCode::End | KeyCode::Char('G') => {
                if line_count > 0 {
                    self.detail_cursor_line = line_count - 1;
                }
                let lines: Vec<Line<'static>> = entries.into_iter().map(|e| e.line).collect();
                let approx_max = count_wrapped_lines(&lines, 40);
                self.detail_scroll_offset = approx_max;
            }
            KeyCode::Right | KeyCode::Char('l')
                if col_count > 0 && self.selected_col + 1 < col_count =>
            {
                self.selected_col += 1;
                if self.selected_col >= self.col_scroll_offset + 6 {
                    self.col_scroll_offset = self.selected_col.saturating_sub(5);
                }
                self.detail_cursor_line = 0;
                self.detail_scroll_offset = 0;
            }
            KeyCode::Left | KeyCode::Char('h') if self.selected_col > 0 => {
                self.selected_col -= 1;
                if self.selected_col < self.col_scroll_offset {
                    self.col_scroll_offset = self.selected_col;
                }
                self.detail_cursor_line = 0;
                self.detail_scroll_offset = 0;
            }
            _ => {}
        }
    }

    /// Toggle collapse/expand state for the JSON node at `self.detail_cursor_line` in the Detail Inspector.
    pub fn toggle_detail_collapse_at_cursor(&mut self) -> bool {
        let entries = build_detail_modal_entries(self);
        if entries.is_empty() {
            return false;
        }
        let cursor = self.detail_cursor_line.min(entries.len() - 1);
        let entry = &entries[cursor];
        let target_path = match &entry.toggle_path {
            Some(p) => p.clone(),
            None => return false,
        };
        let snap_target = entry
            .snap_to_path
            .clone()
            .unwrap_or_else(|| target_path.clone());
        let was_header = entry.is_node_header;

        if self.detail_collapsed_paths.contains(&target_path) {
            self.detail_collapsed_paths.remove(&target_path);
        } else {
            self.detail_collapsed_paths.insert(target_path);
        }

        let new_entries = build_detail_modal_entries(self);
        if !new_entries.is_empty() {
            if !was_header {
                if let Some(header_idx) = new_entries.iter().position(|e| {
                    e.is_node_header && e.toggle_path.as_deref() == Some(snap_target.as_str())
                }) {
                    if header_idx < cursor {
                        self.detail_cursor_line = header_idx;
                    } else {
                        self.detail_cursor_line =
                            self.detail_cursor_line.min(new_entries.len() - 1);
                    }
                } else {
                    self.detail_cursor_line = self.detail_cursor_line.min(new_entries.len() - 1);
                }
            } else {
                self.detail_cursor_line = self.detail_cursor_line.min(new_entries.len() - 1);
            }
        } else {
            self.detail_cursor_line = 0;
        }
        true
    }

    /// Toggle collapse/expand for all JSON fields/containers in the Detail Inspector (`a`).
    pub fn toggle_detail_collapse_all(&mut self) {
        let (field_paths, all_paths) = collect_detail_collapsible_paths(self);
        let target_paths = if !field_paths.is_empty() {
            &field_paths
        } else {
            &all_paths
        };
        if target_paths.is_empty() {
            return;
        }

        let all_targets_collapsed = target_paths
            .iter()
            .all(|p| self.detail_collapsed_paths.contains(p));
        let all_roots_collapsed = !all_paths.is_empty()
            && all_paths
                .iter()
                .filter(|p| !p.contains('/'))
                .all(|p| self.detail_collapsed_paths.contains(p));

        if all_targets_collapsed || all_roots_collapsed {
            self.detail_collapsed_paths.clear();
        } else {
            if !field_paths.is_empty() {
                self.detail_collapsed_paths.retain(|p| p.contains('/'));
            }
            for p in target_paths {
                self.detail_collapsed_paths.insert(p.clone());
            }
        }

        let new_entries = build_detail_modal_entries(self);
        if !new_entries.is_empty() {
            self.detail_cursor_line = self.detail_cursor_line.min(new_entries.len() - 1);
        } else {
            self.detail_cursor_line = 0;
        }
    }

    fn handle_quick_filter_modal_key(&mut self, key: KeyEvent) {
        let count = self.quick_filter_actions.len();
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('w') => {
                self.show_quick_filter_modal = false;
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if count > 0 {
                    self.quick_filter_selected = (self.quick_filter_selected + 1) % count;
                }
            }
            KeyCode::Up | KeyCode::Char('k') => {
                if count > 0 {
                    self.quick_filter_selected = (self.quick_filter_selected + count - 1) % count;
                }
            }
            KeyCode::Enter => {
                if let Some(action) = self.quick_filter_actions.get(self.quick_filter_selected) {
                    self.sql_input = action.sql.clone();
                    self.sql_cursor = self.sql_input.chars().count();
                    self.show_quick_filter_modal = false;
                    self.execute_current_sql();
                    self.focus = FocusPane::ResultTable;
                }
            }
            KeyCode::Char('e') | KeyCode::Char('/') => {
                if let Some(action) = self.quick_filter_actions.get(self.quick_filter_selected) {
                    self.sql_input = action.sql.clone();
                    self.sql_cursor = self.sql_input.chars().count();
                    self.show_quick_filter_modal = false;
                    self.focus = FocusPane::SqlInput;
                    self.refresh_sql_assistant(false);
                }
            }
            KeyCode::Char(c @ '1'..='9') => {
                let idx = (c as usize) - ('1' as usize);
                if let Some(action) = self.quick_filter_actions.get(idx) {
                    self.sql_input = action.sql.clone();
                    self.sql_cursor = self.sql_input.chars().count();
                    self.show_quick_filter_modal = false;
                    self.execute_current_sql();
                    self.focus = FocusPane::ResultTable;
                }
            }
            _ => {}
        }
    }

    fn handle_template_modal_key(&mut self, key: KeyEvent) {
        let count = self.sql_templates.len();
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('t') => {
                self.show_template_modal = false;
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if count > 0 {
                    self.template_selected = (self.template_selected + 1) % count;
                }
            }
            KeyCode::Up | KeyCode::Char('k') => {
                if count > 0 {
                    self.template_selected = (self.template_selected + count - 1) % count;
                }
            }
            KeyCode::Enter | KeyCode::Char('e') => {
                // Load recipe into SQL input bar and focus it for editing/execution
                if let Some(tpl) = self.sql_templates.get(self.template_selected).cloned() {
                    self.sql_input = tpl.sql;
                    self.sql_cursor = self.sql_input.chars().count();
                    self.show_template_modal = false;
                    self.focus = FocusPane::SqlInput;
                    self.refresh_sql_assistant(false);
                    self.status_message = format!(
                        "Loaded template '{}'. Edit values or press Enter to execute.",
                        tpl.title
                    );
                }
            }
            KeyCode::Char('r') => {
                // Load and immediately execute the selected template
                if let Some(tpl) = self.sql_templates.get(self.template_selected).cloned() {
                    self.sql_input = tpl.sql;
                    self.sql_cursor = self.sql_input.chars().count();
                    self.show_template_modal = false;
                    self.execute_current_sql();
                    self.focus = FocusPane::ResultTable;
                }
            }
            KeyCode::Char(c @ '1'..='9') => {
                let idx = (c as usize) - ('1' as usize);
                if let Some(tpl) = self.sql_templates.get(idx).cloned() {
                    self.sql_input = tpl.sql;
                    self.sql_cursor = self.sql_input.chars().count();
                    self.show_template_modal = false;
                    self.focus = FocusPane::SqlInput;
                    self.refresh_sql_assistant(false);
                }
            }
            _ => {}
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
                if self.show_suggestions {
                    self.show_suggestions = false;
                } else {
                    self.focus = FocusPane::ResultTable;
                }
            }
            KeyCode::Left => {
                self.sql_cursor = self.sql_cursor.saturating_sub(1);
                self.refresh_sql_assistant(self.show_suggestions);
            }
            KeyCode::Right => {
                let len = self.sql_input.chars().count();
                if self.sql_cursor < len {
                    self.sql_cursor += 1;
                    self.refresh_sql_assistant(self.show_suggestions);
                } else if self.show_suggestions {
                    self.accept_selected_suggestion();
                }
            }
            KeyCode::Home => {
                self.sql_cursor = 0;
                self.refresh_sql_assistant(false);
            }
            KeyCode::End => {
                self.sql_cursor = self.sql_input.chars().count();
                self.refresh_sql_assistant(self.show_suggestions);
            }
            KeyCode::Backspace => {
                if self.sql_cursor > 0 {
                    let mut chars: Vec<char> = self.sql_input.chars().collect();
                    chars.remove(self.sql_cursor - 1);
                    self.sql_input = chars.into_iter().collect();
                    self.sql_cursor -= 1;
                    self.refresh_sql_assistant(true);
                }
            }
            KeyCode::Delete => {
                let mut chars: Vec<char> = self.sql_input.chars().collect();
                if self.sql_cursor < chars.len() {
                    chars.remove(self.sql_cursor);
                    self.sql_input = chars.into_iter().collect();
                    self.refresh_sql_assistant(true);
                }
            }
            KeyCode::Up => {
                let force_history = key.modifiers.contains(KeyModifiers::CONTROL);
                if self.show_suggestions && !self.sql_suggestions.is_empty() && !force_history {
                    let count = self.sql_suggestions.len();
                    self.suggestion_selected = (self.suggestion_selected + count - 1) % count;
                } else if !self.query_history.is_empty() {
                    let next_idx = match self.history_index {
                        None => self.query_history.len() - 1,
                        Some(i) => i.saturating_sub(1),
                    };
                    self.history_index = Some(next_idx);
                    self.sql_input = self.query_history[next_idx].clone();
                    self.sql_cursor = self.sql_input.chars().count();
                    self.refresh_sql_assistant(false);
                }
            }
            KeyCode::Down => {
                let force_history = key.modifiers.contains(KeyModifiers::CONTROL);
                if self.show_suggestions && !self.sql_suggestions.is_empty() && !force_history {
                    let count = self.sql_suggestions.len();
                    self.suggestion_selected = (self.suggestion_selected + 1) % count;
                } else if let Some(i) = self.history_index {
                    if i + 1 < self.query_history.len() {
                        self.history_index = Some(i + 1);
                        self.sql_input = self.query_history[i + 1].clone();
                        self.sql_cursor = self.sql_input.chars().count();
                    } else {
                        self.history_index = None;
                    }
                    self.refresh_sql_assistant(false);
                } else {
                    // Pressing Down when popup is closed opens autocomplete suggestions
                    self.refresh_sql_assistant(true);
                }
            }
            KeyCode::Char(' ') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.refresh_sql_assistant(true);
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.sql_input.clear();
                self.sql_cursor = 0;
                self.refresh_sql_assistant(true);
            }
            KeyCode::Char(c) => {
                let mut chars: Vec<char> = self.sql_input.chars().collect();
                let pos = self.sql_cursor.min(chars.len());
                chars.insert(pos, c);
                self.sql_input = chars.into_iter().collect();
                self.sql_cursor = pos + 1;
                self.refresh_sql_assistant(true);
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
                self.refresh_sql_assistant(true);
            }
            KeyCode::Char('w') => {
                self.open_quick_filter_for_selected_cell();
            }
            KeyCode::Char('t') => {
                self.open_template_picker();
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
                    self.detail_scroll_offset = 0;
                    self.detail_cursor_line = 0;
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
                self.refresh_sql_assistant(true);
            }
            KeyCode::Char('t') => {
                self.open_template_picker();
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if count > 0 && self.schema_selected + 1 < count {
                    self.schema_selected += 1;
                }
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.schema_selected = self.schema_selected.saturating_sub(1);
            }
            KeyCode::Char('w') => {
                // Quick WHERE filter on the selected schema field
                if let Some((_, snippet)) = self.schema_entries.get(self.schema_selected) {
                    if !snippet.starts_with("SELECT ") {
                        let field_name = snippet.clone();
                        let first_table = self.engine.tables().first().copied();
                        let table_name = first_table
                            .map(|t| t.name.clone())
                            .unwrap_or_else(|| "data".to_string());
                        let sample_lit = first_table
                            .and_then(|t| {
                                collect_field_sample_values(t, &field_name, 1)
                                    .first()
                                    .map(format_sql_literal)
                            })
                            .unwrap_or_else(|| "'value'".to_string());
                        let cond = format!("{} = {}", field_name, sample_lit);
                        self.sql_input =
                            merge_where_condition(&self.sql_input, &table_name, &cond, true);
                        self.sql_cursor = self.sql_input.chars().count();
                        self.focus = FocusPane::SqlInput;
                        self.refresh_sql_assistant(true);
                    }
                }
            }
            KeyCode::Char('g') => {
                // Quick GROUP BY & Count on the selected schema field
                if let Some((_, snippet)) = self.schema_entries.get(self.schema_selected) {
                    if !snippet.starts_with("SELECT ") {
                        let field_name = snippet.clone();
                        let table_name = self
                            .engine
                            .tables()
                            .first()
                            .map(|t| t.name.clone())
                            .unwrap_or_else(|| "data".to_string());
                        self.sql_input = format!(
                            "SELECT {}, COUNT(*) AS count FROM {} GROUP BY {} ORDER BY count DESC LIMIT 50",
                            field_name, table_name, field_name
                        );
                        self.sql_cursor = self.sql_input.chars().count();
                        self.execute_current_sql();
                    }
                }
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
                        self.refresh_sql_assistant(true);
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
        let default_sql = if table.rows.len() > 1000 {
            format!("SELECT * FROM {} LIMIT 1000", table.name)
        } else {
            format!("SELECT * FROM {}", table.name)
        };
        items.push((header_line, default_sql));

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
pub fn draw_tui(frame: &mut Frame, app: &mut TuiApp) {
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

    // Floating autocomplete dropdown right below the SQL bar when focused
    if app.focus == FocusPane::SqlInput && app.show_suggestions && !app.sql_suggestions.is_empty() {
        draw_sql_autocomplete_popup(frame, app, right_chunks[0]);
    }

    if app.show_quick_filter_modal {
        draw_quick_filter_modal(frame, app);
    }
    if app.show_template_modal {
        draw_template_modal(frame, app);
    }
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
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD)
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
                .title(" Schema (Enter:Add  w:Where  g:Group) "),
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
            Constraint::Percentage(74), // SQL Input
            Constraint::Percentage(26), // Quick Filter Input
        ])
        .split(area);

    // 1. SQL Input box with live validation badge, syntax highlighting, and ghost text
    let sql_focused = app.focus == FocusPane::SqlInput;
    let validation_color = app.sql_validation.color();
    let sql_border = if sql_focused {
        Style::default()
            .fg(validation_color)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::DarkGray)
    };

    let validation_badge = app.sql_validation.badge_text();
    let title_line = match app.history_index {
        Some(idx) => Line::from(vec![
            Span::styled(
                format!(" SQL [History {}/{}] ", idx + 1, app.query_history.len()),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("│ {} ", validation_badge),
                Style::default().fg(validation_color),
            ),
        ]),
        None => Line::from(vec![
            Span::styled(
                " SQL (Enter:Run  Tab:Complete  Ctrl+T:Templates) ",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("│ {} ", validation_badge),
                Style::default().fg(validation_color),
            ),
        ]),
    };

    let sql_inner_w = top_chunks[0].width.saturating_sub(2) as usize;
    let sql_scroll_x = if sql_inner_w > 0 && app.sql_cursor >= sql_inner_w {
        (app.sql_cursor - sql_inner_w + 1) as u16
    } else {
        0
    };

    let active_ghost = if sql_focused && app.show_suggestions {
        app.sql_suggestions.get(app.suggestion_selected)
    } else {
        None
    };
    let highlighted_sql =
        highlight_sql_line(&app.sql_input, app.sql_cursor, active_ghost, &app.engine);

    let sql_para = Paragraph::new(highlighted_sql)
        .scroll((0, sql_scroll_x))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(sql_border)
                .title(title_line),
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
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD)
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
                .title(" Quick Row Search ('f') "),
        );
    frame.render_widget(filter_para, top_chunks[1]);

    if filter_focused {
        let visible_offset = filter_len.saturating_sub(filter_scroll_x as usize);
        let cx = top_chunks[1].x + 1 + (visible_offset.min(filter_inner_w) as u16);
        let cy = top_chunks[1].y + 1;
        frame.set_cursor_position((cx, cy));
    }
}

fn draw_sql_autocomplete_popup(frame: &mut Frame, app: &TuiApp, top_bar_area: Rect) {
    let max_items = app.sql_suggestions.len().min(8);
    if max_items == 0 {
        return;
    }

    let popup_height = (max_items as u16) + 2;
    let popup_width = top_bar_area.width.clamp(40, 78);
    let popup_x = top_bar_area.x;
    let popup_y = (top_bar_area.y + top_bar_area.height)
        .min(frame.area().height.saturating_sub(popup_height));
    let popup_area = Rect::new(popup_x, popup_y, popup_width, popup_height);

    let items: Vec<ListItem> = app
        .sql_suggestions
        .iter()
        .take(max_items)
        .map(|sug| {
            let cat_color = match sug.category {
                "field" => Color::LightCyan,
                "table" => Color::LightGreen,
                "operator" | "value" => Color::Yellow,
                "function" => Color::LightMagenta,
                "clause" | "template" => Color::LightBlue,
                _ => Color::Gray,
            };
            let line = Line::from(vec![
                Span::styled(
                    format!(" [{:^8}] ", sug.category),
                    Style::default().fg(cat_color).add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!("{:<24} ", sug.label),
                    Style::default()
                        .fg(Color::White)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(sug.description.clone(), Style::default().fg(Color::Gray)),
            ]);
            ListItem::new(line)
        })
        .collect();

    frame.render_widget(Clear, popup_area);
    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD))
                .title(" 💡 SQL Suggestions (Tab:Accept  Up/Down:Select  Esc:Dismiss  Ctrl+T:Templates) "),
        )
        .highlight_style(
            Style::default()
                .bg(Color::Rgb(40, 65, 95))
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("▸");

    let mut state = ListState::default();
    state.select(Some(app.suggestion_selected.min(max_items - 1)));
    frame.render_stateful_widget(list, popup_area, &mut state);
}

fn draw_error_banner(frame: &mut Frame, app: &TuiApp, area: Rect) {
    let msg = app.error_message.as_deref().unwrap_or("");
    let err_para = Paragraph::new(Span::styled(
        format!(" ⚠ {}", msg),
        Style::default()
            .fg(Color::LightRed)
            .add_modifier(Modifier::BOLD),
    ))
    .block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Red))
            .title(" SQL Error & Suggestion "),
    );
    frame.render_widget(err_para, area);
}

fn draw_result_table(frame: &mut Frame, app: &mut TuiApp, area: Rect) {
    let focused = app.focus == FocusPane::ResultTable;
    let border_style = if focused {
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::DarkGray)
    };

    let res = match &app.current_result {
        Some(r) if !r.columns.is_empty() => r,
        _ => {
            let empty_para = Paragraph::new("No rows or columns to display.").block(
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

    let header_cells = visible_cols_slice
        .iter()
        .enumerate()
        .map(|(offset, col_name)| {
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

    // Virtual window calculations: only construct widget rows for what fits on screen
    let total_vis_rows = app.visible_row_indices.len();
    let visible_capacity = (area.height.saturating_sub(4) as usize).max(1);

    if total_vis_rows > 0 {
        app.selected_row = app.selected_row.min(total_vis_rows - 1);
    } else {
        app.selected_row = 0;
    }

    if app.selected_row < app.row_scroll_offset {
        app.row_scroll_offset = app.selected_row;
    } else if app.selected_row >= app.row_scroll_offset + visible_capacity {
        app.row_scroll_offset = app.selected_row.saturating_sub(visible_capacity - 1);
    }

    let start_row = app.row_scroll_offset.min(total_vis_rows);
    let end_row = (start_row + visible_capacity).min(total_vis_rows);
    let window_row_indices = &app.visible_row_indices[start_row..end_row];

    let rows = window_row_indices
        .iter()
        .enumerate()
        .map(|(win_idx, &r_idx)| {
            let is_selected_row = start_row + win_idx == app.selected_row;
            let row_data = &res.rows[r_idx];
            let cells = (start_col..total_cols).map(|c_idx| {
                let val = row_data.get(c_idx).unwrap_or(&Value::Null);
                let is_selected_col = c_idx == app.selected_col;
                let is_active_cell = is_selected_row && is_selected_col;
                format_table_cell(val, is_selected_col, is_active_cell)
            });
            Row::new(cells).height(1)
        });

    // Compute dynamic column widths over visible window rows only (instantaneous)
    let num_vis_cols = visible_cols_slice.len().max(1);
    let mut constraints = Vec::with_capacity(num_vis_cols);
    for (offset, col_name) in visible_cols_slice.iter().enumerate() {
        let actual_col = start_col + offset;
        let mut max_w = col_name.chars().count() + 3;
        for &r_idx in window_row_indices {
            if let Some(val) = res.rows[r_idx].get(actual_col) {
                let len = value_to_table_cell_string(val).chars().count();
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
        " Results (Row {}/{} [visible {}-{}], col {}/{}){} — w:QuickFilter  t:Templates  s:Sort  f:Search  Enter:Inspect ",
        if total_vis_rows == 0 { 0 } else { app.selected_row + 1 },
        total_vis_rows,
        if total_vis_rows == 0 { 0 } else { start_row + 1 },
        end_row,
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
    if !window_row_indices.is_empty() {
        let relative_selected = app.selected_row.saturating_sub(start_row);
        table_state.select(Some(relative_selected));
    }
    frame.render_stateful_widget(table, area, &mut table_state);
}

/// Bounded writer that aborts serialization once `max_bytes` have been written,
/// preventing huge nested JSON payloads from being serialized in full just for a 36-char table cell.
struct BoundedWriter {
    buf: Vec<u8>,
    max_bytes: usize,
}

impl io::Write for BoundedWriter {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        let remaining = self.max_bytes.saturating_sub(self.buf.len());
        if remaining == 0 {
            return Err(io::Error::new(io::ErrorKind::WriteZero, "limit reached"));
        }
        let take = data.len().min(remaining);
        self.buf.extend_from_slice(&data[..take]);
        if take < data.len() {
            return Err(io::Error::new(io::ErrorKind::WriteZero, "limit reached"));
        }
        Ok(take)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Format a cell value for the TUI table, capping serialization of large nested objects/arrays at 64 bytes
/// and sanitizing control characters for safe terminal display.
fn value_to_table_cell_string(val: &Value) -> String {
    match val {
        Value::Array(_) | Value::Object(_) => {
            let mut writer = BoundedWriter {
                buf: Vec::with_capacity(64),
                max_bytes: 64,
            };
            let truncated = serde_json::to_writer(&mut writer, val).is_err();
            // Ensure valid UTF-8 boundary if truncated mid-character
            while !writer.buf.is_empty() && std::str::from_utf8(&writer.buf).is_err() {
                writer.buf.pop();
            }
            let mut s = String::from_utf8(writer.buf).unwrap_or_default();
            if truncated {
                s.push('…');
            }
            sanitize_terminal_text(&s)
        }
        Value::String(s) => {
            let safe = sanitize_terminal_text(s);
            if safe.len() > 64 {
                let mut out: String = safe.chars().take(60).collect();
                out.push('…');
                out
            } else {
                safe
            }
        }
        _ => value_to_display_string(val),
    }
}

/// Fast recursive case-insensitive filter match that avoids serializing nested JSON objects to strings.
fn value_matches_filter(val: &Value, needle_lower: &str) -> bool {
    value_matches_filter_bounded(val, needle_lower, 0)
}

fn value_matches_filter_bounded(val: &Value, needle_lower: &str, depth: usize) -> bool {
    if depth >= 64 {
        return false;
    }
    match val {
        Value::Null => "null".contains(needle_lower),
        Value::Bool(true) => "true".contains(needle_lower),
        Value::Bool(false) => "false".contains(needle_lower),
        Value::Number(n) => n.to_string().contains(needle_lower),
        Value::String(s) => str_contains_ignore_case(s, needle_lower),
        Value::Array(arr) => arr
            .iter()
            .any(|el| value_matches_filter_bounded(el, needle_lower, depth + 1)),
        Value::Object(map) => map.iter().any(|(k, v)| {
            str_contains_ignore_case(k, needle_lower)
                || value_matches_filter_bounded(v, needle_lower, depth + 1)
        }),
    }
}

#[inline]
fn str_contains_ignore_case(haystack: &str, needle_lower: &str) -> bool {
    if needle_lower.is_empty() {
        return true;
    }
    if haystack.is_ascii() && needle_lower.is_ascii() {
        let h_bytes = haystack.as_bytes();
        let n_bytes = needle_lower.as_bytes();
        if n_bytes.len() > h_bytes.len() {
            return false;
        }
        return h_bytes
            .windows(n_bytes.len())
            .any(|w| w.eq_ignore_ascii_case(n_bytes));
    }
    haystack.to_lowercase().contains(needle_lower)
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
            "/:SQL  w:QuickFilter  t:Templates  f:Search  s:Sort  c:Reset  Enter:JSON  ?:Help  q:Quit",
            Style::default().fg(Color::Yellow),
        ),
    ]);

    frame.render_widget(Paragraph::new(line), area);
}

fn draw_quick_filter_modal(frame: &mut Frame, app: &TuiApp) {
    if app.quick_filter_actions.is_empty() {
        return;
    }

    let area = centered_rect(76, 68, frame.area());
    frame.render_widget(Clear, area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(6),    // Action list
            Constraint::Length(4), // Preview of generated SQL
        ])
        .split(area);

    let items: Vec<ListItem> = app
        .quick_filter_actions
        .iter()
        .enumerate()
        .map(|(idx, act)| {
            let num_badge = if idx < 9 {
                format!(" [{}] ", idx + 1)
            } else {
                "     ".to_string()
            };
            let line = Line::from(vec![
                Span::styled(
                    num_badge,
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!("{:<44} ", act.title),
                    Style::default()
                        .fg(Color::White)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(act.description.clone(), Style::default().fg(Color::Gray)),
            ]);
            ListItem::new(line)
        })
        .collect();

    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD))
                .title(" ⚡ Quick SQL Filter & Actions for Selected Cell (Enter/1-9:Run  e:Edit in SQL Bar  Esc:Cancel) "),
        )
        .highlight_style(
            Style::default()
                .bg(Color::Rgb(40, 65, 95))
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("▶ ");

    let mut state = ListState::default();
    state.select(Some(app.quick_filter_selected));
    frame.render_stateful_widget(list, chunks[0], &mut state);

    let preview_sql = app
        .quick_filter_actions
        .get(app.quick_filter_selected)
        .map(|a| a.sql.as_str())
        .unwrap_or("");
    let preview_line = highlight_sql_line(preview_sql, preview_sql.len(), None, &app.engine);
    let preview_para = Paragraph::new(preview_line)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Yellow))
                .title(" Generated SQL Preview (Press Enter to execute, or 'e' to customize in SQL bar) "),
        )
        .wrap(Wrap { trim: false });
    frame.render_widget(preview_para, chunks[1]);
}

fn draw_template_modal(frame: &mut Frame, app: &TuiApp) {
    if app.sql_templates.is_empty() {
        return;
    }

    let area = centered_rect(82, 76, frame.area());
    frame.render_widget(Clear, area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(8),    // Recipe list
            Constraint::Length(5), // SQL + Explanation preview
        ])
        .split(area);

    let items: Vec<ListItem> = app
        .sql_templates
        .iter()
        .enumerate()
        .map(|(idx, tpl)| {
            let num_badge = if idx < 9 {
                format!(" [{}] ", idx + 1)
            } else {
                "     ".to_string()
            };
            let line = Line::from(vec![
                Span::styled(
                    num_badge,
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!("[{:^11}] ", tpl.category),
                    Style::default()
                        .fg(Color::LightCyan)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    tpl.title.clone(),
                    Style::default()
                        .fg(Color::White)
                        .add_modifier(Modifier::BOLD),
                ),
            ]);
            ListItem::new(line)
        })
        .collect();

    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(
                    Style::default()
                        .fg(Color::LightMagenta)
                        .add_modifier(Modifier::BOLD),
                )
                .title(" 📖 SQL Recipe & Cheat-Sheet Picker (Enter/1-9:Load into SQL Editor  r:Run Immediately  Esc:Close) "),
        )
        .highlight_style(
            Style::default()
                .bg(Color::Rgb(55, 45, 85))
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("▶ ");

    let mut state = ListState::default();
    state.select(Some(app.template_selected));
    frame.render_stateful_widget(list, chunks[0], &mut state);

    if let Some(selected_tpl) = app.sql_templates.get(app.template_selected) {
        let sql_line =
            highlight_sql_line(&selected_tpl.sql, selected_tpl.sql.len(), None, &app.engine);
        let exp_line = Line::from(vec![
            Span::styled(" 💡 ", Style::default().fg(Color::Yellow)),
            Span::styled(
                selected_tpl.explanation.clone(),
                Style::default().fg(Color::Gray),
            ),
        ]);
        let preview_para = Paragraph::new(vec![sql_line, Line::from(""), exp_line])
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::Cyan))
                    .title(" Template SQL & Syntax Guide "),
            )
            .wrap(Wrap { trim: false });
        frame.render_widget(preview_para, chunks[1]);
    }
}

fn format_table_cell(val: &Value, is_selected_col: bool, is_active_cell: bool) -> Cell<'static> {
    let extra_mod = if is_selected_col {
        Modifier::BOLD | Modifier::UNDERLINED
    } else {
        Modifier::empty()
    };
    let text = value_to_table_cell_string(val);
    let line = match val {
        Value::Null => Line::from(Span::styled(
            text,
            Style::default().fg(Color::DarkGray).add_modifier(extra_mod),
        )),
        Value::Bool(_) => Line::from(Span::styled(
            text,
            Style::default()
                .fg(Color::LightMagenta)
                .add_modifier(Modifier::BOLD | extra_mod),
        )),
        Value::Number(_) => Line::from(Span::styled(
            text,
            Style::default()
                .fg(Color::LightCyan)
                .add_modifier(extra_mod),
        )),
        Value::String(_) => Line::from(Span::styled(
            text,
            Style::default()
                .fg(Color::LightGreen)
                .add_modifier(extra_mod),
        )),
        Value::Array(_) | Value::Object(_) => {
            Line::from(highlight_json_line_spans(&text, extra_mod))
        }
    };
    let cell = Cell::from(line);
    if is_active_cell {
        cell.style(Style::default().bg(Color::Rgb(55, 78, 115)))
    } else {
        cell
    }
}

/// Syntax-highlight a single line of JSON (pretty-printed or compact inline) using the RustQL theme:
/// - Fold indicators (`▾` / `▸`): `Yellow` (bold)
/// - Object keys: `Cyan` (bold)
/// - String values: `LightGreen`
/// - Numeric values: `LightCyan`
/// - Booleans (`true`/`false`): `LightMagenta` (bold)
/// - `null` / `NULL`: `DarkGray`
/// - Braces, brackets, colons, commas: `Gray`
pub fn highlight_json_line_spans(line: &str, extra_modifier: Modifier) -> Vec<Span<'static>> {
    let chars: Vec<char> = line.chars().collect();
    let n = chars.len();
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut i = 0;

    while i < n {
        let c = chars[i];

        // 1. Whitespace
        if c.is_whitespace() {
            let start = i;
            while i < n && chars[i].is_whitespace() {
                i += 1;
            }
            let ws: String = chars[start..i].iter().collect();
            spans.push(Span::styled(
                ws,
                Style::default().add_modifier(extra_modifier),
            ));
            continue;
        }

        // 2. Fold arrows (▾ expanded, ▸ collapsed)
        if c == '▾' || c == '▸' {
            i += 1;
            spans.push(Span::styled(
                c.to_string(),
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD | extra_modifier),
            ));
            continue;
        }

        // 3. Quoted JSON string (either object key or string value)
        if c == '"' {
            let start = i;
            i += 1;
            let mut escaped = false;
            while i < n {
                let ch = chars[i];
                i += 1;
                if escaped {
                    escaped = false;
                } else if ch == '\\' {
                    escaped = true;
                } else if ch == '"' {
                    break;
                }
            }
            let mut lookahead = i;
            while lookahead < n && chars[lookahead].is_whitespace() {
                lookahead += 1;
            }
            let is_key = lookahead < n && chars[lookahead] == ':';
            let token: String = chars[start..i].iter().collect();
            let style = if is_key {
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD | extra_modifier)
            } else {
                Style::default()
                    .fg(Color::LightGreen)
                    .add_modifier(extra_modifier)
            };
            spans.push(Span::styled(token, style));
            continue;
        }

        // 4. Numeric literal
        if c.is_ascii_digit() || (c == '-' && i + 1 < n && chars[i + 1].is_ascii_digit()) {
            let start = i;
            i += 1;
            while i < n
                && (chars[i].is_ascii_digit() || matches!(chars[i], '.' | 'e' | 'E' | '+' | '-'))
            {
                i += 1;
            }
            let num_str: String = chars[start..i].iter().collect();
            spans.push(Span::styled(
                num_str,
                Style::default()
                    .fg(Color::LightCyan)
                    .add_modifier(extra_modifier),
            ));
            continue;
        }

        // 5. Keywords / literals (true, false, null, NULL, field/fields/item/items)
        if c.is_alphabetic() {
            let start = i;
            while i < n && chars[i].is_alphanumeric() {
                i += 1;
            }
            let word: String = chars[start..i].iter().collect();
            let style = match word.as_str() {
                "true" | "false" => Style::default()
                    .fg(Color::LightMagenta)
                    .add_modifier(Modifier::BOLD | extra_modifier),
                "null" | "NULL" => Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(extra_modifier),
                "field" | "fields" | "item" | "items" => Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::ITALIC | extra_modifier),
                _ => Style::default()
                    .fg(Color::White)
                    .add_modifier(extra_modifier),
            };
            spans.push(Span::styled(word, style));
            continue;
        }

        // 6. Braces, brackets, colons, commas, ellipsis
        let start = i;
        i += 1;
        let punct: String = chars[start..i].iter().collect();
        let color = if c == '…' && i < n {
            Color::Yellow
        } else {
            Color::Gray
        };
        spans.push(Span::styled(
            punct,
            Style::default().fg(color).add_modifier(extra_modifier),
        ));
    }

    spans
}

/// A single rendered line inside the Row & Cell JSON Detail Inspector modal,
/// paired with optional collapsible JSON node metadata for cursor-based folding.
#[derive(Debug, Clone)]
pub struct DetailModalEntry {
    pub line: Line<'static>,
    /// Node path toggled when pressing Space/Enter on this line.
    pub toggle_path: Option<String>,
    /// Node path whose header line the cursor should snap to when collapsing from a child/closing line.
    pub snap_to_path: Option<String>,
    /// Whether this line is the primary opening/collapsed header line of `toggle_path`.
    pub is_node_header: bool,
}

fn escape_json_pointer(token: &str) -> String {
    token.replace('~', "~0").replace('/', "~1")
}

fn is_collapsible_json_container(val: &Value) -> bool {
    match val {
        Value::Object(map) => !map.is_empty(),
        Value::Array(arr) => !arr.is_empty(),
        _ => false,
    }
}

/// Maximum recursion depth for rendering collapsible JSON nodes in the Detail Inspector.
const MAX_JSON_RENDER_DEPTH: usize = 64;

#[allow(clippy::too_many_arguments)]
fn push_collapsible_json_value(
    out: &mut Vec<DetailModalEntry>,
    val: &Value,
    path: &str,
    key_prefix: Option<&str>,
    indent_depth: usize,
    trailing_comma: bool,
    collapsed_paths: &BTreeSet<String>,
    enclosing_nested_path: Option<&str>,
) {
    let indent = "  ".repeat(indent_depth.min(MAX_JSON_RENDER_DEPTH));
    let comma = if trailing_comma { "," } else { "" };
    let key_part = match key_prefix {
        Some(k) => {
            let escaped = serde_json::to_string(k).unwrap_or_else(|_| format!("\"{}\"", k));
            format!("{}: ", escaped)
        }
        None => String::new(),
    };

    if indent_depth >= MAX_JSON_RENDER_DEPTH {
        let text = format!(
            "{}  {}{}\"<max depth reached>\"{}",
            indent, key_part, "", comma
        );
        out.push(DetailModalEntry {
            line: Line::from(highlight_json_line_spans(&text, Modifier::empty())),
            toggle_path: enclosing_nested_path.map(str::to_string),
            snap_to_path: enclosing_nested_path.map(str::to_string),
            is_node_header: false,
        });
        return;
    }

    match val {
        Value::Object(map) => {
            if map.is_empty() {
                let text = format!("{}  {}{}{{}}{}", indent, key_part, "", comma);
                out.push(DetailModalEntry {
                    line: Line::from(highlight_json_line_spans(&text, Modifier::empty())),
                    toggle_path: enclosing_nested_path.map(str::to_string),
                    snap_to_path: enclosing_nested_path.map(str::to_string),
                    is_node_header: false,
                });
            } else if collapsed_paths.contains(path) {
                let count = map.len();
                let noun = if count == 1 { "field" } else { "fields" };
                let text = format!(
                    "{}▸ {}{{ … {} {} }}{}",
                    indent, key_part, count, noun, comma
                );
                out.push(DetailModalEntry {
                    line: Line::from(highlight_json_line_spans(&text, Modifier::empty())),
                    toggle_path: Some(path.to_string()),
                    snap_to_path: Some(path.to_string()),
                    is_node_header: true,
                });
            } else {
                let open_text = format!("{}▾ {}{{", indent, key_part);
                out.push(DetailModalEntry {
                    line: Line::from(highlight_json_line_spans(&open_text, Modifier::empty())),
                    toggle_path: Some(path.to_string()),
                    snap_to_path: Some(path.to_string()),
                    is_node_header: true,
                });
                let child_enclosing = if indent_depth >= 1 { Some(path) } else { None };
                let len = map.len();
                for (idx, (k, v)) in map.iter().enumerate() {
                    let child_comma = idx + 1 < len;
                    let child_path = format!("{}/{}", path, escape_json_pointer(k));
                    push_collapsible_json_value(
                        out,
                        v,
                        &child_path,
                        Some(k),
                        indent_depth + 1,
                        child_comma,
                        collapsed_paths,
                        child_enclosing,
                    );
                }
                let close_text = format!("{}  }}{}", indent, comma);
                out.push(DetailModalEntry {
                    line: Line::from(highlight_json_line_spans(&close_text, Modifier::empty())),
                    toggle_path: Some(path.to_string()),
                    snap_to_path: Some(path.to_string()),
                    is_node_header: false,
                });
            }
        }
        Value::Array(arr) => {
            if arr.is_empty() {
                let text = format!("{}  {}[]{}", indent, key_part, comma);
                out.push(DetailModalEntry {
                    line: Line::from(highlight_json_line_spans(&text, Modifier::empty())),
                    toggle_path: enclosing_nested_path.map(str::to_string),
                    snap_to_path: enclosing_nested_path.map(str::to_string),
                    is_node_header: false,
                });
            } else if collapsed_paths.contains(path) {
                let count = arr.len();
                let noun = if count == 1 { "item" } else { "items" };
                let text = format!("{}▸ {}[ … {} {} ]{}", indent, key_part, count, noun, comma);
                out.push(DetailModalEntry {
                    line: Line::from(highlight_json_line_spans(&text, Modifier::empty())),
                    toggle_path: Some(path.to_string()),
                    snap_to_path: Some(path.to_string()),
                    is_node_header: true,
                });
            } else {
                let open_text = format!("{}▾ {}[", indent, key_part);
                out.push(DetailModalEntry {
                    line: Line::from(highlight_json_line_spans(&open_text, Modifier::empty())),
                    toggle_path: Some(path.to_string()),
                    snap_to_path: Some(path.to_string()),
                    is_node_header: true,
                });
                let child_enclosing = if indent_depth >= 1 { Some(path) } else { None };
                let len = arr.len();
                for (idx, v) in arr.iter().enumerate() {
                    let child_comma = idx + 1 < len;
                    let child_path = format!("{}/{}", path, idx);
                    push_collapsible_json_value(
                        out,
                        v,
                        &child_path,
                        None,
                        indent_depth + 1,
                        child_comma,
                        collapsed_paths,
                        child_enclosing,
                    );
                }
                let close_text = format!("{}  ]{}", indent, comma);
                out.push(DetailModalEntry {
                    line: Line::from(highlight_json_line_spans(&close_text, Modifier::empty())),
                    toggle_path: Some(path.to_string()),
                    snap_to_path: Some(path.to_string()),
                    is_node_header: false,
                });
            }
        }
        _ => {
            let val_str = serde_json::to_string(val).unwrap_or_default();
            let text = if indent_depth == 0 && key_prefix.is_none() {
                val_str
            } else {
                format!("{}  {}{}{}", indent, key_part, val_str, comma)
            };
            out.push(DetailModalEntry {
                line: Line::from(highlight_json_line_spans(&text, Modifier::empty())),
                toggle_path: enclosing_nested_path.map(str::to_string),
                snap_to_path: enclosing_nested_path.map(str::to_string),
                is_node_header: false,
            });
        }
    }
}

fn collect_json_collapsible_paths_recursive(
    val: &Value,
    path: &str,
    depth: usize,
    field_paths: &mut Vec<String>,
    all_paths: &mut Vec<String>,
) {
    if depth >= MAX_JSON_RENDER_DEPTH {
        return;
    }
    match val {
        Value::Object(map) if !map.is_empty() => {
            all_paths.push(path.to_string());
            if depth >= 1 {
                field_paths.push(path.to_string());
            }
            for (k, v) in map {
                let child_path = format!("{}/{}", path, escape_json_pointer(k));
                collect_json_collapsible_paths_recursive(
                    v,
                    &child_path,
                    depth + 1,
                    field_paths,
                    all_paths,
                );
            }
        }
        Value::Array(arr) if !arr.is_empty() => {
            all_paths.push(path.to_string());
            if depth >= 1 {
                field_paths.push(path.to_string());
            }
            for (idx, v) in arr.iter().enumerate() {
                let child_path = format!("{}/{}", path, idx);
                collect_json_collapsible_paths_recursive(
                    v,
                    &child_path,
                    depth + 1,
                    field_paths,
                    all_paths,
                );
            }
        }
        _ => {}
    }
}

/// Collect `(field_collapsible_paths, all_collapsible_paths)` for the active row & cell in the Detail Inspector.
fn collect_detail_collapsible_paths(app: &TuiApp) -> (Vec<String>, Vec<String>) {
    let mut field_paths = Vec::new();
    let mut all_paths = Vec::new();

    let res = match &app.current_result {
        Some(r) => r,
        None => return (field_paths, all_paths),
    };
    let &actual_row_idx = match app.visible_row_indices.get(app.selected_row) {
        Some(i) => i,
        None => return (field_paths, all_paths),
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
    let cell_root_path = format!("cell:{}", escape_json_pointer(&selected_col_name));

    match &selected_val {
        Value::String(s) => {
            if let Some(embedded_json) = try_parse_embedded_json(s) {
                collect_json_collapsible_paths_recursive(
                    &embedded_json,
                    &cell_root_path,
                    0,
                    &mut field_paths,
                    &mut all_paths,
                );
            }
        }
        _ => {
            collect_json_collapsible_paths_recursive(
                &selected_val,
                &cell_root_path,
                0,
                &mut field_paths,
                &mut all_paths,
            );
        }
    }

    collect_json_collapsible_paths_recursive(
        &Value::Object(row_obj),
        "row",
        0,
        &mut field_paths,
        &mut all_paths,
    );

    (field_paths, all_paths)
}

/// Try to parse an embedded JSON object or array from a string cell value.
fn try_parse_embedded_json(s: &str) -> Option<Value> {
    let trimmed = s.trim();
    let looks_like_json_container = (trimmed.starts_with('{') && trimmed.ends_with('}'))
        || (trimmed.starts_with('[') && trimmed.ends_with(']'));
    if !looks_like_json_container {
        return None;
    }
    match serde_json::from_str::<Value>(trimmed) {
        Ok(parsed @ (Value::Object(_) | Value::Array(_))) => Some(parsed),
        _ => None,
    }
}

/// Build the structured line entries (with collapsible node metadata) for the Row & Cell JSON Detail Inspector modal.
pub fn build_detail_modal_entries(app: &TuiApp) -> Vec<DetailModalEntry> {
    let res = match &app.current_result {
        Some(r) => r,
        None => return Vec::new(),
    };
    let &actual_row_idx = match app.visible_row_indices.get(app.selected_row) {
        Some(i) => i,
        None => return Vec::new(),
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
    let cell_root_path = format!("cell:{}", escape_json_pointer(&selected_col_name));

    let mut entries: Vec<DetailModalEntry> = Vec::new();

    match &selected_val {
        Value::String(s) => {
            if let Some(embedded_json) = try_parse_embedded_json(s) {
                let root_toggle =
                    is_collapsible_json_container(&embedded_json).then(|| cell_root_path.clone());
                entries.push(DetailModalEntry {
                    line: Line::from(vec![
                        Span::styled(
                            format!("Selected Cell [{}]", selected_col_name),
                            Style::default()
                                .fg(Color::Yellow)
                                .add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(
                            " (JSON string):",
                            Style::default()
                                .fg(Color::LightCyan)
                                .add_modifier(Modifier::BOLD),
                        ),
                    ]),
                    toggle_path: root_toggle.clone(),
                    snap_to_path: root_toggle,
                    is_node_header: false,
                });
                push_collapsible_json_value(
                    &mut entries,
                    &embedded_json,
                    &cell_root_path,
                    None,
                    0,
                    false,
                    &app.detail_collapsed_paths,
                    None,
                );
            } else if s.contains('\n') {
                entries.push(DetailModalEntry {
                    line: Line::from(Span::styled(
                        format!("Selected Cell [{}]:", selected_col_name),
                        Style::default()
                            .fg(Color::Yellow)
                            .add_modifier(Modifier::BOLD),
                    )),
                    toggle_path: None,
                    snap_to_path: None,
                    is_node_header: false,
                });
                for raw_line in s.lines() {
                    entries.push(DetailModalEntry {
                        line: Line::from(Span::styled(
                            sanitize_terminal_text(raw_line),
                            Style::default().fg(Color::LightGreen),
                        )),
                        toggle_path: None,
                        snap_to_path: None,
                        is_node_header: false,
                    });
                }
            } else {
                entries.push(DetailModalEntry {
                    line: Line::from(Span::styled(
                        format!("Selected Cell [{}]:", selected_col_name),
                        Style::default()
                            .fg(Color::Yellow)
                            .add_modifier(Modifier::BOLD),
                    )),
                    toggle_path: None,
                    snap_to_path: None,
                    is_node_header: false,
                });
                push_collapsible_json_value(
                    &mut entries,
                    &selected_val,
                    &cell_root_path,
                    None,
                    0,
                    false,
                    &app.detail_collapsed_paths,
                    None,
                );
            }
        }
        _ => {
            let root_toggle =
                is_collapsible_json_container(&selected_val).then(|| cell_root_path.clone());
            entries.push(DetailModalEntry {
                line: Line::from(Span::styled(
                    format!("Selected Cell [{}]:", selected_col_name),
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                )),
                toggle_path: root_toggle.clone(),
                snap_to_path: root_toggle,
                is_node_header: false,
            });
            push_collapsible_json_value(
                &mut entries,
                &selected_val,
                &cell_root_path,
                None,
                0,
                false,
                &app.detail_collapsed_paths,
                None,
            );
        }
    }

    entries.push(DetailModalEntry {
        line: Line::from(""),
        toggle_path: None,
        snap_to_path: None,
        is_node_header: false,
    });

    let row_val = Value::Object(row_obj);
    let row_toggle = is_collapsible_json_container(&row_val).then(|| "row".to_string());
    entries.push(DetailModalEntry {
        line: Line::from(Span::styled(
            format!("Full Row JSON (Row #{}):", actual_row_idx + 1),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )),
        toggle_path: row_toggle.clone(),
        snap_to_path: row_toggle,
        is_node_header: false,
    });
    push_collapsible_json_value(
        &mut entries,
        &row_val,
        "row",
        None,
        0,
        false,
        &app.detail_collapsed_paths,
        None,
    );

    entries
}

/// Build the syntax-highlighted lines for the Row & Cell JSON Detail Inspector modal.
pub fn build_detail_modal_lines(app: &TuiApp) -> Vec<Line<'static>> {
    build_detail_modal_entries(app)
        .into_iter()
        .map(|e| e.line)
        .collect()
}

/// Compute the visual wrapped height of a single [`Line`] at `wrap_width`.
fn line_wrapped_height(line: &Line<'_>, wrap_width: u16) -> usize {
    let width = (wrap_width as usize).max(1);
    let char_len: usize = line.spans.iter().map(|s| s.content.chars().count()).sum();
    if char_len == 0 {
        1
    } else {
        char_len.div_ceil(width)
    }
}

/// Compute the total number of visual lines after soft-wrapping `lines` to `wrap_width` columns.
fn count_wrapped_lines(lines: &[Line<'_>], wrap_width: u16) -> usize {
    lines
        .iter()
        .map(|line| line_wrapped_height(line, wrap_width))
        .sum::<usize>()
        .max(1)
}

fn draw_detail_modal(frame: &mut Frame, app: &mut TuiApp) {
    let entries = build_detail_modal_entries(app);
    if entries.is_empty() {
        return;
    }

    let area = centered_rect(70, 75, frame.area());
    frame.render_widget(Clear, area);

    let total_entries = entries.len();
    app.detail_cursor_line = app.detail_cursor_line.min(total_entries - 1);

    let inner_width = area.width.saturating_sub(2).max(1);
    let inner_height = (area.height.saturating_sub(2) as usize).max(1);

    let mut cursor_visual_start = 0usize;
    let mut cursor_visual_height = 1usize;
    let mut total_visual_lines = 0usize;

    for (idx, entry) in entries.iter().enumerate() {
        let h = line_wrapped_height(&entry.line, inner_width);
        if idx == app.detail_cursor_line {
            cursor_visual_start = total_visual_lines;
            cursor_visual_height = h;
        }
        total_visual_lines += h;
    }
    total_visual_lines = total_visual_lines.max(1);
    let cursor_visual_end = cursor_visual_start + cursor_visual_height;
    let max_scroll = total_visual_lines.saturating_sub(inner_height);

    if cursor_visual_start < app.detail_scroll_offset {
        app.detail_scroll_offset = cursor_visual_start;
    } else if cursor_visual_height <= inner_height {
        if cursor_visual_end > app.detail_scroll_offset + inner_height {
            app.detail_scroll_offset = cursor_visual_end.saturating_sub(inner_height);
        }
    } else if cursor_visual_start >= app.detail_scroll_offset + inner_height {
        app.detail_scroll_offset = cursor_visual_start;
    }

    app.detail_scroll_offset = app.detail_scroll_offset.min(max_scroll);
    let scroll_y = app.detail_scroll_offset.min(u16::MAX as usize) as u16;

    let highlight_bg = Color::Rgb(45, 68, 102);
    let lines: Vec<Line<'static>> = entries
        .into_iter()
        .enumerate()
        .map(|(idx, entry)| {
            if idx == app.detail_cursor_line {
                let spans: Vec<Span<'static>> = if entry.line.spans.is_empty()
                    || entry.line.spans.iter().all(|s| s.content.is_empty())
                {
                    vec![Span::styled(" ", Style::default().bg(highlight_bg))]
                } else {
                    entry
                        .line
                        .spans
                        .into_iter()
                        .map(|mut s| {
                            s.style = s.style.bg(highlight_bg);
                            s
                        })
                        .collect()
                };
                Line::from(spans).style(Style::default().bg(highlight_bg))
            } else {
                entry.line
            }
        })
        .collect();

    let fold_badge = if app.detail_collapsed_paths.is_empty() {
        String::new()
    } else {
        format!(" [{} collapsed]", app.detail_collapsed_paths.len())
    };
    let title = format!(
        " Row & Cell JSON Inspector [Line {}/{}{}] (j/k:Move  Space/Enter:Fold  a:FoldAll  h/l:Col  Esc/q:Close) ",
        app.detail_cursor_line + 1,
        total_entries,
        fold_badge
    );

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )
        .title(title);
    let para = Paragraph::new(lines)
        .block(block)
        .wrap(Wrap { trim: false })
        .scroll((scroll_y, 0));
    frame.render_widget(para, area);
}

fn draw_help_modal(frame: &mut Frame) {
    let help_text = "\
Keyboard Shortcuts & Guided SQL Assistant:
──────────────────────────────────────────────────────────────────────
  / or e            Focus SQL Query bar (with live syntax check & autocomplete)
  Tab (in SQL)      Accept highlighted autocomplete suggestion (or switch pane)
  Up / Down (SQL)   Cycle autocomplete suggestions (or Ctrl+Up/Down for history)
  Ctrl+Space (SQL)  Open autocomplete suggestions at cursor
  Enter (in SQL)    Execute SQL query
  Ctrl+U (in SQL)   Clear SQL input line

  w (in Table)      ⚡ Quick SQL Filter menu for the currently selected cell!
                    (Filter = value, != value, ILIKE, >=, <=, GROUP BY column)
  t or Ctrl+T       📖 Open SQL Recipe & Template Picker (pre-filled with schema)
  w / g (in Schema) Add WHERE filter ('w') or run GROUP BY count ('g') on field
  Enter (in Schema) Run SELECT * on table or insert field path into SQL

  Arrows / hjkl     Navigate rows and columns in Result Table
  PageUp / PageDown Scroll 15 rows up or down
  Home/g / End/G    Jump to first / last row
  s                 Sort Result Table by selected column (Asc → Desc → Off)
  f                 Focus Quick Row Search input (live text search)
  c                 Clear active row search and column sort
  Enter or d        Open JSON Detail Inspector for current row & cell
                    (In Inspector: j/k:Move  Space/Enter:Fold  a:FoldAll  h/l:Col)
  ? or F1           Toggle this Help modal
  q or Ctrl+C       Quit RustQL";

    let area = centered_rect(74, 76, frame.area());
    frame.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )
        .title(" RustQL Help (Press Esc, Enter, or ? to close) ");
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

/// RAII guard that ensures raw mode and alternate screen are always exited cleanly,
/// even if an error or unexpected panic occurs during the TUI event loop.
struct TerminalRestoreGuard;

impl Drop for TerminalRestoreGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen, crossterm::cursor::Show);
    }
}

/// Launch the interactive TUI in the user's terminal.
pub fn run_tui(engine: QueryEngine) -> Result<(), Box<dyn std::error::Error>> {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |panic_info| {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen, crossterm::cursor::Show);
        default_hook(panic_info);
    }));

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let _restore_guard = TerminalRestoreGuard;

    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut app = TuiApp::new(engine);
    let res = run_event_loop(&mut terminal, &mut app);

    terminal.show_cursor()?;
    res?;
    Ok(())
}

fn run_event_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut TuiApp,
) -> io::Result<()> {
    let mut needs_redraw = true;
    loop {
        if needs_redraw {
            terminal.draw(|f| draw_tui(f, app))?;
            needs_redraw = false;
        }

        if event::poll(Duration::from_millis(250))? {
            match event::read()? {
                Event::Key(key) => {
                    app.handle_key_event(key);
                    needs_redraw = true;
                }
                Event::Resize(_, _) => {
                    needs_redraw = true;
                }
                _ => {}
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
        terminal.draw(|f| draw_tui(f, &mut app)).unwrap();

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
        terminal.draw(|f| draw_tui(f, &mut app)).unwrap();
    }

    #[test]
    fn test_tui_render_speed_large_dataset() {
        use std::time::Instant;
        let mut engine = QueryEngine::new();
        let row_count = 50_000;
        let mut rows = Vec::with_capacity(row_count);
        for i in 0..row_count {
            let mut obj = serde_json::Map::new();
            obj.insert("id".to_string(), serde_json::Value::from(i));
            obj.insert(
                "name".to_string(),
                serde_json::Value::from(format!("user_{}", i)),
            );
            obj.insert(
                "city".to_string(),
                serde_json::Value::from(if i % 2 == 0 { "Berlin" } else { "Tokyo" }),
            );
            rows.push(obj);
        }
        let table = crate::loader::LoadedTable {
            name: "big_data".to_string(),
            source_path: std::path::PathBuf::from("/tmp/big_data.ndjson"),
            shape: crate::loader::JsonShape::Ndjson,
            schema_fields: Vec::new(),
            rows: std::sync::Arc::new(rows),
            columns: vec!["id".to_string(), "name".to_string(), "city".to_string()],
            loaded_from_cache: false,
        };
        engine.register_table(table);

        // 1. Verify TuiApp::new startup with automatic LIMIT 1000 is instantaneous (< 50ms)
        let t_init = Instant::now();
        let mut app = TuiApp::new(engine);
        let init_elapsed = t_init.elapsed();
        assert_eq!(app.visible_row_indices.len(), 1000);
        assert!(
            init_elapsed.as_millis() < 100,
            "TuiApp::new took too long on 50k rows: {:?}",
            init_elapsed
        );

        // 2. Execute SELECT * without limit (materializing all 50,000 rows) and render 100 scrolling frames
        app.sql_input = "SELECT * FROM big_data".to_string();
        app.execute_current_sql();
        assert_eq!(app.visible_row_indices.len(), row_count);

        let backend = TestBackend::new(120, 35);
        let mut terminal = Terminal::new(backend).unwrap();

        let t0 = Instant::now();
        for step in 0..100 {
            terminal.draw(|f| draw_tui(f, &mut app)).unwrap();
            let key = match step {
                50 => KeyCode::End,  // Jump to row 50,000
                75 => KeyCode::Home, // Jump back to row 1
                _ => KeyCode::Down,
            };
            app.handle_key_event(KeyEvent::new(key, KeyModifiers::NONE));
        }
        let render_elapsed = t0.elapsed();
        assert!(
            render_elapsed.as_millis() < 500,
            "100 frames over 50k rows took too long: {:?}",
            render_elapsed
        );

        // 3. Verify live row filtering over 50,000 rows is fast
        let t_filter = Instant::now();
        app.filter_input = "Berlin".to_string();
        app.recompute_visible_rows();
        let filter_elapsed = t_filter.elapsed();
        assert_eq!(app.visible_row_indices.len(), 25_000);
        assert!(
            filter_elapsed.as_millis() < 300,
            "Filtering 50k rows took too long: {:?}",
            filter_elapsed
        );

        // 4. Verify GROUP BY over 50,000 rows is fast (zero-copy EvalRow + hash grouping)
        let t_group = Instant::now();
        app.filter_input.clear();
        app.sql_input =
            "SELECT city, COUNT(*) AS cnt FROM big_data GROUP BY city ORDER BY cnt DESC"
                .to_string();
        app.execute_current_sql();
        let group_elapsed = t_group.elapsed();
        assert_eq!(app.visible_row_indices.len(), 2);
        assert!(
            group_elapsed.as_millis() < 300,
            "GROUP BY over 50k rows took too long: {:?}",
            group_elapsed
        );
    }

    #[test]
    fn test_tui_autocomplete_quick_filter_and_templates() {
        let tmp = TempDir::new().unwrap();
        let mut engine = QueryEngine::new();
        let (name, path) = parse_file_spec("examples/users.json");
        let table = load_json_file(&name, &path, Some(tmp.path()), false).unwrap();
        engine.register_table(table);

        let mut app = TuiApp::new(engine);
        let backend = TestBackend::new(120, 35);
        let mut terminal = Terminal::new(backend).unwrap();

        // 1. Test Quick-Filter modal ('w') on row 0, col 1 ('name' = 'Alice Vance')
        app.selected_row = 0;
        app.selected_col = 1;
        app.handle_key_event(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::NONE));
        assert!(app.show_quick_filter_modal);
        assert!(!app.quick_filter_actions.is_empty());
        terminal.draw(|f| draw_tui(f, &mut app)).unwrap();

        // Press Enter to apply the first quick filter ("name = 'Alice Vance'")
        app.handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(!app.show_quick_filter_modal);
        assert_eq!(app.visible_row_indices.len(), 1);
        assert!(app.sql_input.contains("WHERE name = 'Alice Vance'"));

        // 2. Test SQL Template Picker modal ('t')
        app.handle_key_event(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::NONE));
        assert!(app.show_template_modal);
        terminal.draw(|f| draw_tui(f, &mut app)).unwrap();

        // Press 'r' to run the first template immediately
        app.handle_key_event(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE));
        assert!(!app.show_template_modal);
        assert!(app.error_message.is_none());

        // 3. Test live SQL autocomplete & Tab completion in SQL input bar
        app.handle_key_event(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE));
        assert_eq!(app.focus, FocusPane::SqlInput);
        app.handle_key_event(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        for ch in "SELECT * FROM users WH".chars() {
            app.handle_key_event(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE));
        }
        assert!(app.show_suggestions);
        terminal.draw(|f| draw_tui(f, &mut app)).unwrap();

        // Press Tab to accept "WHERE " completion
        app.handle_key_event(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(app.sql_input, "SELECT * FROM users WHERE ");
    }

    #[test]
    fn test_detail_modal_scroll_and_json_color_coding() {
        let mut engine = QueryEngine::new();
        let mut obj = serde_json::Map::new();
        obj.insert("id".to_string(), serde_json::Value::from(1));
        // Large nested JSON array in cell
        let large_items: Vec<serde_json::Value> = (0..60)
            .map(|i| {
                serde_json::json!({
                    "index": i,
                    "label": format!("item_description_{}", i),
                    "active": i % 2 == 0,
                    "extra": null
                })
            })
            .collect();
        obj.insert("payload".to_string(), serde_json::Value::Array(large_items));
        // Embedded JSON string cell
        obj.insert(
            "embedded".to_string(),
            serde_json::Value::from(
                "{\"nested_key\": \"nested_val\", \"count\": 42, \"ok\": true}",
            ),
        );
        // Very long single-line string cell that requires soft-wrap height calculation
        obj.insert(
            "long_text".to_string(),
            serde_json::Value::from("A".repeat(2500)),
        );

        let table = crate::loader::LoadedTable {
            name: "records".to_string(),
            source_path: std::path::PathBuf::from("/tmp/records.json"),
            shape: crate::loader::JsonShape::Ndjson,
            schema_fields: Vec::new(),
            rows: std::sync::Arc::new(vec![obj]),
            columns: vec![
                "id".to_string(),
                "payload".to_string(),
                "embedded".to_string(),
                "long_text".to_string(),
            ],
            loaded_from_cache: false,
        };
        engine.register_table(table);

        let mut app = TuiApp::new(engine);
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();

        // Select column 1 ('payload') and open Detail Inspector
        app.selected_col = 1;
        app.handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(app.show_detail_modal);
        assert_eq!(app.detail_cursor_line, 0);
        assert_eq!(app.detail_scroll_offset, 0);

        // Move line cursor down with 'j', Down, and PageDown
        app.handle_key_event(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
        assert_eq!(app.detail_cursor_line, 1);
        app.handle_key_event(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(app.detail_cursor_line, 2);
        app.handle_key_event(KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE));
        assert_eq!(app.detail_cursor_line, 17);
        app.handle_key_event(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE));
        assert_eq!(app.detail_cursor_line, 16);

        // Jump to bottom ('G') and verify draw_tui clamps scroll offset to max_scroll
        app.handle_key_event(KeyEvent::new(KeyCode::Char('G'), KeyModifiers::NONE));
        assert!(app.detail_scroll_offset > 100);
        terminal.draw(|f| draw_tui(f, &mut app)).unwrap();
        let clamped_bottom = app.detail_scroll_offset;
        assert!(clamped_bottom > 50);

        // Jump back to top ('g')
        app.handle_key_event(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE));
        assert_eq!(app.detail_cursor_line, 0);
        assert_eq!(app.detail_scroll_offset, 0);

        // Switch column inside modal with Right ('l') to 'embedded' (col 2)
        app.handle_key_event(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
        assert_eq!(app.detail_cursor_line, 1);
        app.handle_key_event(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE));
        assert_eq!(app.selected_col, 2);
        assert_eq!(app.detail_cursor_line, 0);
        assert_eq!(app.detail_scroll_offset, 0);

        // Verify embedded JSON string is detected and pretty-printed
        let embedded_lines = build_detail_modal_lines(&app);
        let header_text: String = embedded_lines[0]
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert!(header_text.contains("Selected Cell [embedded] (JSON string):"));

        // Switch column to 'long_text' (col 3) and verify soft-wrapped single-line string can scroll
        app.handle_key_event(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
        assert_eq!(app.selected_col, 3);
        app.handle_key_event(KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE));
        terminal.draw(|f| draw_tui(f, &mut app)).unwrap();
        assert!(
            app.detail_scroll_offset > 0,
            "Long single-line string should allow vertical scrolling via wrapped line count"
        );

        // Verify JSON syntax highlighting colors on tokens
        let sample_spans = highlight_json_line_spans(
            "  \"active\": true, \"count\": 42, \"name\": \"Alice\", \"extra\": null",
            Modifier::empty(),
        );
        let find_color = |token: &str| -> Option<Color> {
            sample_spans
                .iter()
                .find(|s| s.content.as_ref() == token)
                .and_then(|s| s.style.fg)
        };
        assert_eq!(find_color("\"active\""), Some(Color::Cyan));
        assert_eq!(find_color("true"), Some(Color::LightMagenta));
        assert_eq!(find_color("42"), Some(Color::LightCyan));
        assert_eq!(find_color("\"Alice\""), Some(Color::LightGreen));
        assert_eq!(find_color("null"), Some(Color::DarkGray));
        assert_eq!(find_color(":"), Some(Color::Gray));
    }

    #[test]
    fn test_detail_modal_json_collapse_and_expand() {
        let tmp = TempDir::new().unwrap();
        let mut engine = QueryEngine::new();
        let (name, path) = parse_file_spec("examples/users.json");
        let table = load_json_file(&name, &path, Some(tmp.path()), false).unwrap();
        engine.register_table(table);

        let mut app = TuiApp::new(engine);
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();

        // Select column 6 ('address') on Row 0 (Alice Vance) and open Detail Inspector
        app.selected_row = 0;
        app.selected_col = 6;
        app.handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(app.show_detail_modal);

        let lines_to_strings = |app: &TuiApp| -> Vec<String> {
            build_detail_modal_lines(app)
                .into_iter()
                .map(|l| {
                    l.spans
                        .into_iter()
                        .map(|s| s.content.into_owned())
                        .collect()
                })
                .collect()
        };

        let initial_lines = lines_to_strings(&app);
        assert!(initial_lines.iter().any(|l| l.contains("▾ \"address\": {")));
        assert!(initial_lines.iter().any(|l| l.contains("▾ \"tags\": [")));
        assert!(initial_lines
            .iter()
            .any(|l| l.contains("\"city\": \"Seattle\"")));

        // 1. Move cursor to line 1 (`▾ {` of Selected Cell [address]) and press Space to collapse it
        app.handle_key_event(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
        assert_eq!(app.detail_cursor_line, 1);
        app.handle_key_event(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE));
        let after_cell_collapse = lines_to_strings(&app);
        assert!(after_cell_collapse
            .iter()
            .any(|l| l.contains("▸ { … 3 fields }")));

        // Press Enter to expand Selected Cell [address] back
        app.handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(
            app.show_detail_modal,
            "Enter inside inspector toggles fold rather than closing"
        );
        assert!(app.detail_collapsed_paths.is_empty());

        // 2. Navigate down to `"address": {` inside Full Row JSON and collapse it
        let entries = build_detail_modal_entries(&app);
        let row_addr_idx = entries
            .iter()
            .position(|e| e.is_node_header && e.toggle_path.as_deref() == Some("row/address"))
            .expect("row/address header line should exist");
        app.detail_cursor_line = row_addr_idx;
        app.handle_key_event(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE));

        let after_row_addr_collapse = lines_to_strings(&app);
        assert!(after_row_addr_collapse
            .iter()
            .any(|l| l.contains("▸ \"address\": { … 3 fields },")));

        // 3. Test collapsing from a child line inside `"tags"` (snaps cursor to `"tags"` header)
        let entries = build_detail_modal_entries(&app);
        let tag_child_idx = entries
            .iter()
            .position(|e| !e.is_node_header && e.toggle_path.as_deref() == Some("row/tags"))
            .expect("row/tags child line should exist");
        let tag_header_idx = entries
            .iter()
            .position(|e| e.is_node_header && e.toggle_path.as_deref() == Some("row/tags"))
            .expect("row/tags header line should exist");
        app.detail_cursor_line = tag_child_idx;
        app.handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(
            app.detail_cursor_line, tag_header_idx,
            "Collapsing from inside a nested array should snap cursor to its header line"
        );
        let after_tags_collapse = lines_to_strings(&app);
        assert!(after_tags_collapse
            .iter()
            .any(|l| l.contains("▸ \"tags\": [ … 3 items ]")));

        // 4. Press 'a' when all row fields are collapsed -> expands all!
        app.handle_key_event(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE));
        assert!(app.detail_collapsed_paths.is_empty());

        // Press 'a' again -> collapses all fields!
        app.handle_key_event(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE));
        assert!(app.detail_collapsed_paths.contains("row/address"));
        assert!(app.detail_collapsed_paths.contains("row/tags"));
        terminal.draw(|f| draw_tui(f, &mut app)).unwrap();

        // 5. Close inspector with 'q'
        app.handle_key_event(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE));
        assert!(!app.show_detail_modal);
    }
}
