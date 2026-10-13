use std::hint::black_box;

use criterion::{Criterion, criterion_group, criterion_main};
use sluice_core::parse_tags;

/// A ~1 KB statement: a report query padded with an IN list, then the tag comment.
fn statement(comment: &str) -> String {
    let ids: Vec<String> = (100_000..100_140).map(|i| i.to_string()).collect();
    let mut sql = format!(
        "SELECT o.id, o.created_at, c.name, sum(li.amount) AS total \
         FROM orders o JOIN customers c ON c.id = o.customer_id \
         JOIN line_items li ON li.order_id = o.id \
         WHERE o.id IN ({}) GROUP BY 1, 2, 3 ORDER BY total DESC LIMIT 50",
        ids.join(", ")
    );
    sql.truncate(1024 - comment.len() - 1);
    sql.push(' ');
    sql.push_str(comment);
    assert_eq!(sql.len(), 1024);
    sql
}

fn bench(c: &mut Criterion) {
    let tagged = statement(
        "/*app='dashboard',controller='reports',feature='export_csv',route='%2Freports%2Fmonthly'*/",
    );
    let plain = statement("");

    c.bench_function("parse_tags/1KB, 4 tags", |b| {
        b.iter(|| parse_tags(black_box(&tagged)))
    });
    c.bench_function("parse_tags/1KB, no comment", |b| {
        b.iter(|| parse_tags(black_box(&plain)))
    });
}

criterion_group!(benches, bench);
criterion_main!(benches);
