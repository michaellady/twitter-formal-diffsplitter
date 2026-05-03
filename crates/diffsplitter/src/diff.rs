//! Diff comparator.
//!
//! Compares primary vs shadow responses and produces a normalized diff blob.
//! Bodies are parsed as JSON when possible — order-insensitive for objects,
//! order-sensitive for arrays (timeline ordering matters). Non-JSON bodies
//! are compared verbatim as strings.

use serde_json::Value;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiffOutcome {
    pub diverged: bool,
    pub severity: Severity,
    pub blob: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Severity {
    Critical,
    High,
    Medium,
    Low,
    Noise,
}

impl Severity {
    pub fn as_str(&self) -> &'static str {
        match self {
            Severity::Critical => "critical",
            Severity::High => "high",
            Severity::Medium => "medium",
            Severity::Low => "low",
            Severity::Noise => "noise",
        }
    }
}

/// Compare two responses. `path` is used to bias severity (e.g. /version
/// differences are noise — image_digest will always differ across impls).
pub fn compare(
    path: &str,
    primary_status: u16,
    shadow_status: u16,
    primary_body: &str,
    shadow_body: &str,
) -> DiffOutcome {
    if primary_status != shadow_status {
        let blob = format!(
            "{{\"kind\":\"status\",\"primary\":{primary_status},\"shadow\":{shadow_status}}}"
        );
        let severity = severity_for_status_diff(path, primary_status, shadow_status);
        return DiffOutcome {
            diverged: true,
            severity,
            blob,
        };
    }

    // Try JSON-aware comparison first.
    let p_json: Result<Value, _> = serde_json::from_str(primary_body);
    let s_json: Result<Value, _> = serde_json::from_str(shadow_body);

    if let (Ok(mut p), Ok(mut s)) = (p_json, s_json) {
        // /version always differs across implementations on image_digest and
        // sometimes git_sha; mask those before comparing so we don't generate
        // noise for every read sample.
        if path == "/version" {
            mask_version_fields(&mut p);
            mask_version_fields(&mut s);
        }
        if json_equal(&p, &s) {
            return DiffOutcome {
                diverged: false,
                severity: Severity::Noise,
                blob: String::new(),
            };
        }
        let blob = serde_json::to_string(&serde_json::json!({
            "kind": "json",
            "primary": p,
            "shadow": s,
        }))
        .unwrap_or_else(|_| "{\"kind\":\"json\",\"err\":\"serialize\"}".into());
        let severity = severity_for_path(path);
        return DiffOutcome {
            diverged: true,
            severity,
            blob,
        };
    }

    // Fall back to byte equality.
    if primary_body == shadow_body {
        return DiffOutcome {
            diverged: false,
            severity: Severity::Noise,
            blob: String::new(),
        };
    }
    let blob = format!(
        "{{\"kind\":\"text\",\"primary_len\":{},\"shadow_len\":{}}}",
        primary_body.len(),
        shadow_body.len()
    );
    DiffOutcome {
        diverged: true,
        severity: severity_for_path(path),
        blob,
    }
}

fn severity_for_status_diff(path: &str, p: u16, s: u16) -> Severity {
    // 5xx vs anything → critical (one impl crashed).
    if p >= 500 || s >= 500 {
        return Severity::Critical;
    }
    // 4xx mismatch — one impl rejected, one accepted.
    if (p / 100) != (s / 100) {
        return severity_for_path(path).max(Severity::High);
    }
    severity_for_path(path)
}

fn severity_for_path(path: &str) -> Severity {
    // /timeline divergence is the highest signal — that's what users see.
    if path.starts_with("/timeline") || path.starts_with("/u/") {
        return Severity::High;
    }
    if path == "/version" || path == "/healthz" {
        return Severity::Noise;
    }
    Severity::Medium
}

impl std::cmp::PartialOrd for Severity {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl std::cmp::Ord for Severity {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        // Higher = more severe.
        let rank = |s: &Severity| match s {
            Severity::Critical => 5,
            Severity::High => 4,
            Severity::Medium => 3,
            Severity::Low => 2,
            Severity::Noise => 1,
        };
        rank(self).cmp(&rank(other))
    }
}
fn mask_version_fields(v: &mut Value) {
    if let Value::Object(m) = v {
        for k in [
            "image_digest",
            "git_sha",
            "process_uptime_seconds",
            "captured_at_unix_nanos",
        ] {
            m.remove(k);
        }
    }
}

/// Order-insensitive object equality, order-sensitive array equality.
fn json_equal(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Object(am), Value::Object(bm)) => {
            if am.len() != bm.len() {
                return false;
            }
            am.iter()
                .all(|(k, av)| bm.get(k).map(|bv| json_equal(av, bv)).unwrap_or(false))
        }
        (Value::Array(aa), Value::Array(ba)) => {
            aa.len() == ba.len() && aa.iter().zip(ba.iter()).all(|(x, y)| json_equal(x, y))
        }
        _ => a == b,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn equal_json_no_diff() {
        let r = compare(
            "/timeline",
            200,
            200,
            r#"{"a":1,"b":2}"#,
            r#"{"b":2,"a":1}"#,
        );
        assert!(!r.diverged);
    }

    #[test]
    fn array_order_matters() {
        let r = compare("/timeline", 200, 200, r#"[1,2,3]"#, r#"[3,2,1]"#);
        assert!(r.diverged);
        assert_eq!(r.severity, Severity::High);
    }

    #[test]
    fn version_image_digest_masked() {
        let r = compare(
            "/version",
            200,
            200,
            r#"{"git_sha":"a","image_digest":"sha256:1","snapshot_version":1}"#,
            r#"{"git_sha":"b","image_digest":"sha256:2","snapshot_version":1}"#,
        );
        assert!(!r.diverged, "blob: {}", r.blob);
    }

    #[test]
    fn status_5xx_is_critical() {
        let r = compare("/tweets", 500, 200, "x", "y");
        assert!(r.diverged);
        assert_eq!(r.severity, Severity::Critical);
    }
}
