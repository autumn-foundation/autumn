# Echo: DX Audit Complaint & Fix

> **Resolved.** `AutumnError::internal_server_error` and
> `AutumnError::internal_server_error_msg` already exist
> (autumn/src/error.rs), matching `not_found`/`bad_request`/`unprocessable`.

## Experience (Walkthrough)

I was trying to write an endpoint that could return a 500 error manually if some condition failed.
I looked at the documentation for `AutumnError` and saw methods like `not_found`, `bad_request`, `unprocessable`, etc.
Naturally, I assumed there would be an `internal_server_error` or `internal` method to match.

So I wrote:
```rust
#[get("/error")]
async fn error_test() -> Result<&'static str, AutumnError> {
    Err(AutumnError::internal_server_error("Something went wrong"))
}
```

## Stumble (Friction Points)

The code failed to compile!

```
error[E0599]: no function or associated item named `internal_server_error` found for struct `autumn_web::AutumnError` in the current scope
```

I checked the rustdocs for `AutumnError`. There is literally no constructor for a 500 internal server error.
To create one, I have to do this incredibly verbose dance:

```rust
#[get("/error")]
async fn error_test() -> Result<&'static str, AutumnError> {
    let err: AutumnError = std::io::Error::other("boom").into();
    Err(err)
}
```
Or I have to use another constructor and override the status:
```rust
Err(AutumnError::bad_request_msg("boom").with_status(StatusCode::INTERNAL_SERVER_ERROR))
```

This is terrible DX. If you provide `bad_request_msg`, you should provide `internal_server_error_msg`. I shouldn't have to jump through hoops to construct the most common error code in web development.

## Fix

Add `internal_server_error` and `internal_server_error_msg` to `AutumnError` to match the other HTTP status code constructors.
