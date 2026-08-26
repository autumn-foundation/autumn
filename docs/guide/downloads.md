# File Downloads and Range Requests

Autumn ships a typed [`Download`] response so a handler can serve a file — owned
bytes, a byte stream, an [`AsyncRead`], or a stored blob — with the right
`Content-Type`, `Content-Disposition`, and `Content-Length` headers without
hand-rolling header strings. When you serve it through
[`into_response_ranged`][ranged] it also honours the request's `Range` header and
answers with `206 Partial Content`, so a browser `<video>` element can seek and a
resumed download can pick up where it left off.

`Download` is a plain `IntoResponse`, so returning it from a `#[get]` handler is a
single expression. The RFC 7233 range machinery lives in the reusable
[`autumn_web::range`] module and is driven for you by `into_response_ranged`.

## Why it matters

Serving a file "by hand" means building `Content-Disposition` strings (and
getting the quoting and non-ASCII `filename*` encoding right), inferring a MIME
type, streaming large objects without buffering them in memory, and — if you want
seekable media or resumable downloads — parsing `Range`/`If-Range` and emitting
`206`/`416`/`Content-Range`. `Download` does all of that, and keeps the bytes
flowing through your own authorized handler rather than a public presigned URL.

## Quick start

Serve owned bytes as a downloadable file:

```rust
use autumn_web::download::Download;
use autumn_web::prelude::*;

#[get("/export.csv")]
async fn export_csv() -> Download {
    let csv = b"id,name\n1,ada\n".to_vec();
    Download::from_bytes(csv).filename("export.csv")
}
```

Naming the file with `.filename("export.csv")` sets three things at once:

- `Content-Disposition: attachment; filename="export.csv"` — the browser offers a
  save dialog with that name.
- `Content-Type: text/csv; charset=utf-8` — inferred from the `.csv` extension
  (override it with [`content_type`]).
- `Content-Length` — taken from the byte length.

The filename is sanitized: control characters (including CR/LF) are stripped, a
path-like name is reduced to its basename (`.filename("../../etc/passwd")` emits
`filename="passwd"`), and a non-ASCII name additionally gets an RFC 5987
`filename*=UTF-8''…` parameter — so a caller-supplied name can never inject an
extra header directive.

## Constructing a download

Pick the constructor that matches where the bytes come from:

| Constructor | Source | Range-capable? |
|---|---|---|
| [`Download::from_bytes`] | owned, in-memory bytes (`Vec<u8>`, `Bytes`, …) | **yes** — a slice is served from memory |
| [`Download::from_stream`] | any `Stream<Item = Result<Bytes, io::Error>>` | no — an opaque stream cannot be re-seeked |
| [`Download::from_async_read`] | any [`AsyncRead`] (wrapped incrementally, not buffered) | no |
| [`Download::from_blob`] | a stored [`Blob`] (needs the `storage` feature) | **yes** — a ranged request fetches only the slice |

`from_stream` and `from_async_read` transfer the body incrementally with chunked
encoding and emit no `Content-Length` (the length is unknown). Because their
source cannot be re-seeked, they are always served in full and never advertise
`Accept-Ranges`, even through `into_response_ranged`.

## Builder options

Chain setters on the constructed download before returning it:

```rust
use autumn_web::download::Download;
use autumn_web::prelude::*;

# fn demo(bytes: Vec<u8>) -> Download {
Download::from_bytes(bytes)
    .filename("report.pdf")            // Content-Disposition + inferred type
    .content_type("application/pdf")   // override the inferred type
    .etag(ETag::strong("v42"))         // strong validator (see Range, below)
    .last_modified("Wed, 21 Oct 2015 07:28:00 GMT")
# }
```

- [`filename`] — sets the `Content-Disposition` filename (and MIME inference).
- [`content_type`] — sets `Content-Type` explicitly, overriding the type inferred
  from the filename extension or blob metadata.
- [`inline`] — serves the file inline (`Content-Disposition: inline`) instead of
  forcing a save dialog. Use it for media you want rendered in place.
- [`etag`] — attaches a strong `ETag`. It is emitted on the response and used as
  the `If-Range` validator for ranged requests.
- [`last_modified`] — attaches a `Last-Modified` HTTP-date, likewise emitted and
  usable as an `If-Range` validator (the value must already be a formatted
  HTTP-date, e.g. `Wed, 21 Oct 2015 07:28:00 GMT`).

The effective content type is resolved in order: explicit `.content_type()` →
inferred from the filename extension → blob-metadata default →
`application/octet-stream`.

## Serving a private stored file behind auth

Because `Download` is a plain `IntoResponse`, a policy-protected handler can
stream a stored blob as a download in one expression. `from_blob` reads only the
object's metadata up front and opens the byte stream lazily, so the full object
is never buffered in memory — it works for large files behind authorization
without issuing a public presigned URL:

```rust
use autumn_web::download::Download;
use autumn_web::storage::SharedBlobStore;
use autumn_web::{secured, AutumnError};

#[secured(policy = "reports.read")]
async fn download_report(
    store: SharedBlobStore,
    report_key: String,
) -> Result<Download, AutumnError> {
    Ok(Download::from_blob(&store, report_key).await?.filename("report.pdf"))
}
```

`from_blob` returns a [`BlobStoreError`] if the blob does not exist or its
metadata cannot be read.

## Range requests and `206 Partial Content`

The plain `IntoResponse` conversion cannot see the request, so it always serves
the full body. To honour a `Range` header, call
[`into_response_ranged(&req_headers)`][ranged] instead — an `async` method that
reads `Range` / `If-Range` from the request headers and can answer with `206`:

```rust
use autumn_web::download::Download;
use autumn_web::prelude::*;
use autumn_web::reexports::axum::response::Response;
use autumn_web::reexports::http::HeaderMap;

#[get("/reports/{id}/download")]
async fn download(id: Path<i64>, headers: HeaderMap) -> AutumnResult<Response> {
    let bytes = load_report_bytes(*id).await?;
    Ok(Download::from_bytes(bytes)
        .filename("report.pdf")
        .etag(ETag::strong(format!("report-{}", *id)))
        .into_response_ranged(&headers)
        .await)
}
# async fn load_report_bytes(_id: i64) -> AutumnResult<Vec<u8>> { Ok(Vec::new()) }
```

What `into_response_ranged` does for a **range-capable** body
(`from_bytes` / `from_blob`):

- **No `Range` header** (or an invalid/unparseable one) → the full `200` with
  `Accept-Ranges: bytes`. An invalid range is ignored, never rejected (RFC 7233
  §3.1).
- **A satisfiable range** (`bytes=0-1023`, `bytes=1024-`, or the suffix form
  `bytes=-1024`) → `206 Partial Content` with `Content-Range: bytes start-end/total`
  and just the requested slice. For a blob, only that slice is fetched from the
  store ([`BlobStore::get_range`]) — the whole object is never read for a seek.
- **An unsatisfiable range** (start past the end of the file) → `416 Range Not
  Satisfiable` with `Content-Range: bytes */total`.

For an **opaque stream** (`from_stream` / `from_async_read`) it always serves the
full `200` and does not advertise `Accept-Ranges`, because the source cannot be
re-seeked.

### `If-Range` and validators

When a client caches a partial response and later asks to continue, it sends the
range plus an `If-Range` header carrying the validator it saw. If that validator
no longer matches the current representation — the file changed underneath it —
serving the stale bytes as a `206` would corrupt the client's copy. So the range
is honoured only when the `If-Range` validator still matches; otherwise the whole
representation is served as a fresh `200`.

Attach a validator with `.etag(…)` and/or `.last_modified(…)`. Note that
`If-Range` requires a **strong** entity-tag (a weak `ETag` never matches), so use
[`ETag::strong`] for a download you expect to serve ranged. Without any validator,
an `If-Range` request simply falls back to the full `200`.

### Seekable media

A browser `<video>` element seeks by issuing `Range` requests, so returning a
media download through `into_response_ranged` is all it takes to make it
scrubbable — combine `.inline()` (render in place) with `.content_type("video/mp4")`:

```rust
use autumn_web::download::Download;
use autumn_web::storage::SharedBlobStore;
use autumn_web::{secured, AutumnError};
use http::HeaderMap;

#[secured(policy = "media.watch")]
async fn watch(
    store: SharedBlobStore,
    key: String,
    headers: HeaderMap,
) -> Result<axum::response::Response, AutumnError> {
    Ok(Download::from_blob(&store, key)
        .await?
        .content_type("video/mp4")
        .inline()
        .into_response_ranged(&headers)
        .await)
}
```

## Working example

The [`bookmarks`](../../examples/bookmarks) example serves
`GET /bookmarks/export.csv`: it builds an RFC 4180 CSV of every bookmark, wraps it
in `Download::from_bytes(...).filename("bookmarks.csv")`, attaches a strong
content-derived `ETag`, and returns it through `into_response_ranged(&headers)`.
A plain fetch downloads the whole file; a ranged fetch gets a `206`:

```bash
# Full download (200, Content-Disposition: attachment; filename="bookmarks.csv")
curl -OJ http://localhost:3000/bookmarks/export.csv

# A byte range (206 Partial Content, Content-Range: bytes 0-99/<total>)
curl -H 'Range: bytes=0-99' -i http://localhost:3000/bookmarks/export.csv
```

## The `range` module

The `Range` handling above is powered by [`autumn_web::range`], the reusable
RFC 7233 core. `into_response_ranged` drives it for you, but you can call it
directly to add ranged responses to a hand-built response:

- [`range::resolve(headers, total, validator)`][resolve] parses the request's
  `Range`/`If-Range` against a known `total` size and returns a
  [`RangeResolution`] — `Full`, `Partial { start, end, total }`, or
  `Unsatisfiable { total }`.
- [`Validator`] carries the current strong `ETag` and/or `Last-Modified` for the
  `If-Range` check (`Validator::new().with_etag(&tag).with_last_modified(lm)`).
- [`range::partial_bytes_response(&resolution, bytes)`][pbr] builds the matching
  `200`/`206`/`416` response over in-memory bytes.
- [`content_range_value(start, end, total)`][crv] and
  [`set_accept_ranges(&mut headers)`][sar] are the small header helpers.

Ranges are inclusive byte offsets, matching the `Content-Range` convention. A
multi-range request (`bytes=0-50,100-150`) is collapsed deterministically to the
first satisfiable sub-range and served as a single-range `206` — a
`multipart/byteranges` body is intentionally not produced.

[`Download`]: https://docs.rs/autumn-web/latest/autumn_web/download/struct.Download.html
[`Download::from_bytes`]: https://docs.rs/autumn-web/latest/autumn_web/download/struct.Download.html#method.from_bytes
[`Download::from_stream`]: https://docs.rs/autumn-web/latest/autumn_web/download/struct.Download.html#method.from_stream
[`Download::from_async_read`]: https://docs.rs/autumn-web/latest/autumn_web/download/struct.Download.html#method.from_async_read
[`Download::from_blob`]: https://docs.rs/autumn-web/latest/autumn_web/download/struct.Download.html#method.from_blob
[`filename`]: https://docs.rs/autumn-web/latest/autumn_web/download/struct.Download.html#method.filename
[`content_type`]: https://docs.rs/autumn-web/latest/autumn_web/download/struct.Download.html#method.content_type
[`inline`]: https://docs.rs/autumn-web/latest/autumn_web/download/struct.Download.html#method.inline
[`etag`]: https://docs.rs/autumn-web/latest/autumn_web/download/struct.Download.html#method.etag
[`last_modified`]: https://docs.rs/autumn-web/latest/autumn_web/download/struct.Download.html#method.last_modified
[ranged]: https://docs.rs/autumn-web/latest/autumn_web/download/struct.Download.html#method.into_response_ranged
[`autumn_web::range`]: https://docs.rs/autumn-web/latest/autumn_web/range/index.html
[resolve]: https://docs.rs/autumn-web/latest/autumn_web/range/fn.resolve.html
[`RangeResolution`]: https://docs.rs/autumn-web/latest/autumn_web/range/enum.RangeResolution.html
[`Validator`]: https://docs.rs/autumn-web/latest/autumn_web/range/struct.Validator.html
[pbr]: https://docs.rs/autumn-web/latest/autumn_web/range/fn.partial_bytes_response.html
[crv]: https://docs.rs/autumn-web/latest/autumn_web/range/fn.content_range_value.html
[sar]: https://docs.rs/autumn-web/latest/autumn_web/range/fn.set_accept_ranges.html
[`ETag::strong`]: https://docs.rs/autumn-web/latest/autumn_web/etag/struct.ETag.html#method.strong
[`BlobStore::get_range`]: https://docs.rs/autumn-web/latest/autumn_web/storage/trait.BlobStore.html#method.get_range
[`BlobStoreError`]: https://docs.rs/autumn-web/latest/autumn_web/storage/enum.BlobStoreError.html
[`Blob`]: https://docs.rs/autumn-web/latest/autumn_web/storage/struct.Blob.html
[`AsyncRead`]: https://docs.rs/tokio/latest/tokio/io/trait.AsyncRead.html
