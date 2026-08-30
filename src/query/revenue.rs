use super::QueryScope;
use duckdb::Connection;
use serde::{Deserialize, Serialize};

/// Revenue totals for one currency.
///
/// Amounts are never summed across currencies: adding 10 USD to 10 EUR
/// produces a number that means nothing, and this project has no exchange-rate
/// source. Each currency is reported on its own row.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RevenueByCurrency {
    /// ISO 4217 code.
    pub currency: String,
    /// Sum of `revenue_amount`.
    pub total: f64,
    /// Number of revenue-bearing events.
    pub transactions: u64,
    /// Distinct visitors who generated revenue.
    pub paying_visitors: u64,
    /// `total / transactions`.
    pub average_order_value: f64,
}

/// Revenue attributed to one dimension value (an event name or a page).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RevenueByDimension {
    pub value: String,
    pub currency: String,
    pub total: f64,
    pub transactions: u64,
}

/// A full revenue report.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RevenueReport {
    pub by_currency: Vec<RevenueByCurrency>,
    pub by_event: Vec<RevenueByDimension>,
    pub by_page: Vec<RevenueByDimension>,
}

/// Rows kept in each per-dimension list.
const TOP_N: usize = 20;

/// Query revenue for a scope.
///
/// The ingest path has always accepted `revenue_amount` and `revenue_currency`,
/// and the schema has always stored them, but nothing ever read them back:
/// there was no query, no endpoint, and no dashboard panel. This is that path.
///
/// # Errors
///
/// Returns an error if a query fails.
pub fn query_revenue(
    conn: &Connection,
    scope: &QueryScope,
) -> Result<RevenueReport, duckdb::Error> {
    Ok(RevenueReport {
        by_currency: query_by_currency(conn, scope)?,
        by_event: query_by_dimension(conn, scope, "event_name")?,
        by_page: query_by_dimension(conn, scope, "pathname")?,
    })
}

fn query_by_currency(
    conn: &Connection,
    scope: &QueryScope,
) -> Result<Vec<RevenueByCurrency>, duckdb::Error> {
    let sql = format!(
        "SELECT COALESCE(revenue_currency, '(unknown)') AS currency,
                SUM(revenue_amount)::DOUBLE AS total,
                COUNT(*) AS transactions,
                COUNT(DISTINCT visitor_id) AS paying_visitors
         FROM events_all
         WHERE {} AND revenue_amount IS NOT NULL
         GROUP BY currency
         ORDER BY total DESC, currency",
        scope.where_clause()
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map(duckdb::params_from_iter(scope.params()), |row| {
            let total: f64 = row.get(1)?;
            let transactions: u64 = row.get(2)?;
            #[allow(clippy::cast_precision_loss)]
            let average_order_value = if transactions > 0 {
                total / transactions as f64
            } else {
                0.0
            };
            Ok(RevenueByCurrency {
                currency: row.get(0)?,
                total,
                transactions,
                paying_visitors: row.get(3)?,
                average_order_value,
            })
        })?
        .filter_map(Result::ok)
        .collect();
    Ok(rows)
}

/// Revenue grouped by a dimension column and currency.
///
/// `column` is a fixed identifier chosen by the caller, never request input.
fn query_by_dimension(
    conn: &Connection,
    scope: &QueryScope,
    column: &str,
) -> Result<Vec<RevenueByDimension>, duckdb::Error> {
    let sql = format!(
        "SELECT COALESCE({column}, '(unknown)') AS value,
                COALESCE(revenue_currency, '(unknown)') AS currency,
                SUM(revenue_amount)::DOUBLE AS total,
                COUNT(*) AS transactions
         FROM events_all
         WHERE {} AND revenue_amount IS NOT NULL
         GROUP BY value, currency
         ORDER BY total DESC, value
         LIMIT {TOP_N}",
        scope.where_clause()
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map(duckdb::params_from_iter(scope.params()), |row| {
            Ok(RevenueByDimension {
                value: row.get(0)?,
                currency: row.get(1)?,
                total: row.get(2)?,
                transactions: row.get(3)?,
            })
        })?
        .filter_map(Result::ok)
        .collect();
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::test_support::{TestDb, scope};

    #[test]
    fn test_revenue_totals_per_currency() {
        let db = TestDb::new();
        db.insert_revenue("v1", "2024-01-15 10:00:00", "purchase", 100.00, "USD");
        db.insert_revenue("v2", "2024-01-15 11:00:00", "purchase", 50.00, "USD");
        db.insert_revenue("v3", "2024-01-15 12:00:00", "purchase", 30.00, "EUR");

        let report = query_revenue(&db.conn, &scope("2024-01-01", "2024-02-01")).unwrap();

        let usd = report
            .by_currency
            .iter()
            .find(|r| r.currency == "USD")
            .unwrap();
        assert!((usd.total - 150.0).abs() < 0.001);
        assert_eq!(usd.transactions, 2);
        assert_eq!(usd.paying_visitors, 2);
        assert!((usd.average_order_value - 75.0).abs() < 0.001);

        let eur = report
            .by_currency
            .iter()
            .find(|r| r.currency == "EUR")
            .unwrap();
        assert!((eur.total - 30.0).abs() < 0.001);
    }

    #[test]
    fn test_currencies_are_never_summed_together() {
        // Adding 100 USD to 30 EUR would produce a meaningless 130.
        let db = TestDb::new();
        db.insert_revenue("v1", "2024-01-15 10:00:00", "purchase", 100.00, "USD");
        db.insert_revenue("v2", "2024-01-15 11:00:00", "purchase", 30.00, "EUR");

        let report = query_revenue(&db.conn, &scope("2024-01-01", "2024-02-01")).unwrap();
        assert_eq!(report.by_currency.len(), 2);
        assert!(report.by_currency.iter().all(|r| r.total < 101.0));
    }

    #[test]
    fn test_revenue_by_event_and_page() {
        let db = TestDb::new();
        db.insert_revenue("v1", "2024-01-15 10:00:00", "purchase", 100.00, "USD");
        db.insert_revenue("v2", "2024-01-15 11:00:00", "upgrade", 20.00, "USD");

        let report = query_revenue(&db.conn, &scope("2024-01-01", "2024-02-01")).unwrap();
        assert_eq!(report.by_event[0].value, "purchase");
        assert!((report.by_event[0].total - 100.0).abs() < 0.001);
        assert_eq!(report.by_event[1].value, "upgrade");
        // insert_revenue writes every event to "/".
        assert_eq!(report.by_page[0].value, "/");
    }

    #[test]
    fn test_events_without_revenue_are_ignored() {
        let db = TestDb::new();
        crate::query::test_support::insert_pageview(&db.conn, "v1", "2024-01-15 10:00:00", "/");
        db.insert_revenue("v2", "2024-01-15 11:00:00", "purchase", 10.00, "USD");

        let report = query_revenue(&db.conn, &scope("2024-01-01", "2024-02-01")).unwrap();
        assert_eq!(report.by_currency.len(), 1);
        assert_eq!(report.by_currency[0].transactions, 1);
    }

    #[test]
    fn test_empty_revenue_report() {
        let db = TestDb::new();
        let report = query_revenue(&db.conn, &scope("2024-01-01", "2024-02-01")).unwrap();
        assert!(report.by_currency.is_empty());
        assert!(report.by_event.is_empty());
        assert!(report.by_page.is_empty());
    }

    #[test]
    fn test_repeat_purchases_by_one_visitor() {
        let db = TestDb::new();
        db.insert_revenue("v1", "2024-01-15 10:00:00", "purchase", 10.00, "USD");
        db.insert_revenue("v1", "2024-01-15 11:00:00", "purchase", 15.00, "USD");

        let report = query_revenue(&db.conn, &scope("2024-01-01", "2024-02-01")).unwrap();
        let usd = &report.by_currency[0];
        assert_eq!(usd.transactions, 2);
        assert_eq!(usd.paying_visitors, 1, "one person, two orders");
    }

    #[test]
    fn test_revenue_respects_the_date_range() {
        let db = TestDb::new();
        db.insert_revenue("v1", "2024-01-15 10:00:00", "purchase", 10.00, "USD");
        db.insert_revenue("v2", "2024-03-15 10:00:00", "purchase", 99.00, "USD");

        let report = query_revenue(&db.conn, &scope("2024-01-01", "2024-02-01")).unwrap();
        assert!((report.by_currency[0].total - 10.0).abs() < 0.001);
    }
}
