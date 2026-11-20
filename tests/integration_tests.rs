//! Integration tests for Rustrix SQL features, shape detection, caching, and read-only safety.

use rustrix::cli::{format_query_result, OutputFormat};
use rustrix::engine::QueryEngine;
use rustrix::loader::{detect_shape_from_str, load_json_file, parse_file_spec, JsonShape};
use serde_json::{json, Value};
use std::fs;
use std::path::Path;
use tempfile::TempDir;

fn setup_examples_engine(cache_dir: &Path) -> QueryEngine {
    let mut engine = QueryEngine::new();
    let files = [
        "examples/users.json",
        "examples/orders.ndjson",
        "examples/nested_catalog.json",
        "examples/single_config.json",
    ];
    for spec in &files {
        let (name, path) = parse_file_spec(spec);
        let loaded = load_json_file(&name, &path, Some(cache_dir), false)
            .unwrap_or_else(|e| panic!("Failed to load {}: {}", spec, e));
        engine.register_table(loaded);
    }
    engine
}

#[test]
fn test_shape_detection_across_all_example_files() {
    let users_content = fs::read_to_string("examples/users.json").unwrap();
    assert_eq!(
        detect_shape_from_str(&users_content, "users.json").unwrap(),
        JsonShape::TopLevelArray
    );

    let orders_content = fs::read_to_string("examples/orders.ndjson").unwrap();
    assert_eq!(
        detect_shape_from_str(&orders_content, "orders.ndjson").unwrap(),
        JsonShape::Ndjson
    );

    let catalog_content = fs::read_to_string("examples/nested_catalog.json").unwrap();
    assert_eq!(
        detect_shape_from_str(&catalog_content, "nested_catalog.json").unwrap(),
        JsonShape::NestedObjectArray {
            extracted_key: "records".to_string()
        }
    );

    let config_content = fs::read_to_string("examples/single_config.json").unwrap();
    assert_eq!(
        detect_shape_from_str(&config_content, "single_config.json").unwrap(),
        JsonShape::SingleObject
    );
}

#[test]
fn test_select_where_order_by_limit() {
    let tmp = TempDir::new().unwrap();
    let engine = setup_examples_engine(tmp.path());

    let res = engine
        .execute(
            "SELECT id, name, salary \
             FROM users \
             WHERE active = true AND salary >= 145000 \
             ORDER BY salary DESC \
             LIMIT 2",
        )
        .unwrap();

    assert_eq!(res.columns, vec!["id", "name", "salary"]);
    assert_eq!(res.rows.len(), 2);
    assert_eq!(res.rows[0], vec![json!(5), json!("Elena Rostova"), json!(192000)]);
    assert_eq!(res.rows[1], vec![json!(1), json!("Alice Vance"), json!(165000)]);
}

#[test]
fn test_group_by_with_all_aggregates() {
    let tmp = TempDir::new().unwrap();
    let engine = setup_examples_engine(tmp.path());

    let res = engine
        .execute(
            "SELECT \
                department, \
                COUNT(*) AS emp_count, \
                SUM(salary) AS total_salary, \
                AVG(salary) AS avg_salary, \
                MIN(salary) AS min_salary, \
                MAX(salary) AS max_salary \
             FROM users \
             GROUP BY department \
             ORDER BY emp_count DESC, department ASC",
        )
        .unwrap();

    assert_eq!(
        res.columns,
        vec![
            "department",
            "emp_count",
            "total_salary",
            "avg_salary",
            "min_salary",
            "max_salary"
        ]
    );
    assert_eq!(res.rows.len(), 3);

    // Platform department: Alice (165000), Bob (148000), Elena (192000)
    // Count = 3, Sum = 505000, Min = 148000, Max = 192000
    assert_eq!(res.rows[0][0], json!("Platform"));
    assert_eq!(res.rows[0][1], json!(3));
    assert_eq!(res.rows[0][2], json!(505000));
    let avg_platform = res.rows[0][3].as_f64().unwrap();
    assert!((avg_platform - 168333.333333).abs() < 0.01);
    assert_eq!(res.rows[0][4], json!(148000));
    assert_eq!(res.rows[0][5], json!(192000));

    // Analytics department: David (118000)
    assert_eq!(res.rows[1][0], json!("Analytics"));
    assert_eq!(res.rows[1][1], json!(1));
    assert_eq!(res.rows[1][2], json!(118000));

    // Product department: Clara (142000)
    assert_eq!(res.rows[2][0], json!("Product"));
    assert_eq!(res.rows[2][1], json!(1));
    assert_eq!(res.rows[2][2], json!(142000));
}

#[test]
fn test_join_across_two_json_files() {
    let tmp = TempDir::new().unwrap();
    let engine = setup_examples_engine(tmp.path());

    let res = engine
        .execute(
            "SELECT \
                u.name AS customer, \
                u.address.city AS city, \
                o.order_id, \
                o.total, \
                o.status \
             FROM users u \
             JOIN orders o ON u.id = o.user_id \
             WHERE o.status != 'cancelled' \
             ORDER BY o.total DESC",
        )
        .unwrap();

    assert_eq!(
        res.columns,
        vec!["customer", "city", "order_id", "total", "status"]
    );
    assert_eq!(res.rows.len(), 5);
    assert_eq!(
        res.rows[0],
        vec![
            json!("Elena Rostova"),
            json!("San Francisco"),
            json!(105),
            json!(1299.0),
            json!("shipped")
        ]
    );
    assert_eq!(
        res.rows[1],
        vec![
            json!("Bob Chen"),
            json!("Austin"),
            json!(102),
            json!(599.0),
            json!("delivered")
        ]
    );
}

#[test]
fn test_join_with_group_by_across_files() {
    let tmp = TempDir::new().unwrap();
    let engine = setup_examples_engine(tmp.path());

    let res = engine
        .execute(
            "SELECT \
                u.name, \
                COUNT(o.order_id) AS order_count, \
                SUM(o.total) AS total_spent \
             FROM users u \
             LEFT JOIN orders o ON u.id = o.user_id \
             GROUP BY u.name \
             ORDER BY total_spent DESC",
        )
        .unwrap();

    assert_eq!(res.columns, vec!["name", "order_count", "total_spent"]);
    // Elena spent 1299.0, Bob spent 599+45=644.0, Alice spent 249.5+89.99=339.49, Clara spent 145.0, David spent NULL (0 orders)
    assert_eq!(res.rows[0][0], json!("Elena Rostova"));
    assert_eq!(res.rows[0][1], json!(1));
    assert_eq!(res.rows[0][2], json!(1299.0));

    assert_eq!(res.rows[1][0], json!("Bob Chen"));
    assert_eq!(res.rows[1][1], json!(2));
    assert_eq!(res.rows[1][2], json!(644.0));

    // David Kim has 0 orders in LEFT JOIN -> COUNT(o.order_id) = 0, SUM(o.total) = NULL
    let david_row = res
        .rows
        .iter()
        .find(|r| r[0] == json!("David Kim"))
        .expect("David Kim should appear in LEFT JOIN");
    assert_eq!(david_row[1], json!(0));
    assert_eq!(david_row[2], Value::Null);
}

#[test]
fn test_nested_dot_paths_and_array_unnesting() {
    let tmp = TempDir::new().unwrap();
    let engine = setup_examples_engine(tmp.path());

    // 1. Nested dot path + array indexing
    let res_dot = engine
        .execute(
            "SELECT sku, warehouse.city AS wh_city, specs.weight_grams, suppliers[0] AS primary_supplier \
             FROM nested_catalog \
             WHERE specs.wireless = true \
             ORDER BY stock DESC",
        )
        .unwrap();

    assert_eq!(
        res_dot.columns,
        vec!["sku", "wh_city", "specs.weight_grams", "primary_supplier"]
    );
    assert_eq!(res_dot.rows.len(), 2);
    assert_eq!(
        res_dot.rows[0],
        vec![json!("MS-PRO"), json!("Seattle"), json!(145), json!("ErgoMotion")]
    );
    assert_eq!(
        res_dot.rows[1],
        vec![json!("HD-NC"), json!("San Francisco"), json!(260), json!("AcoustiCore")]
    );

    // 2. Lateral UNNEST in FROM clause on scalar array (`users.tags`)
    let res_unnest_from = engine
        .execute(
            "SELECT u.name, tag \
             FROM users u, UNNEST(u.tags) AS tag \
             WHERE tag = 'rust' \
             ORDER BY u.id ASC",
        )
        .unwrap();

    assert_eq!(res_unnest_from.rows.len(), 3);
    assert_eq!(res_unnest_from.rows[0], vec![json!("Alice Vance"), json!("rust")]);
    assert_eq!(res_unnest_from.rows[1], vec![json!("Bob Chen"), json!("rust")]);
    assert_eq!(res_unnest_from.rows[2], vec![json!("Elena Rostova"), json!("rust")]);

    // 3. Lateral UNNEST in FROM clause on array of objects (`orders.items`) + 3-table join!
    let res_unnest_objects = engine
        .execute(
            "SELECT o.order_id, item.sku, item.name, item.qty, c.warehouse.city AS ship_from \
             FROM orders o, UNNEST(o.items) AS item \
             JOIN nested_catalog c ON item.sku = c.sku \
             ORDER BY o.order_id ASC",
        )
        .unwrap();

    assert_eq!(res_unnest_objects.rows.len(), 5);
    assert_eq!(
        res_unnest_objects.rows[0],
        vec![
            json!(101),
            json!("KB-01"),
            json!("Mechanical Keyboard"),
            json!(1),
            json!("Seattle")
        ]
    );

    // 4. Inline UNNEST directly in SELECT clause on SingleObject (`single_config`)
    let res_inline_unnest = engine
        .execute(
            "SELECT organization, cluster.primary_region AS region, UNNEST(features) AS feature \
             FROM single_config",
        )
        .unwrap();

    assert_eq!(res_inline_unnest.rows.len(), 4);
    assert_eq!(
        res_inline_unnest.rows[0],
        vec![json!("Rustrix Labs"), json!("us-east-1"), json!("streaming")]
    );
    assert_eq!(
        res_inline_unnest.rows[3],
        vec![json!("Rustrix Labs"), json!("us-east-1"), json!("unnest")]
    );
}

#[test]
fn test_caching_and_source_files_remain_byte_identical() {
    let tmp = TempDir::new().unwrap();
    let files = [
        "examples/users.json",
        "examples/orders.ndjson",
        "examples/nested_catalog.json",
        "examples/single_config.json",
    ];

    // Snapshot original bytes of all example files
    let before_snapshots: Vec<Vec<u8>> = files
        .iter()
        .map(|p| fs::read(p).expect("read original file"))
        .collect();

    // First load populates the cache
    let (name, path) = parse_file_spec("examples/users.json");
    let first_load = load_json_file(&name, &path, Some(tmp.path()), false).unwrap();
    assert!(!first_load.loaded_from_cache);

    // Second load reads from the normalized NDJSON cache
    let second_load = load_json_file(&name, &path, Some(tmp.path()), false).unwrap();
    assert!(second_load.loaded_from_cache);
    assert_eq!(first_load.rows.len(), second_load.rows.len());

    // Run multiple queries
    let engine = setup_examples_engine(tmp.path());
    let r1 = engine.execute("SELECT * FROM users").unwrap();
    assert_eq!(r1.rows.len(), 5);

    // Format in all 4 formats
    let table_out = format_query_result(&r1, OutputFormat::Table);
    assert!(table_out.contains("Alice Vance"));
    let json_out = format_query_result(&r1, OutputFormat::Json);
    assert!(json_out.contains("\"Alice Vance\""));
    let ndjson_out = format_query_result(&r1, OutputFormat::Ndjson);
    assert_eq!(ndjson_out.lines().count(), 5);
    let csv_out = format_query_result(&r1, OutputFormat::Csv);
    assert_eq!(csv_out.lines().count(), 6);

    // Verify all source files are 100% byte-identical after loading, caching, and querying
    for (p, before) in files.iter().zip(before_snapshots.iter()) {
        let after = fs::read(p).expect("read file after queries");
        assert_eq!(
            before, &after,
            "Source file {} was modified!",
            p
        );
    }
}

#[test]
fn test_edge_cases_distinct_order_by_empty_result_and_having() {
    let tmp = TempDir::new().unwrap();
    let engine = setup_examples_engine(tmp.path());

    // 1. SELECT DISTINCT with ORDER BY on non-projected column and positional ORDER BY
    let res_distinct = engine
        .execute("SELECT DISTINCT department FROM users ORDER BY 1 ASC")
        .unwrap();
    assert_eq!(
        res_distinct.rows,
        vec![
            vec![json!("Analytics")],
            vec![json!("Platform")],
            vec![json!("Product")],
        ]
    );

    // 2. Empty result set for `SELECT *` still preserves table column headers
    let res_empty = engine
        .execute("SELECT * FROM users WHERE salary > 9999999")
        .unwrap();
    assert_eq!(res_empty.rows.len(), 0);
    assert_eq!(
        res_empty.columns,
        vec!["id", "name", "role", "department", "salary", "active", "address", "tags"]
    );

    // 3. GROUP BY with HAVING and COUNT(DISTINCT ...)
    let res_having = engine
        .execute(
            "SELECT department, COUNT(DISTINCT address.city) AS distinct_cities \
             FROM users \
             GROUP BY department \
             HAVING COUNT(*) >= 2",
        )
        .unwrap();
    assert_eq!(res_having.rows.len(), 1);
    assert_eq!(res_having.rows[0], vec![json!("Platform"), json!(3)]);

    // 4. Default `FROM data` alias when only a single file is loaded
    let mut single_engine = QueryEngine::new();
    let (u_name, u_path) = parse_file_spec("examples/users.json");
    single_engine.register_table(load_json_file(&u_name, &u_path, Some(tmp.path()), false).unwrap());
    let res_data = single_engine
        .execute("SELECT COUNT(*) AS cnt FROM data WHERE active = true")
        .unwrap();
    assert_eq!(res_data.rows[0], vec![json!(4)]);
}

