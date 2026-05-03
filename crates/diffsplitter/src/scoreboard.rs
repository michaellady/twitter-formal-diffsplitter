//! Server-rendered conformance scoreboard (Phase 4).
//!
//! The dashboard (Phase 2) shows individual diffs. The scoreboard answers the
//! single user-facing question: **"are the two impls staying converged or
//! diverging?"** It does that with rolling counters over the `diffs` table
//! plus a tiny inline-SVG severity bar chart and a top-10 diverging paths
//! leaderboard. All HTML is rendered server-side; no JS, no external assets.
//!
//! Routes mounted on the existing axum router (see `proxy::router`):
//!
//! * `GET /scoreboard`      — server-rendered HTML summary
//! * `GET /scoreboard.json` — same data, JSON shape
//!
//! Page-weight budget: ≤ 50KB. Typical render is ≈ 4-10KB.
//!
//! ## What the page shows
//!
//! 1. **Counts panel** — total requests, total diffs, divergence rate
//!    (1h / 24h / 7d). Greyed out if shadow is currently degraded (K13).
//! 2. **Severity breakdown** — inline SVG bar chart (24h window) coloured the
//!    same way as the dashboard severity badges.
//! 3. **Top-10 diverging paths** — sorted by diff count over the 24h window.
//! 4. **Last critical diff** — timestamp + a link back to the dashboard.
//!
//! ## "Requests" denominator caveat
//!
//! See `db::requests_in_window` for the full tradeoff. tl;dr: we don't keep a
//! per-request audit row, so we approximate the denominator as
//! `diffs + writes`. The resulting rate is conservative (overstates rather
//! than hides divergence). The page footer notes this.

use std::sync::Arc;

use axum::extract::State as AxState;
use axum::http::{header, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use serde_json::json;

use crate::{db, State};

/// Mount the scoreboard routes onto an existing router.
pub fn routes() -> Router<Arc<State>> {
    Router::new()
        .route("/scoreboard", get(scoreboard_html))
        .route("/scoreboard.json", get(scoreboard_json))
}

// ---------------------------------------------------------------------------
// Window definitions

const NS_PER_SEC: i64 = 1_000_000_000;
const SEC_PER_HOUR: i64 = 3_600;
const SEC_PER_DAY: i64 = 86_400;

#[derive(Copy, Clone)]
struct Window {
    label: &'static str,
    secs: i64,
}

const WINDOWS: &[Window] = &[
    Window {
        label: "1h",
        secs: SEC_PER_HOUR,
    },
    Window {
        label: "24h",
        secs: SEC_PER_DAY,
    },
    Window {
        label: "7d",
        secs: 7 * SEC_PER_DAY,
    },
];

// ---------------------------------------------------------------------------
// Aggregation

#[derive(Clone)]
struct WindowStats {
    label: &'static str,
    diffs: i64,
    requests: i64,
    /// `diffs / requests` as f64 in [0.0, 1.0]; 0.0 when requests == 0.
    rate: f64,
}

#[derive(Clone)]
struct ScoreboardData {
    now_ns: i64,
    diffs_total: i64,
    requests_total_approx: i64,
    windows: Vec<WindowStats>,
    /// 24h severity breakdown, canonical order
    /// (critical, high, medium, low, noise).
    severity_24h: [(String, i64); 5],
    /// Top-10 paths over 24h: (path, diffs, requests).
    top_paths_24h: Vec<(String, i64, i64)>,
    last_critical: Option<(i64, i64)>, // (id, observed_at_ns)
    shadow_degraded: bool,
}

fn collect(state: &Arc<State>) -> Result<ScoreboardData, anyhow::Error> {
    let now_ns = db::now_ns();
    let mut windows = Vec::with_capacity(WINDOWS.len());
    for w in WINDOWS {
        let since = now_ns - w.secs * NS_PER_SEC;
        let diffs = db::diffs_in_window(&state.pool, since, None)?;
        let requests = db::requests_in_window(&state.pool, since)?;
        let rate = if requests > 0 {
            diffs as f64 / requests as f64
        } else {
            0.0
        };
        windows.push(WindowStats {
            label: w.label,
            diffs,
            requests,
            rate,
        });
    }
    // Total approximations: same denominator approach but unbounded window.
    let diffs_total = db::diffs_total(&state.pool)?;
    let requests_total_approx = db::requests_in_window(&state.pool, 0)?;
    let severity_24h = db::severity_breakdown(&state.pool, now_ns - SEC_PER_DAY * NS_PER_SEC)?;
    let top_paths_24h =
        db::top_diverging_paths(&state.pool, now_ns - SEC_PER_DAY * NS_PER_SEC, 10)?;
    let last_critical = db::last_critical_diff(&state.pool)?;
    let shadow_degraded = db::shadow_degraded_now(&state.pool)?;

    Ok(ScoreboardData {
        now_ns,
        diffs_total,
        requests_total_approx,
        windows,
        severity_24h,
        top_paths_24h,
        last_critical,
        shadow_degraded,
    })
}

// ---------------------------------------------------------------------------
// Handlers

async fn scoreboard_html(AxState(state): AxState<Arc<State>>) -> Response {
    match collect(&state) {
        Ok(data) => Html(render_html(&data)).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("db error: {e}")).into_response(),
    }
}

async fn scoreboard_json(AxState(state): AxState<Arc<State>>) -> Response {
    match collect(&state) {
        Ok(data) => {
            let body = render_json(&data);
            (
                StatusCode::OK,
                [(header::CONTENT_TYPE, "application/json")],
                body,
            )
                .into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("db error: {e}")).into_response(),
    }
}

fn render_json(d: &ScoreboardData) -> String {
    let windows: Vec<_> = d
        .windows
        .iter()
        .map(|w| {
            json!({
                "label": w.label,
                "diffs": w.diffs,
                "requests": w.requests,
                "rate": w.rate,
            })
        })
        .collect();
    let severity: Vec<_> = d
        .severity_24h
        .iter()
        .map(|(s, n)| json!({ "severity": s, "count": n }))
        .collect();
    let top_paths: Vec<_> = d
        .top_paths_24h
        .iter()
        .map(|(p, diffs, reqs)| {
            let rate = if *reqs > 0 {
                *diffs as f64 / *reqs as f64
            } else {
                0.0
            };
            json!({
                "path": p,
                "diffs": diffs,
                "requests": reqs,
                "rate": rate,
            })
        })
        .collect();
    let last_crit = d
        .last_critical
        .map(|(id, ns)| json!({ "id": id, "observed_at_ns": ns }))
        .unwrap_or(serde_json::Value::Null);
    serde_json::to_string(&json!({
        "now_ns": d.now_ns,
        "diffs_total": d.diffs_total,
        "requests_total_approx": d.requests_total_approx,
        "shadow_degraded": d.shadow_degraded,
        "windows": windows,
        "severity_24h": severity,
        "top_paths_24h": top_paths,
        "last_critical": last_crit,
    }))
    .unwrap_or_else(|_| "{}".into())
}

// ---------------------------------------------------------------------------
// HTML rendering — pure format!() + inline <style>; no JS, no external CSS.

const STYLE: &str = r#"
:root { color-scheme: light dark; }
body { font: 14px/1.4 -apple-system, BlinkMacSystemFont, "Segoe UI", system-ui, sans-serif;
       margin: 0; padding: 1rem; background: #fafafa; color: #111; }
h1 { font-size: 1.2rem; margin: 0 0 .5rem 0; }
h2 { font-size: 1rem; margin: 1rem 0 .5rem 0; }
header { display: flex; align-items: baseline; gap: 1rem; flex-wrap: wrap;
         border-bottom: 1px solid #ddd; padding-bottom: .5rem; margin-bottom: .75rem; }
header nav a { color: #06c; text-decoration: none; margin-right: .5rem; }
.cards { display: grid; grid-template-columns: repeat(auto-fit, minmax(160px, 1fr));
         gap: .75rem; margin: .5rem 0 1rem 0; }
.card { background: #fff; border: 1px solid #ddd; border-radius: 4px;
        padding: .75rem; box-shadow: 0 1px 2px rgba(0,0,0,0.04); }
.card .lbl { font-size: 11px; color: #777; text-transform: uppercase;
             letter-spacing: .05em; }
.card .val { font: 600 1.4rem/1.1 ui-monospace, SFMono-Regular, Menlo, monospace;
             font-variant-numeric: tabular-nums; margin-top: .25em; }
.card .sub { font-size: 11px; color: #777; margin-top: .25em; }
.card.degraded { opacity: 0.5; }
.banner { background: #fff7e0; border: 1px solid #d4a017; padding: .5rem .75rem;
          border-radius: 4px; margin: .5rem 0; color: #6b4f00; }
.svg-wrap { background: #fff; border: 1px solid #ddd; padding: .75rem;
            border-radius: 4px; box-shadow: 0 1px 2px rgba(0,0,0,0.04); }
table { width: 100%; border-collapse: collapse; background: #fff;
        box-shadow: 0 1px 2px rgba(0,0,0,0.04); border: 1px solid #ddd; }
th, td { text-align: left; padding: 6px 8px; border-bottom: 1px solid #eee;
         font-variant-numeric: tabular-nums; }
th { background: #f3f3f3; font-weight: 600; font-size: 12px;
     text-transform: uppercase; letter-spacing: .04em; color: #555; }
.sev { display: inline-block; padding: 1px 6px; border-radius: 3px; font-size: 11px;
       font-weight: 600; text-transform: uppercase; }
.sev-critical { background: #b00020; color: #fff; }
.sev-high     { background: #d35400; color: #fff; }
.sev-medium   { background: #b58900; color: #fff; }
.sev-low      { background: #888;    color: #fff; }
.sev-noise    { background: #ddd;    color: #333; }
code { font-family: ui-monospace, SFMono-Regular, Menlo, monospace; font-size: 12px; }
footer { margin-top: 1rem; font-size: 12px; color: #777; }
.empty { padding: 2rem; text-align: center; color: #777; }
@media (prefers-color-scheme: dark) {
  body { background: #181818; color: #ddd; }
  header { border-color: #333; }
  .card, .svg-wrap, table { background: #222; box-shadow: none; border-color: #333; }
  .card .lbl, .card .sub, footer { color: #888; }
  th { background: #2a2a2a; color: #aaa; }
  th, td { border-color: #333; }
  header nav a { color: #6cf; }
  .banner { background: #3a2c00; color: #ffd770; border-color: #d4a017; }
}
"#;

fn render_html(d: &ScoreboardData) -> String {
    let mut s = String::with_capacity(8 * 1024);
    push_header(&mut s, "scoreboard");
    s.push_str("<header><h1>diffsplitter — conformance scoreboard</h1>");
    s.push_str("<nav>");
    s.push_str("<a href=\"/scoreboard\">scoreboard</a>");
    s.push_str("<a href=\"/diffs\">diffs</a>");
    s.push_str("<a href=\"/scoreboard.json\">json</a>");
    s.push_str("<a href=\"/metrics\">metrics</a>");
    s.push_str("</nav></header>");

    if d.shadow_degraded {
        s.push_str("<div class=\"banner\">shadow backend is currently <strong>degraded</strong> (K13). Divergence-rate panels are greyed out — they aren't meaningful while writes are being dropped.</div>");
    }

    // Counts panel: totals + per-window divergence rate.
    s.push_str("<h2>at a glance</h2>");
    let degraded_cls = if d.shadow_degraded { " degraded" } else { "" };
    s.push_str("<div class=\"cards\">");
    push_card(
        &mut s,
        "total requests (approx)",
        &format_int(d.requests_total_approx),
        "diffs + writes; see footer",
        false,
    );
    push_card(
        &mut s,
        "total diffs",
        &format_int(d.diffs_total),
        "all-time recorded divergences",
        false,
    );
    for w in &d.windows {
        let val = format_rate(w.rate);
        let sub = format!(
            "{} diffs / {} req",
            format_int(w.diffs),
            format_int(w.requests)
        );
        s.push_str(&format!(
            "<div class=\"card{cls}\"><div class=\"lbl\">divergence rate · {lbl}</div>\
             <div class=\"val\">{val}</div><div class=\"sub\">{sub}</div></div>",
            cls = degraded_cls,
            lbl = html_escape(w.label),
            val = html_escape(&val),
            sub = html_escape(&sub),
        ));
    }
    s.push_str("</div>");

    // Severity breakdown — inline SVG bar chart.
    s.push_str("<h2>severity breakdown · last 24h</h2>");
    s.push_str("<div class=\"svg-wrap\">");
    s.push_str(&render_severity_svg(&d.severity_24h));
    s.push_str("</div>");

    // Top-10 diverging paths.
    s.push_str("<h2>top diverging paths · last 24h</h2>");
    if d.top_paths_24h.is_empty() {
        s.push_str("<div class=\"empty\">no diffs in the last 24h.</div>");
    } else {
        s.push_str("<table><thead><tr>");
        s.push_str("<th>path</th><th>diffs</th><th>requests (approx)</th><th>rate</th>");
        s.push_str("</tr></thead><tbody>");
        for (p, diffs, reqs) in &d.top_paths_24h {
            let rate = if *reqs > 0 {
                *diffs as f64 / *reqs as f64
            } else {
                0.0
            };
            s.push_str("<tr>");
            s.push_str(&format!("<td><code>{}</code></td>", html_escape(p)));
            s.push_str(&format!("<td>{}</td>", format_int(*diffs)));
            s.push_str(&format!("<td>{}</td>", format_int(*reqs)));
            s.push_str(&format!("<td>{}</td>", html_escape(&format_rate(rate))));
            s.push_str("</tr>");
        }
        s.push_str("</tbody></table>");
    }

    // Last critical diff.
    s.push_str("<h2>last critical diff</h2>");
    match d.last_critical {
        Some((id, ns)) => {
            s.push_str(&format!(
                "<div class=\"card\"><div class=\"lbl\">observed</div>\
                 <div class=\"val\"><code>{}</code></div>\
                 <div class=\"sub\"><a href=\"/diffs/{id}\">view diff #{id}</a></div></div>",
                html_escape(&format_ns(ns)),
                id = id
            ));
        }
        None => {
            s.push_str("<div class=\"empty\">no critical diffs ever recorded.</div>");
        }
    }

    s.push_str(&format!(
        "<footer>page generated at <code>{}</code>. ",
        html_escape(&format_ns(d.now_ns))
    ));
    s.push_str("\"requests (approx)\" = diffs + queued writes + failed writes — successful writes are not retained, so this undercounts slightly. The resulting rate is conservative (overstates divergence rather than hiding it).");
    s.push_str("</footer>");
    push_footer(&mut s);
    s
}

fn push_card(s: &mut String, label: &str, val: &str, sub: &str, degraded: bool) {
    let cls = if degraded { " degraded" } else { "" };
    s.push_str(&format!(
        "<div class=\"card{cls}\"><div class=\"lbl\">{}</div>\
         <div class=\"val\">{}</div><div class=\"sub\">{}</div></div>",
        html_escape(label),
        html_escape(val),
        html_escape(sub),
    ));
}

/// Severity colours match the dashboard `.sev-*` classes.
fn severity_colour(sev: &str) -> &'static str {
    match sev {
        "critical" => "#b00020",
        "high" => "#d35400",
        "medium" => "#b58900",
        "low" => "#888888",
        "noise" => "#cccccc",
        _ => "#999999",
    }
}

/// Render an inline SVG horizontal bar chart for the severity breakdown.
/// No external assets, no JS — pure SVG + CSS-friendly inline attrs.
fn render_severity_svg(sevs: &[(String, i64); 5]) -> String {
    let max = sevs.iter().map(|(_, n)| *n).max().unwrap_or(0).max(1);
    let row_h: i64 = 24;
    let row_gap: i64 = 6;
    let label_w: i64 = 80;
    let count_w: i64 = 60;
    let bar_max: i64 = 360; // max bar pixel width
    let width: i64 = label_w + bar_max + count_w + 16;
    let height: i64 = sevs.len() as i64 * (row_h + row_gap) + 8;

    let mut s = String::with_capacity(1024);
    s.push_str(&format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 {w} {h}\" \
         width=\"100%\" style=\"max-width:{w}px;height:auto;font:12px ui-monospace,SFMono-Regular,Menlo,monospace\" \
         role=\"img\" aria-label=\"severity breakdown bar chart\">",
        w = width,
        h = height,
    ));
    for (i, (sev, n)) in sevs.iter().enumerate() {
        let y = i as i64 * (row_h + row_gap) + 4;
        let bar_w = bar_max * *n / max;
        let colour = severity_colour(sev);
        // Label
        s.push_str(&format!(
            "<text x=\"0\" y=\"{ty}\" fill=\"currentColor\">{lbl}</text>",
            ty = y + row_h - 8,
            lbl = html_escape(sev),
        ));
        // Bar (always render at least a 1px stub so empty bars are visible)
        let drawn_w = if *n > 0 { bar_w.max(2) } else { 0 };
        if drawn_w > 0 {
            s.push_str(&format!(
                "<rect x=\"{x}\" y=\"{y}\" width=\"{w}\" height=\"{h}\" fill=\"{c}\" rx=\"2\"/>",
                x = label_w,
                y = y,
                w = drawn_w,
                h = row_h - 4,
                c = colour,
            ));
        }
        // Count
        s.push_str(&format!(
            "<text x=\"{tx}\" y=\"{ty}\" fill=\"currentColor\">{n}</text>",
            tx = label_w + bar_max + 8,
            ty = y + row_h - 8,
            n = n,
        ));
    }
    s.push_str("</svg>");
    s
}

fn push_header(s: &mut String, title: &str) {
    s.push_str("<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">");
    s.push_str("<meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">");
    s.push_str(&format!(
        "<title>{} · diffsplitter</title>",
        html_escape(title)
    ));
    s.push_str("<style>");
    s.push_str(STYLE);
    s.push_str("</style></head><body>");
}

fn push_footer(s: &mut String) {
    s.push_str("</body></html>");
}

fn format_int(n: i64) -> String {
    // Tiny grouping (every 3 digits) without pulling in a dep.
    let neg = n < 0;
    let mut digits: Vec<u8> = n.unsigned_abs().to_string().into_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(digits.len() + digits.len() / 3 + 1);
    let len = digits.len();
    for (i, c) in digits.drain(..).enumerate() {
        if i > 0 && (len - i).is_multiple_of(3) {
            out.push(b',');
        }
        out.push(c);
    }
    let mut s = String::from_utf8(out).unwrap_or_default();
    if neg {
        s.insert(0, '-');
    }
    s
}

fn format_rate(r: f64) -> String {
    // Show as percentage with 3 sig figs; below 0.01% drop to ppm.
    if r <= 0.0 {
        return "0.00%".into();
    }
    let pct = r * 100.0;
    if pct >= 1.0 {
        format!("{:.2}%", pct)
    } else if pct >= 0.01 {
        format!("{:.3}%", pct)
    } else {
        format!("{:.0} ppm", r * 1_000_000.0)
    }
}

/// `YYYY-MM-DDTHH:MM:SSZ` UTC formatter, pulled from the dashboard module.
/// (Duplicated rather than `pub`-exporting from `dashboard` to keep the
/// module surface area minimal — both copies are pure functions of an i64.)
fn format_ns(ns: i64) -> String {
    let secs = ns.div_euclid(1_000_000_000);
    let days = secs.div_euclid(86_400);
    let sod = secs.rem_euclid(86_400);
    let h = sod / 3600;
    let m = (sod % 3600) / 60;
    let s = sod % 60;

    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 };
    let yy = if mo <= 2 { y + 1 } else { y };
    format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z", yy, mo, d, h, m, s)
}

fn html_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_int_grouping() {
        assert_eq!(format_int(0), "0");
        assert_eq!(format_int(7), "7");
        assert_eq!(format_int(1_234), "1,234");
        assert_eq!(format_int(1_000_000), "1,000,000");
        assert_eq!(format_int(-12_345), "-12,345");
    }

    #[test]
    fn format_rate_buckets() {
        assert_eq!(format_rate(0.0), "0.00%");
        assert_eq!(format_rate(0.5), "50.00%");
        assert_eq!(format_rate(0.00005), "50 ppm");
        assert_eq!(format_rate(0.005), "0.500%");
    }

    #[test]
    fn severity_svg_renders_all_rows() {
        let sevs: [(String, i64); 5] = [
            ("critical".into(), 3),
            ("high".into(), 7),
            ("medium".into(), 0),
            ("low".into(), 2),
            ("noise".into(), 1),
        ];
        let svg = render_severity_svg(&sevs);
        assert!(svg.starts_with("<svg"));
        assert!(svg.contains("critical"));
        assert!(svg.contains("noise"));
        assert!(svg.contains("#b00020")); // critical colour
    }
}
