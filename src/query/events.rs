use super::QueryScope;
use duckdb::Connection;
use serde::{Deserialize, Serialize};

/// A custom event (goal) with its conversion figures.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GoalConversion {
    /// The event name.
    pub name: String,
    /// Distinct visitors who triggered it.
    pub visitors: u64,
    /// Total occurrences.
    pub events: u64,
    /// `visitors / total_visitors_in_range`, 0.0–1.0.
    pub conversion_rate: f64,
}

/// One value of one custom property.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PropertyValue {
    pub value: String,
    pub visitors: u64,
    pub events: u64,
}

/// Rows kept per property breakdown.
const TOP_N: usize = 50;

/// `props` as JSON, or NULL when the stored text is not valid JSON.
///
/// Ingest validates props, but `json_keys` and `json_extract_string` raise on
/// malformed input, so one bad row in a restored or hand-edited store would
/// fail the report for the whole range. `CASE` is evaluated per row, unlike a
/// `WHERE json_valid(props)` that the planner may reorder after the call.
const PROPS_JSON: &str = "CASE WHEN json_valid(props) THEN props END";

/// Maximum length of a property key accepted from a request.
const MAX_PROP_KEY_LEN: usize = 128;

/// Validate a custom-property key.
///
/// Keys are interpolated into a `json_extract_string` path, so they are limited
/// to characters that cannot terminate the string literal or alter the path.
pub fn is_valid_property_key(key: &str) -> bool {
    !key.is_empty()
        && key.len() <= MAX_PROP_KEY_LEN
        && key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
}

/// Query conversions for every custom event in range.
///
/// Custom events have been accepted at ingest since the first release, but the
/// only way to see them was to notice that they were excluded from the pageview
/// count. There was no goal or conversion reporting at all.
///
/// # Errors
///
/// Returns an error if the query fails.
pub fn query_goals(
    conn: &Connection,
    scope: &QueryScope,
) -> Result<Vec<GoalConversion>, duckdb::Error> {
    let where_clause = scope.where_clause();
    let events = scope.events();
    let sql = format!(
        "WITH scoped AS (
             SELECT visitor_id, event_name FROM {events} WHERE {where_clause}
         ),
         total AS (SELECT COUNT(DISTINCT visitor_id) AS visitors FROM scoped)
         SELECT event_name,
                COUNT(DISTINCT visitor_id) AS visitors,
                COUNT(*) AS events,
                COALESCE(COUNT(DISTINCT visitor_id)::DOUBLE
                         / NULLIF((SELECT visitors FROM total), 0), 0.0) AS conversion_rate
         FROM scoped
         WHERE event_name <> 'pageview'
         GROUP BY event_name
         ORDER BY visitors DESC, event_name"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map(duckdb::params_from_iter(scope.params()), |row| {
            Ok(GoalConversion {
                name: row.get(0)?,
                visitors: row.get(1)?,
                events: row.get(2)?,
                conversion_rate: row.get(3)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// Distinct property keys seen in range, so a client can offer them for drill-down.
///
/// # Errors
///
/// Returns an error if the query fails.
pub fn query_property_keys(
    conn: &Connection,
    scope: &QueryScope,
) -> Result<Vec<String>, duckdb::Error> {
    let events = scope.events();
    let sql = format!(
        "SELECT DISTINCT UNNEST(json_keys({PROPS_JSON})) AS key
         FROM {events}
         WHERE {} AND props IS NOT NULL
         ORDER BY key
         LIMIT {TOP_N}",
        scope.where_clause()
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map(duckdb::params_from_iter(scope.params()), |row| {
            row.get::<_, String>(0)
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// Break a custom property down by value.
///
/// Optionally restricted to one event name.
///
/// # Errors
///
/// Returns an error if the key is invalid or the query fails.
pub fn query_property_values(
    conn: &Connection,
    scope: &QueryScope,
    key: &str,
    event_name: Option<&str>,
) -> Result<Vec<PropertyValue>, duckdb::Error> {
    if !is_valid_property_key(key) {
        return Ok(Vec::new());
    }

    let event_filter = if event_name.is_some() {
        " AND event_name = ?"
    } else {
        ""
    };
    // A key containing `.` is looked up as a flat key first ({"a.b": ..}) and
    // only then as a nested path ({"a": {"b": ..}}). The key is limited to
    // characters that cannot close the quotes around it.
    let extract = if key.contains('.') {
        format!(
            "COALESCE(json_extract_string({PROPS_JSON}, '$.\"{key}\"'),
                      json_extract_string({PROPS_JSON}, '$.{key}'))"
        )
    } else {
        format!("json_extract_string({PROPS_JSON}, '$.{key}')")
    };
    let events = scope.events();
    let sql = format!(
        "SELECT {extract} AS value,
                COUNT(DISTINCT visitor_id) AS visitors,
                COUNT(*) AS events
         FROM {events}
         WHERE {where_clause} AND props IS NOT NULL{event_filter}
         GROUP BY value
         HAVING value IS NOT NULL
         ORDER BY visitors DESC, value
         LIMIT {TOP_N}",
        where_clause = scope.where_clause()
    );

    let mut stmt = conn.prepare(&sql)?;
    let map_row = |row: &duckdb::Row<'_>| {
        Ok(PropertyValue {
            value: row.get(0)?,
            visitors: row.get(1)?,
            events: row.get(2)?,
        })
    };

    // The event name is the last placeholder, after the scope's own, which
    // include one per segment filter. It used to be bound after only site, start
    // and end, so a request that named an event *and* a segment failed with a
    // parameter-count error.
    let mut bound: Vec<&str> = scope.params();
    bound.extend(event_name);
    let rows: Vec<PropertyValue> = stmt
        .query_map(duckdb::params_from_iter(bound), map_row)?
        .collect::<Result<_, _>>()?;
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::test_support::{TestDb, insert_pageview, scope};

    #[test]
    fn test_goals_exclude_pageviews() {
        let db = TestDb::new();
        insert_pageview(&db.conn, "v1", "2024-01-15 10:00:00", "/");
        db.insert_event("v1", "2024-01-15 10:01:00", "signup", "/");
        db.insert_event("v2", "2024-01-15 10:02:00", "signup", "/");

        let goals = query_goals(&db.conn, &scope("2024-01-01", "2024-02-01")).unwrap();
        assert_eq!(goals.len(), 1);
        assert_eq!(goals[0].name, "signup");
        assert_eq!(goals[0].visitors, 2);
        assert_eq!(goals[0].events, 2);
    }

    #[test]
    fn test_goal_conversion_rate_is_against_all_visitors() {
        let db = TestDb::new();
        // Four visitors overall, one of whom converts.
        for v in ["v1", "v2", "v3", "v4"] {
            insert_pageview(&db.conn, v, "2024-01-15 10:00:00", "/");
        }
        db.insert_event("v1", "2024-01-15 10:01:00", "signup", "/");

        let goals = query_goals(&db.conn, &scope("2024-01-01", "2024-02-01")).unwrap();
        assert!((goals[0].conversion_rate - 0.25).abs() < f64::EPSILON);
    }

    #[test]
    fn test_repeat_conversions_count_once_per_visitor() {
        let db = TestDb::new();
        db.insert_event("v1", "2024-01-15 10:00:00", "signup", "/");
        db.insert_event("v1", "2024-01-15 10:05:00", "signup", "/");

        let goals = query_goals(&db.conn, &scope("2024-01-01", "2024-02-01")).unwrap();
        assert_eq!(goals[0].visitors, 1);
        assert_eq!(goals[0].events, 2);
    }

    #[test]
    fn test_no_goals_without_custom_events() {
        let db = TestDb::new();
        insert_pageview(&db.conn, "v1", "2024-01-15 10:00:00", "/");
        assert!(
            query_goals(&db.conn, &scope("2024-01-01", "2024-02-01"))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn test_property_keys_are_discovered() {
        let db = TestDb::new();
        db.insert_with_props(
            "v1",
            "2024-01-15 10:00:00",
            "signup",
            r#"{"plan":"pro","source":"ad"}"#,
        );
        db.insert_with_props("v2", "2024-01-15 10:01:00", "signup", r#"{"plan":"free"}"#);

        let keys = query_property_keys(&db.conn, &scope("2024-01-01", "2024-02-01")).unwrap();
        assert!(keys.contains(&"plan".to_string()));
        assert!(keys.contains(&"source".to_string()));
    }

    #[test]
    fn test_property_values_breakdown() {
        // props have been ingested since the first release with no way to read them.
        let db = TestDb::new();
        db.insert_with_props("v1", "2024-01-15 10:00:00", "signup", r#"{"plan":"pro"}"#);
        db.insert_with_props("v2", "2024-01-15 10:01:00", "signup", r#"{"plan":"pro"}"#);
        db.insert_with_props("v3", "2024-01-15 10:02:00", "signup", r#"{"plan":"free"}"#);

        let values =
            query_property_values(&db.conn, &scope("2024-01-01", "2024-02-01"), "plan", None)
                .unwrap();
        assert_eq!(values[0].value, "pro");
        assert_eq!(values[0].visitors, 2);
        assert_eq!(values[1].value, "free");
    }

    #[test]
    fn test_property_values_can_be_scoped_to_one_event() {
        let db = TestDb::new();
        db.insert_with_props("v1", "2024-01-15 10:00:00", "signup", r#"{"plan":"pro"}"#);
        db.insert_with_props(
            "v2",
            "2024-01-15 10:01:00",
            "purchase",
            r#"{"plan":"enterprise"}"#,
        );

        let values = query_property_values(
            &db.conn,
            &scope("2024-01-01", "2024-02-01"),
            "plan",
            Some("signup"),
        )
        .unwrap();
        assert_eq!(values.len(), 1);
        assert_eq!(values[0].value, "pro");
    }

    #[test]
    fn test_property_values_for_one_event_honour_the_segment() {
        // Regression: naming an event *and* a segment bound one parameter too
        // few ("Wrong number of parameters passed to query") because the event
        // name was bound right after the date range, ahead of the filter values.
        let db = TestDb::new();
        for (visitor, browser, plan) in [
            ("v1", "Chrome", "pro"),
            ("v2", "Firefox", "free"),
            ("v3", "Chrome", "pro"),
        ] {
            db.conn
                .execute(
                    "INSERT INTO events (site_id, visitor_id, timestamp, event_name, pathname, browser, props)
                     VALUES ('test.com', ?, CAST('2024-01-15 10:00:00' AS TIMESTAMP), 'signup', '/', ?, ?)",
                    duckdb::params![visitor, browser, format!(r#"{{"plan":"{plan}"}}"#)],
                )
                .unwrap();
        }
        let chrome = crate::query::test_support::filtered_scope(
            "2024-01-01",
            "2024-02-01",
            "browsers==Chrome",
        );
        let values = query_property_values(&db.conn, &chrome, "plan", Some("signup")).unwrap();
        assert_eq!(values.len(), 1, "{values:?}");
        assert_eq!(values[0].value, "pro");
        assert_eq!(values[0].visitors, 2);

        // And without an event name the segment still applies.
        let all = query_property_values(&db.conn, &chrome, "plan", None).unwrap();
        assert_eq!(all.len(), 1);
    }

    #[test]
    fn test_missing_property_key_yields_nothing() {
        let db = TestDb::new();
        db.insert_with_props("v1", "2024-01-15 10:00:00", "signup", r#"{"plan":"pro"}"#);
        let values =
            query_property_values(&db.conn, &scope("2024-01-01", "2024-02-01"), "absent", None)
                .unwrap();
        assert!(values.is_empty());
    }

    #[test]
    fn test_a_flat_key_containing_a_dot_can_be_queried() {
        // Regression (S-13): `.` was allowed in keys but became a nested JSON
        // path, so the flat key "a.b" that `/properties/keys` lists could never
        // be queried. A flat match now wins; the nested reading still works for
        // objects like {"user":{"tier":"gold"}}.
        let db = TestDb::new();
        db.insert_with_props("v1", "2024-01-15 10:00:00", "e", r#"{"a.b":"flat"}"#);
        db.insert_with_props("v2", "2024-01-15 10:01:00", "e", r#"{"a":{"b":"nested"}}"#);
        let s = scope("2024-01-01", "2024-02-01");

        let values = query_property_values(&db.conn, &s, "a.b", None).unwrap();
        let found: Vec<&str> = values.iter().map(|v| v.value.as_str()).collect();
        assert!(found.contains(&"flat"), "{found:?}");
        assert!(found.contains(&"nested"), "{found:?}");

        let keys = query_property_keys(&db.conn, &s).unwrap();
        assert!(keys.contains(&"a.b".to_string()), "{keys:?}");
    }

    #[test]
    fn test_one_malformed_props_value_does_not_fail_the_whole_report() {
        // Regression (S-13): json_keys / json_extract_string raise on invalid
        // JSON, so a single tampered row made both property endpoints 500 for
        // the entire range. Ingest validates props; this protects against a
        // store that was edited or restored from somewhere else.
        let db = TestDb::new();
        db.insert_with_props("v1", "2024-01-15 10:00:00", "e", r#"{"plan":"pro"}"#);
        db.insert_with_props("v2", "2024-01-15 10:01:00", "e", "this is not json");
        let s = scope("2024-01-01", "2024-02-01");

        let keys = query_property_keys(&db.conn, &s).unwrap();
        assert_eq!(keys, ["plan"]);
        let values = query_property_values(&db.conn, &s, "plan", None).unwrap();
        assert_eq!(values.len(), 1);
        assert_eq!(values[0].value, "pro");
    }

    #[test]
    fn test_property_key_validation() {
        assert!(is_valid_property_key("plan"));
        assert!(is_valid_property_key("user.tier"));
        assert!(is_valid_property_key("a-b_c1"));
        assert!(!is_valid_property_key(""));
        // The key is interpolated into a JSON path, so anything that could
        // escape the string literal is refused.
        assert!(!is_valid_property_key("plan'"));
        assert!(!is_valid_property_key("plan\""));
        assert!(!is_valid_property_key("$.plan"));
        assert!(!is_valid_property_key("a b"));
        assert!(!is_valid_property_key(&"a".repeat(MAX_PROP_KEY_LEN + 1)));
    }

    #[test]
    fn test_invalid_key_returns_no_rows_rather_than_erroring() {
        let db = TestDb::new();
        let values = query_property_values(
            &db.conn,
            &scope("2024-01-01", "2024-02-01"),
            "'; DROP TABLE events; --",
            None,
        )
        .unwrap();
        assert!(values.is_empty());
        // The table must still be there.
        let count: i64 = db
            .conn
            .query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 0);
    }
}
