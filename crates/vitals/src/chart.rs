//! Inline SVG chart primitives for the Vitals dashboard.
//!
//! The dashboard stays pure SSR: these helpers emit self-contained SVG geometry only, with
//! class-driven tones so rendering can remain token-based in CSS.

#[derive(Clone, Debug)]
pub enum Domain {
    Pct,
    Auto { headroom: f64 },
}

#[derive(Clone, Copy, Debug)]
pub enum Tone {
    Calm,
    Warn,
    Down,
    Mute,
}

/// Accessibility identity for one rendered SVG: a caller-composed, truthful accessible
/// name, an optional longer description, and the unique id used for `<desc>` wiring.
/// No `Default` and no synthesized fallback — every chart is named by its caller.
#[derive(Clone, Copy, Debug)]
pub struct Access<'a> {
    pub label: &'a str,
    pub desc: Option<&'a str>,
    pub name_id: &'a str,
}

#[derive(Clone, Debug)]
pub struct SparkOpts<'a> {
    pub w: f64,
    pub h: f64,
    pub domain: Domain,
    pub tone: Tone,
    pub gap_secs: i64,
    pub with_dot: bool,
    pub access: Access<'a>,
}

#[derive(Clone, Copy, Debug)]
pub struct Line<'a> {
    pub points: &'a [(i64, f64)],
    pub class: &'a str,
    pub area: bool,
}

/// Card/row sparkline over a fixed time range.
pub fn spark_svg(
    series: &[(i64, f64)],
    forecast: &[(i64, f64)],
    range: (i64, i64),
    opts: &SparkOpts<'_>,
) -> String {
    let tone = tone_class(opts.tone);
    let size_class = if opts.h <= 24.0 {
        "vt-spark--row"
    } else {
        "vt-spark--card"
    };
    let domain_max = domain_max(&opts.domain, &[series, forecast]);
    let access = &opts.access;
    let mut out = format!(
        r#"<svg class="vt-spark {size_class} {tone}" viewBox="0 0 {w:.1} {h:.1}" preserveAspectRatio="none" role="img" id="{id}" aria-label="{label}""#,
        size_class = size_class,
        tone = tone,
        w = opts.w,
        h = opts.h,
        id = esc(access.name_id),
        label = esc(access.label),
    );
    if let Some(desc) = access.desc {
        out.push_str(&format!(
            r#" aria-describedby="{id}-desc"><desc id="{id}-desc">{desc}</desc>"#,
            id = esc(access.name_id),
            desc = esc(desc),
        ));
    }
    out.push('>');

    if series.len() < 2 {
        // Fewer than two samples is an honest empty trace — never a synthetic baseline.
        out.push_str("</svg>");
        return out;
    }

    for segment in segments(series, opts.gap_secs) {
        if segment.len() < 2 {
            continue;
        }
        out.push_str(&format!(
            r#"<polyline class="vt-spark__line" points="{points}" vector-effect="non-scaling-stroke"/>"#,
            points = points_attr(segment, range, opts.w, opts.h, &opts.domain, domain_max),
        ));
    }

    if !forecast.is_empty() {
        let mut points = Vec::with_capacity(forecast.len() + 1);
        if let Some(last) = series.last().copied() {
            points.push(last);
        }
        points.extend_from_slice(forecast);
        if points.len() >= 2 {
            out.push_str(&format!(
                r#"<polyline class="vt-spark__forecast" points="{points}" vector-effect="non-scaling-stroke"/>"#,
                points = points_attr(&points, range, opts.w, opts.h, &opts.domain, domain_max),
            ));
        }
    }

    if opts.with_dot {
        if let Some((ts, value)) = series.last().copied() {
            out.push_str(&format!(
                r#"<circle class="vt-spark__dot" cx="{x:.1}" cy="{y:.1}" r="2.2" vector-effect="non-scaling-stroke"><title>{title}</title></circle>"#,
                x = x_at(ts, range, opts.w),
                y = y_at(value, &opts.domain, domain_max, opts.h),
                title = esc(&format!("{ts}: {value:.2}")),
            ));
        }
    }

    out.push_str("</svg>");
    out
}

/// Detail chart inner SVG: geometry only. HTML framing supplies axes and labels.
pub fn detail_svg(
    lines: &[Line<'_>],
    anoms: &[(i64, f64)],
    domain: &Domain,
    range: (i64, i64),
    w: f64,
    h: f64,
    access: &Access<'_>,
) -> String {
    let line_refs: Vec<&[(i64, f64)]> = lines.iter().map(|line| line.points).collect();
    let domain_max = domain_max(domain, &line_refs);
    let mut out = format!(
        r#"<svg class="vt-chart__svg" viewBox="0 0 {w:.1} {h:.1}" preserveAspectRatio="none" role="img" id="{id}" aria-label="{label}""#,
        w = w,
        h = h,
        id = esc(access.name_id),
        label = esc(access.label),
    );
    if let Some(desc) = access.desc {
        out.push_str(&format!(
            r#" aria-describedby="{id}-desc"><desc id="{id}-desc">{desc}</desc>"#,
            id = esc(access.name_id),
            desc = esc(desc),
        ));
    }
    out.push('>');

    for value in grid_values(domain, domain_max) {
        let y = y_at(value, domain, domain_max, h);
        out.push_str(&format!(
            r#"<line class="vt-chart__grid" x1="0.0" y1="{y:.1}" x2="{w:.1}" y2="{y:.1}" vector-effect="non-scaling-stroke"/>"#,
            y = y,
            w = w,
        ));
    }

    // Merged gap union across every supplied line: one band + two dashed edges per merged
    // interval, after the grid and before the threshold rule and the traces. The frozen
    // `default_gap` is the only threshold; polylines stay independently segmented below.
    let gap_secs = default_gap(range);
    let mut spans = Vec::new();
    for line in lines {
        spans.extend(gap_spans(line.points, gap_secs));
    }
    for (start, end) in merge_spans(spans) {
        let x1 = x_at(start, range, w);
        let x2 = x_at(end, range, w);
        out.push_str(&format!(
            r#"<rect class="vt-chart__gapband" x="{x1:.1}" y="0.0" width="{width:.1}" height="{h:.1}"/>"#,
            x1 = x1,
            width = x2 - x1,
            h = h,
        ));
        for x in [x1, x2] {
            out.push_str(&format!(
                r#"<line class="vt-chart__gapedge" x1="{x:.1}" y1="0.0" x2="{x:.1}" y2="{h:.1}" vector-effect="non-scaling-stroke"><title>gap {start}–{end}</title></line>"#,
                x = x,
                h = h,
                start = start,
                end = end,
            ));
        }
    }

    if matches!(domain, Domain::Pct) {
        let y = y_at(90.0, domain, domain_max, h);
        out.push_str(&format!(
            r#"<line class="vt-chart__threshold" x1="0.0" y1="{y:.1}" x2="{w:.1}" y2="{y:.1}" vector-effect="non-scaling-stroke"/>"#,
            y = y,
            w = w,
        ));
    }

    for line in lines {
        for segment in segments(line.points, default_gap(range)) {
            if segment.len() < 2 {
                continue;
            }
            let points = points_attr(segment, range, w, h, domain, domain_max);
            if line.area {
                out.push_str(&format!(
                    r#"<path class="vt-chart__area {class}" d="{path}"/>"#,
                    class = esc(line.class),
                    path = area_path(segment, range, w, h, domain, domain_max),
                ));
            }
            out.push_str(&format!(
                r#"<polyline class="vt-chart__line {class}" points="{points}" vector-effect="non-scaling-stroke"/>"#,
                class = esc(line.class),
                points = points,
            ));
        }
    }

    for (ts, value) in anoms {
        out.push_str(&format!(
            r#"<circle class="vt-chart__anom" cx="{x:.1}" cy="{y:.1}" r="3.0" vector-effect="non-scaling-stroke"><title>{title}</title></circle>"#,
            x = x_at(*ts, range, w),
            y = y_at(*value, domain, domain_max, h),
            title = esc(&format!("{ts}: {value:.2}")),
        ));
    }

    out.push_str("</svg>");
    out
}

fn tone_class(tone: Tone) -> &'static str {
    match tone {
        Tone::Calm => "vt-tone-calm",
        Tone::Warn => "vt-tone-warn",
        Tone::Down => "vt-tone-down",
        Tone::Mute => "vt-tone-mute",
    }
}

fn domain_max(domain: &Domain, series: &[&[(i64, f64)]]) -> f64 {
    match domain {
        Domain::Pct => 100.0,
        Domain::Auto { headroom } => {
            let max = series
                .iter()
                .flat_map(|points| points.iter().map(|(_, v)| *v))
                .filter(|v| v.is_finite())
                .fold(0.0, f64::max);
            nice_max(max * headroom.max(1.0))
        }
    }
}

fn x_at(ts: i64, range: (i64, i64), w: f64) -> f64 {
    let span = (range.1 - range.0).max(1) as f64;
    ((ts - range.0) as f64 / span).clamp(0.0, 1.0) * w
}

fn y_at(value: f64, domain: &Domain, max: f64, h: f64) -> f64 {
    let top = match domain {
        Domain::Pct => 100.0,
        Domain::Auto { .. } => max.max(1.0),
    };
    h - (value.max(0.0).min(top) / top) * h
}

fn segments(points: &[(i64, f64)], gap_secs: i64) -> Vec<&[(i64, f64)]> {
    if points.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut start = 0usize;
    for i in 1..points.len() {
        if points[i].0 - points[i - 1].0 > gap_secs {
            out.push(&points[start..i]);
            start = i;
        }
    }
    out.push(&points[start..]);
    out
}

/// Gap spans under the same predicate as `segments` — reuses the frozen gap authority,
/// no new threshold constant. Returns `(gap_start_ts, gap_end_ts)` per broken adjacency.
pub fn gap_spans(points: &[(i64, f64)], gap_secs: i64) -> Vec<(i64, i64)> {
    let mut spans = Vec::new();
    for i in 1..points.len() {
        if points[i].0 - points[i - 1].0 > gap_secs {
            spans.push((points[i - 1].0, points[i].0));
        }
    }
    spans
}

/// Chart-level union of gap spans: sort by `(start_ts, end_ts)`, then merge overlapping
/// or touching intervals (`next.start <= cur.end`); identical spans collapse to one.
fn merge_spans(mut spans: Vec<(i64, i64)>) -> Vec<(i64, i64)> {
    spans.sort_unstable();
    let mut merged: Vec<(i64, i64)> = Vec::with_capacity(spans.len());
    for (start, end) in spans {
        match merged.last_mut() {
            Some(cur) if start <= cur.1 => {
                cur.1 = cur.1.max(end);
            }
            _ => merged.push((start, end)),
        }
    }
    merged
}

fn nice_max(value: f64) -> f64 {
    if !value.is_finite() || value <= 0.0 {
        return 1.0;
    }
    let exp = value.log10().floor();
    let base = 10f64.powf(exp);
    let frac = value / base;
    let nice = if frac <= 1.0 {
        1.0
    } else if frac <= 2.0 {
        2.0
    } else if frac <= 5.0 {
        5.0
    } else {
        10.0
    };
    nice * base
}

fn points_attr(
    points: &[(i64, f64)],
    range: (i64, i64),
    w: f64,
    h: f64,
    domain: &Domain,
    domain_max: f64,
) -> String {
    points
        .iter()
        .map(|(ts, value)| {
            format!(
                "{:.1},{:.1}",
                x_at(*ts, range, w),
                y_at(*value, domain, domain_max, h)
            )
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn area_path(
    points: &[(i64, f64)],
    range: (i64, i64),
    w: f64,
    h: f64,
    domain: &Domain,
    domain_max: f64,
) -> String {
    let Some((first_ts, _)) = points.first().copied() else {
        return String::new();
    };
    let Some((last_ts, _)) = points.last().copied() else {
        return String::new();
    };
    let mut d = format!("M {:.1} {:.1}", x_at(first_ts, range, w), h);
    for (ts, value) in points {
        d.push_str(&format!(
            " L {:.1} {:.1}",
            x_at(*ts, range, w),
            y_at(*value, domain, domain_max, h)
        ));
    }
    d.push_str(&format!(" L {:.1} {:.1} Z", x_at(last_ts, range, w), h));
    d
}

fn grid_values(domain: &Domain, max: f64) -> [f64; 3] {
    match domain {
        Domain::Pct => [25.0, 50.0, 75.0],
        Domain::Auto { .. } => {
            let step = nice_max(max / 4.0);
            [step, step * 2.0, step * 3.0]
        }
    }
}

fn default_gap(range: (i64, i64)) -> i64 {
    ((range.1 - range.0).max(1) / 60).max(1) * 2
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#x27;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn segments_split_on_large_gaps() {
        let points = [(0, 1.0), (10, 2.0), (40, 3.0), (50, 4.0)];
        let parts = segments(&points, 15);
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0], &points[0..2]);
        assert_eq!(parts[1], &points[2..4]);
    }

    #[test]
    fn nice_max_uses_readable_steps() {
        assert_eq!(nice_max(0.0), 1.0);
        assert_eq!(nice_max(1.2), 2.0);
        assert_eq!(nice_max(4.2), 5.0);
        assert_eq!(nice_max(9.1), 10.0);
        assert_eq!(nice_max(863.0), 1000.0);
    }

    #[test]
    fn gap_spans_marks_broken_adjacency_with_segments_predicate() {
        let points = [(0, 1.0), (10, 2.0), (40, 3.0), (50, 4.0), (90, 5.0)];
        assert_eq!(gap_spans(&points, 15), vec![(10, 40), (50, 90)]);
        assert!(gap_spans(&points[..1], 15).is_empty());
        assert!(gap_spans(&[], 15).is_empty());
    }

    #[test]
    fn merge_spans_unions_overlapping_touching_and_identical() {
        assert_eq!(
            merge_spans(vec![(10, 20), (30, 40), (5, 12), (20, 25), (5, 12)]),
            vec![(5, 25), (30, 40)]
        );
        assert_eq!(merge_spans(vec![(7, 9)]), vec![(7, 9)]);
        assert!(merge_spans(Vec::new()).is_empty());
    }
}
