pub mod breakdowns;
pub mod cache;
pub mod events;
pub mod export;
pub mod flow;
pub mod funnel;
pub mod metrics;
pub mod realtime;
pub mod retention;
pub mod revenue;
pub mod sequences;
pub mod timeseries;

#[cfg(any(test, feature = "testing"))]
pub mod test_support;

/// Parameters shared by every analytics query.
///
/// Passing one struct instead of four positional `&str`s removes a whole class
/// of call-site mistakes (swapping `start` and `end`, or `site_id` and a date)
/// that the compiler could not otherwise catch.
#[derive(Debug, Clone)]
pub struct QueryScope {
    /// Site the query is restricted to.
    pub site_id: String,
    /// Inclusive lower bound, `YYYY-MM-DD` or `YYYY-MM-DD HH:MM:SS`.
    pub start: String,
    /// Exclusive upper bound, same format as `start`.
    pub end: String,
    /// Session inactivity window as a DuckDB interval literal, e.g. `30 minutes`.
    pub session_window: String,
}

impl QueryScope {
    pub fn new(
        site_id: impl Into<String>,
        start: impl Into<String>,
        end: impl Into<String>,
        session_window: impl Into<String>,
    ) -> Self {
        Self {
            site_id: site_id.into(),
            start: start.into(),
            end: end.into(),
            session_window: session_window.into(),
        }
    }

    /// The three bound parameters every scoped query binds, in order.
    pub fn params(&self) -> [&str; 3] {
        [&self.site_id, &self.start, &self.end]
    }

    /// The `WHERE` fragment that scopes a query to this site and time range.
    ///
    /// Always paired with [`QueryScope::params`]; `site_id` and the dates are
    /// bound parameters, never interpolated.
    pub const fn where_clause() -> &'static str {
        "site_id = ? \
         AND timestamp >= CAST(? AS TIMESTAMP) \
         AND timestamp < CAST(? AS TIMESTAMP)"
    }

    /// Validate a DuckDB interval literal such as `30 minutes`.
    ///
    /// Session windows come from operator configuration rather than request
    /// input, but they are interpolated into SQL, so they are checked anyway.
    pub fn session_window_is_safe(&self) -> bool {
        let mut parts = self.session_window.split_whitespace();
        let (Some(n), Some(unit), None) = (parts.next(), parts.next(), parts.next()) else {
            return false;
        };
        n.parse::<u32>().is_ok_and(|v| v > 0)
            && matches!(
                unit,
                "second" | "seconds" | "minute" | "minutes" | "hour" | "hours"
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_params_order() {
        let scope = QueryScope::new("a.com", "2024-01-01", "2024-02-01", "30 minutes");
        assert_eq!(scope.params(), ["a.com", "2024-01-01", "2024-02-01"]);
    }

    #[test]
    fn test_where_clause_uses_only_bound_parameters() {
        let clause = QueryScope::where_clause();
        assert_eq!(clause.matches('?').count(), 3);
        assert!(!clause.contains('\''));
    }

    #[test]
    fn test_session_window_validation() {
        let ok = |w: &str| QueryScope::new("a", "b", "c", w).session_window_is_safe();
        assert!(ok("30 minutes"));
        assert!(ok("1 hour"));
        assert!(ok("90 seconds"));
        assert!(!ok("0 minutes"));
        assert!(!ok("30 fortnights"));
        assert!(!ok("30"));
        assert!(!ok("30 minutes; DROP TABLE events"));
        assert!(!ok(""));
    }
}
