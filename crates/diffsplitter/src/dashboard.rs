//! Server-rendered HTML dashboard for the `diffs` table.
//!
//! Routes mounted on the existing axum router (see `proxy::router`):
//!
//! * `GET /diffs`             — last 100 diffs as an HTML table
//! * `GET /diffs/:id`         — drill-in: side-by-side primary vs shadow body
//! * `GET /diffs.json`        — same data as the listing page, JSON shape
//!   `{ "diffs": [...] }`. Supersedes the placeholder handler from Phase 1
//!   in `proxy.rs`; supports the same `?severity=…&since=…` filters as the
//!   HTML listing.
//!
//! Query params (all optional, all apply to listing and JSON):
//!
//! * `severity=critical|high|medium|low|noise` — exact match
//! * `since=YYYY-MM-DD`                        — UTC date inclusive lower bound
//!
//! Diff rows whose `descended_from_failed_write_id` is non-null are visually
//! greyed out per K13 ("shadow-degraded" state — these diffs are caused by a
//! known dropped write, not a real cross-impl divergence).
//!
//! No JS framework. No external CSS. Inline `<style>` keeps the page self-
//! contained and well under the 50KB budget (typical render ≈ 6-15KB).

use std::sync::Arc;

use axum::extract::{Path, Query, State as AxState};
use axum::http::{header, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use serde::Deserialize;
use serde_json::json;

use crate::{db, State};

/// Mount the dashboard routes onto an existing router.
pub fn routes() -> Router<Arc<State>> {
    Router::new()
        .route("/diffs", get(diffs_index))
        .route("/diffs/:id", get(diffs_show))
        .route("/diffs.json", get(diffs_json))
}

#[derive(Debug, Deserialize, Default)]
pub struct ListQuery {
    /// Optional exact severity filter.
    pub severity: Option<String>,
    /// Optional `YYYY-MM-DD` lower bound (UTC) on `observed_at_ns`.
    pub since: Option<String>,
}

const LIMIT: i64 = 100;

/// Allowed severity filter values. We validate explicitly rather than passing
/// arbitrary user input down to SQL — even though the query is parameterized,
/// there is no reason to query for nonsense.
const SEVERITIES: &[&str] = &["critical", "high", "medium", "low", "noise"];

fn parse_filters(q: &ListQuery) -> Result<(Option<String>, Option<i64>), (StatusCode, String)> {
    let sev = match &q.severity {
        Some(s) if s.is_empty() => None,
        Some(s) => {
            if SEVERITIES.contains(&s.as_str()) {
                Some(s.clone())
            } else {
                return Err((
                    StatusCode::BAD_REQUEST,
                    format!(
                        "invalid severity `{s}`; want one of {}",
                        SEVERITIES.join("|")
                    ),
                ));
            }
        }
        None => None,
    };
    let since_ns = match &q.since {
        Some(s) if s.is_empty() => None,
        Some(s) => match parse_date_to_ns(s) {
            Some(ns) => Some(ns),
            None => {
                return Err((
                    StatusCode::BAD_REQUEST,
                    format!("invalid since `{s}`; want YYYY-MM-DD"),
                ))
            }
        },
        None => None,
    };
    Ok((sev, since_ns))
}

/// Tiny YYYY-MM-DD → unix nanoseconds parser. Pulled in deliberately rather
/// than adding a `chrono` dependency for one date conversion.
fn parse_date_to_ns(s: &str) -> Option<i64> {
    let bytes = s.as_bytes();
    if bytes.len() != 10 || bytes[4] != b'-' || bytes[7] != b'-' {
        return None;
    }
    let y: i64 = s.get(0..4)?.parse().ok()?;
    let m: u32 = s.get(5..7)?.parse().ok()?;
    let d: u32 = s.get(8..10)?.parse().ok()?;
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) || !(1970..=9999).contains(&y) {
        return None;
    }
    // Days since 1970-01-01 (UTC), via Howard Hinnant's civil_from_days inverse.
    // y' = y - (m <= 2)
    let yp = if m <= 2 { y - 1 } else { y };
    let era = if yp >= 0 { yp } else { yp - 399 } / 400;
    let yoe = (yp - era * 400) as u64; // [0..399]
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + d - 1; // [0..365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy as u64; // [0..146096]
    let days = era * 146_097 + doe as i64 - 719_468;
    Some(days * 86_400 * 1_000_000_000)
}

async fn diffs_index(AxState(state): AxState<Arc<State>>, Query(q): Query<ListQuery>) -> Response {
    let (sev, since_ns) = match parse_filters(&q) {
        Ok(t) => t,
        Err(e) => return e.into_response(),
    };
    let rows = match db::list_diffs_filtered(&state.pool, LIMIT, sev.as_deref(), since_ns) {
        Ok(r) => r,
        Err(e) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, format!("db error: {e}")).into_response();
        }
    };
    Html(render_index_html(&rows, &q)).into_response()
}

async fn diffs_show(AxState(state): AxState<Arc<State>>, Path(id): Path<i64>) -> Response {
    match db::get_diff(&state.pool, id) {
        Ok(Some(d)) => Html(render_show_html(&d)).into_response(),
        Ok(None) => (StatusCode::NOT_FOUND, "diff not found").into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("db error: {e}")).into_response(),
    }
}

async fn diffs_json(AxState(state): AxState<Arc<State>>, Query(q): Query<ListQuery>) -> Response {
    let (sev, since_ns) = match parse_filters(&q) {
        Ok(t) => t,
        Err(e) => return e.into_response(),
    };
    match db::list_diffs_filtered(&state.pool, LIMIT, sev.as_deref(), since_ns) {
        Ok(rows) => (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "application/json")],
            serde_json::to_string(&json!({ "diffs": rows }))
                .unwrap_or_else(|_| "{\"diffs\":[]}".into()),
        )
            .into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("db error: {e}")).into_response(),
    }
}

// -- HTML rendering ----------------------------------------------------------

const STYLE: &str = r#"
:root { color-scheme: light dark; }
body { font: 14px/1.4 -apple-system, BlinkMacSystemFont, "Segoe UI", system-ui, sans-serif;
       margin: 0; padding: 1rem; background: #fafafa; color: #111; }
h1 { font-size: 1.2rem; margin: 0 0 .5rem 0; }
header { display: flex; align-items: baseline; gap: 1rem; flex-wrap: wrap;
         border-bottom: 1px solid #ddd; padding-bottom: .5rem; margin-bottom: .75rem; }
header nav a { color: #06c; text-decoration: none; margin-right: .5rem; }
form.filters { display: flex; gap: .5rem; align-items: center; flex-wrap: wrap; margin: .5rem 0; }
form.filters label { font-size: 12px; color: #555; }
form.filters input, form.filters select { font: inherit; padding: 2px 4px; }
table { width: 100%; border-collapse: collapse; background: #fff;
        box-shadow: 0 1px 2px rgba(0,0,0,0.04); }
th, td { text-align: left; padding: 6px 8px; border-bottom: 1px solid #eee;
         font-variant-numeric: tabular-nums; }
th { background: #f3f3f3; font-weight: 600; font-size: 12px;
     text-transform: uppercase; letter-spacing: .04em; color: #555; }
tr.degraded td { color: #888; background: #f5f5f5; font-style: italic; }
tr.degraded td:first-child::after { content: " (degraded)"; color: #c60; font-style: normal; font-size: 11px; }
.sev { display: inline-block; padding: 1px 6px; border-radius: 3px; font-size: 11px;
       font-weight: 600; text-transform: uppercase; }
.sev-critical { background: #b00020; color: #fff; }
.sev-high     { background: #d35400; color: #fff; }
.sev-medium   { background: #b58900; color: #fff; }
.sev-low      { background: #888;    color: #fff; }
.sev-noise    { background: #ddd;    color: #333; }
code, pre { font-family: ui-monospace, SFMono-Regular, Menlo, monospace; font-size: 12px; }
.split { display: grid; grid-template-columns: 1fr 1fr; gap: 1rem; margin-top: 1rem; }
.split section { background: #fff; padding: .75rem; border: 1px solid #ddd;
                 box-shadow: 0 1px 2px rgba(0,0,0,0.04); overflow-x: auto; }
.split h2 { font-size: 1rem; margin: 0 0 .5rem 0; }
.split pre { white-space: pre-wrap; word-break: break-word; max-height: 60vh; overflow: auto;
             margin: 0; background: #fafafa; padding: .5rem; border: 1px solid #eee; }
.empty { padding: 2rem; text-align: center; color: #777; }
footer { margin-top: 1rem; font-size: 12px; color: #777; }
@media (prefers-color-scheme: dark) {
  body { background: #181818; color: #ddd; }
  header { border-color: #333; }
  table, .split section { background: #222; box-shadow: none; border-color: #333; }
  th { background: #2a2a2a; color: #aaa; }
  th, td { border-color: #333; }
  tr.degraded td { background: #1f1f1f; color: #777; }
  .split pre { background: #181818; border-color: #333; }
  header nav a { color: #6cf; }
  form.filters label { color: #aaa; }
}
"#;

fn render_index_html(rows: &[db::DiffRow], q: &ListQuery) -> String {
    let mut s = String::with_capacity(8 * 1024 + rows.len() * 256);
    push_header(&mut s, "diffs");
    s.push_str("<header><h1>diffsplitter — diffs</h1>");
    s.push_str("<nav><a href=\"/diffs\">all</a>");
    s.push_str("<a href=\"/diffs.json\">json</a>");
    s.push_str("<a href=\"/metrics\">metrics</a></nav></header>");

    // Filter form (GET so query params land in the URL).
    s.push_str("<form class=\"filters\" method=\"GET\" action=\"/diffs\">");
    s.push_str("<label>severity ");
    s.push_str("<select name=\"severity\">");
    s.push_str("<option value=\"\">(any)</option>");
    for sev in SEVERITIES {
        let sel = if q.severity.as_deref() == Some(sev) {
            " selected"
        } else {
            ""
        };
        s.push_str(&format!(
            "<option value=\"{sev}\"{sel}>{sev}</option>",
            sev = html_escape(sev),
            sel = sel
        ));
    }
    s.push_str("</select></label>");
    s.push_str("<label>since (UTC date) <input type=\"date\" name=\"since\" value=\"");
    s.push_str(&html_escape(q.since.as_deref().unwrap_or("")));
    s.push_str("\"></label>");
    s.push_str("<button type=\"submit\">apply</button>");
    s.push_str("</form>");

    if rows.is_empty() {
        s.push_str("<div class=\"empty\">no diffs match the current filter.</div>");
    } else {
        s.push_str("<table><thead><tr>");
        for col in [
            "timestamp",
            "method",
            "path",
            "primary",
            "shadow",
            "severity",
            "",
        ] {
            s.push_str(&format!("<th>{}</th>", html_escape(col)));
        }
        s.push_str("</tr></thead><tbody>");
        for r in rows {
            let cls = if r.descended_from_failed_write_id.is_some() {
                " class=\"degraded\""
            } else {
                ""
            };
            s.push_str(&format!("<tr{cls}>"));
            s.push_str(&format!(
                "<td><code>{}</code></td>",
                html_escape(&format_ns(r.observed_at_ns))
            ));
            s.push_str(&format!("<td>{}</td>", html_escape(&r.method)));
            s.push_str(&format!("<td><code>{}</code></td>", html_escape(&r.path)));
            s.push_str(&format!("<td>{}</td>", opt_status(r.primary_status)));
            s.push_str(&format!("<td>{}</td>", opt_status(r.shadow_status)));
            s.push_str(&format!(
                "<td><span class=\"sev sev-{sev}\">{sev}</span></td>",
                sev = html_escape(&r.severity)
            ));
            s.push_str(&format!(
                "<td><a href=\"/diffs/{id}\">view</a></td>",
                id = r.id
            ));
            s.push_str("</tr>");
        }
        s.push_str("</tbody></table>");
    }
    s.push_str(&format!(
        "<footer>showing {} diff(s); limit {}.</footer>",
        rows.len(),
        LIMIT
    ));
    push_footer(&mut s);
    s
}

fn render_show_html(d: &db::DiffDetail) -> String {
    let mut s = String::with_capacity(4096 + d.primary_body.as_deref().map_or(0, |b| b.len()) * 2);
    push_header(&mut s, &format!("diff #{}", d.id));
    s.push_str("<header><h1>");
    s.push_str(&format!("diff #{} ", d.id));
    s.push_str(&format!(
        "<span class=\"sev sev-{sev}\">{sev}</span>",
        sev = html_escape(&d.severity)
    ));
    s.push_str("</h1>");
    s.push_str("<nav><a href=\"/diffs\">&larr; back</a>");
    s.push_str("<a href=\"/diffs.json\">json</a></nav></header>");

    s.push_str("<table><tbody>");
    push_kv(&mut s, "observed", &format_ns(d.observed_at_ns));
    push_kv(&mut s, "method", &d.method);
    push_kv(&mut s, "path", &d.path);
    push_kv(
        &mut s,
        "primary status",
        &d.primary_status
            .map(|n| n.to_string())
            .unwrap_or_else(|| "—".into()),
    );
    push_kv(
        &mut s,
        "shadow status",
        &d.shadow_status
            .map(|n| n.to_string())
            .unwrap_or_else(|| "—".into()),
    );
    if let Some(fid) = d.descended_from_failed_write_id {
        push_kv(
            &mut s,
            "descended from failed write",
            &format!("#{fid} (shadow degraded — see K13)"),
        );
    }
    s.push_str("</tbody></table>");

    s.push_str("<div class=\"split\">");
    s.push_str("<section><h2>primary body</h2><pre>");
    s.push_str(&html_escape(d.primary_body.as_deref().unwrap_or("(none)")));
    s.push_str("</pre></section>");
    s.push_str("<section><h2>shadow body</h2><pre>");
    s.push_str(&html_escape(d.shadow_body.as_deref().unwrap_or("(none)")));
    s.push_str("</pre></section>");
    s.push_str("</div>");

    s.push_str(
        "<section style=\"margin-top:1rem;background:#fff;padding:.75rem;border:1px solid #ddd;\">",
    );
    s.push_str("<h2 style=\"margin:0 0 .5rem 0;font-size:1rem;\">structured diff</h2>");
    s.push_str("<pre style=\"white-space:pre-wrap;word-break:break-word;margin:0;\">");
    s.push_str(&html_escape(&pretty_json_or_raw(&d.diff_blob)));
    s.push_str("</pre></section>");

    push_footer(&mut s);
    s
}

fn push_kv(s: &mut String, k: &str, v: &str) {
    s.push_str(&format!(
        "<tr><th style=\"width:14em\">{}</th><td>{}</td></tr>",
        html_escape(k),
        html_escape(v)
    ));
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

fn opt_status(v: Option<i64>) -> String {
    match v {
        Some(n) => format!("<code>{n}</code>"),
        None => "<code>—</code>".into(),
    }
}

fn pretty_json_or_raw(raw: &str) -> String {
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(raw) {
        serde_json::to_string_pretty(&v).unwrap_or_else(|_| raw.to_string())
    } else {
        raw.to_string()
    }
}

/// Format unix-nanoseconds as `YYYY-MM-DDTHH:MM:SSZ` UTC. Avoids pulling in
/// chrono just for one display fmt; days→YMD via Howard Hinnant's civil_from_days.
fn format_ns(ns: i64) -> String {
    let secs = ns.div_euclid(1_000_000_000);
    let days = secs.div_euclid(86_400);
    let sod = secs.rem_euclid(86_400);
    let h = sod / 3600;
    let m = (sod % 3600) / 60;
    let s = sod % 60;

    // civil_from_days
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
    fn date_parser_epoch() {
        assert_eq!(parse_date_to_ns("1970-01-01"), Some(0));
    }

    #[test]
    fn date_parser_known_day() {
        // 2024-01-01 = 19723 days since epoch
        let want = 19723_i64 * 86_400 * 1_000_000_000;
        assert_eq!(parse_date_to_ns("2024-01-01"), Some(want));
    }

    #[test]
    fn date_parser_rejects_bad_input() {
        assert_eq!(parse_date_to_ns(""), None);
        assert_eq!(parse_date_to_ns("2024/01/01"), None);
        assert_eq!(parse_date_to_ns("2024-13-01"), None);
        assert_eq!(parse_date_to_ns("2024-01-32"), None);
    }

    #[test]
    fn format_ns_epoch() {
        assert_eq!(format_ns(0), "1970-01-01T00:00:00Z");
    }

    #[test]
    fn format_ns_known() {
        // 2024-05-03T12:34:56Z = 1714739696
        let ns = 1_714_739_696_i64 * 1_000_000_000;
        assert_eq!(format_ns(ns), "2024-05-03T12:34:56Z");
    }

    #[test]
    fn html_escape_basics() {
        assert_eq!(html_escape("a&b<c>\"d'"), "a&amp;b&lt;c&gt;&quot;d&#39;");
    }

    #[test]
    fn parse_filters_rejects_bad_severity() {
        let q = ListQuery {
            severity: Some("nope".into()),
            since: None,
        };
        assert!(parse_filters(&q).is_err());
    }

    #[test]
    fn parse_filters_accepts_known_severity() {
        let q = ListQuery {
            severity: Some("critical".into()),
            since: None,
        };
        let (sev, since) = parse_filters(&q).unwrap();
        assert_eq!(sev.as_deref(), Some("critical"));
        assert!(since.is_none());
    }
}
