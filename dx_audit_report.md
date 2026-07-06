# DX Audit Report 🗣️

## DX Audit Report: Missing Path 404 Behavior

### 1. 🔍 EXPERIENCE - The Walkthrough
- Built a simple test application using `autumn new dx_my_app`.
- Configured basic routes and ran the server via `cargo run`.
- Hit an undefined path (`/missing`) using `curl` with various Accept headers.

### 2. 🚧 STUMBLE - The Friction Points
- **Error Check**: Requesting `/missing` with an `Accept: text/html` header causes the server to panic or crash unexpectedly, resulting in an empty response (or a Connection Refused error from curl) instead of returning an HTML error page.
- On the other hand, requesting the same route with `Accept: application/json` correctly returns a JSON payload detailing the 404 error.

### 3. 📢 REPORT - The Complaint
- "Why does asking for an HTML page on a missing route completely crash the connection instead of giving me a friendly 404 page? I shouldn't be able to break the server just by asking for an unknown URL."

### 4. 🧪 VERIFY - The "idiot proofing"
- Verified using `curl -H "Accept: application/json"` that a proper JSON 404 response is returned.
- Verified using `curl -v -H "Accept: text/html"` that the connection is prematurely closed or refused, confirming a likely panic when formatting the 404 HTML fallback.


## DX Audit Report: `autumn dev` Hot Reloading

### 1. 🔍 EXPERIENCE - The Walkthrough
- Following the Quickstart guide in `README.md`, set up a project and ran `autumn dev`.

### 2. 🚧 STUMBLE - The Friction Points
- I created a file `src/views/index.html` and modified it while `autumn dev` was running. However, the server did not detect the change and trigger a rebuild/reload. I expected it to pick up changes in `src/` or common template directories.

### 3. 📢 REPORT - The Complaint
- The `autumn dev` command currently only watches specific directories for changes: `src`, `static`, `templates`, and `migrations`.
- If a developer decides to put their HTML templates in a different directory (e.g., `views`, which is a common convention in web frameworks), `autumn dev` will silently ignore changes to those files. This leads to a frustrating developer experience where the browser doesn't reflect the latest changes.

### 4. 🧪 VERIFY - The "idiot proofing"
- The watcher should ideally watch the entire project directory (excluding `target/`, `.git/`, etc.) rather than a hardcoded list of directories. Alternatively, the documentation must explicitly state *which* directories are watched.


## DX Audit Report: `routes![]` Macro Errors

### 1. 🔍 EXPERIENCE - The Walkthrough
- Attempted to add a new route handler in `routes![]` but misspelled the name.
- Expected a simple 'cannot find function' error for the name I typed.

### 2. 🚧 STUMBLE - The Friction Points
- Got two errors: one for my typo, and a second confusing one: `cannot find function __autumn_route_info_missing_route in this scope`.
- The second error exposes internal macro generation details that I shouldn't have to care about.

### 3. 📢 REPORT - The Complaint
- If I make a typo, just tell me I made a typo. Don't yell at me about `__autumn_route_info_...` which isn't even in my code.

### 4. 🧪 VERIFY - The "idiot proofing"
- Modifying the macro span does NOT remove the second error because rustc will eagerly resolve both. A dummy binding ensures the original user identifier error is surfaced so that developers have clear guidance on what went wrong. We must accept the second macro-level error as unavoidable cost for ergonomic macros.


## DX Audit Report: Handler Trait Bounds

### 1. 🔍 EXPERIENCE - The Walkthrough
- Did the "README Run": Copied the exact example code from `README.md` into a new project's `main.rs`.
- Tested writing a simpler custom route: `async fn foo() -> i32 { 42 }`.

### 2. 🚧 STUMBLE - The Friction Points
- **Error Check**: The `foo` route handler returning `i32` completely fails to compile with a massive, unintelligible error: `the trait bound fn() -> ... {foo}: Handler<_, _> is not satisfied`. The error output references internal Axum routing boundaries.
- **Slang Check**: "Handler trait bound not satisfied" is deep Rust/Axum jargon that breaks the illusion of a simple web framework.

### 3. 📢 REPORT - The Complaint
- "Why can I return a String but not an integer? If I return `42`, the compiler dumps 20 lines of trait bound errors about Axum internals. Simple is better than powerful, and right now simple numbers crash the compiler!"

### 4. 🧪 VERIFY - The "idiot proofing"
- Confirmed that Axum's `IntoResponse` trait is not implemented for `i32`, `i64`, or other plain numbers out-of-the-box, meaning they cannot be returned directly from route handlers without manually converting them to strings or JSON first.
