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

use breakdowns::Dimension;

#[cfg(any(test, feature = "testing"))]
pub mod test_support;

/// The value a filter matches to mean "this column is NULL".
///
/// Breakdowns render a NULL as `(unknown)`, so a dashboard that turns a
/// breakdown row into a filter sends back exactly what it displayed.
pub const UNKNOWN_VALUE: &str = "(unknown)";

/// One condition narrowing a report to a subset of events.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Filter {
    /// The dimension — and therefore the column — being matched.
    pub dimension: Dimension,
    /// `true` for `!=`, `false` for `==`.
    pub negated: bool,
    /// The value to match. [`UNKNOWN_VALUE`] means SQL `NULL`.
    pub value: String,
}

impl Filter {
    /// The SQL predicate for this filter, with the value left as a `?`.
    ///
    /// The column name comes from the [`Dimension`] enum and never from request
    /// input; the value is always bound.
    ///
    /// `NULL` needs its own spelling because SQL's three-valued logic drops
    /// rows a reader would expect to keep: `browser <> 'Chrome'` excludes every
    /// event with no browser at all, which is not what "not Chrome" means to
    /// someone reading a report.
    fn predicate(&self) -> String {
        let col = self.dimension.column_name();
        match (self.value == UNKNOWN_VALUE, self.negated) {
            (true, false) => format!("{col} IS NULL"),
            (true, true) => format!("{col} IS NOT NULL"),
            (false, false) => format!("{col} = ?"),
            (false, true) => format!("({col} IS NULL OR {col} <> ?)"),
        }
    }

    /// The bound value, or `None` when the predicate needs no parameter.
    fn param(&self) -> Option<&str> {
        (self.value != UNKNOWN_VALUE).then_some(self.value.as_str())
    }

    /// Whether this dimension can be filtered on at all.
    ///
    /// Entry and exit pages are session-derived: they are computed by
    /// `MIN_BY`/`MAX_BY` over a sessionised window rather than read from a
    /// column, so there is no row-level predicate that expresses them. Allowing
    /// the slug and quietly filtering on `pathname` instead would answer a
    /// different question than the one asked.
    pub const fn is_filterable(dimension: Dimension) -> bool {
        !matches!(dimension, Dimension::EntryPage | Dimension::ExitPage)
    }
}

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
    /// Conditions narrowing the report; empty means the whole site.
    pub filters: Vec<Filter>,
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
            filters: Vec::new(),
        }
    }

    /// The same scope narrowed by `filters`.
    #[must_use]
    pub fn with_filters(mut self, filters: Vec<Filter>) -> Self {
        self.filters = filters;
        self
    }

    /// The bound parameters a scoped query binds, in order: site, start, end,
    /// then one per filter that needs a value.
    pub fn params(&self) -> Vec<&str> {
        let mut out = Vec::with_capacity(3 + self.filters.len());
        out.push(self.site_id.as_str());
        out.push(self.start.as_str());
        out.push(self.end.as_str());
        out.extend(self.filter_params());
        out
    }

    /// Just the filter values, for queries whose bind order differs.
    pub fn filter_params(&self) -> impl Iterator<Item = &str> {
        self.filters.iter().filter_map(Filter::param)
    }

    /// The `WHERE` fragment that scopes a query to this site, range and filters.
    ///
    /// Always paired with [`QueryScope::params`]; `site_id`, the dates and every
    /// filter value are bound parameters, never interpolated. Only column names
    /// are interpolated, and those come from the [`Dimension`] enum.
    pub fn where_clause(&self) -> String {
        format!("{}{}", Self::SCOPE_CLAUSE, self.filter_clause())
    }

    /// The site-and-range half of [`QueryScope::where_clause`].
    const SCOPE_CLAUSE: &'static str = "site_id = ? \
         AND timestamp >= CAST(? AS TIMESTAMP) \
         AND timestamp < CAST(? AS TIMESTAMP)";

    /// The filter half alone, for queries that spell their own scope clause.
    ///
    /// Returns an empty string when there are no filters, so it can be appended
    /// unconditionally.
    pub fn filter_clause(&self) -> String {
        let mut out = String::new();
        for filter in &self.filters {
            out.push_str(" AND ");
            out.push_str(&filter.predicate());
        }
        out
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

    fn filter(slug: &str, negated: bool, value: &str) -> Filter {
        Filter {
            dimension: Dimension::from_slug(slug).unwrap(),
            negated,
            value: value.to_string(),
        }
    }

    #[test]
    fn test_unfiltered_scope_is_unchanged() {
        let scope = QueryScope::new("a.com", "2024-01-01", "2024-02-01", "30 minutes");
        assert_eq!(scope.filter_clause(), "");
        assert_eq!(scope.where_clause().matches('?').count(), 3);
        assert_eq!(scope.params().len(), 3);
    }

    #[test]
    fn test_filters_add_a_predicate_and_a_parameter_each() {
        let scope = QueryScope::new("a.com", "s", "e", "30 minutes")
            .with_filters(vec![filter("browsers", false, "Chrome")]);
        let clause = scope.where_clause();
        assert!(clause.ends_with(" AND browser = ?"), "{clause}");
        assert_eq!(clause.matches('?').count(), 4);
        assert_eq!(scope.params(), ["a.com", "s", "e", "Chrome"]);
    }

    #[test]
    fn test_negated_filter_keeps_rows_where_the_column_is_null() {
        // Plain `browser <> 'Chrome'` drops every event with no browser at all,
        // which is not what "not Chrome" means to someone reading a report.
        let scope = QueryScope::new("a.com", "s", "e", "30 minutes")
            .with_filters(vec![filter("browsers", true, "Chrome")]);
        let clause = scope.where_clause();
        assert!(
            clause.contains("(browser IS NULL OR browser <> ?)"),
            "{clause}"
        );
        assert_eq!(scope.params(), ["a.com", "s", "e", "Chrome"]);
    }

    #[test]
    fn test_unknown_value_becomes_a_null_check_and_binds_nothing() {
        let scope = QueryScope::new("a.com", "s", "e", "30 minutes").with_filters(vec![filter(
            "countries",
            false,
            UNKNOWN_VALUE,
        )]);
        assert!(scope.where_clause().ends_with(" AND country_code IS NULL"));
        assert_eq!(
            scope.params().len(),
            3,
            "an IS NULL predicate must not consume a parameter"
        );

        let negated = QueryScope::new("a.com", "s", "e", "30 minutes").with_filters(vec![filter(
            "countries",
            true,
            UNKNOWN_VALUE,
        )]);
        assert!(
            negated
                .where_clause()
                .ends_with(" AND country_code IS NOT NULL")
        );
        assert_eq!(negated.params().len(), 3);
    }

    #[test]
    fn test_filter_values_are_never_interpolated() {
        // The whole point of binding: a value that looks like SQL stays a value.
        let nasty = "'; DROP TABLE events; --";
        let scope = QueryScope::new("a.com", "s", "e", "30 minutes")
            .with_filters(vec![filter("pages", false, nasty)]);
        let clause = scope.where_clause();
        assert!(!clause.contains("DROP TABLE"), "{clause}");
        assert!(!clause.contains('\''), "{clause}");
        assert_eq!(scope.params()[3], nasty);
    }

    #[test]
    fn test_predicate_count_matches_parameter_count() {
        let scope = QueryScope::new("a.com", "s", "e", "30 minutes").with_filters(vec![
            filter("browsers", false, "Chrome"),
            filter("countries", true, "US"),
            filter("os", false, UNKNOWN_VALUE),
            filter("utm-sources", false, "newsletter"),
        ]);
        assert_eq!(
            scope.where_clause().matches('?').count(),
            scope.params().len(),
            "every ? must have exactly one bound value"
        );
    }

    #[test]
    fn test_entry_and_exit_pages_are_not_filterable() {
        // They are computed over a sessionised window, so there is no row-level
        // predicate for them; filtering on `pathname` instead would answer a
        // different question.
        assert!(!Filter::is_filterable(Dimension::EntryPage));
        assert!(!Filter::is_filterable(Dimension::ExitPage));
        for slug in Dimension::SLUGS {
            let dim = Dimension::from_slug(slug).unwrap();
            if !matches!(dim, Dimension::EntryPage | Dimension::ExitPage) {
                assert!(Filter::is_filterable(dim), "{slug} should be filterable");
            }
        }
    }

    #[test]
    fn test_filter_clause_can_be_appended_unconditionally() {
        // Queries that spell their own scope clause append this verbatim.
        let scope = QueryScope::new("a.com", "s", "e", "30 minutes");
        assert!(scope.filter_clause().is_empty());
        let filtered = scope.with_filters(vec![filter("devices", false, "mobile")]);
        assert!(filtered.filter_clause().starts_with(" AND "));
    }

    #[test]
    fn test_params_order() {
        let scope = QueryScope::new("a.com", "2024-01-01", "2024-02-01", "30 minutes");
        assert_eq!(scope.params(), ["a.com", "2024-01-01", "2024-02-01"]);
    }

    #[test]
    fn test_where_clause_uses_only_bound_parameters() {
        let clause = QueryScope::new("a.com", "s", "e", "30 minutes").where_clause();
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
