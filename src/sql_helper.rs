//! Intelligent SQL assistant for Rustrix:
//! - Live SQL syntax & schema validation with "Did you mean...?" fuzzy suggestions
//! - Context-aware SQL autocomplete (keywords, tables, nested JSON dot-paths, operators, sample values)
//! - SQL syntax highlighting with inline ghost-text completion preview
//! - Smart WHERE clause merging and one-key Quick Filter actions from table cells & schema fields
//! - Pre-filled SQL Recipe & Template catalog tailored to the loaded JSON schema

use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};
use serde_json::{Map, Value};
use sqlparser::ast::{
    Expr, GroupByExpr, Ident, Query, SelectItem, SetExpr, Statement, TableFactor,
};
use sqlparser::dialect::GenericDialect;
use sqlparser::parser::Parser;

use crate::cli::value_to_display_string;
use crate::engine::QueryEngine;
use crate::loader::LoadedTable;

/// Major SQL keywords supported by Rustrix.
pub const SQL_KEYWORDS: &[&str] = &[
    "SELECT", "FROM", "WHERE", "AND", "OR", "NOT", "IN", "LIKE", "ILIKE", "BETWEEN", "IS", "NULL",
    "GROUP", "BY", "HAVING", "ORDER", "ASC", "DESC", "LIMIT", "OFFSET", "JOIN", "INNER", "LEFT",
    "RIGHT", "FULL", "CROSS", "ON", "USING", "AS", "DISTINCT", "UNNEST", "CASE", "WHEN", "THEN",
    "ELSE", "END", "CAST", "TRUE", "FALSE",
];

/// Built-in aggregate and scalar functions supported by Rustrix.
pub const SQL_FUNCTIONS: &[&str] = &[
    "COUNT",
    "SUM",
    "AVG",
    "MIN",
    "MAX",
    "UNNEST",
    "LOWER",
    "UPPER",
    "LENGTH",
    "LEN",
    "ARRAY_LENGTH",
    "COALESCE",
    "IFNULL",
    "ROUND",
    "ABS",
    "TRIM",
    "CONCAT",
];

/// Result of live SQL validation while the user is typing in the SQL bar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SqlValidationStatus {
    /// Query parses cleanly and all referenced tables/fields look valid.
    Valid { summary: String },
    /// Query parses, but a referenced column/field is not in the discovered schema.
    Warning { message: String },
    /// User is mid-typing an incomplete clause; provides a helpful next-token hint.
    Incomplete { hint: String },
    /// Syntax or table error with an optional "Did you mean...?" suggestion.
    Invalid { message: String },
}

impl SqlValidationStatus {
    /// Short badge text for the SQL input box border title.
    pub fn badge_text(&self) -> String {
        match self {
            SqlValidationStatus::Valid { summary } => summary.clone(),
            SqlValidationStatus::Warning { message } => message.clone(),
            SqlValidationStatus::Incomplete { hint } => hint.clone(),
            SqlValidationStatus::Invalid { message } => message.clone(),
        }
    }

    /// Border/badge color corresponding to the validation status.
    pub fn color(&self) -> Color {
        match self {
            SqlValidationStatus::Valid { .. } => Color::LightGreen,
            SqlValidationStatus::Warning { .. } => Color::Yellow,
            SqlValidationStatus::Incomplete { .. } => Color::LightYellow,
            SqlValidationStatus::Invalid { .. } => Color::LightRed,
        }
    }
}

/// A single autocomplete suggestion item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SqlSuggestion {
    /// Display label in the dropdown list
    pub label: String,
    /// Text to insert (replacing `replace_prefix_len` characters before the cursor)
    pub insert_text: String,
    /// Number of characters before the cursor that this completion replaces
    pub replace_prefix_len: usize,
    /// Category badge (`"field"`, `"table"`, `"keyword"`, `"operator"`, `"value"`, `"function"`, `"clause"`)
    pub category: &'static str,
    /// Helpful description or sample value preview
    pub description: String,
}

/// A quick-filter or aggregation action triggered from a Result Table cell (`w`) or Schema field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuickFilterAction {
    /// Short title of the action (e.g. `"Filter matching (=)"`)
    pub title: String,
    /// Full SQL query that will be applied
    pub sql: String,
    /// Explanation of what this action does
    pub description: String,
}

/// A pre-built SQL recipe/template pre-populated with the user's loaded schema.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SqlTemplateRecipe {
    /// Recipe category (e.g. `"Filter"`, `"Aggregate"`, `"Nested JSON"`, `"Join"`)
    pub category: &'static str,
    /// Human-readable title
    pub title: String,
    /// Ready-to-run SQL query pre-filled with loaded table & column names
    pub sql: String,
    /// Brief explanation of the SQL syntax pattern
    pub explanation: String,
}

/// Validate a SQL query in real time without executing it, returning friendly syntax hints
/// or `"Did you mean...?"` suggestions for typos in keywords, table names, or JSON field paths.
pub fn validate_sql_live(sql: &str, engine: &QueryEngine) -> SqlValidationStatus {
    let trimmed = sql.trim();
    if trimmed.is_empty() {
        return SqlValidationStatus::Incomplete {
            hint: "… Type a SQL query (or press Ctrl+T for SQL templates)".to_string(),
        };
    }

    // 1. Check for unclosed quotes
    if has_unclosed_quote(trimmed) {
        return SqlValidationStatus::Incomplete {
            hint: "… Unclosed quote (' or \") — close string literal".to_string(),
        };
    }

    // 2. Check for common misspelled SQL keywords in top-level tokens before full parsing
    let tokens = tokenize_simple_words(trimmed);
    if let Some((bad_word, suggestion)) = detect_keyword_typo(&tokens, engine) {
        return SqlValidationStatus::Invalid {
            message: format!(
                "✗ Unknown keyword '{}' — 💡 Did you mean '{}'?",
                bad_word, suggestion
            ),
        };
    }

    // 3. Check if the query ends with a trailing keyword/operator while the user is still typing
    if let Some(hint) = detect_incomplete_trailing_clause(trimmed, &tokens) {
        return SqlValidationStatus::Incomplete { hint };
    }

    // 4. Parse with sqlparser
    let dialect = GenericDialect {};
    let statements = match Parser::parse_sql(&dialect, trimmed) {
        Ok(stmts) => stmts,
        Err(err) => {
            let diagnosed = diagnose_sql_error(trimmed, &err.to_string(), engine);
            return SqlValidationStatus::Invalid {
                message: format!("✗ {}", diagnosed),
            };
        }
    };

    if statements.is_empty() {
        return SqlValidationStatus::Incomplete {
            hint: "… Type a SELECT query".to_string(),
        };
    }
    if statements.len() > 1 {
        return SqlValidationStatus::Invalid {
            message: "✗ Only a single SELECT statement is supported".to_string(),
        };
    }

    match &statements[0] {
        Statement::Query(query) => validate_parsed_query(query, engine),
        _ => SqlValidationStatus::Invalid {
            message: "✗ Only SELECT queries are supported".to_string(),
        },
    }
}

/// Enhance an execution or parse error message with `"Did you mean...?"` suggestions.
pub fn diagnose_sql_error(sql: &str, raw_error: &str, engine: &QueryEngine) -> String {
    let tokens = tokenize_simple_words(sql);
    if let Some((bad_word, suggestion)) = detect_keyword_typo(&tokens, engine) {
        return format!(
            "{} — 💡 Did you mean '{}' instead of '{}'?",
            simplify_parser_error(raw_error),
            suggestion,
            bad_word
        );
    }

    if has_unclosed_quote(sql.trim()) {
        return format!(
            "{} — 💡 Hint: You have an unclosed string quote (' or \").",
            simplify_parser_error(raw_error)
        );
    }

    // Check if an unknown table name was used after FROM or JOIN
    for window in tokens.windows(2) {
        if window[0].eq_ignore_ascii_case("FROM") || window[0].eq_ignore_ascii_case("JOIN") {
            let candidate_table = window[1].trim_matches(|c: char| !c.is_alphanumeric() && c != '_');
            if !candidate_table.is_empty()
                && !candidate_table.eq_ignore_ascii_case("UNNEST")
                && engine.resolve_table(candidate_table).is_err()
            {
                let table_names: Vec<String> =
                    engine.tables().iter().map(|t| t.name.clone()).collect();
                if let Some(best) = closest_match(candidate_table, &table_names, 3) {
                    return format!(
                        "Table '{}' not found — 💡 Did you mean '{}'?",
                        candidate_table, best
                    );
                }
            }
        }
    }

    simplify_parser_error(raw_error)
}

fn simplify_parser_error(raw: &str) -> String {
    let cleaned = raw
        .trim_start_matches("SQL syntax error: ")
        .trim_start_matches("sql parser error: ");
    cleaned.to_string()
}

fn validate_parsed_query(query: &Query, engine: &QueryEngine) -> SqlValidationStatus {
    let select = match query.body.as_ref() {
        SetExpr::Select(sel) => sel,
        _ => {
            return SqlValidationStatus::Invalid {
                message: "✗ Only standard SELECT queries are supported".to_string(),
            }
        }
    };

    // Collect referenced tables and their known columns/schema paths
    let mut active_tables: Vec<&LoadedTable> = Vec::new();
    let mut table_aliases: Vec<String> = Vec::new();

    for twj in &select.from {
        if let Err(status) =
            collect_and_validate_table_factor(&twj.relation, engine, &mut active_tables, &mut table_aliases)
        {
            return status;
        }
        for join in &twj.joins {
            if let Err(status) = collect_and_validate_table_factor(
                &join.relation,
                engine,
                &mut active_tables,
                &mut table_aliases,
            ) {
                return status;
            }
        }
    }

    if active_tables.is_empty() && !select.from.is_empty() {
        return SqlValidationStatus::Valid {
            summary: "✓ Valid SQL".to_string(),
        };
    }

    // Build list of known field names and nested dot-paths across active tables
    let mut valid_fields: Vec<String> = Vec::new();
    for t in &active_tables {
        for c in &t.columns {
            if !valid_fields.contains(c) {
                valid_fields.push(c.clone());
            }
        }
        for sf in &t.schema_fields {
            if !valid_fields.contains(&sf.path) {
                valid_fields.push(sf.path.clone());
            }
        }
    }

    // Collect select aliases so ORDER BY / HAVING / GROUP BY can reference them without warning
    let mut select_aliases: Vec<String> = Vec::new();
    for item in &select.projection {
        if let SelectItem::ExprWithAlias { alias, .. } = item {
            select_aliases.push(alias.value.clone());
        }
    }

    // Collect referenced identifiers from SELECT, WHERE, GROUP BY, HAVING, ORDER BY
    let mut referenced_idents: Vec<(String, Option<char>)> = Vec::new();
    for item in &select.projection {
        match item {
            SelectItem::UnnamedExpr(e) | SelectItem::ExprWithAlias { expr: e, .. } => {
                collect_expr_identifiers(e, &mut referenced_idents);
            }
            _ => {}
        }
    }
    if let Some(ref w) = select.selection {
        collect_expr_identifiers(w, &mut referenced_idents);
    }
    if let GroupByExpr::Expressions(exprs, _) = &select.group_by {
        for e in exprs {
            collect_expr_identifiers(e, &mut referenced_idents);
        }
    }
    if let Some(ref h) = select.having {
        collect_expr_identifiers(h, &mut referenced_idents);
    }
    if let Some(ref ob) = query.order_by {
        for o in &ob.exprs {
            collect_expr_identifiers(&o.expr, &mut referenced_idents);
        }
    }

    // Verify each referenced identifier against valid_fields, table_aliases, and select_aliases
    if !valid_fields.is_empty() {
        for (ident_path, quote_style) in referenced_idents {
            // Double-quoted strings that don't match columns are treated as string literals by the engine
            if quote_style == Some('"') {
                continue;
            }
            if select_aliases
                .iter()
                .any(|a| a.eq_ignore_ascii_case(&ident_path))
            {
                continue;
            }
            if table_aliases
                .iter()
                .any(|a| a.eq_ignore_ascii_case(&ident_path))
            {
                continue;
            }

            // Strip table alias prefix if present (e.g. `u.name` -> `name`)
            let stripped = if let Some((prefix, rest)) = ident_path.split_once('.') {
                if table_aliases.iter().any(|a| a.eq_ignore_ascii_case(prefix)) {
                    rest
                } else {
                    ident_path.as_str()
                }
            } else {
                ident_path.as_str()
            };

            let first_segment = stripped.split('.').next().unwrap_or(stripped);
            let matches_known = valid_fields.iter().any(|f| {
                f.eq_ignore_ascii_case(stripped)
                    || f.eq_ignore_ascii_case(first_segment)
                    || stripped
                        .to_lowercase()
                        .starts_with(&format!("{}.", f.to_lowercase()))
            });

            if !matches_known {
                if let Some(suggestion) = closest_match(stripped, &valid_fields, 3) {
                    return SqlValidationStatus::Warning {
                        message: format!(
                            "⚠ Unknown field '{}' — 💡 Did you mean '{}'?",
                            ident_path, suggestion
                        ),
                    };
                } else {
                    return SqlValidationStatus::Warning {
                        message: format!(
                            "⚠ Field '{}' not found in schema (will evaluate to NULL)",
                            ident_path
                        ),
                    };
                }
            }
        }
    }

    SqlValidationStatus::Valid {
        summary: "✓ Valid SQL".to_string(),
    }
}

fn collect_and_validate_table_factor<'a>(
    factor: &TableFactor,
    engine: &'a QueryEngine,
    active_tables: &mut Vec<&'a LoadedTable>,
    table_aliases: &mut Vec<String>,
) -> Result<(), SqlValidationStatus> {
    if let TableFactor::Table { name, alias, args, .. } = factor {
        let raw_name = name
            .0
            .iter()
            .map(|p| p.value.as_str())
            .collect::<Vec<_>>()
            .join(".");
        if raw_name.eq_ignore_ascii_case("unnest") || args.is_some() {
            if let Some(a) = alias {
                table_aliases.push(a.name.value.clone());
            }
            return Ok(());
        }
        match engine.resolve_table(&raw_name) {
            Ok(t) => {
                active_tables.push(t);
                table_aliases.push(t.name.clone());
                if let Some(a) = alias {
                    table_aliases.push(a.name.value.clone());
                }
                Ok(())
            }
            Err(_) => {
                let all_tables: Vec<String> =
                    engine.tables().iter().map(|t| t.name.clone()).collect();
                let msg = if let Some(best) = closest_match(&raw_name, &all_tables, 3) {
                    format!(
                        "✗ Table '{}' not found — 💡 Did you mean '{}'?",
                        raw_name, best
                    )
                } else {
                    format!(
                        "✗ Table '{}' not found (available: {})",
                        raw_name,
                        all_tables.join(", ")
                    )
                };
                Err(SqlValidationStatus::Invalid { message: msg })
            }
        }
    } else {
        if let TableFactor::UNNEST {
            alias: Some(a), ..
        }
        | TableFactor::Function {
            alias: Some(a), ..
        }
        | TableFactor::Derived {
            alias: Some(a), ..
        } = factor
        {
            table_aliases.push(a.name.value.clone());
        }
        Ok(())
    }
}

fn collect_expr_identifiers(expr: &Expr, out: &mut Vec<(String, Option<char>)>) {
    match expr {
        Expr::Identifier(Ident {
            value, quote_style, ..
        }) => {
            out.push((value.clone(), *quote_style));
        }
        Expr::CompoundIdentifier(idents) => {
            let joined = idents
                .iter()
                .map(|i| i.value.as_str())
                .collect::<Vec<_>>()
                .join(".");
            let quote = idents.first().and_then(|i| i.quote_style);
            out.push((joined, quote));
        }
        Expr::BinaryOp { left, right, .. } => {
            collect_expr_identifiers(left, out);
            collect_expr_identifiers(right, out);
        }
        Expr::UnaryOp { expr: inner, .. }
        | Expr::Nested(inner)
        | Expr::IsNull(inner)
        | Expr::IsNotNull(inner)
        | Expr::IsTrue(inner)
        | Expr::IsFalse(inner)
        | Expr::Cast { expr: inner, .. } => {
            collect_expr_identifiers(inner, out);
        }
        Expr::InList {
            expr: target, list, ..
        } => {
            collect_expr_identifiers(target, out);
            for item in list {
                collect_expr_identifiers(item, out);
            }
        }
        Expr::Between {
            expr: target,
            low,
            high,
            ..
        } => {
            collect_expr_identifiers(target, out);
            collect_expr_identifiers(low, out);
            collect_expr_identifiers(high, out);
        }
        Expr::Like {
            expr: target,
            pattern,
            ..
        }
        | Expr::ILike {
            expr: target,
            pattern,
            ..
        } => {
            collect_expr_identifiers(target, out);
            collect_expr_identifiers(pattern, out);
        }
        Expr::Function(func) => {
            if let sqlparser::ast::FunctionArguments::List(ref list) = func.args {
                for arg in &list.args {
                    if let sqlparser::ast::FunctionArg::Unnamed(
                        sqlparser::ast::FunctionArgExpr::Expr(ref e),
                    ) = arg
                    {
                        collect_expr_identifiers(e, out);
                    }
                }
            }
        }
        Expr::Subscript { expr: base, .. } | Expr::CompositeAccess { expr: base, .. } => {
            collect_expr_identifiers(base, out);
        }
        Expr::Case {
            operand,
            conditions,
            results,
            else_result,
        } => {
            if let Some(op) = operand {
                collect_expr_identifiers(op, out);
            }
            for c in conditions {
                collect_expr_identifiers(c, out);
            }
            for r in results {
                collect_expr_identifiers(r, out);
            }
            if let Some(el) = else_result {
                collect_expr_identifiers(el, out);
            }
        }
        _ => {}
    }
}

fn has_unclosed_quote(s: &str) -> bool {
    let mut in_single = false;
    let mut in_double = false;
    let mut prev = '\0';
    for c in s.chars() {
        if c == '\'' && !in_double && prev != '\\' {
            in_single = !in_single;
        } else if c == '"' && !in_single && prev != '\\' {
            in_double = !in_double;
        }
        prev = c;
    }
    in_single || in_double
}

fn tokenize_simple_words(sql: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut in_single = false;
    let mut in_double = false;

    for c in sql.chars() {
        if c == '\'' && !in_double {
            in_single = !in_single;
            current.push(c);
        } else if c == '"' && !in_single {
            in_double = !in_double;
            current.push(c);
        } else if in_single || in_double {
            current.push(c);
        } else if c.is_whitespace() || matches!(c, ',' | '(' | ')') {
            if !current.is_empty() {
                tokens.push(std::mem::take(&mut current));
            }
        } else {
            current.push(c);
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

fn detect_keyword_typo(tokens: &[String], engine: &QueryEngine) -> Option<(String, String)> {
    if tokens.is_empty() {
        return None;
    }

    // Collect known table names and column names so we never flag a valid column/table as a keyword typo
    let mut schema_names: Vec<String> = Vec::new();
    for t in engine.tables() {
        schema_names.push(t.name.clone());
        for c in &t.columns {
            schema_names.push(c.clone());
        }
        for sf in &t.schema_fields {
            schema_names.push(sf.path.clone());
        }
    }

    // Check the first token (must be SELECT)
    let first = &tokens[0];
    if !first.eq_ignore_ascii_case("SELECT") {
        if let Some(m) = closest_match_slice(first, &["SELECT"], 2) {
            return Some((first.clone(), m));
        }
    }

    // Check clause-position tokens for common typos like FORM, WHER, GRUOP, ORDR, LIMT, JION, HAVNG, DISTICT
    let clause_keywords = [
        "SELECT", "FROM", "WHERE", "GROUP", "ORDER", "LIMIT", "OFFSET", "HAVING", "DISTINCT",
        "JOIN", "INNER", "LEFT", "RIGHT", "CROSS", "UNNEST", "BETWEEN", "ILIKE",
    ];

    for tok in tokens.iter().skip(1) {
        if tok.starts_with('\'')
            || tok.starts_with('"')
            || tok.chars().next().map(|c| c.is_ascii_digit()).unwrap_or(false)
            || tok.contains('.')
            || tok.contains('=')
            || tok.contains('>')
            || tok.contains('<')
            || tok.len() < 4
        {
            continue;
        }
        let upper = tok.to_uppercase();
        if SQL_KEYWORDS.contains(&upper.as_str()) || SQL_FUNCTIONS.contains(&upper.as_str()) {
            continue;
        }
        if schema_names.iter().any(|s| s.eq_ignore_ascii_case(tok)) {
            continue;
        }
        if let Some(best) = closest_match_slice(&upper, &clause_keywords, 1) {
            return Some((tok.clone(), best));
        }
        // Also check common 2-edit transpositions like FORM -> FROM, GRUOP -> GROUP, WHER -> WHERE
        if matches!(
            upper.as_str(),
            "FORM" | "FRMO" | "WHER" | "WHRE" | "WEHRE" | "GRUOP" | "ORDR" | "LIMT" | "JION"
        ) {
            if let Some(best) = closest_match_slice(&upper, &clause_keywords, 2) {
                return Some((tok.clone(), best));
            }
        }
    }
    None
}

fn detect_incomplete_trailing_clause(trimmed: &str, tokens: &[String]) -> Option<String> {
    let last = tokens.last()?.to_uppercase();
    if trimmed.ends_with(',') {
        return Some("… Expecting next column or expression after ','".to_string());
    }
    match last.as_str() {
        "SELECT" => Some("… Expecting columns or * after SELECT (press Tab to autocomplete)".to_string()),
        "DISTINCT" => Some("… Expecting column name after DISTINCT".to_string()),
        "FROM" | "JOIN" => Some("… Expecting table name after FROM/JOIN (press Tab to autocomplete)".to_string()),
        "WHERE" | "AND" | "OR" => {
            Some("… Expecting condition (e.g. col = 'value', col > 10, col ILIKE '%text%')".to_string())
        }
        "GROUP" => Some("… Expecting BY <column> after GROUP".to_string()),
        "ORDER" => Some("… Expecting BY <column> [ASC|DESC] after ORDER".to_string()),
        "BY" => Some("… Expecting column name after BY (press Tab to autocomplete)".to_string()),
        "HAVING" => Some("… Expecting aggregate condition after HAVING (e.g. COUNT(*) > 1)".to_string()),
        "LIMIT" => Some("… Expecting row count after LIMIT (e.g. 100)".to_string()),
        "OFFSET" => Some("… Expecting row offset number after OFFSET".to_string()),
        "ON" => Some("… Expecting join condition after ON (e.g. a.id = b.user_id)".to_string()),
        "=" | "!=" | "<>" | ">" | "<" | ">=" | "<=" => {
            Some("… Expecting value after operator (press Tab for sample values)".to_string())
        }
        "LIKE" | "ILIKE" => {
            Some("… Expecting pattern string (e.g. '%search%')".to_string())
        }
        "IN" => Some("… Expecting value list in parentheses: ('val1', 'val2')".to_string()),
        "IS" => Some("… Expecting NULL or NOT NULL after IS".to_string()),
        "NOT" => Some("… Expecting NULL, IN, LIKE, or condition after NOT".to_string()),
        _ => None,
    }
}

/// Compute context-aware SQL autocomplete suggestions at the current cursor position.
pub fn compute_sql_suggestions(
    sql: &str,
    cursor_chars: usize,
    engine: &QueryEngine,
) -> Vec<SqlSuggestion> {
    let chars: Vec<char> = sql.chars().collect();
    let pos = cursor_chars.min(chars.len());
    let before_cursor: String = chars[..pos].iter().collect();

    // Determine the partial word currently being typed right before the cursor
    let mut word_start = pos;
    while word_start > 0 {
        let c = chars[word_start - 1];
        if c.is_alphanumeric() || matches!(c, '_' | '.' | '*' | '\'' | '"' | '=' | '!' | '<' | '>')
        {
            word_start -= 1;
        } else {
            break;
        }
    }
    let current_word: String = chars[word_start..pos].iter().collect();
    let prefix_before_word: String = chars[..word_start].iter().collect();
    let prev_tokens = tokenize_simple_words(&prefix_before_word);
    let last_tok = prev_tokens
        .last()
        .map(|s| s.to_uppercase())
        .unwrap_or_default();
    let second_last_tok = if prev_tokens.len() >= 2 {
        prev_tokens[prev_tokens.len() - 2].to_uppercase()
    } else {
        String::new()
    };

    let primary_table = resolve_active_table_from_tokens(&prev_tokens, engine);
    let mut candidates: Vec<SqlSuggestion> = Vec::new();
    let replace_len = current_word.chars().count();

    // 1. Empty query or typing first keyword
    if prev_tokens.is_empty() {
        if let Some(t) = primary_table {
            let lim = if t.rows.len() > 1000 { " LIMIT 1000" } else { "" };
            candidates.push(SqlSuggestion {
                label: format!("SELECT * FROM {}{}", t.name, lim),
                insert_text: format!("SELECT * FROM {}{}", t.name, lim),
                replace_prefix_len: replace_len,
                category: "template",
                description: format!("Browse rows from {} ({} rows)", t.name, t.rows.len()),
            });
            candidates.push(SqlSuggestion {
                label: format!("SELECT * FROM {} WHERE ", t.name),
                insert_text: format!("SELECT * FROM {} WHERE ", t.name),
                replace_prefix_len: replace_len,
                category: "template",
                description: format!("Filter {} rows with a WHERE condition", t.name),
            });
        }
        candidates.push(SqlSuggestion {
            label: "SELECT".to_string(),
            insert_text: "SELECT ".to_string(),
            replace_prefix_len: replace_len,
            category: "keyword",
            description: "Start a SQL SELECT query".to_string(),
        });
        return filter_and_rank_suggestions(candidates, &current_word);
    }

    // 2. Right after FROM or JOIN -> suggest table names & UNNEST
    if matches!(last_tok.as_str(), "FROM" | "JOIN") {
        for t in engine.tables() {
            candidates.push(SqlSuggestion {
                label: t.name.clone(),
                insert_text: format!("{} ", t.name),
                replace_prefix_len: replace_len,
                category: "table",
                description: format!("{} rows • {}", t.rows.len(), t.shape.description()),
            });
        }
        candidates.push(SqlSuggestion {
            label: "UNNEST(array_field) AS item".to_string(),
            insert_text: "UNNEST(".to_string(),
            replace_prefix_len: replace_len,
            category: "function",
            description: "Unnest a JSON array field into rows".to_string(),
        });
        return filter_and_rank_suggestions(candidates, &current_word);
    }

    // 3. Right after an operator (`=`, `!=`, `<>`, `>=`, `<=`, `>`, `<`, `LIKE`, `ILIKE`) in WHERE
    if matches!(
        last_tok.as_str(),
        "=" | "!=" | "<>" | ">" | "<" | ">=" | "<=" | "LIKE" | "ILIKE"
    ) {
        let field_name = prev_tokens
            .get(prev_tokens.len().saturating_sub(2))
            .cloned()
            .unwrap_or_default();
        let is_like = matches!(last_tok.as_str(), "LIKE" | "ILIKE");
        if let Some(t) = primary_table {
            let samples = collect_field_sample_values(t, &field_name, 8);
            for sample in samples {
                let literal = if is_like {
                    if let Value::String(s) = &sample {
                        format!("'%{}%' ", s.replace('\'', "''"))
                    } else {
                        format!("'%{}%' ", value_to_display_string(&sample))
                    }
                } else {
                    format!("{} ", format_sql_literal(&sample))
                };
                candidates.push(SqlSuggestion {
                    label: literal.trim().to_string(),
                    insert_text: literal,
                    replace_prefix_len: replace_len,
                    category: "value",
                    description: format!("Sample value from '{}'", field_name),
                });
            }
        }
        return filter_and_rank_suggestions(candidates, &current_word);
    }

    // 4. Right after `WHERE <field>` or `AND <field>` or `OR <field>` -> suggest operators & pre-filled sample comparisons!
    if matches!(second_last_tok.as_str(), "WHERE" | "AND" | "OR")
        && !SQL_KEYWORDS.contains(&last_tok.as_str())
    {
        let raw_field = &prev_tokens[prev_tokens.len() - 1];
        let samples = primary_table
            .map(|t| collect_field_sample_values(t, raw_field, 3))
            .unwrap_or_default();

        if let Some(first_val) = samples.first() {
            let lit = format_sql_literal(first_val);
            candidates.push(SqlSuggestion {
                label: format!("= {}", lit),
                insert_text: format!("= {} ", lit),
                replace_prefix_len: replace_len,
                category: "operator",
                description: format!("Exact match on {}", raw_field),
            });
            candidates.push(SqlSuggestion {
                label: format!("!= {}", lit),
                insert_text: format!("!= {} ", lit),
                replace_prefix_len: replace_len,
                category: "operator",
                description: format!("Exclude {}", lit),
            });
            if let Value::String(s) = first_val {
                let short: String = s.chars().take(18).collect();
                candidates.push(SqlSuggestion {
                    label: format!("ILIKE '%{}%'", short),
                    insert_text: format!("ILIKE '%{}%' ", short.replace('\'', "''")),
                    replace_prefix_len: replace_len,
                    category: "operator",
                    description: "Case-insensitive substring search".to_string(),
                });
            }
            if first_val.is_number() {
                candidates.push(SqlSuggestion {
                    label: format!(">= {}", lit),
                    insert_text: format!(">= {} ", lit),
                    replace_prefix_len: replace_len,
                    category: "operator",
                    description: "Greater than or equal".to_string(),
                });
                candidates.push(SqlSuggestion {
                    label: format!("<= {}", lit),
                    insert_text: format!("<= {} ", lit),
                    replace_prefix_len: replace_len,
                    category: "operator",
                    description: "Less than or equal".to_string(),
                });
            }
        }

        for (op, desc) in [
            ("= ", "Equals value"),
            ("!= ", "Not equal to value"),
            ("ILIKE '%...%'", "Case-insensitive text search"),
            ("LIKE '%...%'", "Case-sensitive pattern match"),
            ("IS NOT NULL ", "Field exists and is not null"),
            ("IS NULL ", "Field is null or missing"),
            ("IN (", "Match any value in list"),
            (">= ", "Greater than or equal to"),
            ("<= ", "Less than or equal to"),
        ] {
            candidates.push(SqlSuggestion {
                label: op.trim().to_string(),
                insert_text: op.to_string(),
                replace_prefix_len: replace_len,
                category: "operator",
                description: desc.to_string(),
            });
        }
        return filter_and_rank_suggestions(candidates, &current_word);
    }

    // 5. Right after `GROUP` or `ORDER` -> suggest `BY `
    if matches!(last_tok.as_str(), "GROUP" | "ORDER") {
        candidates.push(SqlSuggestion {
            label: "BY".to_string(),
            insert_text: "BY ".to_string(),
            replace_prefix_len: replace_len,
            category: "keyword",
            description: format!("Complete {} BY clause", last_tok),
        });
        return filter_and_rank_suggestions(candidates, &current_word);
    }

    // 6. Determine active clause for general suggestions
    let current_clause = find_active_clause(&prev_tokens);

    match current_clause {
        "SELECT" => {
            if last_tok == "SELECT" || before_cursor.trim_end().ends_with(',') {
                if last_tok == "SELECT" {
                    candidates.push(SqlSuggestion {
                        label: "*".to_string(),
                        insert_text: "* ".to_string(),
                        replace_prefix_len: replace_len,
                        category: "field",
                        description: "All columns".to_string(),
                    });
                }
                add_schema_field_suggestions(&mut candidates, engine, primary_table, replace_len);
                for (fn_label, fn_insert, desc) in [
                    ("COUNT(*)", "COUNT(*) AS count ", "Count total rows in group"),
                    ("SUM(col)", "SUM(", "Sum numeric values"),
                    ("AVG(col)", "AVG(", "Average numeric values"),
                    ("MIN(col)", "MIN(", "Minimum value"),
                    ("MAX(col)", "MAX(", "Maximum value"),
                    ("DISTINCT", "DISTINCT ", "Return unique rows only"),
                    ("UNNEST(array_col)", "UNNEST(", "Expand array elements into rows"),
                ] {
                    candidates.push(SqlSuggestion {
                        label: fn_label.to_string(),
                        insert_text: fn_insert.to_string(),
                        replace_prefix_len: replace_len,
                        category: "function",
                        description: desc.to_string(),
                    });
                }
            } else {
                // After a column name in SELECT -> suggest FROM <table> or AS
                if let Some(t) = primary_table {
                    candidates.push(SqlSuggestion {
                        label: format!("FROM {}", t.name),
                        insert_text: format!("FROM {} ", t.name),
                        replace_prefix_len: replace_len,
                        category: "clause",
                        description: format!("Query from table '{}'", t.name),
                    });
                }
                candidates.push(SqlSuggestion {
                    label: "FROM".to_string(),
                    insert_text: "FROM ".to_string(),
                    replace_prefix_len: replace_len,
                    category: "keyword",
                    description: "Specify source table".to_string(),
                });
                candidates.push(SqlSuggestion {
                    label: "AS".to_string(),
                    insert_text: "AS ".to_string(),
                    replace_prefix_len: replace_len,
                    category: "keyword",
                    description: "Alias column name".to_string(),
                });
                add_schema_field_suggestions(&mut candidates, engine, primary_table, replace_len);
            }
        }
        "FROM" => {
            // We are after `FROM <table>` -> suggest WHERE, GROUP BY, ORDER BY, LIMIT, JOIN
            for (label, insert, desc) in [
                ("WHERE", "WHERE ", "Filter rows by condition (e.g. col = 'value')"),
                ("GROUP BY", "GROUP BY ", "Group rows for COUNT / SUM / AVG"),
                ("ORDER BY", "ORDER BY ", "Sort results ascending or descending"),
                ("LIMIT 100", "LIMIT 100", "Cap result to 100 rows"),
                ("LIMIT 10", "LIMIT 10", "Cap result to 10 rows"),
                ("JOIN", "JOIN ", "Inner join with another loaded JSON file"),
                ("LEFT JOIN", "LEFT JOIN ", "Left outer join with another JSON file"),
            ] {
                candidates.push(SqlSuggestion {
                    label: label.to_string(),
                    insert_text: insert.to_string(),
                    replace_prefix_len: replace_len,
                    category: "clause",
                    description: desc.to_string(),
                });
            }
        }
        "WHERE" | "ON" | "HAVING" => {
            if matches!(last_tok.as_str(), "WHERE" | "AND" | "OR" | "NOT" | "ON" | "HAVING") {
                add_schema_field_suggestions(&mut candidates, engine, primary_table, replace_len);
                candidates.push(SqlSuggestion {
                    label: "LOWER(col)".to_string(),
                    insert_text: "LOWER(".to_string(),
                    replace_prefix_len: replace_len,
                    category: "function",
                    description: "Convert string to lowercase".to_string(),
                });
            } else {
                // After a complete condition `WHERE col = 'val' ` -> suggest AND, OR, ORDER BY, GROUP BY, LIMIT
                for (label, insert, desc) in [
                    ("AND", "AND ", "Add another required filter condition"),
                    ("OR", "OR ", "Match either filter condition"),
                    ("ORDER BY", "ORDER BY ", "Sort matching rows"),
                    ("GROUP BY", "GROUP BY ", "Group matching rows"),
                    ("LIMIT 100", "LIMIT 100", "Limit output to 100 rows"),
                ] {
                    candidates.push(SqlSuggestion {
                        label: label.to_string(),
                        insert_text: insert.to_string(),
                        replace_prefix_len: replace_len,
                        category: "clause",
                        description: desc.to_string(),
                    });
                }
                add_schema_field_suggestions(&mut candidates, engine, primary_table, replace_len);
            }
        }
        "GROUP_BY" => {
            add_schema_field_suggestions(&mut candidates, engine, primary_table, replace_len);
            for (label, insert, desc) in [
                ("ORDER BY", "ORDER BY ", "Sort grouped results"),
                ("HAVING", "HAVING ", "Filter groups by aggregate condition"),
                ("LIMIT 50", "LIMIT 50", "Limit output rows"),
            ] {
                candidates.push(SqlSuggestion {
                    label: label.to_string(),
                    insert_text: insert.to_string(),
                    replace_prefix_len: replace_len,
                    category: "clause",
                    description: desc.to_string(),
                });
            }
        }
        "ORDER_BY" => {
            if matches!(last_tok.as_str(), "BY") || before_cursor.trim_end().ends_with(',') {
                add_schema_field_suggestions(&mut candidates, engine, primary_table, replace_len);
            } else {
                for (label, insert, desc) in [
                    ("DESC", "DESC ", "Sort highest to lowest"),
                    ("ASC", "ASC ", "Sort lowest to highest"),
                    ("LIMIT 100", "LIMIT 100", "Limit output to 100 rows"),
                ] {
                    candidates.push(SqlSuggestion {
                        label: label.to_string(),
                        insert_text: insert.to_string(),
                        replace_prefix_len: replace_len,
                        category: "keyword",
                        description: desc.to_string(),
                    });
                }
                add_schema_field_suggestions(&mut candidates, engine, primary_table, replace_len);
            }
        }
        "LIMIT" => {
            for n in ["10", "50", "100", "500", "1000"] {
                candidates.push(SqlSuggestion {
                    label: n.to_string(),
                    insert_text: n.to_string(),
                    replace_prefix_len: replace_len,
                    category: "value",
                    description: format!("Return at most {} rows", n),
                });
            }
        }
        _ => {
            add_schema_field_suggestions(&mut candidates, engine, primary_table, replace_len);
            for kw in SQL_KEYWORDS {
                candidates.push(SqlSuggestion {
                    label: (*kw).to_string(),
                    insert_text: format!("{} ", kw),
                    replace_prefix_len: replace_len,
                    category: "keyword",
                    description: "SQL keyword".to_string(),
                });
            }
        }
    }

    filter_and_rank_suggestions(candidates, &current_word)
}

fn find_active_clause(tokens: &[String]) -> &'static str {
    for i in (0..tokens.len()).rev() {
        let upper = tokens[i].to_uppercase();
        match upper.as_str() {
            "LIMIT" => return "LIMIT",
            "HAVING" => return "HAVING",
            "WHERE" => return "WHERE",
            "ON" => return "ON",
            "FROM" | "JOIN" => return "FROM",
            "SELECT" => return "SELECT",
            "BY" if i > 0 => {
                let prev = tokens[i - 1].to_uppercase();
                if prev == "GROUP" {
                    return "GROUP_BY";
                } else if prev == "ORDER" {
                    return "ORDER_BY";
                }
            }
            _ => {}
        }
    }
    "SELECT"
}

fn resolve_active_table_from_tokens<'a>(
    tokens: &[String],
    engine: &'a QueryEngine,
) -> Option<&'a LoadedTable> {
    for window in tokens.windows(2) {
        if window[0].eq_ignore_ascii_case("FROM") || window[0].eq_ignore_ascii_case("JOIN") {
            if let Ok(t) = engine.resolve_table(&window[1]) {
                return Some(t);
            }
        }
    }
    engine.tables().first().copied()
}

fn add_schema_field_suggestions(
    candidates: &mut Vec<SqlSuggestion>,
    engine: &QueryEngine,
    primary_table: Option<&LoadedTable>,
    replace_len: usize,
) {
    let tables: Vec<&LoadedTable> = match primary_table {
        Some(t) => vec![t],
        None => engine.tables(),
    };

    for t in tables {
        for field in &t.schema_fields {
            let sample_preview = collect_field_sample_values(t, &field.path, 1)
                .first()
                .map(|v| {
                    let s = format_sql_literal(v);
                    let short: String = s.chars().take(24).collect();
                    format!(" • e.g. {}", short)
                })
                .unwrap_or_default();

            candidates.push(SqlSuggestion {
                label: field.path.clone(),
                insert_text: format!("{} ", field.path),
                replace_prefix_len: replace_len,
                category: "field",
                description: format!("{}{}", field.data_type, sample_preview),
            });
        }
    }
}

fn filter_and_rank_suggestions(
    candidates: Vec<SqlSuggestion>,
    current_word: &str,
) -> Vec<SqlSuggestion> {
    let needle = current_word.trim().to_lowercase();
    if needle.is_empty() {
        return candidates.into_iter().take(8).collect();
    }

    let mut prefix_matches = Vec::new();
    let mut substring_matches = Vec::new();
    let mut fuzzy_matches = Vec::new();

    for item in candidates {
        let label_lower = item.label.to_lowercase();
        // Do not suggest if the user already typed the exact complete token
        if label_lower == needle {
            continue;
        }
        if label_lower.starts_with(&needle) {
            prefix_matches.push(item);
        } else if label_lower.contains(&needle) {
            substring_matches.push(item);
        } else if needle.len() >= 3 && levenshtein(&label_lower, &needle) <= 2 {
            fuzzy_matches.push(item);
        }
    }

    prefix_matches.extend(substring_matches);
    if prefix_matches.is_empty() {
        prefix_matches.extend(fuzzy_matches);
    }
    prefix_matches.into_iter().take(8).collect()
}

/// Collect up to `max_samples` distinct non-null values for `field_path` (supporting nested dot paths)
/// from the first 300 rows of `table`.
pub fn collect_field_sample_values(
    table: &LoadedTable,
    field_path: &str,
    max_samples: usize,
) -> Vec<Value> {
    let stripped = if let Some((prefix, rest)) = field_path.split_once('.') {
        if prefix.eq_ignore_ascii_case(&table.name) {
            rest
        } else {
            field_path
        }
    } else {
        field_path
    };
    let parts: Vec<&str> = stripped.split('.').collect();
    let mut out: Vec<Value> = Vec::new();
    let scan_limit = table.rows.len().min(300);

    for row in &table.rows[..scan_limit] {
        if let Some(val) = lookup_dot_path_in_row(row, &parts) {
            if !val.is_null() && !val.is_object() && !val.is_array() && !out.contains(val) {
                out.push(val.clone());
                if out.len() >= max_samples {
                    break;
                }
            }
        }
    }
    out
}

fn lookup_dot_path_in_row<'a>(row: &'a Map<String, Value>, parts: &[&str]) -> Option<&'a Value> {
    if parts.is_empty() {
        return None;
    }
    let mut current = row
        .get(parts[0])
        .or_else(|| {
            row.iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(parts[0]))
                .map(|(_, v)| v)
        })?;

    for &part in &parts[1..] {
        match current {
            Value::Object(map) => {
                current = map.get(part).or_else(|| {
                    map.iter()
                        .find(|(k, _)| k.eq_ignore_ascii_case(part))
                        .map(|(_, v)| v)
                })?;
            }
            _ => return None,
        }
    }
    Some(current)
}

/// Format a JSON [`Value`] as a valid SQL literal (`'text'`, `123`, `true`, `NULL`).
pub fn format_sql_literal(val: &Value) -> String {
    match val {
        Value::Null => "NULL".to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::String(s) => format!("'{}'", s.replace('\'', "''")),
        other => format!("'{}'", value_to_display_string(other).replace('\'', "''")),
    }
}

/// Render a syntax-highlighted [`Line`] for the SQL input box, with optional inline ghost completion text.
pub fn highlight_sql_line(
    sql: &str,
    cursor_chars: usize,
    ghost_suggestion: Option<&SqlSuggestion>,
    engine: &QueryEngine,
) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let table_names: Vec<String> = engine.tables().iter().map(|t| t.name.to_lowercase()).collect();

    // Compute inline ghost text suffix if the cursor is at the end of the current word
    let ghost_suffix: Option<String> = ghost_suggestion.and_then(|sug| {
        let chars: Vec<char> = sql.chars().collect();
        if cursor_chars < chars.len() && !chars[cursor_chars].is_whitespace() {
            return None;
        }
        let start = cursor_chars.saturating_sub(sug.replace_prefix_len);
        let typed_prefix: String = chars[start..cursor_chars.min(chars.len())].iter().collect();
        if sug
            .insert_text
            .to_lowercase()
            .starts_with(&typed_prefix.to_lowercase())
        {
            let suffix: String = sug
                .insert_text
                .chars()
                .skip(typed_prefix.chars().count())
                .collect();
            if !suffix.is_empty() {
                return Some(suffix);
            }
        }
        None
    });

    let chars: Vec<char> = sql.chars().collect();
    let n = chars.len();
    let mut i = 0;
    let mut ghost_inserted = false;

    while i < n {
        if i == cursor_chars && !ghost_inserted {
            if let Some(ref g) = ghost_suffix {
                spans.push(Span::styled(
                    g.clone(),
                    Style::default()
                        .fg(Color::DarkGray)
                        .add_modifier(Modifier::ITALIC),
                ));
            }
            ghost_inserted = true;
        }

        let c = chars[i];

        // 1. String literal ('...' or "...")
        if c == '\'' || c == '"' {
            let quote = c;
            let start = i;
            i += 1;
            while i < n {
                if chars[i] == quote {
                    i += 1;
                    break;
                }
                i += 1;
            }
            let literal: String = chars[start..i].iter().collect();
            spans.push(Span::styled(literal, Style::default().fg(Color::LightGreen)));
            continue;
        }

        // 2. Numeric literal
        if c.is_ascii_digit() {
            let start = i;
            while i < n && (chars[i].is_ascii_digit() || chars[i] == '.') {
                i += 1;
            }
            let num_str: String = chars[start..i].iter().collect();
            spans.push(Span::styled(num_str, Style::default().fg(Color::LightCyan)));
            continue;
        }

        // 3. Identifier or Keyword
        if c.is_alphabetic() || c == '_' {
            let start = i;
            while i < n && (chars[i].is_alphanumeric() || matches!(chars[i], '_' | '.')) {
                i += 1;
            }
            let word: String = chars[start..i].iter().collect();
            let upper = word.to_uppercase();
            let style = if SQL_KEYWORDS.contains(&upper.as_str()) {
                Style::default()
                    .fg(Color::LightMagenta)
                    .add_modifier(Modifier::BOLD)
            } else if SQL_FUNCTIONS.contains(&upper.as_str()) {
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD)
            } else if table_names.contains(&word.to_lowercase()) {
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::White)
            };
            spans.push(Span::styled(word, style));
            continue;
        }

        // 4. Whitespace or operators
        let start = i;
        i += 1;
        let sym: String = chars[start..i].iter().collect();
        spans.push(Span::styled(sym, Style::default().fg(Color::Gray)));
    }

    if !ghost_inserted {
        if let Some(g) = ghost_suffix {
            spans.push(Span::styled(
                g,
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::ITALIC),
            ));
        }
    }

    Line::from(spans)
}

/// Smartly merge or replace a `WHERE` condition into `current_sql` while preserving existing
/// `GROUP BY`, `HAVING`, `ORDER BY`, and `LIMIT` clauses.
pub fn merge_where_condition(
    current_sql: &str,
    default_table: &str,
    condition: &str,
    append_and: bool,
) -> String {
    let trimmed = current_sql.trim();
    if trimmed.is_empty() {
        return format!("SELECT * FROM {} WHERE {}", default_table, condition);
    }

    // Locate trailing clauses (GROUP BY, HAVING, ORDER BY, LIMIT, OFFSET) outside quotes
    let tail_split = find_clause_offset_outside_quotes(
        trimmed,
        &["GROUP BY", "HAVING", "ORDER BY", "LIMIT", "OFFSET"],
    );

    let (head, tail) = match tail_split {
        Some(idx) => (trimmed[..idx].trim_end(), format!(" {}", trimmed[idx..].trim_start())),
        None => (trimmed, String::new()),
    };

    if let Some(where_idx) = find_clause_offset_outside_quotes(head, &["WHERE"]) {
        if append_and {
            format!("{} AND {}{}", head, condition, tail)
        } else {
            let before_where = head[..where_idx].trim_end();
            format!("{} WHERE {}{}", before_where, condition, tail)
        }
    } else {
        format!("{} WHERE {}{}", head, condition, tail)
    }
}

fn find_clause_offset_outside_quotes(sql: &str, clauses: &[&str]) -> Option<usize> {
    let upper = sql.to_uppercase();
    let bytes = sql.as_bytes();
    let mut in_single = false;
    let mut in_double = false;
    let mut paren_depth = 0usize;

    let mut best_offset: Option<usize> = None;

    for i in 0..bytes.len() {
        let b = bytes[i];
        if b == b'\'' && !in_double {
            in_single = !in_single;
            continue;
        }
        if b == b'"' && !in_single {
            in_double = !in_double;
            continue;
        }
        if in_single || in_double {
            continue;
        }
        if b == b'(' {
            paren_depth += 1;
            continue;
        }
        if b == b')' {
            paren_depth = paren_depth.saturating_sub(1);
            continue;
        }
        if paren_depth > 0 {
            continue;
        }

        // Check word boundary before index i
        let boundary_before = i == 0 || bytes[i - 1].is_ascii_whitespace();
        if !boundary_before {
            continue;
        }

        for clause in clauses {
            if upper[i..].starts_with(clause) {
                let end = i + clause.len();
                let boundary_after = end == bytes.len() || bytes[end].is_ascii_whitespace();
                if boundary_after {
                    best_offset = Some(match best_offset {
                        Some(curr) => curr.min(i),
                        None => i,
                    });
                }
            }
        }
    }

    best_offset
}

/// Build the list of one-key Quick Filter & Aggregation actions when pressing `w` on a cell in the Result Table.
pub fn build_cell_quick_actions(
    current_sql: &str,
    table_name: &str,
    col_name: &str,
    cell_val: &Value,
    table_row_count: usize,
) -> Vec<QuickFilterAction> {
    let mut actions = Vec::new();
    let literal = format_sql_literal(cell_val);
    let default_limit = if table_row_count > 1000 {
        " LIMIT 1000"
    } else {
        ""
    };

    if cell_val.is_null() {
        let cond_null = format!("{} IS NULL", col_name);
        let cond_not_null = format!("{} IS NOT NULL", col_name);
        actions.push(QuickFilterAction {
            title: format!("Filter: {} IS NULL", col_name),
            sql: merge_where_condition(current_sql, table_name, &cond_null, true),
            description: "Keep only rows where this column is NULL or missing".to_string(),
        });
        actions.push(QuickFilterAction {
            title: format!("Filter: {} IS NOT NULL", col_name),
            sql: merge_where_condition(current_sql, table_name, &cond_not_null, true),
            description: "Keep only rows where this column has a non-null value".to_string(),
        });
    } else {
        let eq_cond = format!("{} = {}", col_name, literal);
        let neq_cond = format!("{} != {}", col_name, literal);

        actions.push(QuickFilterAction {
            title: format!("Filter matching: {} = {}", col_name, literal),
            sql: merge_where_condition(current_sql, table_name, &eq_cond, true),
            description: "Filter current query to rows matching this cell's value".to_string(),
        });
        actions.push(QuickFilterAction {
            title: format!("Exclude value:   {} != {}", col_name, literal),
            sql: merge_where_condition(current_sql, table_name, &neq_cond, true),
            description: "Exclude rows with this value from the current query".to_string(),
        });

        if let Value::String(s) = cell_val {
            let sample_sub: String = s.chars().take(24).collect();
            let escaped = sample_sub.replace('\'', "''");
            let ilike_cond = format!("{} ILIKE '%{}%'", col_name, escaped);
            actions.push(QuickFilterAction {
                title: format!("Contains text:   {} ILIKE '%{}%'", col_name, sample_sub),
                sql: merge_where_condition(current_sql, table_name, &ilike_cond, true),
                description: "Case-insensitive substring match".to_string(),
            });
        }

        if cell_val.is_number() {
            let ge_cond = format!("{} >= {}", col_name, literal);
            let le_cond = format!("{} <= {}", col_name, literal);
            actions.push(QuickFilterAction {
                title: format!("Greater/Equal:   {} >= {}", col_name, literal),
                sql: merge_where_condition(current_sql, table_name, &ge_cond, true),
                description: "Keep rows with values >= this cell".to_string(),
            });
            actions.push(QuickFilterAction {
                title: format!("Less/Equal:      {} <= {}", col_name, literal),
                sql: merge_where_condition(current_sql, table_name, &le_cond, true),
                description: "Keep rows with values <= this cell".to_string(),
            });
        }

        let not_null_cond = format!("{} IS NOT NULL", col_name);
        actions.push(QuickFilterAction {
            title: format!("Non-null only:   {} IS NOT NULL", col_name),
            sql: merge_where_condition(current_sql, table_name, &not_null_cond, true),
            description: "Filter out rows where this field is missing/null".to_string(),
        });

        actions.push(QuickFilterAction {
            title: format!("Replace WHERE:   SELECT * FROM {} WHERE {} = {}", table_name, col_name, literal),
            sql: format!(
                "SELECT * FROM {} WHERE {} = {}{}",
                table_name, col_name, literal, default_limit
            ),
            description: "Start a fresh SELECT * query with only this WHERE filter".to_string(),
        });
    }

    // Group & Count by this column
    actions.push(QuickFilterAction {
        title: format!("Group & Count:   GROUP BY {} (Top counts)", col_name),
        sql: format!(
            "SELECT {}, COUNT(*) AS count FROM {} GROUP BY {} ORDER BY count DESC LIMIT 50",
            col_name, table_name, col_name
        ),
        description: "Count occurrences of each unique value in this column".to_string(),
    });

    // Distinct values of this column
    actions.push(QuickFilterAction {
        title: format!("Unique values:   SELECT DISTINCT {}", col_name),
        sql: format!(
            "SELECT DISTINCT {} FROM {} ORDER BY {} ASC LIMIT 100",
            col_name, table_name, col_name
        ),
        description: "List all unique values for this column".to_string(),
    });

    actions
}

/// Build a catalog of ready-to-use SQL recipes pre-populated with the user's loaded table and column names.
pub fn build_sql_templates(engine: &QueryEngine) -> Vec<SqlTemplateRecipe> {
    let tables = engine.tables();
    let first_table = tables.first().copied();
    let table_name = first_table
        .map(|t| t.name.as_str())
        .unwrap_or("data");
    let row_count = first_table.map(|t| t.rows.len()).unwrap_or(0);
    let default_limit = if row_count > 1000 { " LIMIT 100" } else { "" };

    // Discover representative columns from the primary table
    let mut str_col = "name".to_string();
    let mut str_sample = "'Alice'".to_string();
    let mut num_col = "id".to_string();
    let mut num_sample = "10".to_string();
    let mut nested_col: Option<String> = None;
    let mut array_col: Option<String> = None;

    if let Some(t) = first_table {
        for sf in &t.schema_fields {
            if sf.data_type == "String" && str_col == "name" {
                str_col = sf.path.clone();
                if let Some(v) = collect_field_sample_values(t, &sf.path, 1).first() {
                    str_sample = format_sql_literal(v);
                }
            } else if (sf.data_type == "Int" || sf.data_type == "Float") && num_col == "id" {
                num_col = sf.path.clone();
                if let Some(v) = collect_field_sample_values(t, &sf.path, 1).first() {
                    num_sample = format_sql_literal(v);
                }
            }
            if sf.depth >= 1 && nested_col.is_none() && sf.data_type != "Object" {
                nested_col = Some(sf.path.clone());
            }
            if sf.data_type.starts_with("Array") && array_col.is_none() {
                array_col = Some(sf.path.clone());
            }
        }
    }

    let nested_field = nested_col.unwrap_or_else(|| str_col.clone());
    let arr_field = array_col.unwrap_or_else(|| "items".to_string());

    let mut recipes = vec![
        SqlTemplateRecipe {
            category: "Filter",
            title: "Filter by exact value (=)".to_string(),
            sql: format!(
                "SELECT * FROM {} WHERE {} = {}{}",
                table_name, str_col, str_sample, default_limit
            ),
            explanation: "Use WHERE col = 'value' (single or double quotes) to match exact values."
                .to_string(),
        },
        SqlTemplateRecipe {
            category: "Filter",
            title: "Case-insensitive text search (ILIKE)".to_string(),
            sql: format!(
                "SELECT * FROM {} WHERE {} ILIKE '%a%'{}",
                table_name, str_col, default_limit
            ),
            explanation:
                "Use ILIKE '%text%' for case-insensitive substring matching (% matches any characters)."
                    .to_string(),
        },
        SqlTemplateRecipe {
            category: "Filter",
            title: "Combine multiple conditions (AND / OR)".to_string(),
            sql: format!(
                "SELECT * FROM {} WHERE {} = {} AND {} >= {}{}",
                table_name, str_col, str_sample, num_col, num_sample, default_limit
            ),
            explanation: "Combine conditions with AND, OR, !=, >=, <=, IS NOT NULL.".to_string(),
        },
        SqlTemplateRecipe {
            category: "Filter",
            title: "Match any value in a list (IN)".to_string(),
            sql: format!(
                "SELECT * FROM {} WHERE {} IN ({}){}",
                table_name, str_col, str_sample, default_limit
            ),
            explanation: "Use WHERE col IN ('val1', 'val2') to match multiple allowed values."
                .to_string(),
        },
        SqlTemplateRecipe {
            category: "Aggregate",
            title: "Group & Count top values (GROUP BY + COUNT)".to_string(),
            sql: format!(
                "SELECT {}, COUNT(*) AS count FROM {} GROUP BY {} ORDER BY count DESC LIMIT 20",
                str_col, table_name, str_col
            ),
            explanation:
                "Group rows by a field and count occurrences, sorted from most frequent to least."
                    .to_string(),
        },
        SqlTemplateRecipe {
            category: "Aggregate",
            title: "Numeric summary stats (COUNT, SUM, AVG, MIN, MAX)".to_string(),
            sql: format!(
                "SELECT {}, COUNT(*) AS cnt, SUM({}) AS total, AVG({}) AS avg_val, MIN({}) AS min_val, MAX({}) AS max_val FROM {} GROUP BY {}",
                str_col, num_col, num_col, num_col, num_col, table_name, str_col
            ),
            explanation: "Compute COUNT, SUM, AVG, MIN, and MAX across groups.".to_string(),
        },
        SqlTemplateRecipe {
            category: "Explore",
            title: "List unique values (SELECT DISTINCT)".to_string(),
            sql: format!(
                "SELECT DISTINCT {} FROM {} ORDER BY {} ASC LIMIT 100",
                str_col, table_name, str_col
            ),
            explanation: "Use SELECT DISTINCT to see all unique values in a column.".to_string(),
        },
        SqlTemplateRecipe {
            category: "Nested JSON",
            title: "Query nested JSON object fields via dot-path".to_string(),
            sql: format!(
                "SELECT {}, {} FROM {} WHERE {} IS NOT NULL{}",
                num_col, nested_field, table_name, nested_field, default_limit
            ),
            explanation:
                "Access deeply nested JSON fields directly using dot paths (e.g. actor.login or address.city)."
                    .to_string(),
        },
        SqlTemplateRecipe {
            category: "Nested JSON",
            title: "Unnest a JSON array into individual rows (UNNEST)".to_string(),
            sql: format!(
                "SELECT t.{}, item FROM {} t, UNNEST(t.{}) AS item LIMIT 100",
                str_col, table_name, arr_field
            ),
            explanation:
                "Use FROM table t, UNNEST(t.array_field) AS item to flatten JSON arrays into rows."
                    .to_string(),
        },
    ];

    if tables.len() >= 2 {
        let t1 = &tables[0].name;
        let t2 = &tables[1].name;
        let t1_key = tables[0]
            .columns
            .first()
            .map(|s| s.as_str())
            .unwrap_or("id");
        let t2_key = tables[1]
            .columns
            .iter()
            .find(|c| c.contains("id"))
            .map(|s| s.as_str())
            .unwrap_or("id");
        recipes.push(SqlTemplateRecipe {
            category: "Join",
            title: format!("Join '{}' and '{}' across files", t1, t2),
            sql: format!(
                "SELECT a.*, b.* FROM {} a JOIN {} b ON a.{} = b.{} LIMIT 100",
                t1, t2, t1_key, t2_key
            ),
            explanation: "Join records across multiple loaded JSON files using JOIN ... ON."
                .to_string(),
        });
    }

    recipes
}

/// Find the closest string in `candidates` within `max_dist` Levenshtein edit distance.
pub fn closest_match(target: &str, candidates: &[String], max_dist: usize) -> Option<String> {
    let target_lower = target.to_lowercase();
    let mut best: Option<(String, usize)> = None;
    for cand in candidates {
        let d = levenshtein(&target_lower, &cand.to_lowercase());
        if d <= max_dist {
            match &best {
                Some((_, best_d)) if d >= *best_d => {}
                _ => best = Some((cand.clone(), d)),
            }
        }
    }
    best.map(|(s, _)| s)
}

fn closest_match_slice(target: &str, candidates: &[&str], max_dist: usize) -> Option<String> {
    let target_lower = target.to_lowercase();
    let mut best: Option<(String, usize)> = None;
    for &cand in candidates {
        let d = levenshtein(&target_lower, &cand.to_lowercase());
        if d <= max_dist {
            match &best {
                Some((_, best_d)) if d >= *best_d => {}
                _ => best = Some((cand.to_string(), d)),
            }
        }
    }
    best.map(|(s, _)| s)
}

/// Standard Levenshtein edit distance between two strings.
pub fn levenshtein(a: &str, b: &str) -> usize {
    let a_chars: Vec<char> = a.chars().collect();
    let b_chars: Vec<char> = b.chars().collect();
    let (m, n) = (a_chars.len(), b_chars.len());
    if m == 0 {
        return n;
    }
    if n == 0 {
        return m;
    }
    let mut prev: Vec<usize> = (0..=n).collect();
    let mut curr = vec![0usize; n + 1];

    for i in 1..=m {
        curr[0] = i;
        for j in 1..=n {
            let cost = if a_chars[i - 1] == b_chars[j - 1] {
                0
            } else {
                1
            };
            curr[j] = (prev[j] + 1).min(curr[j - 1] + 1).min(prev[j - 1] + cost);
        }
        std::mem::swap(&mut prev, &mut curr);
    }
    prev[n]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::loader::{load_json_file, parse_file_spec};
    use tempfile::TempDir;

    fn sample_engine() -> (TempDir, QueryEngine) {
        let tmp = TempDir::new().unwrap();
        let mut engine = QueryEngine::new();
        for spec in ["examples/users.json", "examples/orders.ndjson"] {
            let (name, path) = parse_file_spec(spec);
            let table = load_json_file(&name, &path, Some(tmp.path()), false).unwrap();
            engine.register_table(table);
        }
        (tmp, engine)
    }

    #[test]
    fn test_live_validation_and_did_you_mean_hints() {
        let (_tmp, engine) = sample_engine();

        // 1. Valid SQL
        let status_ok = validate_sql_live("SELECT name, salary FROM users WHERE active = true", &engine);
        assert!(matches!(status_ok, SqlValidationStatus::Valid { .. }));

        // 2. Typo in keyword ('WHER' -> 'WHERE', 'FORM' -> 'FROM')
        let status_kw = validate_sql_live("SELECT * FORM users", &engine);
        assert!(
            matches!(status_kw, SqlValidationStatus::Invalid { ref message } if message.contains("Did you mean 'FROM'")),
            "Expected FROM suggestion, got: {:?}",
            status_kw
        );

        // 3. Typo in table name ('userz' -> 'users')
        let status_tbl = validate_sql_live("SELECT * FROM userz", &engine);
        assert!(
            matches!(status_tbl, SqlValidationStatus::Invalid { ref message } if message.contains("Did you mean 'users'")),
            "Expected users suggestion, got: {:?}",
            status_tbl
        );

        // 4. Typo in column name ('salry' -> 'salary')
        let status_col = validate_sql_live("SELECT salry FROM users", &engine);
        assert!(
            matches!(status_col, SqlValidationStatus::Warning { ref message } if message.contains("Did you mean 'salary'")),
            "Expected salary suggestion, got: {:?}",
            status_col
        );

        // 5. Incomplete trailing WHERE
        let status_inc = validate_sql_live("SELECT * FROM users WHERE", &engine);
        assert!(matches!(status_inc, SqlValidationStatus::Incomplete { .. }));
    }

    #[test]
    fn test_context_aware_autocomplete_suggestions() {
        let (_tmp, engine) = sample_engine();

        // 1. After `FROM ` -> suggests tables (`users`, `orders`)
        let q1 = "SELECT * FROM ";
        let s1 = compute_sql_suggestions(q1, q1.len(), &engine);
        assert!(s1.iter().any(|s| s.label == "users" && s.category == "table"));
        assert!(s1.iter().any(|s| s.label == "orders" && s.category == "table"));

        // 2. After `SELECT * FROM users WHERE ` -> suggests fields (`name`, `address.city`, etc.)
        let q2 = "SELECT * FROM users WHERE ";
        let s2 = compute_sql_suggestions(q2, q2.len(), &engine);
        assert!(s2.iter().any(|s| s.label == "name" && s.category == "field"));
        assert!(s2.iter().any(|s| s.label == "address.city" && s.category == "field"));

        // 3. After `SELECT * FROM users WHERE name ` -> suggests operators with real sample value (`= 'Alice Vance'`)
        let q3 = "SELECT * FROM users WHERE name ";
        let s3 = compute_sql_suggestions(q3, q3.len(), &engine);
        assert!(
            s3.iter().any(|s| s.label.contains("'Alice Vance'")),
            "Expected sample value completion, got: {:?}",
            s3
        );
    }

    #[test]
    fn test_smart_where_merging_and_quick_actions() {
        let merged1 = merge_where_condition(
            "SELECT * FROM users LIMIT 100",
            "users",
            "active = true",
            true,
        );
        assert_eq!(merged1, "SELECT * FROM users WHERE active = true LIMIT 100");

        let merged2 = merge_where_condition(
            "SELECT * FROM users WHERE active = true ORDER BY salary DESC LIMIT 10",
            "users",
            "department = 'Platform'",
            true,
        );
        assert_eq!(
            merged2,
            "SELECT * FROM users WHERE active = true AND department = 'Platform' ORDER BY salary DESC LIMIT 10"
        );
    }
}
