# Errors and diagnostics

Keep the HTTP status, response body, and **X-LocalSky-Request-Id** when an API request fails. API 2.4.0 also adds request context in `request: {id, method, route}`.

A status describes the request outcome. The diagnostic code and evidence narrow down the failed operation.

## Interpret the response

| Result | What to do |
|---|---|
| 401 from LocalSky | Check the LocalSky token or session |
| 403 | Check access policy and browser origin |
| 400 or 422 | Correct the request or configuration using the returned details |
| 429 | Respect throttling and any supplied retry guidance |
| 5xx | Preserve the diagnostic and check the named component |
| Network timeout | Determine whether the operation was accepted before retrying a write |

Controller authentication failures can use 424 with `controller_auth_failed`. This distinguishes a downstream credential problem from LocalSky rejecting its own API token.

A 200 response can still contain null readings or an incomplete forecast window. Validate the data as well as the status.

## Diagnostic records

Where available, `diagnostic` contains `at_epoch` and `failure`. A failure includes:

- stable `code`
- `operation`, `message`, and `next_step`
- evidence such as HTTP status, response format, entity ID, OS/SQLite code, field, or timeout
- nested `causes` for separate failed attempts

Absent evidence is omitted. Cause lists are bounded; `omitted_causes` reports truncation.

The diagnostic deliberately excludes credentials and raw upstream response bodies. An upstream HTTP 500 does not reveal a server exception unless that server supplies usable evidence elsewhere.

## Source failures

Authenticated health responses can include `sources[].error`. A successful poll clears the current source error, but does not refresh an old measurement's observation time.

For HA passthrough, a bulk `/api/states` HTTP 500 triggers bounded individual reads of mapped entities. Recovery can restore readings while the bulk endpoint continues failing. Check the HA or proxy log at the same timestamp to identify the underlying exception.

## Health and support bundle

- **GET /api/v1/health:** liveness, with full detail for privileged callers.
- **GET /api/v1/health?strict=1:** 503 when health is not OK.
- **GET /api/v1/diagnostics:** privileged diagnostic bundle.
- **GET /metrics:** operational Prometheus metrics.

Review a support bundle before sharing it. Configuration secrets are scrubbed, but location, names, and operational history can still be personal.

[Error code catalog](source-errors.md) · [Troubleshooting](troubleshooting.md) · [API reference](api.md)
