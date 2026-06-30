//! Trace reconstruction: group spans into traces, pick the root, and lay out the waterfall.
//!
//! The parent/child tree is rebuilt in Rust with a BOUNDED depth + a visited-set, so a cyclic or
//! self-referential `parent_id` (a buggy/hostile collector) can never loop or recurse without
//! bound. Spans never reached by the walk (orphaned by a cycle/depth cap) are still appended, so
//! the waterfall never silently drops a row.

use std::collections::{HashMap, HashSet};

use crate::config::MAX_TREE_DEPTH;
use crate::store::Span;

/// One reconstructed trace, summarized for the dashboard list + `/api/traces`.
#[derive(Clone, Debug, serde::Serialize)]
pub struct TraceSummary {
    pub trace_id: String,
    pub root_service: String,
    pub root_name: String,
    /// Wall-clock span of the whole trace, microseconds (`max(end) - min(start)`).
    pub total_us: i64,
    pub span_count: usize,
    pub has_error: bool,
    /// Earliest `start_us` in the trace — the ordering key (newest trace first).
    pub start_us: i64,
}

/// Group a flat span window into per-trace summaries, newest-first by trace start.
pub fn summarize(spans: &[Span]) -> Vec<TraceSummary> {
    let mut groups: HashMap<&str, Vec<&Span>> = HashMap::new();
    for s in spans {
        groups.entry(s.trace_id.as_str()).or_default().push(s);
    }

    let mut out: Vec<TraceSummary> = groups
        .into_iter()
        .map(|(trace_id, spans)| {
            let root = pick_root(&spans);
            let start_us = spans.iter().map(|s| s.start_us).min().unwrap_or(0);
            let end_us = spans.iter().map(|s| s.end_us).max().unwrap_or(start_us);
            TraceSummary {
                trace_id: trace_id.to_string(),
                root_service: root.service.clone(),
                root_name: root.name.clone(),
                total_us: (end_us - start_us).max(0),
                span_count: spans.len(),
                has_error: spans.iter().any(|s| s.is_error()),
                start_us,
            }
        })
        .collect();

    out.sort_by(|a, b| {
        b.start_us
            .cmp(&a.start_us)
            .then_with(|| b.trace_id.cmp(&a.trace_id))
    });
    out
}

/// Choose the representative root span of a trace: prefer a span with no parent (or a parent that
/// is absent from this window — a severed entry point); among candidates, the earliest start. With
/// no candidate (every span's parent is present, i.e. a cycle) fall back to the earliest span.
pub fn pick_root<'a>(spans: &[&'a Span]) -> &'a Span {
    let ids: HashSet<&str> = spans.iter().map(|s| s.span_id.as_str()).collect();
    let is_rootish = |s: &Span| s.parent_id.is_empty() || !ids.contains(s.parent_id.as_str());

    spans
        .iter()
        .filter(|s| is_rootish(s))
        .min_by(|a, b| a.start_us.cmp(&b.start_us).then_with(|| a.span_id.cmp(&b.span_id)))
        .or_else(|| {
            spans
                .iter()
                .min_by(|a, b| a.start_us.cmp(&b.start_us).then_with(|| a.span_id.cmp(&b.span_id)))
        })
        .copied()
        .expect("pick_root called on empty span set")
}

/// A laid-out waterfall row: the span plus its tree depth and the CSS bar geometry (percentages of
/// the whole-trace duration).
#[derive(Clone, Debug)]
pub struct WaterfallRow {
    pub span: Span,
    pub depth: usize,
    /// Left offset of the duration bar, percent of the trace duration (0..=100).
    pub offset_pct: f64,
    /// Width of the duration bar, percent of the trace duration (a small floor keeps a
    /// zero-duration span visible).
    pub width_pct: f64,
    pub dur_us: i64,
}

/// The full waterfall layout for one trace.
#[derive(Clone, Debug)]
pub struct Waterfall {
    pub total_us: i64,
    pub start_us: i64,
    pub rows: Vec<WaterfallRow>,
    pub service_count: usize,
    pub has_error: bool,
}

/// Build the waterfall from a trace's spans (already ordered by `start_us` ASC by the store).
pub fn build_waterfall(spans: &[Span]) -> Waterfall {
    let start_us = spans.iter().map(|s| s.start_us).min().unwrap_or(0);
    let end_us = spans.iter().map(|s| s.end_us).max().unwrap_or(start_us);
    let total_us = (end_us - start_us).max(1);

    // children: parent_id -> child span indexes, each child list ordered by start (the input order).
    let ids: HashSet<&str> = spans.iter().map(|s| s.span_id.as_str()).collect();
    let mut children: HashMap<&str, Vec<usize>> = HashMap::new();
    let mut roots: Vec<usize> = Vec::new();
    for (i, s) in spans.iter().enumerate() {
        if s.parent_id.is_empty() || !ids.contains(s.parent_id.as_str()) {
            roots.push(i);
        } else {
            children.entry(s.parent_id.as_str()).or_default().push(i);
        }
    }

    let mut rows: Vec<WaterfallRow> = Vec::with_capacity(spans.len());
    let mut visited: HashSet<usize> = HashSet::new();
    // Explicit stack DFS (depth, span index), so deeply/maliciously nested traces never recurse
    // the native stack. `depth` is capped at MAX_TREE_DEPTH.
    let mut stack: Vec<(usize, usize)> = roots.into_iter().rev().map(|i| (0usize, i)).collect();
    while let Some((depth, idx)) = stack.pop() {
        if !visited.insert(idx) {
            continue;
        }
        rows.push(make_row(&spans[idx], depth, start_us, total_us));
        if depth + 1 <= MAX_TREE_DEPTH {
            if let Some(kids) = children.get(spans[idx].span_id.as_str()) {
                // Push reversed so the earliest child is processed first (stack is LIFO).
                for &child in kids.iter().rev() {
                    if !visited.contains(&child) {
                        stack.push((depth + 1, child));
                    }
                }
            }
        }
    }

    // Anything unreached (cycle / depth cap) is appended at depth 0 so no span is dropped.
    for (i, s) in spans.iter().enumerate() {
        if !visited.contains(&i) {
            rows.push(make_row(s, 0, start_us, total_us));
        }
    }

    let services: HashSet<&str> = spans.iter().map(|s| s.service.as_str()).collect();
    Waterfall {
        total_us,
        start_us,
        has_error: spans.iter().any(|s| s.is_error()),
        service_count: services.len(),
        rows,
    }
}

fn make_row(span: &Span, depth: usize, trace_start: i64, total_us: i64) -> WaterfallRow {
    let total = total_us as f64;
    let offset = ((span.start_us - trace_start).max(0) as f64) / total * 100.0;
    let dur = span.duration_us();
    let width = (dur as f64) / total * 100.0;
    WaterfallRow {
        span: span.clone(),
        depth,
        offset_pct: offset.clamp(0.0, 100.0),
        // 0.8% floor keeps an instantaneous span as a visible sliver.
        width_pct: width.max(0.8).min(100.0),
        dur_us: dur,
    }
}

/// Distinct trace ids in a span window that contain at least one error span.
pub fn error_trace_ids(spans: &[Span]) -> Vec<String> {
    let mut seen: HashSet<&str> = HashSet::new();
    let mut out = Vec::new();
    for s in spans {
        if s.is_error() && seen.insert(s.trace_id.as_str()) {
            out.push(s.trace_id.clone());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn span(id: &str, parent: &str, trace: &str, svc: &str, start: i64, end: i64, status: &str) -> Span {
        Span {
            span_id: id.to_string(),
            trace_id: trace.to_string(),
            parent_id: parent.to_string(),
            name: format!("{id}-op"),
            service: svc.to_string(),
            start_us: start,
            end_us: end,
            status: status.to_string(),
            attrs: String::new(),
        }
    }

    #[test]
    fn summarize_groups_and_picks_root() {
        let spans = vec![
            span("a", "", "t1", "gateway", 1000, 5000, "ok"),
            span("b", "a", "t1", "db", 1500, 2500, "error"),
            span("c", "", "t2", "worker", 9000, 9500, "ok"),
        ];
        let mut sums = summarize(&spans);
        sums.sort_by(|x, y| x.trace_id.cmp(&y.trace_id));
        assert_eq!(sums.len(), 2);
        let t1 = sums.iter().find(|s| s.trace_id == "t1").unwrap();
        assert_eq!(t1.root_service, "gateway");
        assert_eq!(t1.root_name, "a-op");
        assert_eq!(t1.span_count, 2);
        assert_eq!(t1.total_us, 4000);
        assert!(t1.has_error);
    }

    #[test]
    fn waterfall_indents_children_and_lays_out_bars() {
        let spans = vec![
            span("a", "", "t1", "gateway", 0, 1000, "ok"),
            span("b", "a", "t1", "db", 250, 750, "ok"),
        ];
        let wf = build_waterfall(&spans);
        assert_eq!(wf.total_us, 1000);
        assert_eq!(wf.rows.len(), 2);
        assert_eq!(wf.rows[0].span.span_id, "a");
        assert_eq!(wf.rows[0].depth, 0);
        assert_eq!(wf.rows[1].span.span_id, "b");
        assert_eq!(wf.rows[1].depth, 1);
        // child bar offset = 250/1000 = 25%, width = 500/1000 = 50%.
        assert!((wf.rows[1].offset_pct - 25.0).abs() < 0.01);
        assert!((wf.rows[1].width_pct - 50.0).abs() < 0.01);
        assert_eq!(wf.service_count, 2);
    }

    #[test]
    fn waterfall_survives_a_cycle() {
        // a -> b -> a (a cycle): no infinite loop, every span appears exactly once.
        let spans = vec![
            span("a", "b", "t1", "x", 0, 10, "ok"),
            span("b", "a", "t1", "x", 0, 10, "ok"),
        ];
        let wf = build_waterfall(&spans);
        assert_eq!(wf.rows.len(), 2, "both spans rendered, none dropped");
    }

    #[test]
    fn error_trace_ids_dedup() {
        let spans = vec![
            span("a", "", "t1", "x", 0, 1, "error"),
            span("b", "a", "t1", "x", 0, 1, "ok"),
            span("c", "", "t2", "x", 0, 1, "ok"),
        ];
        assert_eq!(error_trace_ids(&spans), vec!["t1".to_string()]);
    }
}
