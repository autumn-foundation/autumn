# API Versioning, Deprecations, and Sunset Lifecycles

Autumn provides first-class support for managing API version lifecycles. By defining version lifecycles and tagging routes, Autumn handles RFC 9745 `Deprecation` and `Sunset` headers, automatic `410 Gone` responses, OpenAPI spec grouping, and CI route auditing out-of-the-box.

---

## 1. Registering API Versions

Define your API versions and their lifecycles when initializing the application on `AppBuilder`. Each version specifies when it becomes deprecated and when it is sunsetted.

```rust
use autumn_web::app::ApiVersion;
use chrono::TimeZone;

#[autumn_web::main]
async fn main() -> Result<(), autumn_web::Error> {
    let app = autumn_web::app()
        // Register API v1: Deprecated on 2026-06-01, Sunset on 2026-12-01
        .api_version(ApiVersion {
            version: "v1".to_string(),
            deprecated_at: Some(chrono::Utc.with_ymd_and_hms(2026, 6, 1, 0, 0, 0).unwrap()),
            sunset_at: Some(chrono::Utc.with_ymd_and_hms(2026, 12, 1, 0, 0, 0).unwrap()),
        })
        // Register API v2: Active (no deprecation/sunset schedule yet)
        .api_version(ApiVersion {
            version: "v2".to_string(),
            deprecated_at: None,
            sunset_at: None,
        });

    app.run().await
}
```

---

## 2. Tagging Routes

Tag route handlers with their corresponding API version using the `api_version` macro parameter. 

```rust
use autumn_web::{get, post};

// Route belonging to v1 (currently deprecated)
#[get("/v1/users", api_version = "v1")]
async fn list_users_v1() -> &'static str {
    "v1 users list"
}

// Route belonging to v2
#[get("/v2/users", api_version = "v2")]
async fn list_users_v2() -> &'static str {
    "v2 users list"
}
```

### Sunset Opt-Out

If a specific route must remain active even after its API version has passed its sunset date, you can opt it out of the automatic `410 Gone` behavior using the `sunset_opt_out` flag:

```rust
// Remains active after v1 sunset, but continues to emit deprecation/sunset headers
#[get("/v1/legacy-callback", api_version = "v1", sunset_opt_out = true)]
async fn legacy_callback() -> &'static str {
    "legacy callback payload"
}
```

> [!WARNING]
> Unregistered version tags (e.g. `#[get("/foo", api_version = "v99")]` when `"v99"` was never registered on `AppBuilder`) will fail safety validation at startup, causing the application to refuse to boot.

---

## 3. Headers and Lifecycle Middleware

Once configured, Autumn automatically injects RFC 9745 headers and routes traffic depending on the current date:

| Route State | HTTP Status | Response Headers |
|:---|:---|:---|
| **Active** (Not Deprecated) | `200 OK` (normal) | *(None)* |
| **Deprecated** (Past `deprecated_at` date) | `200 OK` (normal) | `Deprecation: @<timestamp>`<br>`Sunset: <sunset_date_gmt>` |
| **Sunset** (Past `sunset_at` date) | `410 Gone` | `Deprecation: @<timestamp>`<br>`Sunset: <sunset_date_gmt>` |
| **Sunset with Opt-Out** (Past `sunset_at`, `sunset_opt_out = true`) | `200 OK` (normal) | `Deprecation: @<timestamp>`<br>`Sunset: <sunset_date_gmt>` |

### RFC 7807 Problem Details Response

When a route is past its sunset date and has not opted out, Autumn automatically aborts the request and returns an RFC 7807 Problem Details document with a `410 Gone` status code:

```json
{
  "type": "https://autumn.dev/problems/gone",
  "title": "Gone",
  "status": 410,
  "detail": "API version 'v1' has been sunsetted."
}
```

---

## 4. Routes CLI Listing

You can inspect the version and status of all mounted routes in your application using `autumn routes`:

```bash
autumn routes
```

This prints a tabular view showing the HTTP method, path, handler, version, status (`active`, `deprecated`, or `sunset`), and source:

```text
Method  Path        Handler         Version  Status      Source  Middleware
---------------------------------------------------------------------------
GET     /v1/users   list_users_v1   v1       deprecated  user    
GET     /v2/users   list_users_v2   v2       active      user    
```

---

## 5. Audit CLI: `autumn check deprecations`

To prevent active, sunsetted routes from remaining deployed in production, use the CI-friendly `autumn check deprecations` command:

```bash
autumn check deprecations
```

This command builds your project, queries the route table, and audits all routes.
- If **any** route is past its sunset date (including opted-out routes), the command logs the offending routes and exits with a non-zero code (**1**), failing the build.
- If no past-sunset routes are found, it exits with code **0**.

```text
✗ Found 1 active past-sunset route(s) (opted out):
  GET /v1/users (handler: list_users_v1, version: v1)
```

---

## 6. OpenAPI Integration

When using the `openapi` feature, Autumn automatically syncs route version metadata with your generated OpenAPI documentation (see the [OpenAPI guide](openapi.md) for the rest of what the spec derives from your handlers):

1. **Tag Grouping**: Operations are automatically tagged with their API version name (e.g. `v1`, `v2`), making them easy to group and navigate.
2. **Deprecation Gating**: Any operation whose API version is past its `deprecated_at` date is marked as `"deprecated": true` in the spec.
