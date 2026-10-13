use std::hint::black_box;

use criterion::{Criterion, criterion_group, criterion_main};
use sluice_core::{RuleIndex, parse_tags, rules::DEFAULT_MAX_BUDGETS, validate};

/// 20 budgets: half by app, half by app + feature.
fn index() -> RuleIndex {
    let budgets: Vec<String> = (0..20)
        .map(|i| {
            let m = if i % 2 == 0 {
                format!(r#"{{"app":"service-{i}"}}"#)
            } else {
                format!(r#"{{"app":"service-{i}","feature":"f{i}"}}"#)
            };
            format!(r#"{{"name":"b{i}","mode":"enforce","match":[{m}]}}"#)
        })
        .collect();
    let json = format!(r#"{{"budgets":[{}]}}"#, budgets.join(","));
    RuleIndex::new(&validate(&json, DEFAULT_MAX_BUDGETS).unwrap())
}

fn bench(c: &mut Criterion) {
    let index = index();
    let hit = parse_tags("SELECT 1 /*app='service-7',feature='f7',route='%2Fx',user='bob'*/");
    let miss = parse_tags("SELECT 1 /*app='api',route='%2Fx',user='bob'*/");

    c.bench_function("matching_budgets/4 tags, 1 match", |b| {
        b.iter(|| index.matching_budgets(black_box(&hit)))
    });
    c.bench_function("matching_budgets/3 tags, no match", |b| {
        b.iter(|| index.matching_budgets(black_box(&miss)))
    });
}

criterion_group!(benches, bench);
criterion_main!(benches);
