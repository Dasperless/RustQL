//! Main entry point for the `rustrix` binary.

use clap::Parser;
use rustrix::cli::{format_query_result, CliArgs};
use rustrix::engine::QueryEngine;
use rustrix::loader::{load_json_file, parse_file_spec};
use rustrix::tui::run_tui;
use std::process;

fn main() {
    let args = CliArgs::parse();

    let mut engine = QueryEngine::new();
    for spec in &args.files {
        let (table_name, path) = parse_file_spec(spec);
        match load_json_file(
            &table_name,
            &path,
            args.cache_dir.as_deref(),
            args.no_cache,
        ) {
            Ok(table) => {
                engine.register_table(table);
            }
            Err(err) => {
                eprintln!("Error loading '{}': {}", spec, err);
                process::exit(1);
            }
        }
    }

    if let Some(ref sql) = args.query {
        // Non-interactive CLI mode (`-q "SELECT ..."`)
        match engine.execute(sql) {
            Ok(result) => {
                let output = format_query_result(&result, args.format);
                print!("{}", output);
            }
            Err(err) => {
                let friendly =
                    rustrix::sql_helper::diagnose_sql_error(sql, &err.to_string(), &engine);
                eprintln!("Query error: {}", friendly);
                process::exit(1);
            }
        }
    } else {
        // Interactive TUI mode
        if let Err(err) = run_tui(engine) {
            eprintln!("TUI error: {}", err);
            process::exit(1);
        }
    }
}
