# Security Policy

## Reporting Vulnerabilities

If you discover a security vulnerability in Mallard Metrics, please report it responsibly by opening a private security advisory on GitHub.

Do NOT open a public issue for security vulnerabilities.

## Security Model

### Privacy

- **No cookies**: Visitor identification uses a daily-rotating hash (HMAC-SHA256) of IP + User-Agent + daily salt.
- **No PII storage**: IP addresses are used only for hashing, then immediately discarded. They are never written to disk or logged.
- **Daily salt rotation**: Visitor IDs change daily, preventing long-term tracking.
- **GDPR/CCPA compliant by design**: No personal data is stored. No consent required.

### Data Protection

- **Parquet files**: Event data stored in Parquet format with ZSTD compression. Files are organized by site and date for partition pruning.
- **No external network calls**: DuckDB is embedded. No data leaves the server except via the dashboard API.

### Input Validation

- **SQL injection prevention**: All DuckDB queries use parameterized statements. User input is never interpolated into SQL strings.
- **XSS prevention**: All user-provided data (page names, referrers, UTM parameters) is sanitized before storage. Control characters are stripped, strings are truncated to maximum lengths.
- **Input length limits**: Domain (256 chars), event name (256 chars), URL (2048 chars), referrer (2048 chars), custom properties (4096 chars).
- **CSRF prevention**: Ingestion endpoint validates request origin. Dashboard API will use authentication tokens (Phase 4).

### Rate Limiting

Rate limiting on the ingestion endpoint will be implemented in Phase 4. Currently, the application relies on reverse proxy rate limiting (nginx, Caddy, etc.).

### Authentication

Authentication (username/password with bcrypt/argon2) and API key management will be implemented in Phase 4. For Phase 1, the dashboard is open access.

## Threat Model

| Threat | Mitigation |
|---|---|
| SQL injection | Parameterized queries for all user input |
| XSS | Input sanitization, control character removal |
| Data exfiltration | No external network calls, embedded DB |
| PII leakage | IP addresses never stored, daily hash rotation |
| Brute force | Rate limiting (Phase 4), reverse proxy |
| Unauthorized access | Authentication (Phase 4) |
| Data tampering | Parquet files are append-only per partition |
