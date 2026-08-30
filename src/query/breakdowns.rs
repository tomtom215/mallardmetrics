use super::QueryScope;
use duckdb::Connection;
use serde::{Deserialize, Serialize};

/// A breakdown row: dimension value plus counts.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BreakdownRow {
    pub value: String,
    pub visitors: u64,
    pub pageviews: u64,
    /// All events, not just pageviews. Distinguishes a page with heavy custom
    /// event traffic from one that is merely viewed.
    pub events: u64,
}

/// Dimensions a breakdown can group by.
///
/// Every column the ingest path populates is exposed. The previous release
/// collected `utm_*`, `region` and `city` on every event but offered no way to
/// query any of them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dimension {
    Page,
    EntryPage,
    ExitPage,
    Referrer,
    ReferrerSource,
    CountryCode,
    Region,
    City,
    Browser,
    BrowserVersion,
    Os,
    OsVersion,
    DeviceType,
    ScreenSize,
    UtmSource,
    UtmMedium,
    UtmCampaign,
    UtmContent,
    UtmTerm,
    EventName,
}

impl Dimension {
    /// The `events_all` column this dimension groups by.
    ///
    /// Interpolated into SQL, which is safe because the value comes from this
    /// fixed enum and never from request input.
    pub const fn column_name(self) -> &'static str {
        match self {
            Self::Page | Self::EntryPage | Self::ExitPage => "pathname",
            Self::Referrer => "referrer",
            Self::ReferrerSource => "referrer_source",
            Self::CountryCode => "country_code",
            Self::Region => "region",
            Self::City => "city",
            Self::Browser => "browser",
            Self::BrowserVersion => "browser_version",
            Self::Os => "os",
            Self::OsVersion => "os_version",
            Self::DeviceType => "device_type",
            Self::ScreenSize => "screen_size",
            Self::UtmSource => "utm_source",
            Self::UtmMedium => "utm_medium",
            Self::UtmCampaign => "utm_campaign",
            Self::UtmContent => "utm_content",
            Self::UtmTerm => "utm_term",
            Self::EventName => "event_name",
        }
    }

    /// Whether this dimension needs the behavioral extension.
    ///
    /// Entry and exit pages are defined per session, so they need `sessionize`.
    pub const fn requires_behavioral(self) -> bool {
        matches!(self, Self::EntryPage | Self::ExitPage)
    }

    /// Parse the dimension name used in the URL path.
    pub fn from_slug(slug: &str) -> Option<Self> {
        Some(match slug {
            "pages" => Self::Page,
            "entry-pages" => Self::EntryPage,
            "exit-pages" => Self::ExitPage,
            "referrers" => Self::Referrer,
            "sources" => Self::ReferrerSource,
            "countries" => Self::CountryCode,
            "regions" => Self::Region,
            "cities" => Self::City,
            "browsers" => Self::Browser,
            "browser-versions" => Self::BrowserVersion,
            "os" => Self::Os,
            "os-versions" => Self::OsVersion,
            "devices" => Self::DeviceType,
            "screen-sizes" => Self::ScreenSize,
            "utm-sources" => Self::UtmSource,
            "utm-mediums" => Self::UtmMedium,
            "utm-campaigns" => Self::UtmCampaign,
            "utm-contents" => Self::UtmContent,
            "utm-terms" => Self::UtmTerm,
            "events" => Self::EventName,
            _ => return None,
        })
    }

    /// The URL slug for this dimension — the inverse of [`Dimension::from_slug`].
    pub const fn slug(self) -> &'static str {
        match self {
            Self::Page => "pages",
            Self::EntryPage => "entry-pages",
            Self::ExitPage => "exit-pages",
            Self::Referrer => "referrers",
            Self::ReferrerSource => "sources",
            Self::CountryCode => "countries",
            Self::Region => "regions",
            Self::City => "cities",
            Self::Browser => "browsers",
            Self::BrowserVersion => "browser-versions",
            Self::Os => "os",
            Self::OsVersion => "os-versions",
            Self::DeviceType => "devices",
            Self::ScreenSize => "screen-sizes",
            Self::UtmSource => "utm-sources",
            Self::UtmMedium => "utm-mediums",
            Self::UtmCampaign => "utm-campaigns",
            Self::UtmContent => "utm-contents",
            Self::UtmTerm => "utm-terms",
            Self::EventName => "events",
        }
    }

    /// Every dimension slug, for API discovery and documentation.
    pub const SLUGS: &'static [&'static str] = &[
        "pages",
        "entry-pages",
        "exit-pages",
        "referrers",
        "sources",
        "countries",
        "regions",
        "cities",
        "browsers",
        "browser-versions",
        "os",
        "os-versions",
        "devices",
        "screen-sizes",
        "utm-sources",
        "utm-mediums",
        "utm-campaigns",
        "utm-contents",
        "utm-terms",
        "events",
    ];
}

/// Query a breakdown of events by a dimension.
///
/// # Errors
///
/// Returns an error if the query fails — including when an entry/exit-page
/// breakdown is requested without the behavioral extension loaded.
pub fn query_breakdown(
    conn: &Connection,
    scope: &QueryScope,
    dimension: Dimension,
    limit: usize,
) -> Result<Vec<BreakdownRow>, duckdb::Error> {
    let sql = breakdown_sql(dimension, scope, limit);
    let mut stmt = conn.prepare(&sql)?;

    let rows = stmt
        .query_map(duckdb::params_from_iter(scope.params()), |row| {
            Ok(BreakdownRow {
                value: row.get(0)?,
                visitors: row.get(1)?,
                pageviews: row.get(2)?,
                events: row.get(3)?,
            })
        })?
        .filter_map(Result::ok)
        .collect();
    Ok(rows)
}

/// SQL for a breakdown. Split out so it can be unit-tested.
///
/// `limit` is interpolated rather than bound. It is a `usize` already capped by
/// the API layer, so it cannot carry SQL, and binding it would put an integer
/// parameter after a variable number of filter values — an ordering that is
/// easy to get wrong and hard to notice.
fn breakdown_sql(dimension: Dimension, scope: &QueryScope, limit: usize) -> String {
    let col = dimension.column_name();
    let where_clause = scope.where_clause();

    match dimension {
        Dimension::EntryPage | Dimension::ExitPage => {
            let pick = if dimension == Dimension::EntryPage {
                "MIN_BY(pathname, timestamp)"
            } else {
                "MAX_BY(pathname, timestamp)"
            };
            let window = if scope.session_window_is_safe() {
                scope.session_window.clone()
            } else {
                "30 minutes".to_string()
            };
            format!(
                "WITH scoped AS (
                     SELECT visitor_id, timestamp, pathname, event_name
                     FROM events_all
                     WHERE {where_clause} AND event_name = 'pageview'
                 ),
                 sessionized AS (
                     SELECT *, sessionize(timestamp, INTERVAL '{window}') OVER (
                         PARTITION BY visitor_id ORDER BY timestamp) AS session_id
                     FROM scoped
                 ),
                 per_session AS (
                     SELECT visitor_id, session_id, {pick} AS page, COUNT(*) AS views
                     FROM sessionized GROUP BY visitor_id, session_id
                 )
                 SELECT COALESCE(page, '(unknown)'),
                        COUNT(DISTINCT visitor_id),
                        COUNT(*),
                        COUNT(*)
                 FROM per_session
                 GROUP BY page
                 ORDER BY COUNT(DISTINCT visitor_id) DESC, page
                 LIMIT {limit}"
            )
        }
        _ => format!(
            "SELECT COALESCE({col}, '(unknown)') AS dim_value,
                    COUNT(DISTINCT visitor_id) AS visitors,
                    COUNT(*) FILTER (WHERE event_name = 'pageview') AS pageviews,
                    COUNT(*) AS events
             FROM events_all
             WHERE {where_clause}
             GROUP BY dim_value
             ORDER BY visitors DESC, dim_value
             LIMIT {limit}"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::test_support::{TestDb, insert_pageview, scope};

    #[test]
    fn test_breakdown_by_page() {
        let db = TestDb::new();
        insert_pageview(&db.conn, "v1", "2024-01-15 10:00:00", "/");
        insert_pageview(&db.conn, "v1", "2024-01-15 10:01:00", "/about");
        insert_pageview(&db.conn, "v2", "2024-01-15 10:02:00", "/");

        let rows = query_breakdown(
            &db.conn,
            &scope("2024-01-01", "2024-02-01"),
            Dimension::Page,
            10,
        )
        .unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].value, "/");
        assert_eq!(rows[0].visitors, 2);
        assert_eq!(rows[0].pageviews, 2);
        assert_eq!(rows[0].events, 2);
    }

    #[test]
    fn test_breakdown_by_browser() {
        let db = TestDb::new();
        db.insert_dimensional(
            "v1",
            "2024-01-15 10:00:00",
            "/",
            Some("Chrome"),
            None,
            None,
            None,
        );
        db.insert_dimensional(
            "v2",
            "2024-01-15 10:00:00",
            "/",
            Some("Firefox"),
            None,
            None,
            None,
        );
        db.insert_dimensional(
            "v3",
            "2024-01-15 10:00:00",
            "/",
            Some("Chrome"),
            None,
            None,
            None,
        );

        let rows = query_breakdown(
            &db.conn,
            &scope("2024-01-01", "2024-02-01"),
            Dimension::Browser,
            10,
        )
        .unwrap();
        assert_eq!(rows[0].value, "Chrome");
        assert_eq!(rows[0].visitors, 2);
    }

    #[test]
    fn test_breakdown_by_utm_source() {
        // utm_* columns were populated on every event but had no query path.
        let db = TestDb::new();
        db.insert_dimensional(
            "v1",
            "2024-01-15 10:00:00",
            "/",
            None,
            None,
            Some("newsletter"),
            None,
        );
        db.insert_dimensional(
            "v2",
            "2024-01-15 10:00:00",
            "/",
            None,
            None,
            Some("newsletter"),
            None,
        );
        db.insert_dimensional(
            "v3",
            "2024-01-15 10:00:00",
            "/",
            None,
            None,
            Some("twitter"),
            None,
        );

        let rows = query_breakdown(
            &db.conn,
            &scope("2024-01-01", "2024-02-01"),
            Dimension::UtmSource,
            10,
        )
        .unwrap();
        assert_eq!(rows[0].value, "newsletter");
        assert_eq!(rows[0].visitors, 2);
        assert_eq!(rows[1].value, "twitter");
    }

    #[test]
    fn test_breakdown_by_city() {
        let db = TestDb::new();
        db.insert_dimensional(
            "v1",
            "2024-01-15 10:00:00",
            "/",
            None,
            Some("US"),
            None,
            Some("Denver"),
        );
        let rows = query_breakdown(
            &db.conn,
            &scope("2024-01-01", "2024-02-01"),
            Dimension::City,
            10,
        )
        .unwrap();
        assert_eq!(rows[0].value, "Denver");
    }

    #[test]
    fn test_breakdown_by_event_name() {
        let db = TestDb::new();
        insert_pageview(&db.conn, "v1", "2024-01-15 10:00:00", "/");
        db.insert_event("v1", "2024-01-15 10:01:00", "signup", "/");
        db.insert_event("v2", "2024-01-15 10:02:00", "signup", "/");

        let rows = query_breakdown(
            &db.conn,
            &scope("2024-01-01", "2024-02-01"),
            Dimension::EventName,
            10,
        )
        .unwrap();
        let signup = rows.iter().find(|r| r.value == "signup").unwrap();
        assert_eq!(signup.visitors, 2);
        assert_eq!(signup.events, 2);
        assert_eq!(signup.pageviews, 0);
    }

    #[test]
    fn test_breakdown_limit() {
        let db = TestDb::new();
        for (i, v) in ["v1", "v2", "v3"].iter().enumerate() {
            insert_pageview(&db.conn, v, "2024-01-15 10:00:00", &format!("/p{i}"));
        }
        let rows = query_breakdown(
            &db.conn,
            &scope("2024-01-01", "2024-02-01"),
            Dimension::Page,
            2,
        )
        .unwrap();
        assert_eq!(rows.len(), 2);
    }

    #[test]
    fn test_breakdown_empty() {
        let db = TestDb::new();
        let rows = query_breakdown(
            &db.conn,
            &scope("2024-01-01", "2024-02-01"),
            Dimension::Page,
            10,
        )
        .unwrap();
        assert!(rows.is_empty());
    }

    #[test]
    fn test_breakdown_null_values_become_unknown() {
        let db = TestDb::new();
        insert_pageview(&db.conn, "v1", "2024-01-15 10:00:00", "/");
        let rows = query_breakdown(
            &db.conn,
            &scope("2024-01-01", "2024-02-01"),
            Dimension::Browser,
            10,
        )
        .unwrap();
        assert_eq!(rows[0].value, "(unknown)");
    }

    #[test]
    fn test_ordering_is_deterministic_on_ties() {
        // Equal visitor counts previously came back in arbitrary order, which
        // made the dashboard's top-N list flicker between refreshes.
        let db = TestDb::new();
        insert_pageview(&db.conn, "v1", "2024-01-15 10:00:00", "/b");
        insert_pageview(&db.conn, "v2", "2024-01-15 10:00:00", "/a");

        let first = query_breakdown(
            &db.conn,
            &scope("2024-01-01", "2024-02-01"),
            Dimension::Page,
            10,
        )
        .unwrap();
        assert_eq!(first[0].value, "/a", "ties break alphabetically");
        assert_eq!(first[1].value, "/b");
    }

    #[test]
    fn test_entry_and_exit_pages() {
        let db = TestDb::new();
        if !db.require_behavioral("entry/exit page breakdown") {
            return;
        }
        insert_pageview(&db.conn, "v1", "2024-01-15 10:00:00", "/landing");
        insert_pageview(&db.conn, "v1", "2024-01-15 10:05:00", "/middle");
        insert_pageview(&db.conn, "v1", "2024-01-15 10:10:00", "/checkout");

        let entry = query_breakdown(
            &db.conn,
            &scope("2024-01-01", "2024-02-01"),
            Dimension::EntryPage,
            10,
        )
        .unwrap();
        assert_eq!(entry[0].value, "/landing");

        let exit = query_breakdown(
            &db.conn,
            &scope("2024-01-01", "2024-02-01"),
            Dimension::ExitPage,
            10,
        )
        .unwrap();
        assert_eq!(exit[0].value, "/checkout");
    }

    #[test]
    fn test_every_slug_maps_to_a_dimension() {
        for slug in Dimension::SLUGS {
            assert!(
                Dimension::from_slug(slug).is_some(),
                "slug {slug} has no dimension"
            );
        }
        assert!(Dimension::from_slug("nonexistent").is_none());
    }

    #[test]
    fn test_breakdown_respects_a_filter() {
        let db = TestDb::new();
        db.insert_dimensional(
            "v1",
            "2024-01-15 10:00:00",
            "/a",
            Some("Chrome"),
            Some("DE"),
            None,
            None,
        );
        db.insert_dimensional(
            "v2",
            "2024-01-15 11:00:00",
            "/b",
            Some("Firefox"),
            Some("DE"),
            None,
            None,
        );
        db.insert_dimensional(
            "v3",
            "2024-01-15 12:00:00",
            "/c",
            Some("Chrome"),
            Some("US"),
            None,
            None,
        );

        let rows = query_breakdown(
            &db.conn,
            &crate::query::test_support::filtered_scope(
                "2024-01-01",
                "2024-02-01",
                "browsers==Chrome",
            ),
            Dimension::Page,
            10,
        )
        .unwrap();
        let pages: Vec<&str> = rows.iter().map(|r| r.value.as_str()).collect();
        assert_eq!(
            pages,
            vec!["/a", "/c"],
            "the Firefox visitor must be excluded"
        );
    }

    #[test]
    fn test_breakdown_negated_filter_keeps_rows_with_no_value() {
        // "not Chrome" plainly covers "no browser recorded"; plain SQL would
        // drop those rows through three-valued logic.
        let db = TestDb::new();
        db.insert_dimensional(
            "v1",
            "2024-01-15 10:00:00",
            "/a",
            Some("Chrome"),
            None,
            None,
            None,
        );
        db.insert_dimensional(
            "v2",
            "2024-01-15 11:00:00",
            "/b",
            Some("Firefox"),
            None,
            None,
            None,
        );
        db.insert_dimensional("v3", "2024-01-15 12:00:00", "/c", None, None, None, None);

        let rows = query_breakdown(
            &db.conn,
            &crate::query::test_support::filtered_scope(
                "2024-01-01",
                "2024-02-01",
                "browsers!=Chrome",
            ),
            Dimension::Page,
            10,
        )
        .unwrap();
        let pages: Vec<&str> = rows.iter().map(|r| r.value.as_str()).collect();
        assert_eq!(pages, vec!["/b", "/c"]);
    }

    #[test]
    fn test_breakdown_unknown_filter_selects_rows_with_no_value() {
        let db = TestDb::new();
        db.insert_dimensional(
            "v1",
            "2024-01-15 10:00:00",
            "/a",
            Some("Chrome"),
            None,
            None,
            None,
        );
        db.insert_dimensional("v2", "2024-01-15 11:00:00", "/b", None, None, None, None);

        let rows = query_breakdown(
            &db.conn,
            &crate::query::test_support::filtered_scope(
                "2024-01-01",
                "2024-02-01",
                "browsers==(unknown)",
            ),
            Dimension::Page,
            10,
        )
        .unwrap();
        let pages: Vec<&str> = rows.iter().map(|r| r.value.as_str()).collect();
        assert_eq!(pages, vec!["/b"]);
    }

    #[test]
    fn test_multiple_filters_are_combined_with_and() {
        let db = TestDb::new();
        db.insert_dimensional(
            "v1",
            "2024-01-15 10:00:00",
            "/a",
            Some("Chrome"),
            Some("DE"),
            None,
            None,
        );
        db.insert_dimensional(
            "v2",
            "2024-01-15 11:00:00",
            "/b",
            Some("Chrome"),
            Some("US"),
            None,
            None,
        );
        db.insert_dimensional(
            "v3",
            "2024-01-15 12:00:00",
            "/c",
            Some("Firefox"),
            Some("DE"),
            None,
            None,
        );

        let rows = query_breakdown(
            &db.conn,
            &crate::query::test_support::filtered_scope(
                "2024-01-01",
                "2024-02-01",
                "browsers==Chrome;countries==DE",
            ),
            Dimension::Page,
            10,
        )
        .unwrap();
        let pages: Vec<&str> = rows.iter().map(|r| r.value.as_str()).collect();
        assert_eq!(pages, vec!["/a"]);
    }

    #[test]
    fn test_filter_value_that_looks_like_sql_is_treated_as_data() {
        let db = TestDb::new();
        db.insert_dimensional(
            "v1",
            "2024-01-15 10:00:00",
            "/a",
            Some("Chrome"),
            None,
            None,
            None,
        );

        let rows = query_breakdown(
            &db.conn,
            &crate::query::test_support::filtered_scope(
                "2024-01-01",
                "2024-02-01",
                "browsers==' OR 1=1 --",
            ),
            Dimension::Page,
            10,
        )
        .unwrap();
        assert!(
            rows.is_empty(),
            "an injection attempt must simply match nothing"
        );

        // And the table is still there.
        let count: i64 = db
            .conn
            .query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn test_slug_round_trips_for_every_dimension() {
        for slug in Dimension::SLUGS {
            let dim = Dimension::from_slug(slug).expect("slug maps to a dimension");
            assert_eq!(dim.slug(), *slug, "slug() disagrees with from_slug()");
        }
    }

    #[test]
    fn test_behavioral_requirement_flags() {
        assert!(Dimension::EntryPage.requires_behavioral());
        assert!(Dimension::ExitPage.requires_behavioral());
        assert!(!Dimension::Page.requires_behavioral());
        assert!(!Dimension::UtmSource.requires_behavioral());
    }

    #[test]
    fn test_sql_uses_bound_parameters_for_user_input() {
        let sql = breakdown_sql(Dimension::Page, &scope("2024-01-01", "2024-02-01"), 10);
        assert_eq!(sql.matches('?').count(), 3, "site, start, end");
        assert!(!sql.contains("test.com"));
        assert!(
            sql.contains("LIMIT 10"),
            "the validated limit is interpolated"
        );
    }

    #[test]
    fn test_filtered_sql_binds_one_parameter_per_filter() {
        let scope = crate::query::test_support::filtered_scope(
            "2024-01-01",
            "2024-02-01",
            "browsers==Chrome;countries!=US",
        );
        let sql = breakdown_sql(Dimension::Page, &scope, 10);
        assert_eq!(
            sql.matches('?').count(),
            scope.params().len(),
            "every ? must have exactly one bound value"
        );
        assert!(
            !sql.contains("Chrome"),
            "filter values must not be interpolated"
        );
    }

    #[test]
    fn test_entry_page_sql_rejects_unsafe_session_window() {
        let bad = QueryScope::new("a.com", "s", "e", "1 minute; DROP TABLE events");
        let sql = breakdown_sql(Dimension::EntryPage, &bad, 10);
        assert!(sql.contains("INTERVAL '30 minutes'"));
        assert!(!sql.contains("DROP TABLE"));
    }
}
