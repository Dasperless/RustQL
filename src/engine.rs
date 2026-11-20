//! Pure-Rust SQL execution engine built on `sqlparser` and `serde_json`.
//!
//! Supports:
//! - `SELECT` (projections, aliases, `*`, `table.*`, `DISTINCT`, arithmetic, functions)
//! - `FROM` and multi-table `JOIN` (`INNER`, `LEFT`, `RIGHT`, `FULL`, `CROSS`)
//! - Lateral `UNNEST(array_path) [AS alias]` in `FROM` and `UNNEST(array_path)` in `SELECT`
//! - Nested field access via dot paths (`user.address.city`) and array indexing (`items[0].name`)
//! - `WHERE` filtering (`=`, `!=`, `<`, `<=`, `>`, `>=`, `AND`, `OR`, `NOT`, `LIKE`, `ILIKE`, `IN`, `BETWEEN`, `IS NULL`, `IS NOT NULL`)
//! - `GROUP BY` and `HAVING` with aggregates (`COUNT`, `SUM`, `AVG`, `MIN`, `MAX`)
//! - `ORDER BY` (`ASC`/`DESC`, multi-column, column aliases or expressions)
//! - `LIMIT` and `OFFSET`

use serde_json::{Map, Number, Value};
use sqlparser::ast::{
    BinaryOperator, DuplicateTreatment, Expr, Function, FunctionArg, FunctionArgExpr,
    FunctionArguments, GroupByExpr, Ident, JoinConstraint, JoinOperator, ObjectName, Offset,
    OrderByExpr, Query, Select, SelectItem, SetExpr, Statement, TableFactor, TableWithJoins,
    UnaryOperator, Value as SqlValue,
};
use sqlparser::dialect::GenericDialect;
use sqlparser::parser::Parser;
use std::cmp::Ordering;
use std::collections::HashMap;
use std::time::Instant;
use thiserror::Error;

use crate::loader::LoadedTable;

/// Errors returned by the SQL query engine.
#[derive(Debug, Error)]
pub enum EngineError {
    #[error("SQL syntax error: {0}")]
    Syntax(String),

    #[error("Table not found: '{0}'. Available tables: {1}")]
    TableNotFound(String, String),

    #[error("Unsupported SQL feature: {0}")]
    Unsupported(String),

    #[error("Evaluation error: {0}")]
    Evaluation(String),
}

/// Result of executing a SQL query.
#[derive(Debug, Clone)]
pub struct QueryResult {
    /// Ordered column headers
    pub columns: Vec<String>,
    /// Rows of JSON values aligned with `columns`
    pub rows: Vec<Vec<Value>>,
    /// Execution duration in milliseconds
    pub elapsed_ms: f64,
}

/// Internal tuple of `(columns, projected_rows, context_rows)` returned by projection evaluation.
type ProjectionOutput = (Vec<String>, Vec<Vec<Value>>, Vec<EvalRow>);

/// An execution row binds table aliases to their row objects, plus a merged flat map for unqualified lookups.
#[derive(Debug, Clone, Default)]
struct EvalRow {
    /// Map of `table_alias` -> row JSON map
    tables: HashMap<String, Map<String, Value>>,
    /// Ordered list of `(table_alias, column_names)` for deterministic `SELECT *` expansion
    table_order: Vec<(String, Vec<String>)>,
}

impl EvalRow {
    fn from_single_table(alias: &str, cols: &[String], row: &Map<String, Value>) -> Self {
        let mut tables = HashMap::new();
        tables.insert(alias.to_string(), row.clone());
        Self {
            tables,
            table_order: vec![(alias.to_string(), cols.to_vec())],
        }
    }

    fn merge(&self, other: &EvalRow) -> Self {
        let mut tables = self.tables.clone();
        for (k, v) in &other.tables {
            tables.insert(k.clone(), v.clone());
        }
        let mut table_order = self.table_order.clone();
        for item in &other.table_order {
            if !table_order.iter().any(|(a, _)| a == &item.0) {
                table_order.push(item.clone());
            }
        }
        Self {
            tables,
            table_order,
        }
    }

    fn null_for_table(alias: &str, cols: &[String]) -> Self {
        let mut empty_map = Map::new();
        for c in cols {
            empty_map.insert(c.clone(), Value::Null);
        }
        let mut tables = HashMap::new();
        tables.insert(alias.to_string(), empty_map);
        Self {
            tables,
            table_order: vec![(alias.to_string(), cols.to_vec())],
        }
    }

    /// Resolve a path of identifiers (`["u", "address", "city"]` or `["address", "city"]` or `["id"]`).
    fn resolve_path(&self, parts: &[String]) -> Value {
        if parts.is_empty() {
            return Value::Null;
        }

        // 1. Check if parts[0] is a known table alias
        if let Some(table_map) = self.tables.get(&parts[0]) {
            if parts.len() == 1 {
                // If the table itself is a scalar unnested alias with a single column matching the alias
                if let Some(v) = table_map.get(&parts[0]) {
                    return v.clone();
                }
                return Value::Object(table_map.clone());
            }
            return traverse_json_map(table_map, &parts[1..]);
        }

        // Case-insensitive table alias check
        let first_lower = parts[0].to_lowercase();
        for (t_alias, table_map) in &self.tables {
            if t_alias.to_lowercase() == first_lower {
                if parts.len() == 1 {
                    if let Some(v) = table_map.get(t_alias) {
                        return v.clone();
                    }
                    return Value::Object(table_map.clone());
                }
                return traverse_json_map(table_map, &parts[1..]);
            }
        }

        // 2. Otherwise search across all tables in order for `parts[0]` as a column name
        for (t_alias, _) in &self.table_order {
            if let Some(table_map) = self.tables.get(t_alias) {
                if has_column_IgnoreCase(table_map, &parts[0]) {
                    return traverse_json_map(table_map, parts);
                }
            }
        }

        Value::Null
    }
}

#[allow(non_snake_case)]
fn has_column_IgnoreCase(map: &Map<String, Value>, key: &str) -> bool {
    if map.contains_key(key) {
        return true;
    }
    let lower = key.to_lowercase();
    map.keys().any(|k| k.to_lowercase() == lower)
}

fn get_field_case_insensitive<'a>(map: &'a Map<String, Value>, key: &str) -> Option<&'a Value> {
    if let Some(v) = map.get(key) {
        return Some(v);
    }
    let lower = key.to_lowercase();
    map.iter()
        .find(|(k, _)| k.to_lowercase() == lower)
        .map(|(_, v)| v)
}

fn traverse_json_map(map: &Map<String, Value>, parts: &[String]) -> Value {
    if parts.is_empty() {
        return Value::Object(map.clone());
    }
    // Check if the map contains a literal dotted key first (e.g. "a.b")
    if parts.len() > 1 {
        let joined = parts.join(".");
        if let Some(v) = get_field_case_insensitive(map, &joined) {
            return v.clone();
        }
    }

    let first = match get_field_case_insensitive(map, &parts[0]) {
        Some(v) => v,
        None => return Value::Null,
    };

    traverse_json_value(first, &parts[1..])
}

fn traverse_json_value( mut current: &Value, parts: &[String]) -> Value {
    for part in parts {
        match current {
            Value::Object(map) => {
                if let Some(next) = get_field_case_insensitive(map, part) {
                    current = next;
                } else {
                    return Value::Null;
                }
            }
            Value::Array(arr) => {
                // Allow numeric index in dot path (`items.0.name`)
                if let Ok(idx) = part.parse::<usize>() {
                    if let Some(next) = arr.get(idx) {
                        current = next;
                    } else {
                        return Value::Null;
                    }
                } else {
                    // Project field across array of objects (`items.name` -> array of names)
                    let projected: Vec<Value> = arr
                        .iter()
                        .filter_map(|el| {
                            if let Value::Object(m) = el {
                                get_field_case_insensitive(m, part).cloned()
                            } else {
                                None
                            }
                        })
                        .collect();
                    return Value::Array(projected);
                }
            }
            _ => return Value::Null,
        }
    }
    current.clone()
}

/// The main SQL Query Engine holding registered JSON tables.
#[derive(Debug, Default, Clone)]
pub struct QueryEngine {
    tables: HashMap<String, LoadedTable>,
    table_names_ordered: Vec<String>,
}

impl QueryEngine {
    /// Create a new empty [`QueryEngine`].
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a loaded JSON table.
    pub fn register_table(&mut self, table: LoadedTable) {
        let name = table.name.clone();
        if !self.table_names_ordered.contains(&name) {
            self.table_names_ordered.push(name.clone());
        }
        self.tables.insert(name, table);
    }

    /// Return ordered list of loaded tables.
    pub fn tables(&self) -> Vec<&LoadedTable> {
        self.table_names_ordered
            .iter()
            .filter_map(|n| self.tables.get(n))
            .collect()
    }

    /// Look up a loaded table by name (Also allows `"data"` or `"t1"` when only one table is loaded).
    pub fn resolve_table(&self, name: &str) -> Result<&LoadedTable, EngineError> {
        if let Some(t) = self.tables.get(name) {
            return Ok(t);
        }
        let lower = name.to_lowercase();
        for (k, t) in &self.tables {
            if k.to_lowercase() == lower {
                return Ok(t);
            }
        }
        // Also check matching by filename (e.g. 'users.json')
        for t in self.tables.values() {
            if let Some(fname) = t.source_path.file_name().and_then(|s| s.to_str()) {
                if fname.eq_ignore_ascii_case(name) {
                    return Ok(t);
                }
            }
            if t.source_path.to_string_lossy().eq_ignore_ascii_case(name) {
                return Ok(t);
            }
        }
        // Single-table default alias 'data' or 't1'
        if self.tables.len() == 1 && (lower == "data" || lower == "t1") {
            return Ok(self.tables.values().next().unwrap());
        }

        let available = self.table_names_ordered.join(", ");
        Err(EngineError::TableNotFound(name.to_string(), available))
    }

    /// Execute a SQL query against the registered JSON tables.
    pub fn execute(&self, sql: &str) -> Result<QueryResult, EngineError> {
        let start = Instant::now();
        let trimmed = sql.trim();
        if trimmed.is_empty() {
            return Err(EngineError::Syntax("Query is empty".to_string()));
        }

        let normalized_sql = preprocess_unnest_syntax(trimmed);
        let dialect = GenericDialect {};
        let statements = Parser::parse_sql(&dialect, &normalized_sql)
            .map_err(|e| EngineError::Syntax(e.to_string()))?;

        if statements.is_empty() {
            return Err(EngineError::Syntax("No SQL statement found".to_string()));
        }
        if statements.len() > 1 {
            return Err(EngineError::Unsupported(
                "Only a single SELECT statement per query is supported".to_string(),
            ));
        }

        match &statements[0] {
            Statement::Query(query) => {
                let mut res = self.execute_query(query)?;
                res.elapsed_ms = start.elapsed().as_secs_f64() * 1000.0;
                Ok(res)
            }
            other => Err(EngineError::Unsupported(format!(
                "Only SELECT queries are supported (found: {})",
                other
            ))),
        }
    }

    fn execute_query(&self, query: &Query) -> Result<QueryResult, EngineError> {
        let select = match query.body.as_ref() {
            SetExpr::Select(sel) => sel,
            _ => {
                return Err(EngineError::Unsupported(
                    "Only standard SELECT queries are supported".to_string(),
                ))
            }
        };

        // 1. Evaluate FROM and JOINs (including lateral UNNESTs)
        let working_rows = self.evaluate_from(&select.from)?;

        // 2. Evaluate WHERE filter
        let filtered_rows = if let Some(ref selection) = select.selection {
            let mut kept = Vec::with_capacity(working_rows.len());
            for row in working_rows {
                let val = eval_scalar_expr(selection, &row)?;
                if is_truthy(&val) {
                    kept.push(row);
                }
            }
            kept
        } else {
            working_rows
        };

        // 3. Expand any `SELECT UNNEST(...)` items if present (row-multiplying projection)
        // Check if query uses GROUP BY or aggregate functions
        let has_group_by = match &select.group_by {
            GroupByExpr::Expressions(exprs, _) => !exprs.is_empty(),
            GroupByExpr::All(_) => {
                return Err(EngineError::Unsupported(
                    "GROUP BY ALL is not supported; list grouping columns explicitly".to_string(),
                ))
            }
        };
        let has_aggregates = select_has_aggregates(select);

        let (columns, mut output_rows, mut order_context_rows) = if has_group_by || has_aggregates {
            self.evaluate_grouped_projection(select, &filtered_rows)?
        } else {
            self.evaluate_scalar_projection(select, &filtered_rows)?
        };

        // 4. DISTINCT
        if matches!(
            select.distinct,
            Some(sqlparser::ast::Distinct::Distinct)
        ) {
            let mut unique_rows: Vec<Vec<Value>> = Vec::new();
            let mut unique_ctx: Vec<EvalRow> = Vec::new();
            for (idx, row) in output_rows.into_iter().enumerate() {
                if !unique_rows.iter().any(|existing| rows_equal(existing, &row)) {
                    unique_rows.push(row);
                    if let Some(ctx) = order_context_rows.get(idx) {
                        unique_ctx.push(ctx.clone());
                    }
                }
            }
            output_rows = unique_rows;
            order_context_rows = unique_ctx;
        }

        // 5. ORDER BY
        if let Some(ref order_by) = query.order_by {
            sort_output_rows(
                &columns,
                &mut output_rows,
                &order_context_rows,
                &order_by.exprs,
            )?;
        }

        // 6. OFFSET & LIMIT
        if let Some(Offset { ref value, .. }) = query.offset {
            let off_val = eval_scalar_expr(value, &EvalRow::default())?;
            if let Some(off) = value_as_usize(&off_val) {
                if off < output_rows.len() {
                    output_rows.drain(0..off);
                } else {
                    output_rows.clear();
                }
            }
        }

        if let Some(ref limit_expr) = query.limit {
            let lim_val = eval_scalar_expr(limit_expr, &EvalRow::default())?;
            if let Some(lim) = value_as_usize(&lim_val) {
                output_rows.truncate(lim);
            }
        }

        Ok(QueryResult {
            columns,
            rows: output_rows,
            elapsed_ms: 0.0,
        })
    }

    fn evaluate_from(&self, from: &[TableWithJoins]) -> Result<Vec<EvalRow>, EngineError> {
        if from.is_empty() {
            // Query without FROM (e.g. `SELECT 1 + 1`)
            return Ok(vec![EvalRow::default()]);
        }

        let mut current_rows: Option<Vec<EvalRow>> = None;

        for twj in from {
            current_rows = Some(self.apply_table_factor(current_rows, &twj.relation)?);
            for join in &twj.joins {
                let prev = current_rows.take().unwrap_or_default();
                current_rows = Some(self.apply_join(prev, &join.relation, &join.join_operator)?);
            }
        }

        Ok(current_rows.unwrap_or_default())
    }

    fn apply_table_factor(
        &self,
        existing: Option<Vec<EvalRow>>,
        factor: &TableFactor,
    ) -> Result<Vec<EvalRow>, EngineError> {
        // Check if this TableFactor is an UNNEST factor (either native AST UNNEST or `__unnest(...)` function factor)
        if let Some((array_expr, alias_name)) = extract_unnest_factor(factor) {
            let base_rows = existing.ok_or_else(|| {
                EngineError::Evaluation(
                    "UNNEST in FROM clause must follow a base table".to_string(),
                )
            })?;
            return apply_lateral_unnest(&base_rows, &array_expr, &alias_name);
        }

        let factor_rows = self.load_table_factor_rows(factor)?;
        match existing {
            None => Ok(factor_rows),
            Some(left_rows) => {
                // Comma join (CROSS JOIN)
                let mut combined = Vec::with_capacity(left_rows.len() * factor_rows.len());
                for l in &left_rows {
                    for r in &factor_rows {
                        combined.push(l.merge(r));
                    }
                }
                Ok(combined)
            }
        }
    }

    fn load_table_factor_rows(&self, factor: &TableFactor) -> Result<Vec<EvalRow>, EngineError> {
        match factor {
            TableFactor::Table { name, alias, .. } => {
                let raw_name = object_name_to_string(name);
                let table = self.resolve_table(&raw_name)?;
                let effective_alias = alias
                    .as_ref()
                    .map(|a| a.name.value.clone())
                    .unwrap_or_else(|| table.name.clone());

                let mut rows = Vec::with_capacity(table.rows.len());
                for r in table.rows.iter() {
                    rows.push(EvalRow::from_single_table(
                        &effective_alias,
                        &table.columns,
                        r,
                    ));
                }
                Ok(rows)
            }
            TableFactor::Derived {
                subquery, alias, ..
            } => {
                let sub_res = self.execute_query(subquery)?;
                let effective_alias = alias
                    .as_ref()
                    .map(|a| a.name.value.clone())
                    .unwrap_or_else(|| "subquery".to_string());
                let mut rows = Vec::with_capacity(sub_res.rows.len());
                for r in sub_res.rows {
                    let mut map = Map::new();
                    for (col, val) in sub_res.columns.iter().zip(r) {
                        map.insert(col.clone(), val);
                    }
                    rows.push(EvalRow::from_single_table(
                        &effective_alias,
                        &sub_res.columns,
                        &map,
                    ));
                }
                Ok(rows)
            }
            other => Err(EngineError::Unsupported(format!(
                "Unsupported FROM clause element: {}",
                other
            ))),
        }
    }

    fn apply_join(
        &self,
        left_rows: Vec<EvalRow>,
        right_factor: &TableFactor,
        join_op: &JoinOperator,
    ) -> Result<Vec<EvalRow>, EngineError> {
        // Support `CROSS JOIN UNNEST(...)` or `JOIN UNNEST(...)`
        if let Some((array_expr, alias_name)) = extract_unnest_factor(right_factor) {
            return apply_lateral_unnest(&left_rows, &array_expr, &alias_name);
        }

        let right_rows = self.load_table_factor_rows(right_factor)?;
        let (right_alias, right_cols) = right_rows
            .first()
            .and_then(|r| r.table_order.first().cloned())
            .unwrap_or_else(|| ("right".to_string(), Vec::new()));

        let left_template_orders: Vec<(String, Vec<String>)> = left_rows
            .first()
            .map(|r| r.table_order.clone())
            .unwrap_or_default();

        match join_op {
            JoinOperator::CrossJoin => {
                let mut out = Vec::with_capacity(left_rows.len() * right_rows.len());
                for l in &left_rows {
                    for r in &right_rows {
                        out.push(l.merge(r));
                    }
                }
                Ok(out)
            }
            JoinOperator::Inner(constraint) => {
                let mut out = Vec::new();
                for l in &left_rows {
                    for r in &right_rows {
                        let merged = l.merge(r);
                        if eval_join_constraint(constraint, l, r, &merged)? {
                            out.push(merged);
                        }
                    }
                }
                Ok(out)
            }
            JoinOperator::LeftOuter(constraint) => {
                let mut out = Vec::new();
                let null_right = EvalRow::null_for_table(&right_alias, &right_cols);
                for l in &left_rows {
                    let mut matched = false;
                    for r in &right_rows {
                        let merged = l.merge(r);
                        if eval_join_constraint(constraint, l, r, &merged)? {
                            out.push(merged);
                            matched = true;
                        }
                    }
                    if !matched {
                        out.push(l.merge(&null_right));
                    }
                }
                Ok(out)
            }
            JoinOperator::RightOuter(constraint) => {
                let mut out = Vec::new();
                let null_left = build_null_left(&left_template_orders);
                for r in &right_rows {
                    let mut matched = false;
                    for l in &left_rows {
                        let merged = l.merge(r);
                        if eval_join_constraint(constraint, l, r, &merged)? {
                            out.push(merged);
                            matched = true;
                        }
                    }
                    if !matched {
                        out.push(null_left.merge(r));
                    }
                }
                Ok(out)
            }
            JoinOperator::FullOuter(constraint) => {
                let mut out = Vec::new();
                let null_right = EvalRow::null_for_table(&right_alias, &right_cols);
                let null_left = build_null_left(&left_template_orders);
                let mut right_matched = vec![false; right_rows.len()];

                for l in &left_rows {
                    let mut matched = false;
                    for (r_idx, r) in right_rows.iter().enumerate() {
                        let merged = l.merge(r);
                        if eval_join_constraint(constraint, l, r, &merged)? {
                            out.push(merged);
                            matched = true;
                            right_matched[r_idx] = true;
                        }
                    }
                    if !matched {
                        out.push(l.merge(&null_right));
                    }
                }
                for (r_idx, r) in right_rows.iter().enumerate() {
                    if !right_matched[r_idx] {
                        out.push(null_left.merge(r));
                    }
                }
                Ok(out)
            }
            other => Err(EngineError::Unsupported(format!(
                "Unsupported JOIN operator: {:?}",
                other
            ))),
        }
    }

    fn evaluate_scalar_projection(
        &self,
        select: &Select,
        rows: &[EvalRow],
    ) -> Result<ProjectionOutput, EngineError> {
        let fallback_order = self.infer_from_table_order(&select.from);
        let columns = determine_projection_columns(&select.projection, rows, &fallback_order)?;
        let mut out_rows = Vec::with_capacity(rows.len());
        let mut out_ctx = Vec::with_capacity(rows.len());

        for row in rows {
            // Check if any projection item is an inline `UNNEST(expr)` call.
            // Each entry is `(is_unnested_col, values_vec)`.
            let mut row_cells_per_col: Vec<(bool, Vec<Value>)> =
                Vec::with_capacity(select.projection.len());
            let mut unnest_len: Option<usize> = None;

            for item in &select.projection {
                match item {
                    SelectItem::Wildcard(_) => {
                        for (t_alias, t_cols) in &row.table_order {
                            if let Some(t_map) = row.tables.get(t_alias) {
                                for c in t_cols {
                                    let v = t_map.get(c).cloned().unwrap_or(Value::Null);
                                    row_cells_per_col.push((false, vec![v]));
                                }
                            }
                        }
                    }
                    SelectItem::QualifiedWildcard(obj_name, _) => {
                        let t_name = object_name_to_string(obj_name);
                        if let Some((t_alias, t_cols)) = row
                            .table_order
                            .iter()
                            .find(|(a, _)| a.eq_ignore_ascii_case(&t_name))
                        {
                            if let Some(t_map) = row.tables.get(t_alias) {
                                for c in t_cols {
                                    let v = t_map.get(c).cloned().unwrap_or(Value::Null);
                                    row_cells_per_col.push((false, vec![v]));
                                }
                            }
                        }
                    }
                    SelectItem::UnnamedExpr(expr) | SelectItem::ExprWithAlias { expr, .. } => {
                        if let Some(inner_arg) = extract_inline_unnest_expr(expr) {
                            let val = eval_scalar_expr(inner_arg, row)?;
                            let expanded = match val {
                                Value::Array(arr) => arr,
                                Value::Null => Vec::new(),
                                single => vec![single],
                            };
                            unnest_len =
                                Some(unnest_len.map_or(expanded.len(), |l| l.max(expanded.len())));
                            row_cells_per_col.push((true, expanded));
                        } else {
                            let val = eval_scalar_expr(expr, row)?;
                            row_cells_per_col.push((false, vec![val]));
                        }
                    }
                }
            }

            if let Some(max_n) = unnest_len {
                for i in 0..max_n {
                    let mut single_row = Vec::with_capacity(row_cells_per_col.len());
                    for (is_unnest, col_vals) in &row_cells_per_col {
                        if !*is_unnest {
                            single_row.push(col_vals.first().cloned().unwrap_or(Value::Null));
                        } else {
                            single_row.push(col_vals.get(i).cloned().unwrap_or(Value::Null));
                        }
                    }
                    out_rows.push(single_row);
                    out_ctx.push(row.clone());
                }
            } else {
                let single_row: Vec<Value> = row_cells_per_col
                    .into_iter()
                    .map(|(_, mut v)| v.pop().unwrap_or(Value::Null))
                    .collect();
                out_rows.push(single_row);
                out_ctx.push(row.clone());
            }
        }

        Ok((columns, out_rows, out_ctx))
    }

    fn evaluate_grouped_projection(
        &self,
        select: &Select,
        rows: &[EvalRow],
    ) -> Result<ProjectionOutput, EngineError> {
        let group_exprs: &[Expr] = match &select.group_by {
            GroupByExpr::Expressions(exprs, _) => exprs,
            GroupByExpr::All(_) => &[],
        };

        // Partition rows into groups preserving insertion order
        let mut groups: Vec<(Vec<Value>, Vec<EvalRow>)> = Vec::new();

        if group_exprs.is_empty() {
            // Single global group (e.g. `SELECT COUNT(*) FROM users`)
            groups.push((Vec::new(), rows.to_vec()));
        } else {
            for row in rows {
                let mut key_vals = Vec::with_capacity(group_exprs.len());
                for g_expr in group_exprs {
                    key_vals.push(eval_scalar_expr(g_expr, row)?);
                }
                if let Some((_, bucket)) = groups
                    .iter_mut()
                    .find(|(k, _)| rows_equal(k, &key_vals))
                {
                    bucket.push(row.clone());
                } else {
                    groups.push((key_vals, vec![row.clone()]));
                }
            }
        }

        let fallback_order = self.infer_from_table_order(&select.from);
        let columns = determine_projection_columns(&select.projection, rows, &fallback_order)?;
        let mut out_rows = Vec::with_capacity(groups.len());
        let mut out_ctx = Vec::with_capacity(groups.len());

        for (_key, group_rows) in groups {
            // Filter by HAVING if specified
            if let Some(ref having_expr) = select.having {
                let h_val = eval_group_expr(having_expr, &group_rows, group_exprs)?;
                if !is_truthy(&h_val) {
                    continue;
                }
            }

            let mut projected_row = Vec::with_capacity(columns.len());
            for item in &select.projection {
                match item {
                    SelectItem::UnnamedExpr(expr) | SelectItem::ExprWithAlias { expr, .. } => {
                        let val = eval_group_expr(expr, &group_rows, group_exprs)?;
                        projected_row.push(val);
                    }
                    _ => {
                        return Err(EngineError::Unsupported(
                            "Wildcard (*) is not allowed in a GROUP BY / aggregate query"
                                .to_string(),
                        ))
                    }
                }
            }

            // Build a synthetic context EvalRow containing the group's first row plus computed output aliases
            let mut ctx = group_rows.first().cloned().unwrap_or_default();
            let mut alias_map = Map::new();
            for (c_name, c_val) in columns.iter().zip(projected_row.iter()) {
                alias_map.insert(c_name.clone(), c_val.clone());
            }
            ctx.tables.insert("__agg_out__".to_string(), alias_map);
            ctx.table_order
                .insert(0, ("__agg_out__".to_string(), columns.clone()));

            out_rows.push(projected_row);
            out_ctx.push(ctx);
        }

        Ok((columns, out_rows, out_ctx))
    }

    fn infer_from_table_order(&self, from: &[TableWithJoins]) -> Vec<(String, Vec<String>)> {
        let mut order = Vec::new();
        for twj in from {
            self.collect_factor_order(&twj.relation, &mut order);
            for j in &twj.joins {
                self.collect_factor_order(&j.relation, &mut order);
            }
        }
        order
    }

    fn collect_factor_order(&self, factor: &TableFactor, out: &mut Vec<(String, Vec<String>)>) {
        if let TableFactor::Table { name, alias, .. } = factor {
            let raw_name = object_name_to_string(name);
            if let Ok(table) = self.resolve_table(&raw_name) {
                let effective_alias = alias
                    .as_ref()
                    .map(|a| a.name.value.clone())
                    .unwrap_or_else(|| table.name.clone());
                out.push((effective_alias, table.columns.clone()));
            }
        }
    }
}

/// Rewrite `FROM table, UNNEST(expr) [AS alias]` or `JOIN UNNEST(expr) [AS alias]` into a function
/// table factor `__unnest_table(expr)` if needed, or let sqlparser parse `UNNEST` natively.
fn preprocess_unnest_syntax(sql: &str) -> String {
    sql.to_string()
}

/// Extract `(array_expr, alias_name)` if `factor` represents `UNNEST(expr) [AS alias]`.
fn extract_unnest_factor(factor: &TableFactor) -> Option<(Expr, String)> {
    match factor {
        TableFactor::UNNEST {
            alias,
            array_exprs,
            ..
        } => {
            let first_expr = array_exprs.first()?.clone();
            let alias_name = alias
                .as_ref()
                .map(|a| a.name.value.clone())
                .unwrap_or_else(|| default_unnest_alias(&first_expr));
            Some((first_expr, alias_name))
        }
        TableFactor::Function {
            name, args, alias, ..
        } => {
            let fn_name = object_name_to_string(name);
            if fn_name.eq_ignore_ascii_case("unnest") {
                for arg in args {
                    if let FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) = arg {
                        let alias_name = alias
                            .as_ref()
                            .map(|a| a.name.value.clone())
                            .unwrap_or_else(|| default_unnest_alias(e));
                        return Some((e.clone(), alias_name));
                    }
                }
            }
            None
        }
        TableFactor::Table {
            name, alias, args, ..
        } => {
            let fn_name = object_name_to_string(name);
            if fn_name.eq_ignore_ascii_case("unnest") {
                if let Some(table_args) = args {
                    for arg in &table_args.args {
                        if let FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) = arg {
                            let alias_name = alias
                                .as_ref()
                                .map(|a| a.name.value.clone())
                                .unwrap_or_else(|| default_unnest_alias(e));
                            return Some((e.clone(), alias_name));
                        }
                    }
                }
            }
            None
        }
        _ => None,
    }
}

fn default_unnest_alias(expr: &Expr) -> String {
    match expr {
        Expr::Identifier(id) => id.value.clone(),
        Expr::CompoundIdentifier(ids) => ids
            .last()
            .map(|i| i.value.clone())
            .unwrap_or_else(|| "unnest".to_string()),
        _ => "unnest".to_string(),
    }
}

fn apply_lateral_unnest(
    base_rows: &[EvalRow],
    array_expr: &Expr,
    alias_name: &str,
) -> Result<Vec<EvalRow>, EngineError> {
    let mut out = Vec::new();
    for row in base_rows {
        let val = eval_scalar_expr(array_expr, row)?;
        if let Value::Array(arr) = val {
            for elem in arr {
                let elem_row = match elem {
                    Value::Object(obj_map) => {
                        let cols: Vec<String> = obj_map.keys().cloned().collect();
                        EvalRow::from_single_table(alias_name, &cols, &obj_map)
                    }
                    scalar => {
                        let mut m = Map::new();
                        m.insert(alias_name.to_string(), scalar);
                        EvalRow::from_single_table(
                            alias_name,
                            &[alias_name.to_string()],
                            &m,
                        )
                    }
                };
                out.push(row.merge(&elem_row));
            }
        }
    }
    Ok(out)
}

fn extract_inline_unnest_expr(expr: &Expr) -> Option<&Expr> {
    if let Expr::Function(func) = expr {
        let name = object_name_to_string(&func.name);
        if name.eq_ignore_ascii_case("unnest") {
            if let FunctionArguments::List(ref list) = func.args {
                if let Some(FunctionArg::Unnamed(FunctionArgExpr::Expr(ref inner))) =
                    list.args.first()
                {
                    return Some(inner);
                }
            }
        }
    }
    None
}

fn build_null_left(orders: &[(String, Vec<String>)]) -> EvalRow {
    let mut row = EvalRow::default();
    for (alias, cols) in orders {
        let nr = EvalRow::null_for_table(alias, cols);
        row = row.merge(&nr);
    }
    row
}

fn eval_join_constraint(
    constraint: &JoinConstraint,
    left: &EvalRow,
    right: &EvalRow,
    merged: &EvalRow,
) -> Result<bool, EngineError> {
    match constraint {
        JoinConstraint::On(expr) => {
            let val = eval_scalar_expr(expr, merged)?;
            Ok(is_truthy(&val))
        }
        JoinConstraint::Using(idents) => {
            for ident in idents {
                let col = vec![ident.value.clone()];
                let lv = left.resolve_path(&col);
                let rv = right.resolve_path(&col);
                if lv.is_null() || rv.is_null() || !values_equal(&lv, &rv) {
                    return Ok(false);
                }
            }
            Ok(true)
        }
        JoinConstraint::None => Ok(true),
        JoinConstraint::Natural => Err(EngineError::Unsupported(
            "NATURAL JOIN is not supported; use ON or USING".to_string(),
        )),
    }
}

fn determine_projection_columns(
    projection: &[SelectItem],
    rows: &[EvalRow],
    fallback_order: &[(String, Vec<String>)],
) -> Result<Vec<String>, EngineError> {
    let mut cols = Vec::new();
    let active_order: &[(String, Vec<String>)] = rows
        .first()
        .map(|r| r.table_order.as_slice())
        .unwrap_or(fallback_order);

    let known_table_aliases: Vec<String> = active_order
        .iter()
        .map(|(a, _)| a.to_lowercase())
        .collect();

    for (idx, item) in projection.iter().enumerate() {
        match item {
            SelectItem::Wildcard(_) => {
                let multi_table = active_order.len() > 1;
                for (t_alias, t_cols) in active_order {
                    for c in t_cols {
                        if multi_table && cols.contains(c) {
                            cols.push(format!("{}.{}", t_alias, c));
                        } else {
                            cols.push(c.clone());
                        }
                    }
                }
            }
            SelectItem::QualifiedWildcard(obj_name, _) => {
                let t_name = object_name_to_string(obj_name);
                if let Some((_, t_cols)) = active_order
                    .iter()
                    .find(|(a, _)| a.eq_ignore_ascii_case(&t_name))
                {
                    for c in t_cols {
                        cols.push(c.clone());
                    }
                }
            }
            SelectItem::ExprWithAlias { alias, .. } => {
                cols.push(alias.value.clone());
            }
            SelectItem::UnnamedExpr(expr) => {
                cols.push(expr_default_column_name(expr, idx, &known_table_aliases));
            }
        }
    }

    Ok(cols)
}

fn expr_default_column_name(expr: &Expr, idx: usize, known_table_aliases: &[String]) -> String {
    match expr {
        Expr::Identifier(id) => id.value.clone(),
        Expr::CompoundIdentifier(ids) => {
            if ids.len() >= 2 && known_table_aliases.contains(&ids[0].value.to_lowercase()) {
                ids[1..]
                    .iter()
                    .map(|i| i.value.as_str())
                    .collect::<Vec<_>>()
                    .join(".")
            } else {
                ids.iter()
                    .map(|i| i.value.as_str())
                    .collect::<Vec<_>>()
                    .join(".")
            }
        }
        Expr::Function(f) => f.to_string(),
        _ => {
            let s = expr.to_string();
            if s.is_empty() {
                format!("col_{}", idx + 1)
            } else {
                s
            }
        }
    }
}

fn select_has_aggregates(select: &Select) -> bool {
    for item in &select.projection {
        match item {
            SelectItem::UnnamedExpr(e) | SelectItem::ExprWithAlias { expr: e, .. }
                if expr_contains_aggregate(e) =>
            {
                return true;
            }
            _ => {}
        }
    }
    if let Some(ref h) = select.having {
        if expr_contains_aggregate(h) {
            return true;
        }
    }
    false
}

fn expr_contains_aggregate(expr: &Expr) -> bool {
    match expr {
        Expr::Function(func) => {
            let name = object_name_to_string(&func.name).to_uppercase();
            if matches!(name.as_str(), "COUNT" | "SUM" | "AVG" | "MIN" | "MAX") {
                return true;
            }
            if let FunctionArguments::List(ref list) = func.args {
                for arg in &list.args {
                    if let FunctionArg::Unnamed(FunctionArgExpr::Expr(ref e)) = arg {
                        if expr_contains_aggregate(e) {
                            return true;
                        }
                    }
                }
            }
            false
        }
        Expr::BinaryOp { left, right, .. } => {
            expr_contains_aggregate(left) || expr_contains_aggregate(right)
        }
        Expr::UnaryOp { expr: inner, .. } | Expr::Nested(inner) => expr_contains_aggregate(inner),
        _ => false,
    }
}

/// Evaluate an expression in a `GROUP BY` context across a slice of grouped [`EvalRow`]s.
fn eval_group_expr(
    expr: &Expr,
    group_rows: &[EvalRow],
    _group_by_exprs: &[Expr],
) -> Result<Value, EngineError> {
    match expr {
        Expr::Function(func) => {
            let name = object_name_to_string(&func.name).to_uppercase();
            if matches!(name.as_str(), "COUNT" | "SUM" | "AVG" | "MIN" | "MAX") {
                return eval_aggregate_function(&name, func, group_rows);
            }
            // Otherwise scalar function evaluated over group expression arguments
            let mut arg_vals = Vec::new();
            if let FunctionArguments::List(ref list) = func.args {
                for arg in &list.args {
                    if let FunctionArg::Unnamed(FunctionArgExpr::Expr(ref e)) = arg {
                        arg_vals.push(eval_group_expr(e, group_rows, _group_by_exprs)?);
                    }
                }
            }
            eval_scalar_function(&name, &arg_vals)
        }
        Expr::BinaryOp { left, op, right } => {
            let lv = eval_group_expr(left, group_rows, _group_by_exprs)?;
            let rv = eval_group_expr(right, group_rows, _group_by_exprs)?;
            eval_binary_op(&lv, op, &rv)
        }
        Expr::UnaryOp { op, expr: inner } => {
            let v = eval_group_expr(inner, group_rows, _group_by_exprs)?;
            eval_unary_op(op, &v)
        }
        Expr::Nested(inner) => eval_group_expr(inner, group_rows, _group_by_exprs),
        _ => {
            // Non-aggregate expression in GROUP BY: evaluate against the representative row of the group
            if let Some(first_row) = group_rows.first() {
                eval_scalar_expr(expr, first_row)
            } else {
                Ok(Value::Null)
            }
        }
    }
}

fn eval_aggregate_function(
    name: &str,
    func: &Function,
    group_rows: &[EvalRow],
) -> Result<Value, EngineError> {
    let mut is_wildcard = false;
    let mut is_distinct = false;
    let mut arg_expr: Option<&Expr> = None;

    match &func.args {
        FunctionArguments::None => {}
        FunctionArguments::Subquery(_) => {
            return Err(EngineError::Unsupported(
                "Subquery inside aggregate function is not supported".to_string(),
            ))
        }
        FunctionArguments::List(list) => {
            if matches!(
                list.duplicate_treatment,
                Some(DuplicateTreatment::Distinct)
            ) {
                is_distinct = true;
            }
            if let Some(first_arg) = list.args.first() {
                match first_arg {
                    FunctionArg::Unnamed(FunctionArgExpr::Wildcard) => is_wildcard = true,
                    FunctionArg::Unnamed(FunctionArgExpr::QualifiedWildcard(_)) => {
                        is_wildcard = true
                    }
                    FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) => arg_expr = Some(e),
                    _ => {}
                }
            }
        }
    }

    if name == "COUNT" && is_wildcard {
        return Ok(Value::Number(Number::from(group_rows.len())));
    }

    let target_expr = arg_expr.ok_or_else(|| {
        EngineError::Evaluation(format!("Aggregate function {} requires an argument", name))
    })?;

    let mut values = Vec::with_capacity(group_rows.len());
    for row in group_rows {
        let v = eval_scalar_expr(target_expr, row)?;
        if !v.is_null() {
            if is_distinct {
                if !values.iter().any(|existing| values_equal(existing, &v)) {
                    values.push(v);
                }
            } else {
                values.push(v);
            }
        }
    }

    match name {
        "COUNT" => Ok(Value::Number(Number::from(values.len()))),
        "SUM" => {
            if values.is_empty() {
                return Ok(Value::Null);
            }
            let mut all_int = true;
            let mut int_sum: i64 = 0;
            let mut float_sum: f64 = 0.0;
            for v in &values {
                if let Some(i) = v.as_i64() {
                    int_sum = int_sum.saturating_add(i);
                    float_sum += i as f64;
                } else if let Some(f) = value_as_f64(v) {
                    all_int = false;
                    float_sum += f;
                }
            }
            if all_int {
                Ok(Value::Number(Number::from(int_sum)))
            } else {
                Ok(number_from_f64(float_sum))
            }
        }
        "AVG" => {
            let mut sum = 0.0;
            let mut count = 0usize;
            for v in &values {
                if let Some(f) = value_as_f64(v) {
                    sum += f;
                    count += 1;
                }
            }
            if count == 0 {
                Ok(Value::Null)
            } else {
                Ok(number_from_f64(sum / (count as f64)))
            }
        }
        "MIN" => {
            let mut best: Option<Value> = None;
            for v in values {
                best = Some(match best {
                    None => v,
                    Some(curr) => {
                        if compare_json_values(&v, &curr) == Ordering::Less {
                            v
                        } else {
                            curr
                        }
                    }
                });
            }
            Ok(best.unwrap_or(Value::Null))
        }
        "MAX" => {
            let mut best: Option<Value> = None;
            for v in values {
                best = Some(match best {
                    None => v,
                    Some(curr) => {
                        if compare_json_values(&v, &curr) == Ordering::Greater {
                            v
                        } else {
                            curr
                        }
                    }
                });
            }
            Ok(best.unwrap_or(Value::Null))
        }
        _ => Err(EngineError::Unsupported(format!(
            "Unsupported aggregate function: {}",
            name
        ))),
    }
}

/// Evaluate a scalar SQL expression against a single [`EvalRow`].
fn eval_scalar_expr(expr: &Expr, row: &EvalRow) -> Result<Value, EngineError> {
    match expr {
        Expr::Value(sql_val) => eval_sql_literal(sql_val),
        Expr::Identifier(ident) => Ok(resolve_identifier_or_dot_path(&ident.value, row)),
        Expr::CompoundIdentifier(idents) => {
            let parts: Vec<String> = idents.iter().map(|i| i.value.clone()).collect();
            Ok(row.resolve_path(&parts))
        }
        Expr::Nested(inner) => eval_scalar_expr(inner, row),
        Expr::UnaryOp { op, expr: inner } => {
            let v = eval_scalar_expr(inner, row)?;
            eval_unary_op(op, &v)
        }
        Expr::BinaryOp { left, op, right } => {
            // Short-circuit logical AND / OR
            match op {
                BinaryOperator::And => {
                    let lv = eval_scalar_expr(left, row)?;
                    if !is_truthy(&lv) {
                        return Ok(Value::Bool(false));
                    }
                    let rv = eval_scalar_expr(right, row)?;
                    return Ok(Value::Bool(is_truthy(&rv)));
                }
                BinaryOperator::Or => {
                    let lv = eval_scalar_expr(left, row)?;
                    if is_truthy(&lv) {
                        return Ok(Value::Bool(true));
                    }
                    let rv = eval_scalar_expr(right, row)?;
                    return Ok(Value::Bool(is_truthy(&rv)));
                }
                _ => {}
            }
            let lv = eval_scalar_expr(left, row)?;
            let rv = eval_scalar_expr(right, row)?;
            eval_binary_op(&lv, op, &rv)
        }
        Expr::IsNull(inner) => {
            let v = eval_scalar_expr(inner, row)?;
            Ok(Value::Bool(v.is_null()))
        }
        Expr::IsNotNull(inner) => {
            let v = eval_scalar_expr(inner, row)?;
            Ok(Value::Bool(!v.is_null()))
        }
        Expr::IsTrue(inner) => {
            let v = eval_scalar_expr(inner, row)?;
            Ok(Value::Bool(v == Value::Bool(true)))
        }
        Expr::IsFalse(inner) => {
            let v = eval_scalar_expr(inner, row)?;
            Ok(Value::Bool(v == Value::Bool(false)))
        }
        Expr::InList {
            expr: target,
            list,
            negated,
        } => {
            let tv = eval_scalar_expr(target, row)?;
            if tv.is_null() {
                return Ok(Value::Bool(false));
            }
            let mut found = false;
            for item in list {
                let iv = eval_scalar_expr(item, row)?;
                if values_equal(&tv, &iv) {
                    found = true;
                    break;
                }
            }
            Ok(Value::Bool(if *negated { !found } else { found }))
        }
        Expr::Between {
            expr: target,
            negated,
            low,
            high,
        } => {
            let tv = eval_scalar_expr(target, row)?;
            let lv = eval_scalar_expr(low, row)?;
            let hv = eval_scalar_expr(high, row)?;
            if tv.is_null() || lv.is_null() || hv.is_null() {
                return Ok(Value::Bool(false));
            }
            let ge_low = matches!(
                compare_json_values(&tv, &lv),
                Ordering::Greater | Ordering::Equal
            );
            let le_high = matches!(
                compare_json_values(&tv, &hv),
                Ordering::Less | Ordering::Equal
            );
            let inside = ge_low && le_high;
            Ok(Value::Bool(if *negated { !inside } else { inside }))
        }
        Expr::Like {
            negated,
            expr: target,
            pattern,
            any: _,
            escape_char: _,
        } => {
            let tv = eval_scalar_expr(target, row)?;
            let pv = eval_scalar_expr(pattern, row)?;
            match (tv.as_str(), pv.as_str()) {
                (Some(s), Some(p)) => {
                    let matched = sql_like_match(s, p, false);
                    Ok(Value::Bool(if *negated { !matched } else { matched }))
                }
                _ => Ok(Value::Bool(false)),
            }
        }
        Expr::ILike {
            negated,
            expr: target,
            pattern,
            any: _,
            escape_char: _,
        } => {
            let tv = eval_scalar_expr(target, row)?;
            let pv = eval_scalar_expr(pattern, row)?;
            match (tv.as_str(), pv.as_str()) {
                (Some(s), Some(p)) => {
                    let matched = sql_like_match(s, p, true);
                    Ok(Value::Bool(if *negated { !matched } else { matched }))
                }
                _ => Ok(Value::Bool(false)),
            }
        }
        Expr::Subscript { expr: base, subscript } => {
            let base_val = eval_scalar_expr(base, row)?;
            match subscript.as_ref() {
                sqlparser::ast::Subscript::Index { index } => {
                    let idx_val = eval_scalar_expr(index, row)?;
                    match (&base_val, &idx_val) {
                        (Value::Array(arr), _) => {
                            if let Some(i) = value_as_usize(&idx_val) {
                                Ok(arr.get(i).cloned().unwrap_or(Value::Null))
                            } else {
                                Ok(Value::Null)
                            }
                        }
                        (Value::Object(map), Value::String(k)) => {
                            Ok(get_field_case_insensitive(map, k).cloned().unwrap_or(Value::Null))
                        }
                        _ => Ok(Value::Null),
                    }
                }
                _ => Err(EngineError::Unsupported(
                    "Array slice subscript is not supported".to_string(),
                )),
            }
        }
        Expr::CompositeAccess { expr: base, key } => {
            let base_val = eval_scalar_expr(base, row)?;
            Ok(traverse_json_value(
                &base_val,
                std::slice::from_ref(&key.value),
            ))
        }
        Expr::JsonAccess { value, path } => {
            let base_val = eval_scalar_expr(value, row)?;
            let raw_path = path.to_string();
            let cleaned = raw_path
                .trim_start_matches("->")
                .trim_start_matches('>')
                .trim()
                .trim_matches('\'')
                .trim_matches('"');
            let parts: Vec<String> = cleaned.split('.').map(|s| s.to_string()).collect();
            Ok(traverse_json_value(&base_val, &parts))
        }
        Expr::Function(func) => {
            let name = object_name_to_string(&func.name).to_uppercase();
            let mut arg_vals = Vec::new();
            if let FunctionArguments::List(ref list) = func.args {
                for arg in &list.args {
                    if let FunctionArg::Unnamed(FunctionArgExpr::Expr(ref e)) = arg {
                        arg_vals.push(eval_scalar_expr(e, row)?);
                    }
                }
            }
            eval_scalar_function(&name, &arg_vals)
        }
        Expr::Cast {
            expr: inner,
            data_type,
            ..
        } => {
            let v = eval_scalar_expr(inner, row)?;
            eval_cast(&v, &data_type.to_string().to_uppercase())
        }
        Expr::Case {
            operand,
            conditions,
            results,
            else_result,
        } => {
            let op_val = match operand {
                Some(op_e) => Some(eval_scalar_expr(op_e, row)?),
                None => None,
            };
            for (cond_expr, res_expr) in conditions.iter().zip(results.iter()) {
                let cond_val = eval_scalar_expr(cond_expr, row)?;
                let matched = match &op_val {
                    Some(ov) => values_equal(ov, &cond_val),
                    None => is_truthy(&cond_val),
                };
                if matched {
                    return eval_scalar_expr(res_expr, row);
                }
            }
            if let Some(else_e) = else_result {
                eval_scalar_expr(else_e, row)
            } else {
                Ok(Value::Null)
            }
        }
        other => Err(EngineError::Unsupported(format!(
            "Unsupported SQL expression: {}",
            other
        ))),
    }
}

fn resolve_identifier_or_dot_path(ident: &str, row: &EvalRow) -> Value {
    // First check exact match (in case column name has dots or is a simple identifier)
    let exact = row.resolve_path(&[ident.to_string()]);
    if !exact.is_null() {
        return exact;
    }
    if ident.contains('.') {
        let parts: Vec<String> = ident.split('.').map(|s| s.to_string()).collect();
        return row.resolve_path(&parts);
    }
    Value::Null
}

fn eval_sql_literal(sql_val: &SqlValue) -> Result<Value, EngineError> {
    match sql_val {
        SqlValue::Number(n_str, _) => {
            if let Ok(i) = n_str.parse::<i64>() {
                Ok(Value::Number(Number::from(i)))
            } else if let Ok(f) = n_str.parse::<f64>() {
                Ok(number_from_f64(f))
            } else {
                Err(EngineError::Evaluation(format!(
                    "Invalid numeric literal: {}",
                    n_str
                )))
            }
        }
        SqlValue::SingleQuotedString(s)
        | SqlValue::DoubleQuotedString(s)
        | SqlValue::NationalStringLiteral(s) => Ok(Value::String(s.clone())),
        SqlValue::Boolean(b) => Ok(Value::Bool(*b)),
        SqlValue::Null => Ok(Value::Null),
        other => Err(EngineError::Unsupported(format!(
            "Unsupported literal value: {}",
            other
        ))),
    }
}

fn eval_unary_op(op: &UnaryOperator, val: &Value) -> Result<Value, EngineError> {
    match op {
        UnaryOperator::Not => Ok(Value::Bool(!is_truthy(val))),
        UnaryOperator::Minus => {
            if let Some(i) = val.as_i64() {
                Ok(Value::Number(Number::from(-i)))
            } else if let Some(f) = value_as_f64(val) {
                Ok(number_from_f64(-f))
            } else {
                Ok(Value::Null)
            }
        }
        UnaryOperator::Plus => Ok(val.clone()),
        other => Err(EngineError::Unsupported(format!(
            "Unsupported unary operator: {}",
            other
        ))),
    }
}

fn eval_binary_op(left: &Value, op: &BinaryOperator, right: &Value) -> Result<Value, EngineError> {
    match op {
        BinaryOperator::Eq => {
            if left.is_null() || right.is_null() {
                Ok(Value::Bool(false))
            } else {
                Ok(Value::Bool(values_equal(left, right)))
            }
        }
        BinaryOperator::NotEq => {
            if left.is_null() || right.is_null() {
                Ok(Value::Bool(false))
            } else {
                Ok(Value::Bool(!values_equal(left, right)))
            }
        }
        BinaryOperator::Lt => {
            if left.is_null() || right.is_null() {
                Ok(Value::Bool(false))
            } else {
                Ok(Value::Bool(
                    compare_json_values(left, right) == Ordering::Less,
                ))
            }
        }
        BinaryOperator::LtEq => {
            if left.is_null() || right.is_null() {
                Ok(Value::Bool(false))
            } else {
                Ok(Value::Bool(matches!(
                    compare_json_values(left, right),
                    Ordering::Less | Ordering::Equal
                )))
            }
        }
        BinaryOperator::Gt => {
            if left.is_null() || right.is_null() {
                Ok(Value::Bool(false))
            } else {
                Ok(Value::Bool(
                    compare_json_values(left, right) == Ordering::Greater,
                ))
            }
        }
        BinaryOperator::GtEq => {
            if left.is_null() || right.is_null() {
                Ok(Value::Bool(false))
            } else {
                Ok(Value::Bool(matches!(
                    compare_json_values(left, right),
                    Ordering::Greater | Ordering::Equal
                )))
            }
        }
        BinaryOperator::Plus => numeric_op(left, right, |a, b| a + b, |a, b| a + b),
        BinaryOperator::Minus => numeric_op(left, right, |a, b| a - b, |a, b| a - b),
        BinaryOperator::Multiply => numeric_op(left, right, |a, b| a * b, |a, b| a * b),
        BinaryOperator::Divide => {
            if let (Some(a), Some(b)) = (value_as_f64(left), value_as_f64(right)) {
                if b.abs() < 1e-15 {
                    Ok(Value::Null)
                } else {
                    Ok(number_from_f64(a / b))
                }
            } else {
                Ok(Value::Null)
            }
        }
        BinaryOperator::Modulo => numeric_op(
            left,
            right,
            |a, b| if b == 0 { 0 } else { a % b },
            |a, b| a % b,
        ),
        BinaryOperator::StringConcat => {
            let ls = crate::cli::value_to_display_string(left);
            let rs = crate::cli::value_to_display_string(right);
            Ok(Value::String(format!("{}{}", ls, rs)))
        }
        other => Err(EngineError::Unsupported(format!(
            "Unsupported binary operator: {}",
            other
        ))),
    }
}

fn numeric_op<FI, FF>(left: &Value, right: &Value, int_fn: FI, float_fn: FF) -> Result<Value, EngineError>
where
    FI: FnOnce(i64, i64) -> i64,
    FF: FnOnce(f64, f64) -> f64,
{
    if let (Some(li), Some(ri)) = (left.as_i64(), right.as_i64()) {
        return Ok(Value::Number(Number::from(int_fn(li, ri))));
    }
    if let (Some(lf), Some(rf)) = (value_as_f64(left), value_as_f64(right)) {
        return Ok(number_from_f64(float_fn(lf, rf)));
    }
    Ok(Value::Null)
}

fn eval_scalar_function(name: &str, args: &[Value]) -> Result<Value, EngineError> {
    match name {
        "LOWER" => Ok(args
            .first()
            .and_then(|v| v.as_str())
            .map(|s| Value::String(s.to_lowercase()))
            .unwrap_or(Value::Null)),
        "UPPER" => Ok(args
            .first()
            .and_then(|v| v.as_str())
            .map(|s| Value::String(s.to_uppercase()))
            .unwrap_or(Value::Null)),
        "LENGTH" | "LEN" | "ARRAY_LENGTH" | "JSON_ARRAY_LENGTH" => match args.first() {
            Some(Value::String(s)) => Ok(Value::Number(Number::from(s.chars().count()))),
            Some(Value::Array(a)) => Ok(Value::Number(Number::from(a.len()))),
            Some(Value::Object(m)) => Ok(Value::Number(Number::from(m.len()))),
            _ => Ok(Value::Null),
        },
        "COALESCE" | "IFNULL" => {
            for a in args {
                if !a.is_null() {
                    return Ok(a.clone());
                }
            }
            Ok(Value::Null)
        }
        "ROUND" => {
            let val = match args.first().and_then(value_as_f64) {
                Some(f) => f,
                None => return Ok(Value::Null),
            };
            let decimals = args
                .get(1)
                .and_then(|v| v.as_i64())
                .unwrap_or(0)
                .clamp(0, 12) as u32;
            let factor = 10_f64.powi(decimals as i32);
            let rounded = (val * factor).round() / factor;
            if decimals == 0 {
                Ok(Value::Number(Number::from(rounded as i64)))
            } else {
                Ok(number_from_f64(rounded))
            }
        }
        "ABS" => {
            if let Some(i) = args.first().and_then(|v| v.as_i64()) {
                Ok(Value::Number(Number::from(i.abs())))
            } else if let Some(f) = args.first().and_then(value_as_f64) {
                Ok(number_from_f64(f.abs()))
            } else {
                Ok(Value::Null)
            }
        }
        "TRIM" => Ok(args
            .first()
            .and_then(|v| v.as_str())
            .map(|s| Value::String(s.trim().to_string()))
            .unwrap_or(Value::Null)),
        "CONCAT" => {
            let mut out = String::new();
            for a in args {
                if !a.is_null() {
                    out.push_str(&crate::cli::value_to_display_string(a));
                }
            }
            Ok(Value::String(out))
        }
        "UNNEST" => Ok(args.first().cloned().unwrap_or(Value::Null)),
        other => Err(EngineError::Unsupported(format!(
            "Unsupported function: {}",
            other
        ))),
    }
}

fn eval_cast(val: &Value, target_type: &str) -> Result<Value, EngineError> {
    if val.is_null() {
        return Ok(Value::Null);
    }
    if target_type.contains("INT") {
        if let Some(i) = val.as_i64() {
            return Ok(Value::Number(Number::from(i)));
        }
        if let Some(f) = val.as_f64() {
            return Ok(Value::Number(Number::from(f as i64)));
        }
        if let Some(s) = val.as_str() {
            if let Ok(i) = s.trim().parse::<i64>() {
                return Ok(Value::Number(Number::from(i)));
            }
        }
        Ok(Value::Null)
    } else if target_type.contains("FLOAT")
        || target_type.contains("DOUBLE")
        || target_type.contains("REAL")
        || target_type.contains("NUMERIC")
        || target_type.contains("DECIMAL")
    {
        if let Some(f) = value_as_f64(val) {
            return Ok(number_from_f64(f));
        }
        if let Some(s) = val.as_str() {
            if let Ok(f) = s.trim().parse::<f64>() {
                return Ok(number_from_f64(f));
            }
        }
        Ok(Value::Null)
    } else if target_type.contains("CHAR")
        || target_type.contains("TEXT")
        || target_type.contains("STRING")
    {
        Ok(Value::String(crate::cli::value_to_display_string(val)))
    } else if target_type.contains("BOOL") {
        Ok(Value::Bool(is_truthy(val)))
    } else {
        Ok(val.clone())
    }
}

fn sort_output_rows(
    columns: &[String],
    output_rows: &mut [Vec<Value>],
    context_rows: &[EvalRow],
    order_exprs: &[OrderByExpr],
) -> Result<(), EngineError> {
    // Precompute sort keys for each row
    let mut keys_per_row: Vec<Vec<Value>> = Vec::with_capacity(output_rows.len());
    for (r_idx, out_row) in output_rows.iter().enumerate() {
        let mut row_keys = Vec::with_capacity(order_exprs.len());
        for ob in order_exprs {
            // 1. Check if ORDER BY is a 1-based positional column index (e.g. `ORDER BY 1 DESC`)
            let key_val = if let Expr::Value(SqlValue::Number(ref n_str, _)) = ob.expr {
                if let Ok(pos) = n_str.parse::<usize>() {
                    if pos >= 1 && pos <= out_row.len() {
                        out_row[pos - 1].clone()
                    } else {
                        Value::Null
                    }
                } else {
                    Value::Null
                }
            } else if let Expr::Identifier(Ident { value: id_name, .. }) = &ob.expr {
                if let Some(col_idx) = columns
                    .iter()
                    .position(|c| c.eq_ignore_ascii_case(id_name))
                {
                    out_row[col_idx].clone()
                } else if let Some(ctx) = context_rows.get(r_idx) {
                    eval_scalar_expr(&ob.expr, ctx)?
                } else {
                    Value::Null
                }
            } else if let Some(col_idx) = columns
                .iter()
                .position(|c| c.eq_ignore_ascii_case(&ob.expr.to_string()))
            {
                out_row[col_idx].clone()
            } else if let Some(ctx) = context_rows.get(r_idx) {
                eval_scalar_expr(&ob.expr, ctx)?
            } else {
                Value::Null
            };
            row_keys.push(key_val);
        }
        keys_per_row.push(row_keys);
    }

    let mut indices: Vec<usize> = (0..output_rows.len()).collect();
    indices.sort_by(|&a, &b| {
        for (k_idx, ob) in order_exprs.iter().enumerate() {
            let va = &keys_per_row[a][k_idx];
            let vb = &keys_per_row[b][k_idx];
            let asc = ob.asc.unwrap_or(true);
            let ord = match (va.is_null(), vb.is_null()) {
                (true, true) => Ordering::Equal,
                (true, false) => Ordering::Greater, // NULLs last
                (false, true) => Ordering::Less,
                (false, false) => {
                    let cmp = compare_json_values(va, vb);
                    if asc {
                        cmp
                    } else {
                        cmp.reverse()
                    }
                }
            };
            if ord != Ordering::Equal {
                return ord;
            }
        }
        a.cmp(&b)
    });

    let sorted: Vec<Vec<Value>> = indices
        .into_iter()
        .map(|i| output_rows[i].clone())
        .collect();
    output_rows.clone_from_slice(&sorted);
    Ok(())
}

/// Compare two JSON values with proper numeric, boolean, and lexicographical ordering.
pub fn compare_json_values(a: &Value, b: &Value) -> Ordering {
    match (a, b) {
        (Value::Null, Value::Null) => Ordering::Equal,
        (Value::Null, _) => Ordering::Less,
        (_, Value::Null) => Ordering::Greater,
        (Value::Bool(ba), Value::Bool(bb)) => ba.cmp(bb),
        (Value::Number(_), Value::Number(_)) => {
            if let (Some(ia), Some(ib)) = (a.as_i64(), b.as_i64()) {
                ia.cmp(&ib)
            } else if let (Some(fa), Some(fb)) = (value_as_f64(a), value_as_f64(b)) {
                fa.partial_cmp(&fb).unwrap_or(Ordering::Equal)
            } else {
                Ordering::Equal
            }
        }
        (Value::String(sa), Value::String(sb)) => sa.cmp(sb),
        _ => {
            let sa = crate::cli::value_to_display_string(a);
            let sb = crate::cli::value_to_display_string(b);
            sa.cmp(&sb)
        }
    }
}

pub fn values_equal(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Null, Value::Null) => true,
        (Value::Bool(ba), Value::Bool(bb)) => ba == bb,
        (Value::Number(_), Value::Number(_)) => {
            if let (Some(ia), Some(ib)) = (a.as_i64(), b.as_i64()) {
                ia == ib
            } else if let (Some(fa), Some(fb)) = (value_as_f64(a), value_as_f64(b)) {
                (fa - fb).abs() < 1e-12
            } else {
                false
            }
        }
        (Value::String(sa), Value::String(sb)) => sa == sb,
        (Value::Array(aa), Value::Array(ab)) => {
            aa.len() == ab.len() && aa.iter().zip(ab.iter()).all(|(x, y)| values_equal(x, y))
        }
        (Value::Object(oa), Value::Object(ob)) => oa == ob,
        _ => false,
    }
}

fn rows_equal(a: &[Value], b: &[Value]) -> bool {
    a.len() == b.len() && a.iter().zip(b.iter()).all(|(x, y)| values_equal(x, y))
}

fn is_truthy(val: &Value) -> bool {
    match val {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().map(|f| f != 0.0).unwrap_or(false),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

fn value_as_f64(val: &Value) -> Option<f64> {
    match val {
        Value::Number(n) => n.as_f64(),
        _ => None,
    }
}

fn value_as_usize(val: &Value) -> Option<usize> {
    if let Some(u) = val.as_u64() {
        Some(u as usize)
    } else if let Some(i) = val.as_i64() {
        if i >= 0 {
            Some(i as usize)
        } else {
            None
        }
    } else {
        None
    }
}

fn number_from_f64(f: f64) -> Value {
    Number::from_f64(f)
        .map(Value::Number)
        .unwrap_or(Value::Null)
}

fn object_name_to_string(name: &ObjectName) -> String {
    name.0
        .iter()
        .map(|p| p.value.as_str())
        .collect::<Vec<_>>()
        .join(".")
}

/// SQL `LIKE` / `ILIKE` pattern matcher supporting `%` (0 or more chars) and `_` (any 1 char).
fn sql_like_match(text: &str, pattern: &str, case_insensitive: bool) -> bool {
    let t_chars: Vec<char> = if case_insensitive {
        text.to_lowercase().chars().collect()
    } else {
        text.chars().collect()
    };
    let p_chars: Vec<char> = if case_insensitive {
        pattern.to_lowercase().chars().collect()
    } else {
        pattern.chars().collect()
    };

    let (m, n) = (t_chars.len(), p_chars.len());
    let mut dp = vec![vec![false; n + 1]; m + 1];
    dp[0][0] = true;

    for j in 1..=n {
        if p_chars[j - 1] == '%' {
            dp[0][j] = dp[0][j - 1];
        }
    }

    for i in 1..=m {
        for j in 1..=n {
            if p_chars[j - 1] == '%' {
                dp[i][j] = dp[i][j - 1] || dp[i - 1][j];
            } else if p_chars[j - 1] == '_' || p_chars[j - 1] == t_chars[i - 1] {
                dp[i][j] = dp[i - 1][j - 1];
            }
        }
    }

    dp[m][n]
}
