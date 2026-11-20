//! Main entry point for the `rustql` binary.

#![forbid(unsafe_code)]

use clap::Parser;
use rustql::cli::{format_query_result, CliArgs};
use rustql::engine::QueryEngine;
use rustql::loader::{load_json_file, parse_file_spec};
use rustql::tui::run_tui;
use std::process::ExitCode;

fn main() -> ExitCode {
    let args = CliArgs::parse();

    let mut engine = QueryEngine::new();
    for spec in &args.files {
        let (table_name, path) = parse_file_spec(spec);
        match load_json_file(&table_name, &path, args.cache_dir.as_deref(), args.no_cache) {
            Ok(table) => {
                engine.register_table(table);
            }
            Err(err) => {
                eprintln!("Error loading '{}': {}", spec, err);
                return ExitCode::FAILURE;
            }
        }
    }

    if let Some(ref sql) = args.query {
        // Non-interactive CLI mode (`-q "SELECT ..."`)
        match engine.execute(sql) {
            Ok(result) => {
                let output = format_query_result(&result, args.format);
                print!("{}", output);
                ExitCode::SUCCESS
            }
            Err(err) => {
                let friendly =
                    rustql::sql_helper::diagnose_sql_error(sql, &err.to_string(), &engine);
                eprintln!("Query error: {}", friendly);
                ExitCode::FAILURE
            }
        }
    } else {
        // Interactive TUI mode
        if let Err(err) = run_tui(engine) {
            eprintln!("TUI error: {}", err);
            return ExitCode::FAILURE;
        }
        ExitCode::SUCCESS
    }
}
