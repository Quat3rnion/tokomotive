//! Small server-side SVG line charts. No JavaScript needed.

pub struct Series<'a> {
    pub name: &'a str,
    pub color: &'a str,
    pub values: Vec<f64>,
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Render a line chart. `fmt` formats axis values.
pub fn line(labels: &[String], series: &[Series], fmt: &dyn Fn(f64) -> String) -> String {
    let (w, h, pl, pr, pt, pb) = (760.0, 220.0, 70.0, 10.0, 10.0, 28.0);
    let n = labels.len();
    if n == 0 {
        return r#"<p class="muted">No data in this range.</p>"#.into();
    }
    let max = series
        .iter()
        .flat_map(|s| s.values.iter().copied())
        .fold(0.0_f64, f64::max);
    let min = series
        .iter()
        .flat_map(|s| s.values.iter().copied())
        .fold(0.0_f64, f64::min);
    let span = if max - min <= 0.0 { 1.0 } else { max - min };
    let x = |i: usize| {
        pl + if n == 1 {
            (w - pl - pr) / 2.0
        } else {
            i as f64 * (w - pl - pr) / (n - 1) as f64
        }
    };
    let y = |v: f64| pt + (h - pt - pb) * (1.0 - (v - min) / span);

    let mut out = format!(r#"<svg viewBox="0 0 {w} {h}" class="chart" role="img">"#);
    for k in 0..=4 {
        let v = min + span * k as f64 / 4.0;
        let yy = y(v);
        out += &format!(
            r#"<line x1="{pl}" x2="{}" y1="{yy:.1}" y2="{yy:.1}" class="grid"/><text x="{}" y="{:.1}" class="axis" text-anchor="end">{}</text>"#,
            w - pr,
            pl - 6.0,
            yy + 4.0,
            esc(&fmt(v))
        );
    }
    let step = (n / 8).max(1);
    for (i, l) in labels.iter().enumerate().step_by(step) {
        out += &format!(
            r#"<text x="{:.1}" y="{}" class="axis" text-anchor="middle">{}</text>"#,
            x(i),
            h - 8.0,
            esc(l)
        );
    }
    for s in series {
        let pts: Vec<String> = s
            .values
            .iter()
            .enumerate()
            .map(|(i, v)| format!("{:.1},{:.1}", x(i), y(*v)))
            .collect();
        out += &format!(
            r#"<polyline fill="none" stroke="{}" stroke-width="2" points="{}"><title>{}</title></polyline>"#,
            s.color,
            pts.join(" "),
            esc(s.name)
        );
        if n == 1 {
            out += &format!(
                r#"<circle cx="{:.1}" cy="{:.1}" r="3" fill="{}"/>"#,
                x(0),
                y(s.values[0]),
                s.color
            );
        }
    }
    out += "</svg><div class=\"legend\">";
    for s in series {
        out += &format!(
            r#"<span><i style="background:{}"></i>{}</span>"#,
            s.color,
            esc(s.name)
        );
    }
    out += "</div>";
    out
}
