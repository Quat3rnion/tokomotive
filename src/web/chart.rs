//! Small server-side line charts. No JavaScript needed.
//!
//! The SVG only draws grid lines and series. It stretches to the box with
//! `preserveAspectRatio="none"` and non-scaling strokes. Axis labels are HTML,
//! so their size does not change with the chart width. Each chart has a
//! one-sentence summary as its accessible name and a data table.
//!
//! No colour is written here: series use the classes `s1`..`s5`, and
//! base.html maps them to the `--s1`..`--s5` tokens of the current theme.

use super::views::{int, money};

pub struct Series<'a> {
    pub name: &'a str,
    /// Palette class, `s1` to `s5` (the `--s1`..`--s5` tokens).
    pub class: &'a str,
    /// SVG `stroke-dasharray`; empty for a solid line. Series must differ in
    /// more than color.
    pub dash: &'a str,
    pub values: Vec<f64>,
}

#[derive(Clone, Copy, PartialEq)]
pub enum Unit {
    Money,
    Count,
}

pub struct Chart<'a> {
    /// Element id prefix; it also keeps the data table open on refresh.
    pub id: &'a str,
    /// For example "Requests per hour".
    pub title: String,
    /// For example "the last 24 hours".
    pub range: &'a str,
    pub unit: Unit,
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// A "nice" tick step (1, 2 or 5 times a power of ten) for `span` over
/// about four intervals.
fn nice_step(span: f64, unit: Unit) -> f64 {
    let raw = span / 4.0;
    let mag = 10f64.powf(raw.log10().floor());
    let n = raw / mag;
    let step = mag
        * if n <= 1.0 {
            1.0
        } else if n <= 2.0 {
            2.0
        } else if n <= 5.0 {
            5.0
        } else {
            10.0
        };
    if unit == Unit::Count {
        step.max(1.0)
    } else {
        step
    }
}

/// Rounded axis limits and ticks that include zero.
fn ticks(min: f64, max: f64, unit: Unit) -> (f64, f64, Vec<f64>) {
    let span = max - min;
    let step = if span <= 0.0 {
        if unit == Unit::Count { 1.0 } else { 0.25 }
    } else {
        nice_step(span, unit)
    };
    let lo = (min / step).floor() * step;
    let mut hi = (max / step).ceil() * step;
    if hi <= lo {
        hi = lo + 4.0 * step;
    }
    let n = ((hi - lo) / step).round() as usize;
    let ticks = (0..=n).map(|i| lo + i as f64 * step).collect();
    (lo, hi, ticks)
}

fn compact(v: f64) -> String {
    let a = v.abs();
    let trim = |s: String| s.replace(".0", "");
    if a >= 1e6 {
        trim(format!("{:.1}M", v / 1e6))
    } else if a >= 1e3 {
        trim(format!("{:.1}k", v / 1e3))
    } else {
        format!("{}", v.round() as i64)
    }
}

fn axis_label(v: f64, step: f64, unit: Unit) -> String {
    match unit {
        Unit::Count => compact(v),
        Unit::Money => {
            let decimals = if step >= 1.0 {
                0
            } else if step >= 0.01 {
                2
            } else {
                4
            };
            let sign = if v < 0.0 { "-" } else { "" };
            format!("{sign}${:.*}", decimals, v.abs())
        }
    }
}

fn value_text(v: f64, unit: Unit) -> String {
    match unit {
        Unit::Count => int(v.round() as i64),
        Unit::Money => money((v * 1e6).round() as i64),
    }
}

fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    c.next()
        .map(|f| f.to_uppercase().chain(c).collect())
        .unwrap_or_default()
}

/// A small sparkline for an Overview tile: a line and a soft area, scaled
/// from zero to the largest value. Decorative: the tile shows the value.
pub fn spark(values: &[f64]) -> String {
    let n = values.len();
    if n < 2 {
        return String::new();
    }
    let max = values.iter().copied().fold(0.0_f64, f64::max);
    let y = |v: f64| {
        if max > 0.0 {
            25.0 - 24.0 * (v.max(0.0) / max)
        } else {
            25.0
        }
    };
    let pts: Vec<String> = values
        .iter()
        .enumerate()
        .map(|(i, v)| format!("{:.1},{:.2}", i as f64 * 100.0 / (n - 1) as f64, y(*v)))
        .collect();
    let pts = pts.join(" ");
    format!(
        r#"<svg class="spark" viewBox="0 0 100 26" preserveAspectRatio="none" aria-hidden="true" focusable="false"><polygon class="area" points="0,26 {pts} 100,26"/><polyline class="line" points="{pts}" vector-effect="non-scaling-stroke"/></svg>"#
    )
}

/// Render a line chart with a summary, a legend and a data table. `labels`
/// name each bucket in full (table and summary); `axis` are the short
/// labels under the plot.
pub fn line(c: &Chart, labels: &[String], axis: &[String], series: &[Series]) -> String {
    let n = labels.len();
    if n == 0 || series.is_empty() {
        return r#"<p class="muted">No data in this range.</p>"#.into();
    }
    let all = || series.iter().flat_map(|s| s.values.iter().copied());
    let max = all().fold(0.0_f64, f64::max);
    let min = all().fold(0.0_f64, f64::min);
    let (lo, hi, ticks) = ticks(min, max, c.unit);
    let step = if ticks.len() > 1 {
        ticks[1] - ticks[0]
    } else {
        1.0
    };
    // Plot coordinates: x 0..1000, y 0..100 (top to bottom).
    let x = |i: usize| {
        if n == 1 {
            500.0
        } else {
            i as f64 * 1000.0 / (n - 1) as f64
        }
    };
    let y = |v: f64| 100.0 * (1.0 - (v - lo) / (hi - lo));

    // One sentence for screen readers: range, total and peak per series.
    let summary: Vec<String> = series
        .iter()
        .map(|s| {
            let total: f64 = s.values.iter().sum();
            let (peak_i, peak) = s
                .values
                .iter()
                .copied()
                .enumerate()
                .fold((0, f64::MIN), |a, (i, v)| if v > a.1 { (i, v) } else { a });
            if peak <= 0.0 {
                format!("{}: none", s.name)
            } else {
                format!(
                    "{}: total {}, peak {} at {}",
                    s.name,
                    value_text(total, c.unit),
                    value_text(peak, c.unit),
                    labels[peak_i]
                )
            }
        })
        .collect();
    let label = format!("{}, {} (UTC). {}.", c.title, c.range, summary.join("; "));

    let mut out = format!(
        r#"<figure class="chart" id="{id}"><figcaption class="chart-title"><span>{title}</span><span class="cap">{range}, UTC</span></figcaption><div class="chart-plot" role="img" aria-label="{label}"><svg viewBox="0 0 1000 100" preserveAspectRatio="none" aria-hidden="true" focusable="false">"#,
        id = c.id,
        title = esc(&c.title),
        range = esc(&capitalize(c.range)),
        label = esc(&label),
    );
    for t in &ticks {
        out += &format!(
            r#"<line x1="0" x2="1000" y1="{yy:.2}" y2="{yy:.2}" class="grid{zero}" vector-effect="non-scaling-stroke"/>"#,
            yy = y(*t),
            zero = if *t == 0.0 { " zero" } else { "" },
        );
    }
    for (k, s) in series.iter().enumerate() {
        let mut pts: Vec<String> = s
            .values
            .iter()
            .enumerate()
            .map(|(i, v)| format!("{:.1},{:.2}", x(i), y(*v)))
            .collect();
        if n == 1 {
            // A single bucket draws as a flat line across the plot.
            let yy = y(s.values[0]);
            pts = vec![format!("0,{yy:.2}"), format!("1000,{yy:.2}")];
        }
        if k == 0 {
            // A soft area under the first series, down to the zero line.
            let base = y(0.0_f64.clamp(lo, hi));
            let first = pts[0].split(',').next().unwrap_or("0");
            let last = pts[pts.len() - 1].split(',').next().unwrap_or("1000");
            out += &format!(
                r#"<polygon class="area {}" points="{first},{base:.2} {} {last},{base:.2}"/>"#,
                s.class,
                pts.join(" ")
            );
        }
        out += &format!(
            r#"<polyline class="{}" fill="none" stroke-width="2" stroke-dasharray="{}" stroke-linejoin="round" vector-effect="non-scaling-stroke" points="{}"/>"#,
            s.class,
            s.dash,
            pts.join(" ")
        );
    }
    out += "</svg>";
    for t in &ticks {
        out += &format!(
            r#"<span class="ytick" style="top:{:.2}%">{}</span>"#,
            y(*t),
            esc(&axis_label(*t, step, c.unit))
        );
    }
    let every = (n.saturating_sub(1)).div_ceil(5).max(1);
    let mut shown: Vec<usize> = (0..n).step_by(every).collect();
    if n > 1 && *shown.last().expect("not empty") != n - 1 {
        // Keep the last bucket; drop the one before it when they are too close.
        if (n - 1 - shown.last().expect("not empty")) * 5 < every * 4 {
            shown.pop();
        }
        shown.push(n - 1);
    }
    for i in shown {
        out += &format!(
            r#"<span class="xtick" style="left:{:.2}%">{}</span>"#,
            x(i) / 10.0,
            esc(axis.get(i).unwrap_or(&labels[i]))
        );
    }
    out += r#"</div><ul class="legend">"#;
    // Legend as in Halogen: a short bar, the name and the total in the range.
    for s in series {
        out += &format!(
            r#"<li><svg width="16" height="8" aria-hidden="true" focusable="false"><line class="{}" x1="1" y1="4" x2="15" y2="4" stroke-width="3" stroke-dasharray="{}"/></svg>{} <b>{}</b></li>"#,
            s.class,
            if s.dash.is_empty() { "" } else { "4 2" },
            esc(s.name),
            value_text(s.values.iter().sum(), c.unit)
        );
    }
    out += &format!(
        r#"</ul><details class="chart-data" id="{id}-data"><summary>Show the data as a table</summary><div class="scroll"><table><caption>{title}, {range} (UTC)</caption><thead><tr><th scope="col">Time (UTC)</th>"#,
        id = c.id,
        title = esc(&c.title),
        range = c.range,
    );
    for s in series {
        out += &format!(r#"<th scope="col" class="n">{}</th>"#, esc(s.name));
    }
    out += "</tr></thead><tbody>";
    for (i, l) in labels.iter().enumerate() {
        out += &format!(r#"<tr><th scope="row">{}</th>"#, esc(l));
        for s in series {
            out += &format!(r#"<td class="n">{}</td>"#, value_text(s.values[i], c.unit));
        }
        out += "</tr>";
    }
    out += "</tbody></table></div></details></figure>";
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ticks_are_round_and_distinct() {
        let (lo, hi, t) = ticks(0.0, 3.2413, Unit::Money);
        assert_eq!((lo, hi), (0.0, 4.0));
        assert_eq!(t, vec![0.0, 1.0, 2.0, 3.0, 4.0]);
        let (_, _, t) = ticks(0.0, 0.0, Unit::Count);
        assert_eq!(t, vec![0.0, 1.0, 2.0, 3.0, 4.0]);
        let (_, _, t) = ticks(0.0, 2.0, Unit::Count);
        assert_eq!(t, vec![0.0, 1.0, 2.0]);
        let (lo, _, _) = ticks(-0.3, 1.0, Unit::Money);
        assert!(lo < 0.0);
        assert_eq!(axis_label(0.5, 0.5, Unit::Money), "$0.50");
        assert_eq!(compact(152_330.0), "152.3k");
        assert_eq!(compact(2_000_000.0), "2M");
    }

    #[test]
    fn chart_has_a_summary_and_a_table() {
        let labels = vec!["11-13 10:00".to_string(), "11-13 11:00".to_string()];
        let html = line(
            &Chart {
                id: "c",
                title: "Requests per hour".into(),
                range: "the last 24 hours",
                unit: Unit::Count,
            },
            &labels,
            &labels,
            &[Series {
                name: "Requests",
                class: "s1",
                dash: "",
                values: vec![3.0, 7.0],
            }],
        );
        assert!(html.contains(r#"role="img" aria-label="Requests per hour, the last 24 hours (UTC). Requests: total 10, peak 7 at 11-13 11:00.""#), "{html}");
        assert!(html.contains("<caption>Requests per hour"));
        assert!(html.contains(r#"<polyline class="s1""#), "{html}");
        assert!(html.contains("Requests <b>10</b>"), "legend total: {html}");
        assert!(!html.contains('#'), "no hard-coded colour: {html}");
    }
}
