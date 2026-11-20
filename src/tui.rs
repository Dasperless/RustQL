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
                self.row_scroll_offset = 0;
                self.selected_col = 0;
                self.col_scroll_offset = 0;
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
            if matches!(
                key.code,
                KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q') | KeyCode::Char('d')
            ) {
                self.show_detail_modal = false;
            }
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
                    self.quick_filter_selected =
                        (self.quick_filter_selected + count - 1) % count;
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
                format!(
                    " SQL [History {}/{}] ",
                    idx + 1,
                    app.query_history.len()
                ),
                Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("│ {} ", validation_badge),
                Style::default().fg(validation_color),
            ),
        ]),
        None => Line::from(vec![
            Span::styled(
                " SQL (Enter:Run  Tab:Complete  Ctrl+T:Templates) ",
                Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
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
    let popup_y = (top_bar_area.y + top_bar_area.height).min(frame.area().height.saturating_sub(popup_height));
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
                Span::styled(
                    sug.description.clone(),
                    Style::default().fg(Color::Gray),
                ),
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
        Style::default().fg(Color::LightRed).add_modifier(Modifier::BOLD),
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

    let rows = window_row_indices.iter().map(|&r_idx| {
        let row_data = &res.rows[r_idx];
        let cells = (start_col..total_cols).map(|c_idx| {
            let val = row_data.get(c_idx).unwrap_or(&Value::Null);
            let text = value_to_table_cell_string(val);
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

/// Format a cell value for the TUI table, capping serialization of large nested objects/arrays at 64 bytes.
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
            s
        }
        Value::String(s) if s.len() > 64 => {
            let mut out: String = s.chars().take(60).collect();
            out.push('…');
            out
        }
        _ => value_to_display_string(val),
    }
}

/// Fast recursive case-insensitive filter match that avoids serializing nested JSON objects to strings.
fn value_matches_filter(val: &Value, needle_lower: &str) -> bool {
    match val {
        Value::Null => "null".contains(needle_lower),
        Value::Bool(true) => "true".contains(needle_lower),
        Value::Bool(false) => "false".contains(needle_lower),
        Value::Number(n) => n.to_string().contains(needle_lower),
        Value::String(s) => str_contains_ignore_case(s, needle_lower),
        Value::Array(arr) => arr.iter().any(|el| value_matches_filter(el, needle_lower)),
        Value::Object(map) => map.iter().any(|(k, v)| {
            str_contains_ignore_case(k, needle_lower) || value_matches_filter(v, needle_lower)
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
        let sql_line = highlight_sql_line(
            &selected_tpl.sql,
            selected_tpl.sql.len(),
            None,
            &app.engine,
        );
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
  ? or F1           Toggle this Help modal
  q or Ctrl+C       Quit Rustrix";

    let area = centered_rect(74, 76, frame.area());
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
}
