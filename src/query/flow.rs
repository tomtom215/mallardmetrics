use super::QueryScope;
use duckdb::Connection;
use serde::{Deserialize, Serialize};

/// The next page a visitor reached, with a count.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FlowNode {
    pub next_page: String,
    pub visitors: u64,
    /// `visitors / total_visitors_who_reached_the_target_page`, 0.0–1.0.
    pub share: f64,
}

/// Default number of destinations returned.
pub const DEFAULT_LIMIT: usize = 10;
/// Hard cap on destinations returned.
pub const MAX_LIMIT: usize = 100;

/// Which way to walk from the target page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// Where visitors went next.
    Forward,
    /// Where visitors came from.
    Backward,
}

impl Direction {
    pub fn from_slug(slug: &str) -> Option<Self> {
        match slug {
            "forward" => Some(Self::Forward),
            "backward" => Some(Self::Backward),
            _ => None,
        }
    }

    const fn as_arg(self) -> &'static str {
        match self {
            Self::Forward => "forward",
            Self::Backward => "backward",
        }
    }
}

/// Query the pages visitors reach immediately before or after `target_page`.
///
/// Uses `sequence_next_node` from the behavioral extension. The limit is a
/// parameter rather than the hard-coded 10 of the previous release.
///
/// # Errors
///
/// Returns an error if the query fails, e.g. when the behavioral extension is
/// not loaded.
pub fn query_flow(
    conn: &Connection,
    scope: &QueryScope,
    target_page: &str,
    direction: Direction,
    limit: usize,
) -> Result<Vec<FlowNode>, duckdb::Error> {
    let limit = limit.clamp(1, MAX_LIMIT);
    // `sequence_next_node`'s condition arguments cannot be parameterised, so the
    // page is interpolated — with its quotes escaped first.
    let escaped_page = target_page.replace('\'', "''");
    let dir = direction.as_arg();

    let sql = format!(
        "WITH per_visitor AS (
             SELECT visitor_id,
                 sequence_next_node('{dir}', 'first_match', timestamp, pathname,
                     TRUE, pathname = '{escaped_page}'
                 ) AS next_page
             FROM events_all
             WHERE {where_clause}
             GROUP BY visitor_id
         ),
         reached AS (
             SELECT COUNT(DISTINCT visitor_id) AS total
             FROM events_all
             WHERE {where_clause} AND pathname = '{escaped_page}'
         )
         SELECT next_page,
                COUNT(*) AS visitors,
                COALESCE(COUNT(*)::DOUBLE / NULLIF((SELECT total FROM reached), 0), 0.0) AS share
         FROM per_visitor
         WHERE next_page IS NOT NULL
         GROUP BY next_page
         ORDER BY visitors DESC, next_page
         LIMIT {limit}",
        where_clause = scope.where_clause()
    );

    let mut stmt = conn.prepare(&sql)?;
    // The scope parameters appear twice: once for each WHERE clause.
    let bound: Vec<&str> = scope.params().into_iter().chain(scope.params()).collect();
    let rows = stmt
        .query_map(duckdb::params_from_iter(bound), |row| {
            Ok(FlowNode {
                next_page: row.get(0)?,
                visitors: row.get(1)?,
                share: row.get(2)?,
            })
        })?
        .filter_map(Result::ok)
        .collect();
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::test_support::{TestDb, insert_pageview, scope};

    fn seed(db: &TestDb) {
        // Two visitors go /pricing -> /signup, one goes /pricing -> /docs.
        insert_pageview(&db.conn, "v1", "2024-01-15 10:00:00", "/pricing");
        insert_pageview(&db.conn, "v1", "2024-01-15 10:01:00", "/signup");
        insert_pageview(&db.conn, "v2", "2024-01-15 11:00:00", "/pricing");
        insert_pageview(&db.conn, "v2", "2024-01-15 11:01:00", "/signup");
        insert_pageview(&db.conn, "v3", "2024-01-15 12:00:00", "/pricing");
        insert_pageview(&db.conn, "v3", "2024-01-15 12:01:00", "/docs");
    }

    #[test]
    fn test_forward_flow_ranks_destinations() {
        let db = TestDb::new();
        if !db.require_behavioral("flow analysis") {
            return;
        }
        seed(&db);
        let nodes = query_flow(
            &db.conn,
            &scope("2024-01-01", "2024-02-01"),
            "/pricing",
            Direction::Forward,
            10,
        )
        .unwrap();
        assert_eq!(nodes[0].next_page, "/signup");
        assert_eq!(nodes[0].visitors, 2);
        assert_eq!(nodes[1].next_page, "/docs");
        assert_eq!(nodes[1].visitors, 1);
    }

    #[test]
    fn test_flow_share_is_relative_to_visitors_who_reached_the_page() {
        let db = TestDb::new();
        if !db.require_behavioral("flow analysis") {
            return;
        }
        seed(&db);
        let nodes = query_flow(
            &db.conn,
            &scope("2024-01-01", "2024-02-01"),
            "/pricing",
            Direction::Forward,
            10,
        )
        .unwrap();
        // Three visitors reached /pricing; two continued to /signup.
        assert!((nodes[0].share - 2.0 / 3.0).abs() < 1e-9);
    }

    #[test]
    fn test_backward_flow_finds_the_previous_page() {
        let db = TestDb::new();
        if !db.require_behavioral("flow analysis") {
            return;
        }
        seed(&db);
        let nodes = query_flow(
            &db.conn,
            &scope("2024-01-01", "2024-02-01"),
            "/signup",
            Direction::Backward,
            10,
        )
        .unwrap();
        assert_eq!(nodes[0].next_page, "/pricing");
        assert_eq!(nodes[0].visitors, 2);
    }

    #[test]
    fn test_flow_limit_is_applied_and_clamped() {
        let db = TestDb::new();
        if !db.require_behavioral("flow analysis") {
            return;
        }
        seed(&db);
        let s = scope("2024-01-01", "2024-02-01");
        assert_eq!(
            query_flow(&db.conn, &s, "/pricing", Direction::Forward, 1)
                .unwrap()
                .len(),
            1
        );
        // A zero or huge limit must not produce an invalid query.
        assert!(query_flow(&db.conn, &s, "/pricing", Direction::Forward, 0).is_ok());
        assert!(query_flow(&db.conn, &s, "/pricing", Direction::Forward, 10_000).is_ok());
    }

    #[test]
    fn test_flow_on_an_unvisited_page_is_empty() {
        let db = TestDb::new();
        if !db.require_behavioral("flow analysis") {
            return;
        }
        seed(&db);
        let nodes = query_flow(
            &db.conn,
            &scope("2024-01-01", "2024-02-01"),
            "/nowhere",
            Direction::Forward,
            10,
        )
        .unwrap();
        assert!(nodes.is_empty());
    }

    #[test]
    fn test_flow_escapes_quotes_in_the_page() {
        let db = TestDb::new();
        if !db.require_behavioral("flow analysis") {
            return;
        }
        // Must not produce a syntax error from an unbalanced quote.
        let nodes = query_flow(
            &db.conn,
            &scope("2024-01-01", "2024-02-01"),
            "/it's-a-page",
            Direction::Forward,
            10,
        )
        .unwrap();
        assert!(nodes.is_empty());
    }

    #[test]
    fn test_direction_slugs() {
        assert_eq!(Direction::from_slug("forward"), Some(Direction::Forward));
        assert_eq!(Direction::from_slug("backward"), Some(Direction::Backward));
        assert_eq!(Direction::from_slug("sideways"), None);
    }
}
