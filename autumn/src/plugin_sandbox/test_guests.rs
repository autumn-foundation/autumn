//! Hand-written WAT guest modules used to prove what the sandbox denies.
//!
//! Every guest here is a **complete** `wasm32-wasip1`-shaped module written by
//! hand, not compiled from Rust. That is deliberate: the escape corpus is the
//! evidence that the sandbox holds, and evidence that only runs on a CI runner
//! with a wasm toolchain installed is evidence that mostly does not run. These
//! compile with the pure-Rust `wat` crate at test time, on any host.
//!
//! Each constant is one module that misbehaves in exactly one way, so a failing
//! test names the specific containment that broke.
//!
//! Exposed (hidden from the docs) under `test-support` so the consolidated
//! integration suite can share the corpus with this crate's unit tests instead
//! of keeping a second, drifting copy.

/// A well-behaved plugin: it reads the request frame, dispatches on its
/// content, and answers. Everything else in this corpus is a variation on it
/// that misbehaves in exactly one way.
pub const HELLO: &str = r#"(module
  (import "wasi_snapshot_preview1" "fd_read" (func $fd_read (param i32 i32 i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "fd_write" (func $fd_write (param i32 i32 i32 i32) (result i32)))
  (memory (export "memory") 2 4)
  (data (i32.const 128) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"aGVsbG8gZnJvbSB0aGUgc2FuZGJveA==\"}\0a\00")
  (data (i32.const 512) "{\"op\":\"response\",\"status\":404,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"bm8gc3VjaCBzYW5kYm94ZWQgcm91dGU=\"}\0a\00")
  (data (i32.const 768) "{\"op\":\"response\",\"status\":405,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"bWV0aG9kIG5vdCBhbGxvd2Vk\"}\0a\00")
  (data (i32.const 1024) "\"method\":\"GET\"\00")
  (data (i32.const 1088) "/greet\00")
  (data (i32.const 1120) "/etc/passwd\00")
  (data (i32.const 1152) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"],[\"set-cookie\",\"session=forged\"]],\"body_b64\":\"Y29va2llIGF0dGVtcHQ=\"}\0a\00")
  (data (i32.const 1600) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"],[\"x-evil\",\"a\\r\\nset-cookie: forged=1\"]],\"body_b64\":\"c3BsaXR0aW5nIGF0dGVtcHQ=\"}\0a\00")
  (data (i32.const 2048) "{\"op\":\"response\",\"status\":200,\0a\00")
  (data (i32.const 2176) "{\"op\":\"exec\",\"cmd\":\"/bin/sh\"}\0a\00")
  (data (i32.const 2304) "{\"op\":\"response\",\"status\":999,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"aW1wb3NzaWJsZSBzdGF0dXM=\"}\0a\00")
  (data (i32.const 2816) "X\00")
  (data (i32.const 3072) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"c2Vjb25kIGFuc3dlcg==\"}\0a\00")
  (data (i32.const 3584) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"],[\"x-autumn-sandboxed\",\"not-this-plugin\"],[\"x-content-type-options\",\"off\"]],\"body_b64\":\"YXR0cmlidXRpb24gYXR0ZW1wdA==\"}\0a\00")
  (data (i32.const 4200) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"aGVsbG8gZnJvbSB0aGUgc2FuZGJveA==\"}\00")
  (data (i32.const 5000) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"ZW50cm9weSBlY2hvZWQgaW4gdGhlIHN0YXR1cw==\"}\0a\00")
  (data (i32.const 6000) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"],[\"x-accel-redirect\",\"/internal/etc/passwd\"],[\"x-sendfile\",\"/etc/passwd\"]],\"body_b64\":\"cHJveHkgcmVkaXJlY3QgYXR0ZW1wdA==\"}\0a\00")
  (data (i32.const 6400) "\ff\fe\fd\fc\0a\00")
  (data (i32.const 6500) "{\"op\":\"error\",\"detail\":\"\\n2026-01-01T00:00:00Z  INFO forged: the host said this\\u001b[2K\\rBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB\"}\0a\00")

  ;; Length of the NUL-terminated string at $p.
  (func $strlen (param $p i32) (result i32)
    (local $n i32)
    (block $done
      (loop $l
        (br_if $done (i32.eqz (i32.load8_u (i32.add (local.get $p) (local.get $n)))))
        (local.set $n (i32.add (local.get $n) (i32.const 1)))
        (br $l)))
    (local.get $n))

  ;; Write the NUL-terminated string at $p to stdout: one wire frame.
  (func $emit (param $p i32)
    (i32.store (i32.const 0) (local.get $p))
    (i32.store (i32.const 4) (call $strlen (local.get $p)))
    (drop (call $fd_write (i32.const 1) (i32.const 0) (i32.const 1) (i32.const 16))))

  ;; Write $len bytes of scratch memory to $fd.
  (func $spew (param $fd i32) (param $len i32)
    (i32.store (i32.const 0) (i32.const 12288))
    (i32.store (i32.const 4) (local.get $len))
    (drop (call $fd_write (local.get $fd) (i32.const 0) (i32.const 1) (i32.const 16))))

  ;; Read one newline-terminated frame from stdin into the input buffer at
  ;; 8192; returns its length, newline excluded.
  (func $read_line (result i32)
    (local $n i32)
    (block $done
      (loop $l
        (i32.store (i32.const 0) (i32.add (i32.const 65536) (local.get $n)))
        (i32.store (i32.const 4) (i32.const 1))
        (i32.store (i32.const 16) (i32.const 0))
        (br_if $done (i32.ne (call $fd_read (i32.const 0) (i32.const 0) (i32.const 1) (i32.const 16)) (i32.const 0)))
        (br_if $done (i32.eqz (i32.load (i32.const 16))))
        (br_if $done (i32.eq (i32.load8_u (i32.add (i32.const 65536) (local.get $n))) (i32.const 10)))
        (local.set $n (i32.add (local.get $n) (i32.const 1)))
        (br_if $done (i32.ge_u (local.get $n) (i32.const 40000)))
        (br $l)))
    (local.get $n))

  ;; Naive substring search of the NUL-terminated needle at $needle inside the
  ;; first $len bytes of the input buffer.
  (func $contains (param $len i32) (param $needle i32) (result i32)
    (local $i i32) (local $j i32) (local $nl i32)
    (local.set $nl (call $strlen (local.get $needle)))
    (if (i32.gt_u (local.get $nl) (local.get $len)) (then (return (i32.const 0))))
    (block $no
      (loop $outer
        (br_if $no (i32.gt_u (i32.add (local.get $i) (local.get $nl)) (local.get $len)))
        (local.set $j (i32.const 0))
        (block $mismatch
          (loop $inner
            (br_if $mismatch (i32.ne
              (i32.load8_u (i32.add (i32.const 65536) (i32.add (local.get $i) (local.get $j))))
              (i32.load8_u (i32.add (local.get $needle) (local.get $j)))))
            (local.set $j (i32.add (local.get $j) (i32.const 1)))
            (br_if $inner (i32.lt_u (local.get $j) (local.get $nl))))
          (return (i32.const 1)))
        (local.set $i (i32.add (local.get $i) (i32.const 1)))
        (br $outer)))
    (i32.const 0))

  ;; Answer the request the way an honest hello-world plugin would.
  (func $answer
    (local $len i32)
    (local.set $len (call $read_line))
    (if (i32.eqz (call $contains (local.get $len) (i32.const 1024)))
      (then (call $emit (i32.const 768)) (return)))
    (if (call $contains (local.get $len) (i32.const 1088))
      (then (call $emit (i32.const 128)) (return)))
    (call $emit (i32.const 512)))
  (func (export "_start")
    (call $answer)
  )
)
"#;

/// Never stops computing. Bounded only by the fuel ceiling.
pub const CPU_SPIN: &str = r#"(module
  (import "wasi_snapshot_preview1" "fd_read" (func $fd_read (param i32 i32 i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "fd_write" (func $fd_write (param i32 i32 i32 i32) (result i32)))
  (memory (export "memory") 2 4)
  (data (i32.const 128) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"aGVsbG8gZnJvbSB0aGUgc2FuZGJveA==\"}\0a\00")
  (data (i32.const 512) "{\"op\":\"response\",\"status\":404,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"bm8gc3VjaCBzYW5kYm94ZWQgcm91dGU=\"}\0a\00")
  (data (i32.const 768) "{\"op\":\"response\",\"status\":405,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"bWV0aG9kIG5vdCBhbGxvd2Vk\"}\0a\00")
  (data (i32.const 1024) "\"method\":\"GET\"\00")
  (data (i32.const 1088) "/greet\00")
  (data (i32.const 1120) "/etc/passwd\00")
  (data (i32.const 1152) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"],[\"set-cookie\",\"session=forged\"]],\"body_b64\":\"Y29va2llIGF0dGVtcHQ=\"}\0a\00")
  (data (i32.const 1600) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"],[\"x-evil\",\"a\\r\\nset-cookie: forged=1\"]],\"body_b64\":\"c3BsaXR0aW5nIGF0dGVtcHQ=\"}\0a\00")
  (data (i32.const 2048) "{\"op\":\"response\",\"status\":200,\0a\00")
  (data (i32.const 2176) "{\"op\":\"exec\",\"cmd\":\"/bin/sh\"}\0a\00")
  (data (i32.const 2304) "{\"op\":\"response\",\"status\":999,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"aW1wb3NzaWJsZSBzdGF0dXM=\"}\0a\00")
  (data (i32.const 2816) "X\00")
  (data (i32.const 3072) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"c2Vjb25kIGFuc3dlcg==\"}\0a\00")
  (data (i32.const 3584) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"],[\"x-autumn-sandboxed\",\"not-this-plugin\"],[\"x-content-type-options\",\"off\"]],\"body_b64\":\"YXR0cmlidXRpb24gYXR0ZW1wdA==\"}\0a\00")
  (data (i32.const 4200) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"aGVsbG8gZnJvbSB0aGUgc2FuZGJveA==\"}\00")
  (data (i32.const 5000) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"ZW50cm9weSBlY2hvZWQgaW4gdGhlIHN0YXR1cw==\"}\0a\00")
  (data (i32.const 6000) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"],[\"x-accel-redirect\",\"/internal/etc/passwd\"],[\"x-sendfile\",\"/etc/passwd\"]],\"body_b64\":\"cHJveHkgcmVkaXJlY3QgYXR0ZW1wdA==\"}\0a\00")
  (data (i32.const 6400) "\ff\fe\fd\fc\0a\00")
  (data (i32.const 6500) "{\"op\":\"error\",\"detail\":\"\\n2026-01-01T00:00:00Z  INFO forged: the host said this\\u001b[2K\\rBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB\"}\0a\00")

  ;; Length of the NUL-terminated string at $p.
  (func $strlen (param $p i32) (result i32)
    (local $n i32)
    (block $done
      (loop $l
        (br_if $done (i32.eqz (i32.load8_u (i32.add (local.get $p) (local.get $n)))))
        (local.set $n (i32.add (local.get $n) (i32.const 1)))
        (br $l)))
    (local.get $n))

  ;; Write the NUL-terminated string at $p to stdout: one wire frame.
  (func $emit (param $p i32)
    (i32.store (i32.const 0) (local.get $p))
    (i32.store (i32.const 4) (call $strlen (local.get $p)))
    (drop (call $fd_write (i32.const 1) (i32.const 0) (i32.const 1) (i32.const 16))))

  ;; Write $len bytes of scratch memory to $fd.
  (func $spew (param $fd i32) (param $len i32)
    (i32.store (i32.const 0) (i32.const 12288))
    (i32.store (i32.const 4) (local.get $len))
    (drop (call $fd_write (local.get $fd) (i32.const 0) (i32.const 1) (i32.const 16))))

  ;; Read one newline-terminated frame from stdin into the input buffer at
  ;; 8192; returns its length, newline excluded.
  (func $read_line (result i32)
    (local $n i32)
    (block $done
      (loop $l
        (i32.store (i32.const 0) (i32.add (i32.const 65536) (local.get $n)))
        (i32.store (i32.const 4) (i32.const 1))
        (i32.store (i32.const 16) (i32.const 0))
        (br_if $done (i32.ne (call $fd_read (i32.const 0) (i32.const 0) (i32.const 1) (i32.const 16)) (i32.const 0)))
        (br_if $done (i32.eqz (i32.load (i32.const 16))))
        (br_if $done (i32.eq (i32.load8_u (i32.add (i32.const 65536) (local.get $n))) (i32.const 10)))
        (local.set $n (i32.add (local.get $n) (i32.const 1)))
        (br_if $done (i32.ge_u (local.get $n) (i32.const 40000)))
        (br $l)))
    (local.get $n))

  ;; Naive substring search of the NUL-terminated needle at $needle inside the
  ;; first $len bytes of the input buffer.
  (func $contains (param $len i32) (param $needle i32) (result i32)
    (local $i i32) (local $j i32) (local $nl i32)
    (local.set $nl (call $strlen (local.get $needle)))
    (if (i32.gt_u (local.get $nl) (local.get $len)) (then (return (i32.const 0))))
    (block $no
      (loop $outer
        (br_if $no (i32.gt_u (i32.add (local.get $i) (local.get $nl)) (local.get $len)))
        (local.set $j (i32.const 0))
        (block $mismatch
          (loop $inner
            (br_if $mismatch (i32.ne
              (i32.load8_u (i32.add (i32.const 65536) (i32.add (local.get $i) (local.get $j))))
              (i32.load8_u (i32.add (local.get $needle) (local.get $j)))))
            (local.set $j (i32.add (local.get $j) (i32.const 1)))
            (br_if $inner (i32.lt_u (local.get $j) (local.get $nl))))
          (return (i32.const 1)))
        (local.set $i (i32.add (local.get $i) (i32.const 1)))
        (br $outer)))
    (i32.const 0))

  ;; Answer the request the way an honest hello-world plugin would.
  (func $answer
    (local $len i32)
    (local.set $len (call $read_line))
    (if (i32.eqz (call $contains (local.get $len) (i32.const 1024)))
      (then (call $emit (i32.const 768)) (return)))
    (if (call $contains (local.get $len) (i32.const 1088))
      (then (call $emit (i32.const 128)) (return)))
    (call $emit (i32.const 512)))
  (func (export "_start")
    (local $i i32)
    (loop $l (local.set $i (i32.add (local.get $i) (i32.const 1))) (br $l))
  )
)
"#;

/// Grows linear memory until the host refuses, then keeps asking.
pub const MEMORY_BOMB: &str = r#"(module
  (import "wasi_snapshot_preview1" "fd_read" (func $fd_read (param i32 i32 i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "fd_write" (func $fd_write (param i32 i32 i32 i32) (result i32)))
  (memory (export "memory") 2)
  (data (i32.const 128) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"aGVsbG8gZnJvbSB0aGUgc2FuZGJveA==\"}\0a\00")
  (data (i32.const 512) "{\"op\":\"response\",\"status\":404,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"bm8gc3VjaCBzYW5kYm94ZWQgcm91dGU=\"}\0a\00")
  (data (i32.const 768) "{\"op\":\"response\",\"status\":405,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"bWV0aG9kIG5vdCBhbGxvd2Vk\"}\0a\00")
  (data (i32.const 1024) "\"method\":\"GET\"\00")
  (data (i32.const 1088) "/greet\00")
  (data (i32.const 1120) "/etc/passwd\00")
  (data (i32.const 1152) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"],[\"set-cookie\",\"session=forged\"]],\"body_b64\":\"Y29va2llIGF0dGVtcHQ=\"}\0a\00")
  (data (i32.const 1600) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"],[\"x-evil\",\"a\\r\\nset-cookie: forged=1\"]],\"body_b64\":\"c3BsaXR0aW5nIGF0dGVtcHQ=\"}\0a\00")
  (data (i32.const 2048) "{\"op\":\"response\",\"status\":200,\0a\00")
  (data (i32.const 2176) "{\"op\":\"exec\",\"cmd\":\"/bin/sh\"}\0a\00")
  (data (i32.const 2304) "{\"op\":\"response\",\"status\":999,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"aW1wb3NzaWJsZSBzdGF0dXM=\"}\0a\00")
  (data (i32.const 2816) "X\00")
  (data (i32.const 3072) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"c2Vjb25kIGFuc3dlcg==\"}\0a\00")
  (data (i32.const 3584) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"],[\"x-autumn-sandboxed\",\"not-this-plugin\"],[\"x-content-type-options\",\"off\"]],\"body_b64\":\"YXR0cmlidXRpb24gYXR0ZW1wdA==\"}\0a\00")
  (data (i32.const 4200) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"aGVsbG8gZnJvbSB0aGUgc2FuZGJveA==\"}\00")
  (data (i32.const 5000) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"ZW50cm9weSBlY2hvZWQgaW4gdGhlIHN0YXR1cw==\"}\0a\00")
  (data (i32.const 6000) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"],[\"x-accel-redirect\",\"/internal/etc/passwd\"],[\"x-sendfile\",\"/etc/passwd\"]],\"body_b64\":\"cHJveHkgcmVkaXJlY3QgYXR0ZW1wdA==\"}\0a\00")
  (data (i32.const 6400) "\ff\fe\fd\fc\0a\00")
  (data (i32.const 6500) "{\"op\":\"error\",\"detail\":\"\\n2026-01-01T00:00:00Z  INFO forged: the host said this\\u001b[2K\\rBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB\"}\0a\00")

  ;; Length of the NUL-terminated string at $p.
  (func $strlen (param $p i32) (result i32)
    (local $n i32)
    (block $done
      (loop $l
        (br_if $done (i32.eqz (i32.load8_u (i32.add (local.get $p) (local.get $n)))))
        (local.set $n (i32.add (local.get $n) (i32.const 1)))
        (br $l)))
    (local.get $n))

  ;; Write the NUL-terminated string at $p to stdout: one wire frame.
  (func $emit (param $p i32)
    (i32.store (i32.const 0) (local.get $p))
    (i32.store (i32.const 4) (call $strlen (local.get $p)))
    (drop (call $fd_write (i32.const 1) (i32.const 0) (i32.const 1) (i32.const 16))))

  ;; Write $len bytes of scratch memory to $fd.
  (func $spew (param $fd i32) (param $len i32)
    (i32.store (i32.const 0) (i32.const 12288))
    (i32.store (i32.const 4) (local.get $len))
    (drop (call $fd_write (local.get $fd) (i32.const 0) (i32.const 1) (i32.const 16))))

  ;; Read one newline-terminated frame from stdin into the input buffer at
  ;; 8192; returns its length, newline excluded.
  (func $read_line (result i32)
    (local $n i32)
    (block $done
      (loop $l
        (i32.store (i32.const 0) (i32.add (i32.const 65536) (local.get $n)))
        (i32.store (i32.const 4) (i32.const 1))
        (i32.store (i32.const 16) (i32.const 0))
        (br_if $done (i32.ne (call $fd_read (i32.const 0) (i32.const 0) (i32.const 1) (i32.const 16)) (i32.const 0)))
        (br_if $done (i32.eqz (i32.load (i32.const 16))))
        (br_if $done (i32.eq (i32.load8_u (i32.add (i32.const 65536) (local.get $n))) (i32.const 10)))
        (local.set $n (i32.add (local.get $n) (i32.const 1)))
        (br_if $done (i32.ge_u (local.get $n) (i32.const 40000)))
        (br $l)))
    (local.get $n))

  ;; Naive substring search of the NUL-terminated needle at $needle inside the
  ;; first $len bytes of the input buffer.
  (func $contains (param $len i32) (param $needle i32) (result i32)
    (local $i i32) (local $j i32) (local $nl i32)
    (local.set $nl (call $strlen (local.get $needle)))
    (if (i32.gt_u (local.get $nl) (local.get $len)) (then (return (i32.const 0))))
    (block $no
      (loop $outer
        (br_if $no (i32.gt_u (i32.add (local.get $i) (local.get $nl)) (local.get $len)))
        (local.set $j (i32.const 0))
        (block $mismatch
          (loop $inner
            (br_if $mismatch (i32.ne
              (i32.load8_u (i32.add (i32.const 65536) (i32.add (local.get $i) (local.get $j))))
              (i32.load8_u (i32.add (local.get $needle) (local.get $j)))))
            (local.set $j (i32.add (local.get $j) (i32.const 1)))
            (br_if $inner (i32.lt_u (local.get $j) (local.get $nl))))
          (return (i32.const 1)))
        (local.set $i (i32.add (local.get $i) (i32.const 1)))
        (br $outer)))
    (i32.const 0))

  ;; Answer the request the way an honest hello-world plugin would.
  (func $answer
    (local $len i32)
    (local.set $len (call $read_line))
    (if (i32.eqz (call $contains (local.get $len) (i32.const 1024)))
      (then (call $emit (i32.const 768)) (return)))
    (if (call $contains (local.get $len) (i32.const 1088))
      (then (call $emit (i32.const 128)) (return)))
    (call $emit (i32.const 512)))
  (func (export "_start")
    (loop $l (drop (memory.grow (i32.const 1))) (br $l))
  )
)
"#;

/// Traps immediately — what a Rust panic compiles to on wasm.
pub const TRAP: &str = r#"(module
  (import "wasi_snapshot_preview1" "fd_read" (func $fd_read (param i32 i32 i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "fd_write" (func $fd_write (param i32 i32 i32 i32) (result i32)))
  (memory (export "memory") 2 4)
  (data (i32.const 128) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"aGVsbG8gZnJvbSB0aGUgc2FuZGJveA==\"}\0a\00")
  (data (i32.const 512) "{\"op\":\"response\",\"status\":404,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"bm8gc3VjaCBzYW5kYm94ZWQgcm91dGU=\"}\0a\00")
  (data (i32.const 768) "{\"op\":\"response\",\"status\":405,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"bWV0aG9kIG5vdCBhbGxvd2Vk\"}\0a\00")
  (data (i32.const 1024) "\"method\":\"GET\"\00")
  (data (i32.const 1088) "/greet\00")
  (data (i32.const 1120) "/etc/passwd\00")
  (data (i32.const 1152) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"],[\"set-cookie\",\"session=forged\"]],\"body_b64\":\"Y29va2llIGF0dGVtcHQ=\"}\0a\00")
  (data (i32.const 1600) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"],[\"x-evil\",\"a\\r\\nset-cookie: forged=1\"]],\"body_b64\":\"c3BsaXR0aW5nIGF0dGVtcHQ=\"}\0a\00")
  (data (i32.const 2048) "{\"op\":\"response\",\"status\":200,\0a\00")
  (data (i32.const 2176) "{\"op\":\"exec\",\"cmd\":\"/bin/sh\"}\0a\00")
  (data (i32.const 2304) "{\"op\":\"response\",\"status\":999,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"aW1wb3NzaWJsZSBzdGF0dXM=\"}\0a\00")
  (data (i32.const 2816) "X\00")
  (data (i32.const 3072) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"c2Vjb25kIGFuc3dlcg==\"}\0a\00")
  (data (i32.const 3584) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"],[\"x-autumn-sandboxed\",\"not-this-plugin\"],[\"x-content-type-options\",\"off\"]],\"body_b64\":\"YXR0cmlidXRpb24gYXR0ZW1wdA==\"}\0a\00")
  (data (i32.const 4200) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"aGVsbG8gZnJvbSB0aGUgc2FuZGJveA==\"}\00")
  (data (i32.const 5000) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"ZW50cm9weSBlY2hvZWQgaW4gdGhlIHN0YXR1cw==\"}\0a\00")
  (data (i32.const 6000) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"],[\"x-accel-redirect\",\"/internal/etc/passwd\"],[\"x-sendfile\",\"/etc/passwd\"]],\"body_b64\":\"cHJveHkgcmVkaXJlY3QgYXR0ZW1wdA==\"}\0a\00")
  (data (i32.const 6400) "\ff\fe\fd\fc\0a\00")
  (data (i32.const 6500) "{\"op\":\"error\",\"detail\":\"\\n2026-01-01T00:00:00Z  INFO forged: the host said this\\u001b[2K\\rBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB\"}\0a\00")

  ;; Length of the NUL-terminated string at $p.
  (func $strlen (param $p i32) (result i32)
    (local $n i32)
    (block $done
      (loop $l
        (br_if $done (i32.eqz (i32.load8_u (i32.add (local.get $p) (local.get $n)))))
        (local.set $n (i32.add (local.get $n) (i32.const 1)))
        (br $l)))
    (local.get $n))

  ;; Write the NUL-terminated string at $p to stdout: one wire frame.
  (func $emit (param $p i32)
    (i32.store (i32.const 0) (local.get $p))
    (i32.store (i32.const 4) (call $strlen (local.get $p)))
    (drop (call $fd_write (i32.const 1) (i32.const 0) (i32.const 1) (i32.const 16))))

  ;; Write $len bytes of scratch memory to $fd.
  (func $spew (param $fd i32) (param $len i32)
    (i32.store (i32.const 0) (i32.const 12288))
    (i32.store (i32.const 4) (local.get $len))
    (drop (call $fd_write (local.get $fd) (i32.const 0) (i32.const 1) (i32.const 16))))

  ;; Read one newline-terminated frame from stdin into the input buffer at
  ;; 8192; returns its length, newline excluded.
  (func $read_line (result i32)
    (local $n i32)
    (block $done
      (loop $l
        (i32.store (i32.const 0) (i32.add (i32.const 65536) (local.get $n)))
        (i32.store (i32.const 4) (i32.const 1))
        (i32.store (i32.const 16) (i32.const 0))
        (br_if $done (i32.ne (call $fd_read (i32.const 0) (i32.const 0) (i32.const 1) (i32.const 16)) (i32.const 0)))
        (br_if $done (i32.eqz (i32.load (i32.const 16))))
        (br_if $done (i32.eq (i32.load8_u (i32.add (i32.const 65536) (local.get $n))) (i32.const 10)))
        (local.set $n (i32.add (local.get $n) (i32.const 1)))
        (br_if $done (i32.ge_u (local.get $n) (i32.const 40000)))
        (br $l)))
    (local.get $n))

  ;; Naive substring search of the NUL-terminated needle at $needle inside the
  ;; first $len bytes of the input buffer.
  (func $contains (param $len i32) (param $needle i32) (result i32)
    (local $i i32) (local $j i32) (local $nl i32)
    (local.set $nl (call $strlen (local.get $needle)))
    (if (i32.gt_u (local.get $nl) (local.get $len)) (then (return (i32.const 0))))
    (block $no
      (loop $outer
        (br_if $no (i32.gt_u (i32.add (local.get $i) (local.get $nl)) (local.get $len)))
        (local.set $j (i32.const 0))
        (block $mismatch
          (loop $inner
            (br_if $mismatch (i32.ne
              (i32.load8_u (i32.add (i32.const 65536) (i32.add (local.get $i) (local.get $j))))
              (i32.load8_u (i32.add (local.get $needle) (local.get $j)))))
            (local.set $j (i32.add (local.get $j) (i32.const 1)))
            (br_if $inner (i32.lt_u (local.get $j) (local.get $nl))))
          (return (i32.const 1)))
        (local.set $i (i32.add (local.get $i) (i32.const 1)))
        (br $outer)))
    (i32.const 0))

  ;; Answer the request the way an honest hello-world plugin would.
  (func $answer
    (local $len i32)
    (local.set $len (call $read_line))
    (if (i32.eqz (call $contains (local.get $len) (i32.const 1024)))
      (then (call $emit (i32.const 768)) (return)))
    (if (call $contains (local.get $len) (i32.const 1088))
      (then (call $emit (i32.const 128)) (return)))
    (call $emit (i32.const 512)))
  (func (export "_start")
    (unreachable)
  )
)
"#;

/// Calls `proc_exit`, the closest a guest can get to killing the process.
pub const EXIT: &str = r#"(module
  (import "wasi_snapshot_preview1" "fd_read" (func $fd_read (param i32 i32 i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "fd_write" (func $fd_write (param i32 i32 i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "proc_exit" (func $proc_exit (param i32)))
  (memory (export "memory") 2 4)
  (data (i32.const 128) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"aGVsbG8gZnJvbSB0aGUgc2FuZGJveA==\"}\0a\00")
  (data (i32.const 512) "{\"op\":\"response\",\"status\":404,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"bm8gc3VjaCBzYW5kYm94ZWQgcm91dGU=\"}\0a\00")
  (data (i32.const 768) "{\"op\":\"response\",\"status\":405,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"bWV0aG9kIG5vdCBhbGxvd2Vk\"}\0a\00")
  (data (i32.const 1024) "\"method\":\"GET\"\00")
  (data (i32.const 1088) "/greet\00")
  (data (i32.const 1120) "/etc/passwd\00")
  (data (i32.const 1152) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"],[\"set-cookie\",\"session=forged\"]],\"body_b64\":\"Y29va2llIGF0dGVtcHQ=\"}\0a\00")
  (data (i32.const 1600) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"],[\"x-evil\",\"a\\r\\nset-cookie: forged=1\"]],\"body_b64\":\"c3BsaXR0aW5nIGF0dGVtcHQ=\"}\0a\00")
  (data (i32.const 2048) "{\"op\":\"response\",\"status\":200,\0a\00")
  (data (i32.const 2176) "{\"op\":\"exec\",\"cmd\":\"/bin/sh\"}\0a\00")
  (data (i32.const 2304) "{\"op\":\"response\",\"status\":999,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"aW1wb3NzaWJsZSBzdGF0dXM=\"}\0a\00")
  (data (i32.const 2816) "X\00")
  (data (i32.const 3072) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"c2Vjb25kIGFuc3dlcg==\"}\0a\00")
  (data (i32.const 3584) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"],[\"x-autumn-sandboxed\",\"not-this-plugin\"],[\"x-content-type-options\",\"off\"]],\"body_b64\":\"YXR0cmlidXRpb24gYXR0ZW1wdA==\"}\0a\00")
  (data (i32.const 4200) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"aGVsbG8gZnJvbSB0aGUgc2FuZGJveA==\"}\00")
  (data (i32.const 5000) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"ZW50cm9weSBlY2hvZWQgaW4gdGhlIHN0YXR1cw==\"}\0a\00")
  (data (i32.const 6000) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"],[\"x-accel-redirect\",\"/internal/etc/passwd\"],[\"x-sendfile\",\"/etc/passwd\"]],\"body_b64\":\"cHJveHkgcmVkaXJlY3QgYXR0ZW1wdA==\"}\0a\00")
  (data (i32.const 6400) "\ff\fe\fd\fc\0a\00")
  (data (i32.const 6500) "{\"op\":\"error\",\"detail\":\"\\n2026-01-01T00:00:00Z  INFO forged: the host said this\\u001b[2K\\rBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB\"}\0a\00")

  ;; Length of the NUL-terminated string at $p.
  (func $strlen (param $p i32) (result i32)
    (local $n i32)
    (block $done
      (loop $l
        (br_if $done (i32.eqz (i32.load8_u (i32.add (local.get $p) (local.get $n)))))
        (local.set $n (i32.add (local.get $n) (i32.const 1)))
        (br $l)))
    (local.get $n))

  ;; Write the NUL-terminated string at $p to stdout: one wire frame.
  (func $emit (param $p i32)
    (i32.store (i32.const 0) (local.get $p))
    (i32.store (i32.const 4) (call $strlen (local.get $p)))
    (drop (call $fd_write (i32.const 1) (i32.const 0) (i32.const 1) (i32.const 16))))

  ;; Write $len bytes of scratch memory to $fd.
  (func $spew (param $fd i32) (param $len i32)
    (i32.store (i32.const 0) (i32.const 12288))
    (i32.store (i32.const 4) (local.get $len))
    (drop (call $fd_write (local.get $fd) (i32.const 0) (i32.const 1) (i32.const 16))))

  ;; Read one newline-terminated frame from stdin into the input buffer at
  ;; 8192; returns its length, newline excluded.
  (func $read_line (result i32)
    (local $n i32)
    (block $done
      (loop $l
        (i32.store (i32.const 0) (i32.add (i32.const 65536) (local.get $n)))
        (i32.store (i32.const 4) (i32.const 1))
        (i32.store (i32.const 16) (i32.const 0))
        (br_if $done (i32.ne (call $fd_read (i32.const 0) (i32.const 0) (i32.const 1) (i32.const 16)) (i32.const 0)))
        (br_if $done (i32.eqz (i32.load (i32.const 16))))
        (br_if $done (i32.eq (i32.load8_u (i32.add (i32.const 65536) (local.get $n))) (i32.const 10)))
        (local.set $n (i32.add (local.get $n) (i32.const 1)))
        (br_if $done (i32.ge_u (local.get $n) (i32.const 40000)))
        (br $l)))
    (local.get $n))

  ;; Naive substring search of the NUL-terminated needle at $needle inside the
  ;; first $len bytes of the input buffer.
  (func $contains (param $len i32) (param $needle i32) (result i32)
    (local $i i32) (local $j i32) (local $nl i32)
    (local.set $nl (call $strlen (local.get $needle)))
    (if (i32.gt_u (local.get $nl) (local.get $len)) (then (return (i32.const 0))))
    (block $no
      (loop $outer
        (br_if $no (i32.gt_u (i32.add (local.get $i) (local.get $nl)) (local.get $len)))
        (local.set $j (i32.const 0))
        (block $mismatch
          (loop $inner
            (br_if $mismatch (i32.ne
              (i32.load8_u (i32.add (i32.const 65536) (i32.add (local.get $i) (local.get $j))))
              (i32.load8_u (i32.add (local.get $needle) (local.get $j)))))
            (local.set $j (i32.add (local.get $j) (i32.const 1)))
            (br_if $inner (i32.lt_u (local.get $j) (local.get $nl))))
          (return (i32.const 1)))
        (local.set $i (i32.add (local.get $i) (i32.const 1)))
        (br $outer)))
    (i32.const 0))

  ;; Answer the request the way an honest hello-world plugin would.
  (func $answer
    (local $len i32)
    (local.set $len (call $read_line))
    (if (i32.eqz (call $contains (local.get $len) (i32.const 1024)))
      (then (call $emit (i32.const 768)) (return)))
    (if (call $contains (local.get $len) (i32.const 1088))
      (then (call $emit (i32.const 128)) (return)))
    (call $emit (i32.const 512)))
  (func (export "_start")
    (call $proc_exit (i32.const 3))
  )
)
"#;

/// Returns without ever answering.
pub const SILENT: &str = r#"(module
  (import "wasi_snapshot_preview1" "fd_read" (func $fd_read (param i32 i32 i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "fd_write" (func $fd_write (param i32 i32 i32 i32) (result i32)))
  (memory (export "memory") 2 4)
  (data (i32.const 128) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"aGVsbG8gZnJvbSB0aGUgc2FuZGJveA==\"}\0a\00")
  (data (i32.const 512) "{\"op\":\"response\",\"status\":404,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"bm8gc3VjaCBzYW5kYm94ZWQgcm91dGU=\"}\0a\00")
  (data (i32.const 768) "{\"op\":\"response\",\"status\":405,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"bWV0aG9kIG5vdCBhbGxvd2Vk\"}\0a\00")
  (data (i32.const 1024) "\"method\":\"GET\"\00")
  (data (i32.const 1088) "/greet\00")
  (data (i32.const 1120) "/etc/passwd\00")
  (data (i32.const 1152) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"],[\"set-cookie\",\"session=forged\"]],\"body_b64\":\"Y29va2llIGF0dGVtcHQ=\"}\0a\00")
  (data (i32.const 1600) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"],[\"x-evil\",\"a\\r\\nset-cookie: forged=1\"]],\"body_b64\":\"c3BsaXR0aW5nIGF0dGVtcHQ=\"}\0a\00")
  (data (i32.const 2048) "{\"op\":\"response\",\"status\":200,\0a\00")
  (data (i32.const 2176) "{\"op\":\"exec\",\"cmd\":\"/bin/sh\"}\0a\00")
  (data (i32.const 2304) "{\"op\":\"response\",\"status\":999,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"aW1wb3NzaWJsZSBzdGF0dXM=\"}\0a\00")
  (data (i32.const 2816) "X\00")
  (data (i32.const 3072) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"c2Vjb25kIGFuc3dlcg==\"}\0a\00")
  (data (i32.const 3584) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"],[\"x-autumn-sandboxed\",\"not-this-plugin\"],[\"x-content-type-options\",\"off\"]],\"body_b64\":\"YXR0cmlidXRpb24gYXR0ZW1wdA==\"}\0a\00")
  (data (i32.const 4200) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"aGVsbG8gZnJvbSB0aGUgc2FuZGJveA==\"}\00")
  (data (i32.const 5000) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"ZW50cm9weSBlY2hvZWQgaW4gdGhlIHN0YXR1cw==\"}\0a\00")
  (data (i32.const 6000) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"],[\"x-accel-redirect\",\"/internal/etc/passwd\"],[\"x-sendfile\",\"/etc/passwd\"]],\"body_b64\":\"cHJveHkgcmVkaXJlY3QgYXR0ZW1wdA==\"}\0a\00")
  (data (i32.const 6400) "\ff\fe\fd\fc\0a\00")
  (data (i32.const 6500) "{\"op\":\"error\",\"detail\":\"\\n2026-01-01T00:00:00Z  INFO forged: the host said this\\u001b[2K\\rBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB\"}\0a\00")

  ;; Length of the NUL-terminated string at $p.
  (func $strlen (param $p i32) (result i32)
    (local $n i32)
    (block $done
      (loop $l
        (br_if $done (i32.eqz (i32.load8_u (i32.add (local.get $p) (local.get $n)))))
        (local.set $n (i32.add (local.get $n) (i32.const 1)))
        (br $l)))
    (local.get $n))

  ;; Write the NUL-terminated string at $p to stdout: one wire frame.
  (func $emit (param $p i32)
    (i32.store (i32.const 0) (local.get $p))
    (i32.store (i32.const 4) (call $strlen (local.get $p)))
    (drop (call $fd_write (i32.const 1) (i32.const 0) (i32.const 1) (i32.const 16))))

  ;; Write $len bytes of scratch memory to $fd.
  (func $spew (param $fd i32) (param $len i32)
    (i32.store (i32.const 0) (i32.const 12288))
    (i32.store (i32.const 4) (local.get $len))
    (drop (call $fd_write (local.get $fd) (i32.const 0) (i32.const 1) (i32.const 16))))

  ;; Read one newline-terminated frame from stdin into the input buffer at
  ;; 8192; returns its length, newline excluded.
  (func $read_line (result i32)
    (local $n i32)
    (block $done
      (loop $l
        (i32.store (i32.const 0) (i32.add (i32.const 65536) (local.get $n)))
        (i32.store (i32.const 4) (i32.const 1))
        (i32.store (i32.const 16) (i32.const 0))
        (br_if $done (i32.ne (call $fd_read (i32.const 0) (i32.const 0) (i32.const 1) (i32.const 16)) (i32.const 0)))
        (br_if $done (i32.eqz (i32.load (i32.const 16))))
        (br_if $done (i32.eq (i32.load8_u (i32.add (i32.const 65536) (local.get $n))) (i32.const 10)))
        (local.set $n (i32.add (local.get $n) (i32.const 1)))
        (br_if $done (i32.ge_u (local.get $n) (i32.const 40000)))
        (br $l)))
    (local.get $n))

  ;; Naive substring search of the NUL-terminated needle at $needle inside the
  ;; first $len bytes of the input buffer.
  (func $contains (param $len i32) (param $needle i32) (result i32)
    (local $i i32) (local $j i32) (local $nl i32)
    (local.set $nl (call $strlen (local.get $needle)))
    (if (i32.gt_u (local.get $nl) (local.get $len)) (then (return (i32.const 0))))
    (block $no
      (loop $outer
        (br_if $no (i32.gt_u (i32.add (local.get $i) (local.get $nl)) (local.get $len)))
        (local.set $j (i32.const 0))
        (block $mismatch
          (loop $inner
            (br_if $mismatch (i32.ne
              (i32.load8_u (i32.add (i32.const 65536) (i32.add (local.get $i) (local.get $j))))
              (i32.load8_u (i32.add (local.get $needle) (local.get $j)))))
            (local.set $j (i32.add (local.get $j) (i32.const 1)))
            (br_if $inner (i32.lt_u (local.get $j) (local.get $nl))))
          (return (i32.const 1)))
        (local.set $i (i32.add (local.get $i) (i32.const 1)))
        (br $outer)))
    (i32.const 0))

  ;; Answer the request the way an honest hello-world plugin would.
  (func $answer
    (local $len i32)
    (local.set $len (call $read_line))
    (if (i32.eqz (call $contains (local.get $len) (i32.const 1024)))
      (then (call $emit (i32.const 768)) (return)))
    (if (call $contains (local.get $len) (i32.const 1088))
      (then (call $emit (i32.const 128)) (return)))
    (call $emit (i32.const 512)))
  (func (export "_start")
    (nop)
  )
)
"#;

/// Tries to open `/etc/passwd`, then answers, so the denial is observable
/// without the guest dying of it.
pub const READ_FILE: &str = r#"(module
  (import "wasi_snapshot_preview1" "fd_read" (func $fd_read (param i32 i32 i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "fd_write" (func $fd_write (param i32 i32 i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "path_open" (func $path_open (param i32 i32 i32 i32 i32 i64 i64 i32 i32) (result i32)))
  (memory (export "memory") 2 4)
  (data (i32.const 128) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"aGVsbG8gZnJvbSB0aGUgc2FuZGJveA==\"}\0a\00")
  (data (i32.const 512) "{\"op\":\"response\",\"status\":404,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"bm8gc3VjaCBzYW5kYm94ZWQgcm91dGU=\"}\0a\00")
  (data (i32.const 768) "{\"op\":\"response\",\"status\":405,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"bWV0aG9kIG5vdCBhbGxvd2Vk\"}\0a\00")
  (data (i32.const 1024) "\"method\":\"GET\"\00")
  (data (i32.const 1088) "/greet\00")
  (data (i32.const 1120) "/etc/passwd\00")
  (data (i32.const 1152) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"],[\"set-cookie\",\"session=forged\"]],\"body_b64\":\"Y29va2llIGF0dGVtcHQ=\"}\0a\00")
  (data (i32.const 1600) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"],[\"x-evil\",\"a\\r\\nset-cookie: forged=1\"]],\"body_b64\":\"c3BsaXR0aW5nIGF0dGVtcHQ=\"}\0a\00")
  (data (i32.const 2048) "{\"op\":\"response\",\"status\":200,\0a\00")
  (data (i32.const 2176) "{\"op\":\"exec\",\"cmd\":\"/bin/sh\"}\0a\00")
  (data (i32.const 2304) "{\"op\":\"response\",\"status\":999,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"aW1wb3NzaWJsZSBzdGF0dXM=\"}\0a\00")
  (data (i32.const 2816) "X\00")
  (data (i32.const 3072) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"c2Vjb25kIGFuc3dlcg==\"}\0a\00")
  (data (i32.const 3584) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"],[\"x-autumn-sandboxed\",\"not-this-plugin\"],[\"x-content-type-options\",\"off\"]],\"body_b64\":\"YXR0cmlidXRpb24gYXR0ZW1wdA==\"}\0a\00")
  (data (i32.const 4200) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"aGVsbG8gZnJvbSB0aGUgc2FuZGJveA==\"}\00")
  (data (i32.const 5000) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"ZW50cm9weSBlY2hvZWQgaW4gdGhlIHN0YXR1cw==\"}\0a\00")
  (data (i32.const 6000) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"],[\"x-accel-redirect\",\"/internal/etc/passwd\"],[\"x-sendfile\",\"/etc/passwd\"]],\"body_b64\":\"cHJveHkgcmVkaXJlY3QgYXR0ZW1wdA==\"}\0a\00")
  (data (i32.const 6400) "\ff\fe\fd\fc\0a\00")
  (data (i32.const 6500) "{\"op\":\"error\",\"detail\":\"\\n2026-01-01T00:00:00Z  INFO forged: the host said this\\u001b[2K\\rBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB\"}\0a\00")

  ;; Length of the NUL-terminated string at $p.
  (func $strlen (param $p i32) (result i32)
    (local $n i32)
    (block $done
      (loop $l
        (br_if $done (i32.eqz (i32.load8_u (i32.add (local.get $p) (local.get $n)))))
        (local.set $n (i32.add (local.get $n) (i32.const 1)))
        (br $l)))
    (local.get $n))

  ;; Write the NUL-terminated string at $p to stdout: one wire frame.
  (func $emit (param $p i32)
    (i32.store (i32.const 0) (local.get $p))
    (i32.store (i32.const 4) (call $strlen (local.get $p)))
    (drop (call $fd_write (i32.const 1) (i32.const 0) (i32.const 1) (i32.const 16))))

  ;; Write $len bytes of scratch memory to $fd.
  (func $spew (param $fd i32) (param $len i32)
    (i32.store (i32.const 0) (i32.const 12288))
    (i32.store (i32.const 4) (local.get $len))
    (drop (call $fd_write (local.get $fd) (i32.const 0) (i32.const 1) (i32.const 16))))

  ;; Read one newline-terminated frame from stdin into the input buffer at
  ;; 8192; returns its length, newline excluded.
  (func $read_line (result i32)
    (local $n i32)
    (block $done
      (loop $l
        (i32.store (i32.const 0) (i32.add (i32.const 65536) (local.get $n)))
        (i32.store (i32.const 4) (i32.const 1))
        (i32.store (i32.const 16) (i32.const 0))
        (br_if $done (i32.ne (call $fd_read (i32.const 0) (i32.const 0) (i32.const 1) (i32.const 16)) (i32.const 0)))
        (br_if $done (i32.eqz (i32.load (i32.const 16))))
        (br_if $done (i32.eq (i32.load8_u (i32.add (i32.const 65536) (local.get $n))) (i32.const 10)))
        (local.set $n (i32.add (local.get $n) (i32.const 1)))
        (br_if $done (i32.ge_u (local.get $n) (i32.const 40000)))
        (br $l)))
    (local.get $n))

  ;; Naive substring search of the NUL-terminated needle at $needle inside the
  ;; first $len bytes of the input buffer.
  (func $contains (param $len i32) (param $needle i32) (result i32)
    (local $i i32) (local $j i32) (local $nl i32)
    (local.set $nl (call $strlen (local.get $needle)))
    (if (i32.gt_u (local.get $nl) (local.get $len)) (then (return (i32.const 0))))
    (block $no
      (loop $outer
        (br_if $no (i32.gt_u (i32.add (local.get $i) (local.get $nl)) (local.get $len)))
        (local.set $j (i32.const 0))
        (block $mismatch
          (loop $inner
            (br_if $mismatch (i32.ne
              (i32.load8_u (i32.add (i32.const 65536) (i32.add (local.get $i) (local.get $j))))
              (i32.load8_u (i32.add (local.get $needle) (local.get $j)))))
            (local.set $j (i32.add (local.get $j) (i32.const 1)))
            (br_if $inner (i32.lt_u (local.get $j) (local.get $nl))))
          (return (i32.const 1)))
        (local.set $i (i32.add (local.get $i) (i32.const 1)))
        (br $outer)))
    (i32.const 0))

  ;; Answer the request the way an honest hello-world plugin would.
  (func $answer
    (local $len i32)
    (local.set $len (call $read_line))
    (if (i32.eqz (call $contains (local.get $len) (i32.const 1024)))
      (then (call $emit (i32.const 768)) (return)))
    (if (call $contains (local.get $len) (i32.const 1088))
      (then (call $emit (i32.const 128)) (return)))
    (call $emit (i32.const 512)))
  (func (export "_start")
    (drop (call $path_open (i32.const 3) (i32.const 1) (i32.const 1120) (i32.const 11)
                           (i32.const 0) (i64.const 0) (i64.const 0) (i32.const 0) (i32.const 16)))
    (call $answer)
  )
)
"#;

/// Asks for the pre-opened directories a WASI host normally hands out.
pub const DISCOVER_PREOPENS: &str = r#"(module
  (import "wasi_snapshot_preview1" "fd_read" (func $fd_read (param i32 i32 i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "fd_write" (func $fd_write (param i32 i32 i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "fd_prestat_get" (func $fd_prestat_get (param i32 i32) (result i32)))
  (memory (export "memory") 2 4)
  (data (i32.const 128) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"aGVsbG8gZnJvbSB0aGUgc2FuZGJveA==\"}\0a\00")
  (data (i32.const 512) "{\"op\":\"response\",\"status\":404,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"bm8gc3VjaCBzYW5kYm94ZWQgcm91dGU=\"}\0a\00")
  (data (i32.const 768) "{\"op\":\"response\",\"status\":405,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"bWV0aG9kIG5vdCBhbGxvd2Vk\"}\0a\00")
  (data (i32.const 1024) "\"method\":\"GET\"\00")
  (data (i32.const 1088) "/greet\00")
  (data (i32.const 1120) "/etc/passwd\00")
  (data (i32.const 1152) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"],[\"set-cookie\",\"session=forged\"]],\"body_b64\":\"Y29va2llIGF0dGVtcHQ=\"}\0a\00")
  (data (i32.const 1600) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"],[\"x-evil\",\"a\\r\\nset-cookie: forged=1\"]],\"body_b64\":\"c3BsaXR0aW5nIGF0dGVtcHQ=\"}\0a\00")
  (data (i32.const 2048) "{\"op\":\"response\",\"status\":200,\0a\00")
  (data (i32.const 2176) "{\"op\":\"exec\",\"cmd\":\"/bin/sh\"}\0a\00")
  (data (i32.const 2304) "{\"op\":\"response\",\"status\":999,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"aW1wb3NzaWJsZSBzdGF0dXM=\"}\0a\00")
  (data (i32.const 2816) "X\00")
  (data (i32.const 3072) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"c2Vjb25kIGFuc3dlcg==\"}\0a\00")
  (data (i32.const 3584) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"],[\"x-autumn-sandboxed\",\"not-this-plugin\"],[\"x-content-type-options\",\"off\"]],\"body_b64\":\"YXR0cmlidXRpb24gYXR0ZW1wdA==\"}\0a\00")
  (data (i32.const 4200) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"aGVsbG8gZnJvbSB0aGUgc2FuZGJveA==\"}\00")
  (data (i32.const 5000) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"ZW50cm9weSBlY2hvZWQgaW4gdGhlIHN0YXR1cw==\"}\0a\00")
  (data (i32.const 6000) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"],[\"x-accel-redirect\",\"/internal/etc/passwd\"],[\"x-sendfile\",\"/etc/passwd\"]],\"body_b64\":\"cHJveHkgcmVkaXJlY3QgYXR0ZW1wdA==\"}\0a\00")
  (data (i32.const 6400) "\ff\fe\fd\fc\0a\00")
  (data (i32.const 6500) "{\"op\":\"error\",\"detail\":\"\\n2026-01-01T00:00:00Z  INFO forged: the host said this\\u001b[2K\\rBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB\"}\0a\00")

  ;; Length of the NUL-terminated string at $p.
  (func $strlen (param $p i32) (result i32)
    (local $n i32)
    (block $done
      (loop $l
        (br_if $done (i32.eqz (i32.load8_u (i32.add (local.get $p) (local.get $n)))))
        (local.set $n (i32.add (local.get $n) (i32.const 1)))
        (br $l)))
    (local.get $n))

  ;; Write the NUL-terminated string at $p to stdout: one wire frame.
  (func $emit (param $p i32)
    (i32.store (i32.const 0) (local.get $p))
    (i32.store (i32.const 4) (call $strlen (local.get $p)))
    (drop (call $fd_write (i32.const 1) (i32.const 0) (i32.const 1) (i32.const 16))))

  ;; Write $len bytes of scratch memory to $fd.
  (func $spew (param $fd i32) (param $len i32)
    (i32.store (i32.const 0) (i32.const 12288))
    (i32.store (i32.const 4) (local.get $len))
    (drop (call $fd_write (local.get $fd) (i32.const 0) (i32.const 1) (i32.const 16))))

  ;; Read one newline-terminated frame from stdin into the input buffer at
  ;; 8192; returns its length, newline excluded.
  (func $read_line (result i32)
    (local $n i32)
    (block $done
      (loop $l
        (i32.store (i32.const 0) (i32.add (i32.const 65536) (local.get $n)))
        (i32.store (i32.const 4) (i32.const 1))
        (i32.store (i32.const 16) (i32.const 0))
        (br_if $done (i32.ne (call $fd_read (i32.const 0) (i32.const 0) (i32.const 1) (i32.const 16)) (i32.const 0)))
        (br_if $done (i32.eqz (i32.load (i32.const 16))))
        (br_if $done (i32.eq (i32.load8_u (i32.add (i32.const 65536) (local.get $n))) (i32.const 10)))
        (local.set $n (i32.add (local.get $n) (i32.const 1)))
        (br_if $done (i32.ge_u (local.get $n) (i32.const 40000)))
        (br $l)))
    (local.get $n))

  ;; Naive substring search of the NUL-terminated needle at $needle inside the
  ;; first $len bytes of the input buffer.
  (func $contains (param $len i32) (param $needle i32) (result i32)
    (local $i i32) (local $j i32) (local $nl i32)
    (local.set $nl (call $strlen (local.get $needle)))
    (if (i32.gt_u (local.get $nl) (local.get $len)) (then (return (i32.const 0))))
    (block $no
      (loop $outer
        (br_if $no (i32.gt_u (i32.add (local.get $i) (local.get $nl)) (local.get $len)))
        (local.set $j (i32.const 0))
        (block $mismatch
          (loop $inner
            (br_if $mismatch (i32.ne
              (i32.load8_u (i32.add (i32.const 65536) (i32.add (local.get $i) (local.get $j))))
              (i32.load8_u (i32.add (local.get $needle) (local.get $j)))))
            (local.set $j (i32.add (local.get $j) (i32.const 1)))
            (br_if $inner (i32.lt_u (local.get $j) (local.get $nl))))
          (return (i32.const 1)))
        (local.set $i (i32.add (local.get $i) (i32.const 1)))
        (br $outer)))
    (i32.const 0))

  ;; Answer the request the way an honest hello-world plugin would.
  (func $answer
    (local $len i32)
    (local.set $len (call $read_line))
    (if (i32.eqz (call $contains (local.get $len) (i32.const 1024)))
      (then (call $emit (i32.const 768)) (return)))
    (if (call $contains (local.get $len) (i32.const 1088))
      (then (call $emit (i32.const 128)) (return)))
    (call $emit (i32.const 512)))
  (func (export "_start")
    (drop (call $fd_prestat_get (i32.const 3) (i32.const 16)))
    (call $answer)
  )
)
"#;

/// Reads from a descriptor it was never given.
pub const READ_STRAY_FD: &str = r#"(module
  (import "wasi_snapshot_preview1" "fd_read" (func $fd_read (param i32 i32 i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "fd_write" (func $fd_write (param i32 i32 i32 i32) (result i32)))
  (memory (export "memory") 2 4)
  (data (i32.const 128) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"aGVsbG8gZnJvbSB0aGUgc2FuZGJveA==\"}\0a\00")
  (data (i32.const 512) "{\"op\":\"response\",\"status\":404,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"bm8gc3VjaCBzYW5kYm94ZWQgcm91dGU=\"}\0a\00")
  (data (i32.const 768) "{\"op\":\"response\",\"status\":405,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"bWV0aG9kIG5vdCBhbGxvd2Vk\"}\0a\00")
  (data (i32.const 1024) "\"method\":\"GET\"\00")
  (data (i32.const 1088) "/greet\00")
  (data (i32.const 1120) "/etc/passwd\00")
  (data (i32.const 1152) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"],[\"set-cookie\",\"session=forged\"]],\"body_b64\":\"Y29va2llIGF0dGVtcHQ=\"}\0a\00")
  (data (i32.const 1600) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"],[\"x-evil\",\"a\\r\\nset-cookie: forged=1\"]],\"body_b64\":\"c3BsaXR0aW5nIGF0dGVtcHQ=\"}\0a\00")
  (data (i32.const 2048) "{\"op\":\"response\",\"status\":200,\0a\00")
  (data (i32.const 2176) "{\"op\":\"exec\",\"cmd\":\"/bin/sh\"}\0a\00")
  (data (i32.const 2304) "{\"op\":\"response\",\"status\":999,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"aW1wb3NzaWJsZSBzdGF0dXM=\"}\0a\00")
  (data (i32.const 2816) "X\00")
  (data (i32.const 3072) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"c2Vjb25kIGFuc3dlcg==\"}\0a\00")
  (data (i32.const 3584) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"],[\"x-autumn-sandboxed\",\"not-this-plugin\"],[\"x-content-type-options\",\"off\"]],\"body_b64\":\"YXR0cmlidXRpb24gYXR0ZW1wdA==\"}\0a\00")
  (data (i32.const 4200) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"aGVsbG8gZnJvbSB0aGUgc2FuZGJveA==\"}\00")
  (data (i32.const 5000) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"ZW50cm9weSBlY2hvZWQgaW4gdGhlIHN0YXR1cw==\"}\0a\00")
  (data (i32.const 6000) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"],[\"x-accel-redirect\",\"/internal/etc/passwd\"],[\"x-sendfile\",\"/etc/passwd\"]],\"body_b64\":\"cHJveHkgcmVkaXJlY3QgYXR0ZW1wdA==\"}\0a\00")
  (data (i32.const 6400) "\ff\fe\fd\fc\0a\00")
  (data (i32.const 6500) "{\"op\":\"error\",\"detail\":\"\\n2026-01-01T00:00:00Z  INFO forged: the host said this\\u001b[2K\\rBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB\"}\0a\00")

  ;; Length of the NUL-terminated string at $p.
  (func $strlen (param $p i32) (result i32)
    (local $n i32)
    (block $done
      (loop $l
        (br_if $done (i32.eqz (i32.load8_u (i32.add (local.get $p) (local.get $n)))))
        (local.set $n (i32.add (local.get $n) (i32.const 1)))
        (br $l)))
    (local.get $n))

  ;; Write the NUL-terminated string at $p to stdout: one wire frame.
  (func $emit (param $p i32)
    (i32.store (i32.const 0) (local.get $p))
    (i32.store (i32.const 4) (call $strlen (local.get $p)))
    (drop (call $fd_write (i32.const 1) (i32.const 0) (i32.const 1) (i32.const 16))))

  ;; Write $len bytes of scratch memory to $fd.
  (func $spew (param $fd i32) (param $len i32)
    (i32.store (i32.const 0) (i32.const 12288))
    (i32.store (i32.const 4) (local.get $len))
    (drop (call $fd_write (local.get $fd) (i32.const 0) (i32.const 1) (i32.const 16))))

  ;; Read one newline-terminated frame from stdin into the input buffer at
  ;; 8192; returns its length, newline excluded.
  (func $read_line (result i32)
    (local $n i32)
    (block $done
      (loop $l
        (i32.store (i32.const 0) (i32.add (i32.const 65536) (local.get $n)))
        (i32.store (i32.const 4) (i32.const 1))
        (i32.store (i32.const 16) (i32.const 0))
        (br_if $done (i32.ne (call $fd_read (i32.const 0) (i32.const 0) (i32.const 1) (i32.const 16)) (i32.const 0)))
        (br_if $done (i32.eqz (i32.load (i32.const 16))))
        (br_if $done (i32.eq (i32.load8_u (i32.add (i32.const 65536) (local.get $n))) (i32.const 10)))
        (local.set $n (i32.add (local.get $n) (i32.const 1)))
        (br_if $done (i32.ge_u (local.get $n) (i32.const 40000)))
        (br $l)))
    (local.get $n))

  ;; Naive substring search of the NUL-terminated needle at $needle inside the
  ;; first $len bytes of the input buffer.
  (func $contains (param $len i32) (param $needle i32) (result i32)
    (local $i i32) (local $j i32) (local $nl i32)
    (local.set $nl (call $strlen (local.get $needle)))
    (if (i32.gt_u (local.get $nl) (local.get $len)) (then (return (i32.const 0))))
    (block $no
      (loop $outer
        (br_if $no (i32.gt_u (i32.add (local.get $i) (local.get $nl)) (local.get $len)))
        (local.set $j (i32.const 0))
        (block $mismatch
          (loop $inner
            (br_if $mismatch (i32.ne
              (i32.load8_u (i32.add (i32.const 65536) (i32.add (local.get $i) (local.get $j))))
              (i32.load8_u (i32.add (local.get $needle) (local.get $j)))))
            (local.set $j (i32.add (local.get $j) (i32.const 1)))
            (br_if $inner (i32.lt_u (local.get $j) (local.get $nl))))
          (return (i32.const 1)))
        (local.set $i (i32.add (local.get $i) (i32.const 1)))
        (br $outer)))
    (i32.const 0))

  ;; Answer the request the way an honest hello-world plugin would.
  (func $answer
    (local $len i32)
    (local.set $len (call $read_line))
    (if (i32.eqz (call $contains (local.get $len) (i32.const 1024)))
      (then (call $emit (i32.const 768)) (return)))
    (if (call $contains (local.get $len) (i32.const 1088))
      (then (call $emit (i32.const 128)) (return)))
    (call $emit (i32.const 512)))
  (func (export "_start")
    (i32.store (i32.const 0) (i32.const 4096))
    (i32.store (i32.const 4) (i32.const 16))
    (drop (call $fd_read (i32.const 3) (i32.const 0) (i32.const 1) (i32.const 16)))
    (call $answer)
  )
)
"#;

/// Tries to send on a socket.
pub const NETWORK: &str = r#"(module
  (import "wasi_snapshot_preview1" "fd_read" (func $fd_read (param i32 i32 i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "fd_write" (func $fd_write (param i32 i32 i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "sock_send" (func $sock_send (param i32 i32 i32 i32 i32) (result i32)))
  (memory (export "memory") 2 4)
  (data (i32.const 128) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"aGVsbG8gZnJvbSB0aGUgc2FuZGJveA==\"}\0a\00")
  (data (i32.const 512) "{\"op\":\"response\",\"status\":404,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"bm8gc3VjaCBzYW5kYm94ZWQgcm91dGU=\"}\0a\00")
  (data (i32.const 768) "{\"op\":\"response\",\"status\":405,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"bWV0aG9kIG5vdCBhbGxvd2Vk\"}\0a\00")
  (data (i32.const 1024) "\"method\":\"GET\"\00")
  (data (i32.const 1088) "/greet\00")
  (data (i32.const 1120) "/etc/passwd\00")
  (data (i32.const 1152) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"],[\"set-cookie\",\"session=forged\"]],\"body_b64\":\"Y29va2llIGF0dGVtcHQ=\"}\0a\00")
  (data (i32.const 1600) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"],[\"x-evil\",\"a\\r\\nset-cookie: forged=1\"]],\"body_b64\":\"c3BsaXR0aW5nIGF0dGVtcHQ=\"}\0a\00")
  (data (i32.const 2048) "{\"op\":\"response\",\"status\":200,\0a\00")
  (data (i32.const 2176) "{\"op\":\"exec\",\"cmd\":\"/bin/sh\"}\0a\00")
  (data (i32.const 2304) "{\"op\":\"response\",\"status\":999,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"aW1wb3NzaWJsZSBzdGF0dXM=\"}\0a\00")
  (data (i32.const 2816) "X\00")
  (data (i32.const 3072) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"c2Vjb25kIGFuc3dlcg==\"}\0a\00")
  (data (i32.const 3584) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"],[\"x-autumn-sandboxed\",\"not-this-plugin\"],[\"x-content-type-options\",\"off\"]],\"body_b64\":\"YXR0cmlidXRpb24gYXR0ZW1wdA==\"}\0a\00")
  (data (i32.const 4200) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"aGVsbG8gZnJvbSB0aGUgc2FuZGJveA==\"}\00")
  (data (i32.const 5000) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"ZW50cm9weSBlY2hvZWQgaW4gdGhlIHN0YXR1cw==\"}\0a\00")
  (data (i32.const 6000) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"],[\"x-accel-redirect\",\"/internal/etc/passwd\"],[\"x-sendfile\",\"/etc/passwd\"]],\"body_b64\":\"cHJveHkgcmVkaXJlY3QgYXR0ZW1wdA==\"}\0a\00")
  (data (i32.const 6400) "\ff\fe\fd\fc\0a\00")
  (data (i32.const 6500) "{\"op\":\"error\",\"detail\":\"\\n2026-01-01T00:00:00Z  INFO forged: the host said this\\u001b[2K\\rBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB\"}\0a\00")

  ;; Length of the NUL-terminated string at $p.
  (func $strlen (param $p i32) (result i32)
    (local $n i32)
    (block $done
      (loop $l
        (br_if $done (i32.eqz (i32.load8_u (i32.add (local.get $p) (local.get $n)))))
        (local.set $n (i32.add (local.get $n) (i32.const 1)))
        (br $l)))
    (local.get $n))

  ;; Write the NUL-terminated string at $p to stdout: one wire frame.
  (func $emit (param $p i32)
    (i32.store (i32.const 0) (local.get $p))
    (i32.store (i32.const 4) (call $strlen (local.get $p)))
    (drop (call $fd_write (i32.const 1) (i32.const 0) (i32.const 1) (i32.const 16))))

  ;; Write $len bytes of scratch memory to $fd.
  (func $spew (param $fd i32) (param $len i32)
    (i32.store (i32.const 0) (i32.const 12288))
    (i32.store (i32.const 4) (local.get $len))
    (drop (call $fd_write (local.get $fd) (i32.const 0) (i32.const 1) (i32.const 16))))

  ;; Read one newline-terminated frame from stdin into the input buffer at
  ;; 8192; returns its length, newline excluded.
  (func $read_line (result i32)
    (local $n i32)
    (block $done
      (loop $l
        (i32.store (i32.const 0) (i32.add (i32.const 65536) (local.get $n)))
        (i32.store (i32.const 4) (i32.const 1))
        (i32.store (i32.const 16) (i32.const 0))
        (br_if $done (i32.ne (call $fd_read (i32.const 0) (i32.const 0) (i32.const 1) (i32.const 16)) (i32.const 0)))
        (br_if $done (i32.eqz (i32.load (i32.const 16))))
        (br_if $done (i32.eq (i32.load8_u (i32.add (i32.const 65536) (local.get $n))) (i32.const 10)))
        (local.set $n (i32.add (local.get $n) (i32.const 1)))
        (br_if $done (i32.ge_u (local.get $n) (i32.const 40000)))
        (br $l)))
    (local.get $n))

  ;; Naive substring search of the NUL-terminated needle at $needle inside the
  ;; first $len bytes of the input buffer.
  (func $contains (param $len i32) (param $needle i32) (result i32)
    (local $i i32) (local $j i32) (local $nl i32)
    (local.set $nl (call $strlen (local.get $needle)))
    (if (i32.gt_u (local.get $nl) (local.get $len)) (then (return (i32.const 0))))
    (block $no
      (loop $outer
        (br_if $no (i32.gt_u (i32.add (local.get $i) (local.get $nl)) (local.get $len)))
        (local.set $j (i32.const 0))
        (block $mismatch
          (loop $inner
            (br_if $mismatch (i32.ne
              (i32.load8_u (i32.add (i32.const 65536) (i32.add (local.get $i) (local.get $j))))
              (i32.load8_u (i32.add (local.get $needle) (local.get $j)))))
            (local.set $j (i32.add (local.get $j) (i32.const 1)))
            (br_if $inner (i32.lt_u (local.get $j) (local.get $nl))))
          (return (i32.const 1)))
        (local.set $i (i32.add (local.get $i) (i32.const 1)))
        (br $outer)))
    (i32.const 0))

  ;; Answer the request the way an honest hello-world plugin would.
  (func $answer
    (local $len i32)
    (local.set $len (call $read_line))
    (if (i32.eqz (call $contains (local.get $len) (i32.const 1024)))
      (then (call $emit (i32.const 768)) (return)))
    (if (call $contains (local.get $len) (i32.const 1088))
      (then (call $emit (i32.const 128)) (return)))
    (call $emit (i32.const 512)))
  (func (export "_start")
    (drop (call $sock_send (i32.const 3) (i32.const 0) (i32.const 1) (i32.const 0) (i32.const 16)))
    (call $answer)
  )
)
"#;

/// Reads the host process's environment.
pub const ENVIRONMENT: &str = r#"(module
  (import "wasi_snapshot_preview1" "fd_read" (func $fd_read (param i32 i32 i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "fd_write" (func $fd_write (param i32 i32 i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "environ_sizes_get" (func $environ_sizes_get (param i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "environ_get" (func $environ_get (param i32 i32) (result i32)))
  (memory (export "memory") 2 4)
  (data (i32.const 128) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"aGVsbG8gZnJvbSB0aGUgc2FuZGJveA==\"}\0a\00")
  (data (i32.const 512) "{\"op\":\"response\",\"status\":404,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"bm8gc3VjaCBzYW5kYm94ZWQgcm91dGU=\"}\0a\00")
  (data (i32.const 768) "{\"op\":\"response\",\"status\":405,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"bWV0aG9kIG5vdCBhbGxvd2Vk\"}\0a\00")
  (data (i32.const 1024) "\"method\":\"GET\"\00")
  (data (i32.const 1088) "/greet\00")
  (data (i32.const 1120) "/etc/passwd\00")
  (data (i32.const 1152) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"],[\"set-cookie\",\"session=forged\"]],\"body_b64\":\"Y29va2llIGF0dGVtcHQ=\"}\0a\00")
  (data (i32.const 1600) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"],[\"x-evil\",\"a\\r\\nset-cookie: forged=1\"]],\"body_b64\":\"c3BsaXR0aW5nIGF0dGVtcHQ=\"}\0a\00")
  (data (i32.const 2048) "{\"op\":\"response\",\"status\":200,\0a\00")
  (data (i32.const 2176) "{\"op\":\"exec\",\"cmd\":\"/bin/sh\"}\0a\00")
  (data (i32.const 2304) "{\"op\":\"response\",\"status\":999,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"aW1wb3NzaWJsZSBzdGF0dXM=\"}\0a\00")
  (data (i32.const 2816) "X\00")
  (data (i32.const 3072) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"c2Vjb25kIGFuc3dlcg==\"}\0a\00")
  (data (i32.const 3584) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"],[\"x-autumn-sandboxed\",\"not-this-plugin\"],[\"x-content-type-options\",\"off\"]],\"body_b64\":\"YXR0cmlidXRpb24gYXR0ZW1wdA==\"}\0a\00")
  (data (i32.const 4200) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"aGVsbG8gZnJvbSB0aGUgc2FuZGJveA==\"}\00")
  (data (i32.const 5000) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"ZW50cm9weSBlY2hvZWQgaW4gdGhlIHN0YXR1cw==\"}\0a\00")
  (data (i32.const 6000) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"],[\"x-accel-redirect\",\"/internal/etc/passwd\"],[\"x-sendfile\",\"/etc/passwd\"]],\"body_b64\":\"cHJveHkgcmVkaXJlY3QgYXR0ZW1wdA==\"}\0a\00")
  (data (i32.const 6400) "\ff\fe\fd\fc\0a\00")
  (data (i32.const 6500) "{\"op\":\"error\",\"detail\":\"\\n2026-01-01T00:00:00Z  INFO forged: the host said this\\u001b[2K\\rBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB\"}\0a\00")

  ;; Length of the NUL-terminated string at $p.
  (func $strlen (param $p i32) (result i32)
    (local $n i32)
    (block $done
      (loop $l
        (br_if $done (i32.eqz (i32.load8_u (i32.add (local.get $p) (local.get $n)))))
        (local.set $n (i32.add (local.get $n) (i32.const 1)))
        (br $l)))
    (local.get $n))

  ;; Write the NUL-terminated string at $p to stdout: one wire frame.
  (func $emit (param $p i32)
    (i32.store (i32.const 0) (local.get $p))
    (i32.store (i32.const 4) (call $strlen (local.get $p)))
    (drop (call $fd_write (i32.const 1) (i32.const 0) (i32.const 1) (i32.const 16))))

  ;; Write $len bytes of scratch memory to $fd.
  (func $spew (param $fd i32) (param $len i32)
    (i32.store (i32.const 0) (i32.const 12288))
    (i32.store (i32.const 4) (local.get $len))
    (drop (call $fd_write (local.get $fd) (i32.const 0) (i32.const 1) (i32.const 16))))

  ;; Read one newline-terminated frame from stdin into the input buffer at
  ;; 8192; returns its length, newline excluded.
  (func $read_line (result i32)
    (local $n i32)
    (block $done
      (loop $l
        (i32.store (i32.const 0) (i32.add (i32.const 65536) (local.get $n)))
        (i32.store (i32.const 4) (i32.const 1))
        (i32.store (i32.const 16) (i32.const 0))
        (br_if $done (i32.ne (call $fd_read (i32.const 0) (i32.const 0) (i32.const 1) (i32.const 16)) (i32.const 0)))
        (br_if $done (i32.eqz (i32.load (i32.const 16))))
        (br_if $done (i32.eq (i32.load8_u (i32.add (i32.const 65536) (local.get $n))) (i32.const 10)))
        (local.set $n (i32.add (local.get $n) (i32.const 1)))
        (br_if $done (i32.ge_u (local.get $n) (i32.const 40000)))
        (br $l)))
    (local.get $n))

  ;; Naive substring search of the NUL-terminated needle at $needle inside the
  ;; first $len bytes of the input buffer.
  (func $contains (param $len i32) (param $needle i32) (result i32)
    (local $i i32) (local $j i32) (local $nl i32)
    (local.set $nl (call $strlen (local.get $needle)))
    (if (i32.gt_u (local.get $nl) (local.get $len)) (then (return (i32.const 0))))
    (block $no
      (loop $outer
        (br_if $no (i32.gt_u (i32.add (local.get $i) (local.get $nl)) (local.get $len)))
        (local.set $j (i32.const 0))
        (block $mismatch
          (loop $inner
            (br_if $mismatch (i32.ne
              (i32.load8_u (i32.add (i32.const 65536) (i32.add (local.get $i) (local.get $j))))
              (i32.load8_u (i32.add (local.get $needle) (local.get $j)))))
            (local.set $j (i32.add (local.get $j) (i32.const 1)))
            (br_if $inner (i32.lt_u (local.get $j) (local.get $nl))))
          (return (i32.const 1)))
        (local.set $i (i32.add (local.get $i) (i32.const 1)))
        (br $outer)))
    (i32.const 0))

  ;; Answer the request the way an honest hello-world plugin would.
  (func $answer
    (local $len i32)
    (local.set $len (call $read_line))
    (if (i32.eqz (call $contains (local.get $len) (i32.const 1024)))
      (then (call $emit (i32.const 768)) (return)))
    (if (call $contains (local.get $len) (i32.const 1088))
      (then (call $emit (i32.const 128)) (return)))
    (call $emit (i32.const 512)))
  (func (export "_start")
    (drop (call $environ_sizes_get (i32.const 16) (i32.const 20)))
    (drop (call $environ_get (i32.const 4096) (i32.const 5120)))
    (call $answer)
  )
)
"#;

/// Reads the host process's argv.
pub const ARGUMENTS: &str = r#"(module
  (import "wasi_snapshot_preview1" "fd_read" (func $fd_read (param i32 i32 i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "fd_write" (func $fd_write (param i32 i32 i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "args_sizes_get" (func $args_sizes_get (param i32 i32) (result i32)))
  (memory (export "memory") 2 4)
  (data (i32.const 128) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"aGVsbG8gZnJvbSB0aGUgc2FuZGJveA==\"}\0a\00")
  (data (i32.const 512) "{\"op\":\"response\",\"status\":404,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"bm8gc3VjaCBzYW5kYm94ZWQgcm91dGU=\"}\0a\00")
  (data (i32.const 768) "{\"op\":\"response\",\"status\":405,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"bWV0aG9kIG5vdCBhbGxvd2Vk\"}\0a\00")
  (data (i32.const 1024) "\"method\":\"GET\"\00")
  (data (i32.const 1088) "/greet\00")
  (data (i32.const 1120) "/etc/passwd\00")
  (data (i32.const 1152) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"],[\"set-cookie\",\"session=forged\"]],\"body_b64\":\"Y29va2llIGF0dGVtcHQ=\"}\0a\00")
  (data (i32.const 1600) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"],[\"x-evil\",\"a\\r\\nset-cookie: forged=1\"]],\"body_b64\":\"c3BsaXR0aW5nIGF0dGVtcHQ=\"}\0a\00")
  (data (i32.const 2048) "{\"op\":\"response\",\"status\":200,\0a\00")
  (data (i32.const 2176) "{\"op\":\"exec\",\"cmd\":\"/bin/sh\"}\0a\00")
  (data (i32.const 2304) "{\"op\":\"response\",\"status\":999,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"aW1wb3NzaWJsZSBzdGF0dXM=\"}\0a\00")
  (data (i32.const 2816) "X\00")
  (data (i32.const 3072) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"c2Vjb25kIGFuc3dlcg==\"}\0a\00")
  (data (i32.const 3584) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"],[\"x-autumn-sandboxed\",\"not-this-plugin\"],[\"x-content-type-options\",\"off\"]],\"body_b64\":\"YXR0cmlidXRpb24gYXR0ZW1wdA==\"}\0a\00")
  (data (i32.const 4200) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"aGVsbG8gZnJvbSB0aGUgc2FuZGJveA==\"}\00")
  (data (i32.const 5000) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"ZW50cm9weSBlY2hvZWQgaW4gdGhlIHN0YXR1cw==\"}\0a\00")
  (data (i32.const 6000) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"],[\"x-accel-redirect\",\"/internal/etc/passwd\"],[\"x-sendfile\",\"/etc/passwd\"]],\"body_b64\":\"cHJveHkgcmVkaXJlY3QgYXR0ZW1wdA==\"}\0a\00")
  (data (i32.const 6400) "\ff\fe\fd\fc\0a\00")
  (data (i32.const 6500) "{\"op\":\"error\",\"detail\":\"\\n2026-01-01T00:00:00Z  INFO forged: the host said this\\u001b[2K\\rBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB\"}\0a\00")

  ;; Length of the NUL-terminated string at $p.
  (func $strlen (param $p i32) (result i32)
    (local $n i32)
    (block $done
      (loop $l
        (br_if $done (i32.eqz (i32.load8_u (i32.add (local.get $p) (local.get $n)))))
        (local.set $n (i32.add (local.get $n) (i32.const 1)))
        (br $l)))
    (local.get $n))

  ;; Write the NUL-terminated string at $p to stdout: one wire frame.
  (func $emit (param $p i32)
    (i32.store (i32.const 0) (local.get $p))
    (i32.store (i32.const 4) (call $strlen (local.get $p)))
    (drop (call $fd_write (i32.const 1) (i32.const 0) (i32.const 1) (i32.const 16))))

  ;; Write $len bytes of scratch memory to $fd.
  (func $spew (param $fd i32) (param $len i32)
    (i32.store (i32.const 0) (i32.const 12288))
    (i32.store (i32.const 4) (local.get $len))
    (drop (call $fd_write (local.get $fd) (i32.const 0) (i32.const 1) (i32.const 16))))

  ;; Read one newline-terminated frame from stdin into the input buffer at
  ;; 8192; returns its length, newline excluded.
  (func $read_line (result i32)
    (local $n i32)
    (block $done
      (loop $l
        (i32.store (i32.const 0) (i32.add (i32.const 65536) (local.get $n)))
        (i32.store (i32.const 4) (i32.const 1))
        (i32.store (i32.const 16) (i32.const 0))
        (br_if $done (i32.ne (call $fd_read (i32.const 0) (i32.const 0) (i32.const 1) (i32.const 16)) (i32.const 0)))
        (br_if $done (i32.eqz (i32.load (i32.const 16))))
        (br_if $done (i32.eq (i32.load8_u (i32.add (i32.const 65536) (local.get $n))) (i32.const 10)))
        (local.set $n (i32.add (local.get $n) (i32.const 1)))
        (br_if $done (i32.ge_u (local.get $n) (i32.const 40000)))
        (br $l)))
    (local.get $n))

  ;; Naive substring search of the NUL-terminated needle at $needle inside the
  ;; first $len bytes of the input buffer.
  (func $contains (param $len i32) (param $needle i32) (result i32)
    (local $i i32) (local $j i32) (local $nl i32)
    (local.set $nl (call $strlen (local.get $needle)))
    (if (i32.gt_u (local.get $nl) (local.get $len)) (then (return (i32.const 0))))
    (block $no
      (loop $outer
        (br_if $no (i32.gt_u (i32.add (local.get $i) (local.get $nl)) (local.get $len)))
        (local.set $j (i32.const 0))
        (block $mismatch
          (loop $inner
            (br_if $mismatch (i32.ne
              (i32.load8_u (i32.add (i32.const 65536) (i32.add (local.get $i) (local.get $j))))
              (i32.load8_u (i32.add (local.get $needle) (local.get $j)))))
            (local.set $j (i32.add (local.get $j) (i32.const 1)))
            (br_if $inner (i32.lt_u (local.get $j) (local.get $nl))))
          (return (i32.const 1)))
        (local.set $i (i32.add (local.get $i) (i32.const 1)))
        (br $outer)))
    (i32.const 0))

  ;; Answer the request the way an honest hello-world plugin would.
  (func $answer
    (local $len i32)
    (local.set $len (call $read_line))
    (if (i32.eqz (call $contains (local.get $len) (i32.const 1024)))
      (then (call $emit (i32.const 768)) (return)))
    (if (call $contains (local.get $len) (i32.const 1088))
      (then (call $emit (i32.const 128)) (return)))
    (call $emit (i32.const 512)))
  (func (export "_start")
    (drop (call $args_sizes_get (i32.const 16) (i32.const 20)))
    (call $answer)
  )
)
"#;

/// Asks the host to block it on a poll subscription.
pub const POLL: &str = r#"(module
  (import "wasi_snapshot_preview1" "fd_read" (func $fd_read (param i32 i32 i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "fd_write" (func $fd_write (param i32 i32 i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "poll_oneoff" (func $poll_oneoff (param i32 i32 i32 i32) (result i32)))
  (memory (export "memory") 2 4)
  (data (i32.const 128) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"aGVsbG8gZnJvbSB0aGUgc2FuZGJveA==\"}\0a\00")
  (data (i32.const 512) "{\"op\":\"response\",\"status\":404,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"bm8gc3VjaCBzYW5kYm94ZWQgcm91dGU=\"}\0a\00")
  (data (i32.const 768) "{\"op\":\"response\",\"status\":405,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"bWV0aG9kIG5vdCBhbGxvd2Vk\"}\0a\00")
  (data (i32.const 1024) "\"method\":\"GET\"\00")
  (data (i32.const 1088) "/greet\00")
  (data (i32.const 1120) "/etc/passwd\00")
  (data (i32.const 1152) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"],[\"set-cookie\",\"session=forged\"]],\"body_b64\":\"Y29va2llIGF0dGVtcHQ=\"}\0a\00")
  (data (i32.const 1600) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"],[\"x-evil\",\"a\\r\\nset-cookie: forged=1\"]],\"body_b64\":\"c3BsaXR0aW5nIGF0dGVtcHQ=\"}\0a\00")
  (data (i32.const 2048) "{\"op\":\"response\",\"status\":200,\0a\00")
  (data (i32.const 2176) "{\"op\":\"exec\",\"cmd\":\"/bin/sh\"}\0a\00")
  (data (i32.const 2304) "{\"op\":\"response\",\"status\":999,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"aW1wb3NzaWJsZSBzdGF0dXM=\"}\0a\00")
  (data (i32.const 2816) "X\00")
  (data (i32.const 3072) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"c2Vjb25kIGFuc3dlcg==\"}\0a\00")
  (data (i32.const 3584) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"],[\"x-autumn-sandboxed\",\"not-this-plugin\"],[\"x-content-type-options\",\"off\"]],\"body_b64\":\"YXR0cmlidXRpb24gYXR0ZW1wdA==\"}\0a\00")
  (data (i32.const 4200) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"aGVsbG8gZnJvbSB0aGUgc2FuZGJveA==\"}\00")
  (data (i32.const 5000) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"ZW50cm9weSBlY2hvZWQgaW4gdGhlIHN0YXR1cw==\"}\0a\00")
  (data (i32.const 6000) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"],[\"x-accel-redirect\",\"/internal/etc/passwd\"],[\"x-sendfile\",\"/etc/passwd\"]],\"body_b64\":\"cHJveHkgcmVkaXJlY3QgYXR0ZW1wdA==\"}\0a\00")
  (data (i32.const 6400) "\ff\fe\fd\fc\0a\00")
  (data (i32.const 6500) "{\"op\":\"error\",\"detail\":\"\\n2026-01-01T00:00:00Z  INFO forged: the host said this\\u001b[2K\\rBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB\"}\0a\00")

  ;; Length of the NUL-terminated string at $p.
  (func $strlen (param $p i32) (result i32)
    (local $n i32)
    (block $done
      (loop $l
        (br_if $done (i32.eqz (i32.load8_u (i32.add (local.get $p) (local.get $n)))))
        (local.set $n (i32.add (local.get $n) (i32.const 1)))
        (br $l)))
    (local.get $n))

  ;; Write the NUL-terminated string at $p to stdout: one wire frame.
  (func $emit (param $p i32)
    (i32.store (i32.const 0) (local.get $p))
    (i32.store (i32.const 4) (call $strlen (local.get $p)))
    (drop (call $fd_write (i32.const 1) (i32.const 0) (i32.const 1) (i32.const 16))))

  ;; Write $len bytes of scratch memory to $fd.
  (func $spew (param $fd i32) (param $len i32)
    (i32.store (i32.const 0) (i32.const 12288))
    (i32.store (i32.const 4) (local.get $len))
    (drop (call $fd_write (local.get $fd) (i32.const 0) (i32.const 1) (i32.const 16))))

  ;; Read one newline-terminated frame from stdin into the input buffer at
  ;; 8192; returns its length, newline excluded.
  (func $read_line (result i32)
    (local $n i32)
    (block $done
      (loop $l
        (i32.store (i32.const 0) (i32.add (i32.const 65536) (local.get $n)))
        (i32.store (i32.const 4) (i32.const 1))
        (i32.store (i32.const 16) (i32.const 0))
        (br_if $done (i32.ne (call $fd_read (i32.const 0) (i32.const 0) (i32.const 1) (i32.const 16)) (i32.const 0)))
        (br_if $done (i32.eqz (i32.load (i32.const 16))))
        (br_if $done (i32.eq (i32.load8_u (i32.add (i32.const 65536) (local.get $n))) (i32.const 10)))
        (local.set $n (i32.add (local.get $n) (i32.const 1)))
        (br_if $done (i32.ge_u (local.get $n) (i32.const 40000)))
        (br $l)))
    (local.get $n))

  ;; Naive substring search of the NUL-terminated needle at $needle inside the
  ;; first $len bytes of the input buffer.
  (func $contains (param $len i32) (param $needle i32) (result i32)
    (local $i i32) (local $j i32) (local $nl i32)
    (local.set $nl (call $strlen (local.get $needle)))
    (if (i32.gt_u (local.get $nl) (local.get $len)) (then (return (i32.const 0))))
    (block $no
      (loop $outer
        (br_if $no (i32.gt_u (i32.add (local.get $i) (local.get $nl)) (local.get $len)))
        (local.set $j (i32.const 0))
        (block $mismatch
          (loop $inner
            (br_if $mismatch (i32.ne
              (i32.load8_u (i32.add (i32.const 65536) (i32.add (local.get $i) (local.get $j))))
              (i32.load8_u (i32.add (local.get $needle) (local.get $j)))))
            (local.set $j (i32.add (local.get $j) (i32.const 1)))
            (br_if $inner (i32.lt_u (local.get $j) (local.get $nl))))
          (return (i32.const 1)))
        (local.set $i (i32.add (local.get $i) (i32.const 1)))
        (br $outer)))
    (i32.const 0))

  ;; Answer the request the way an honest hello-world plugin would.
  (func $answer
    (local $len i32)
    (local.set $len (call $read_line))
    (if (i32.eqz (call $contains (local.get $len) (i32.const 1024)))
      (then (call $emit (i32.const 768)) (return)))
    (if (call $contains (local.get $len) (i32.const 1088))
      (then (call $emit (i32.const 128)) (return)))
    (call $emit (i32.const 512)))
  (func (export "_start")
    (drop (call $poll_oneoff (i32.const 4096) (i32.const 5120) (i32.const 1) (i32.const 16)))
    (call $answer)
  )
)
"#;

/// Imports a database seam that does not exist. Refused at load: the linker's
/// world is closed, so an import nothing defines is not a runtime error a
/// guest can retry — the artifact never runs.
pub const DATABASE: &str = r#"(module
  (import "wasi_snapshot_preview1" "fd_read" (func $fd_read (param i32 i32 i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "fd_write" (func $fd_write (param i32 i32 i32 i32) (result i32)))
  (import "autumn_db" "query" (func $db_query (param i32 i32) (result i32)))
  (memory (export "memory") 2 4)
  (data (i32.const 128) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"aGVsbG8gZnJvbSB0aGUgc2FuZGJveA==\"}\0a\00")
  (data (i32.const 512) "{\"op\":\"response\",\"status\":404,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"bm8gc3VjaCBzYW5kYm94ZWQgcm91dGU=\"}\0a\00")
  (data (i32.const 768) "{\"op\":\"response\",\"status\":405,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"bWV0aG9kIG5vdCBhbGxvd2Vk\"}\0a\00")
  (data (i32.const 1024) "\"method\":\"GET\"\00")
  (data (i32.const 1088) "/greet\00")
  (data (i32.const 1120) "/etc/passwd\00")
  (data (i32.const 1152) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"],[\"set-cookie\",\"session=forged\"]],\"body_b64\":\"Y29va2llIGF0dGVtcHQ=\"}\0a\00")
  (data (i32.const 1600) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"],[\"x-evil\",\"a\\r\\nset-cookie: forged=1\"]],\"body_b64\":\"c3BsaXR0aW5nIGF0dGVtcHQ=\"}\0a\00")
  (data (i32.const 2048) "{\"op\":\"response\",\"status\":200,\0a\00")
  (data (i32.const 2176) "{\"op\":\"exec\",\"cmd\":\"/bin/sh\"}\0a\00")
  (data (i32.const 2304) "{\"op\":\"response\",\"status\":999,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"aW1wb3NzaWJsZSBzdGF0dXM=\"}\0a\00")
  (data (i32.const 2816) "X\00")
  (data (i32.const 3072) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"c2Vjb25kIGFuc3dlcg==\"}\0a\00")
  (data (i32.const 3584) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"],[\"x-autumn-sandboxed\",\"not-this-plugin\"],[\"x-content-type-options\",\"off\"]],\"body_b64\":\"YXR0cmlidXRpb24gYXR0ZW1wdA==\"}\0a\00")
  (data (i32.const 4200) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"aGVsbG8gZnJvbSB0aGUgc2FuZGJveA==\"}\00")
  (data (i32.const 5000) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"ZW50cm9weSBlY2hvZWQgaW4gdGhlIHN0YXR1cw==\"}\0a\00")
  (data (i32.const 6000) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"],[\"x-accel-redirect\",\"/internal/etc/passwd\"],[\"x-sendfile\",\"/etc/passwd\"]],\"body_b64\":\"cHJveHkgcmVkaXJlY3QgYXR0ZW1wdA==\"}\0a\00")
  (data (i32.const 6400) "\ff\fe\fd\fc\0a\00")
  (data (i32.const 6500) "{\"op\":\"error\",\"detail\":\"\\n2026-01-01T00:00:00Z  INFO forged: the host said this\\u001b[2K\\rBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB\"}\0a\00")

  ;; Length of the NUL-terminated string at $p.
  (func $strlen (param $p i32) (result i32)
    (local $n i32)
    (block $done
      (loop $l
        (br_if $done (i32.eqz (i32.load8_u (i32.add (local.get $p) (local.get $n)))))
        (local.set $n (i32.add (local.get $n) (i32.const 1)))
        (br $l)))
    (local.get $n))

  ;; Write the NUL-terminated string at $p to stdout: one wire frame.
  (func $emit (param $p i32)
    (i32.store (i32.const 0) (local.get $p))
    (i32.store (i32.const 4) (call $strlen (local.get $p)))
    (drop (call $fd_write (i32.const 1) (i32.const 0) (i32.const 1) (i32.const 16))))

  ;; Write $len bytes of scratch memory to $fd.
  (func $spew (param $fd i32) (param $len i32)
    (i32.store (i32.const 0) (i32.const 12288))
    (i32.store (i32.const 4) (local.get $len))
    (drop (call $fd_write (local.get $fd) (i32.const 0) (i32.const 1) (i32.const 16))))

  ;; Read one newline-terminated frame from stdin into the input buffer at
  ;; 8192; returns its length, newline excluded.
  (func $read_line (result i32)
    (local $n i32)
    (block $done
      (loop $l
        (i32.store (i32.const 0) (i32.add (i32.const 65536) (local.get $n)))
        (i32.store (i32.const 4) (i32.const 1))
        (i32.store (i32.const 16) (i32.const 0))
        (br_if $done (i32.ne (call $fd_read (i32.const 0) (i32.const 0) (i32.const 1) (i32.const 16)) (i32.const 0)))
        (br_if $done (i32.eqz (i32.load (i32.const 16))))
        (br_if $done (i32.eq (i32.load8_u (i32.add (i32.const 65536) (local.get $n))) (i32.const 10)))
        (local.set $n (i32.add (local.get $n) (i32.const 1)))
        (br_if $done (i32.ge_u (local.get $n) (i32.const 40000)))
        (br $l)))
    (local.get $n))

  ;; Naive substring search of the NUL-terminated needle at $needle inside the
  ;; first $len bytes of the input buffer.
  (func $contains (param $len i32) (param $needle i32) (result i32)
    (local $i i32) (local $j i32) (local $nl i32)
    (local.set $nl (call $strlen (local.get $needle)))
    (if (i32.gt_u (local.get $nl) (local.get $len)) (then (return (i32.const 0))))
    (block $no
      (loop $outer
        (br_if $no (i32.gt_u (i32.add (local.get $i) (local.get $nl)) (local.get $len)))
        (local.set $j (i32.const 0))
        (block $mismatch
          (loop $inner
            (br_if $mismatch (i32.ne
              (i32.load8_u (i32.add (i32.const 65536) (i32.add (local.get $i) (local.get $j))))
              (i32.load8_u (i32.add (local.get $needle) (local.get $j)))))
            (local.set $j (i32.add (local.get $j) (i32.const 1)))
            (br_if $inner (i32.lt_u (local.get $j) (local.get $nl))))
          (return (i32.const 1)))
        (local.set $i (i32.add (local.get $i) (i32.const 1)))
        (br $outer)))
    (i32.const 0))

  ;; Answer the request the way an honest hello-world plugin would.
  (func $answer
    (local $len i32)
    (local.set $len (call $read_line))
    (if (i32.eqz (call $contains (local.get $len) (i32.const 1024)))
      (then (call $emit (i32.const 768)) (return)))
    (if (call $contains (local.get $len) (i32.const 1088))
      (then (call $emit (i32.const 128)) (return)))
    (call $emit (i32.const 512)))
  (func (export "_start")
    (drop (call $db_query (i32.const 0) (i32.const 0)))
    (call $answer)
  )
)
"#;

/// Imports a host escape from a namespace of its own invention.
pub const HOST_COMMAND: &str = r#"(module
  (import "wasi_snapshot_preview1" "fd_read" (func $fd_read (param i32 i32 i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "fd_write" (func $fd_write (param i32 i32 i32 i32) (result i32)))
  (import "env" "system" (func $system (param i32) (result i32)))
  (memory (export "memory") 2 4)
  (data (i32.const 128) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"aGVsbG8gZnJvbSB0aGUgc2FuZGJveA==\"}\0a\00")
  (data (i32.const 512) "{\"op\":\"response\",\"status\":404,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"bm8gc3VjaCBzYW5kYm94ZWQgcm91dGU=\"}\0a\00")
  (data (i32.const 768) "{\"op\":\"response\",\"status\":405,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"bWV0aG9kIG5vdCBhbGxvd2Vk\"}\0a\00")
  (data (i32.const 1024) "\"method\":\"GET\"\00")
  (data (i32.const 1088) "/greet\00")
  (data (i32.const 1120) "/etc/passwd\00")
  (data (i32.const 1152) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"],[\"set-cookie\",\"session=forged\"]],\"body_b64\":\"Y29va2llIGF0dGVtcHQ=\"}\0a\00")
  (data (i32.const 1600) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"],[\"x-evil\",\"a\\r\\nset-cookie: forged=1\"]],\"body_b64\":\"c3BsaXR0aW5nIGF0dGVtcHQ=\"}\0a\00")
  (data (i32.const 2048) "{\"op\":\"response\",\"status\":200,\0a\00")
  (data (i32.const 2176) "{\"op\":\"exec\",\"cmd\":\"/bin/sh\"}\0a\00")
  (data (i32.const 2304) "{\"op\":\"response\",\"status\":999,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"aW1wb3NzaWJsZSBzdGF0dXM=\"}\0a\00")
  (data (i32.const 2816) "X\00")
  (data (i32.const 3072) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"c2Vjb25kIGFuc3dlcg==\"}\0a\00")
  (data (i32.const 3584) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"],[\"x-autumn-sandboxed\",\"not-this-plugin\"],[\"x-content-type-options\",\"off\"]],\"body_b64\":\"YXR0cmlidXRpb24gYXR0ZW1wdA==\"}\0a\00")
  (data (i32.const 4200) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"aGVsbG8gZnJvbSB0aGUgc2FuZGJveA==\"}\00")
  (data (i32.const 5000) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"ZW50cm9weSBlY2hvZWQgaW4gdGhlIHN0YXR1cw==\"}\0a\00")
  (data (i32.const 6000) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"],[\"x-accel-redirect\",\"/internal/etc/passwd\"],[\"x-sendfile\",\"/etc/passwd\"]],\"body_b64\":\"cHJveHkgcmVkaXJlY3QgYXR0ZW1wdA==\"}\0a\00")
  (data (i32.const 6400) "\ff\fe\fd\fc\0a\00")
  (data (i32.const 6500) "{\"op\":\"error\",\"detail\":\"\\n2026-01-01T00:00:00Z  INFO forged: the host said this\\u001b[2K\\rBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB\"}\0a\00")

  ;; Length of the NUL-terminated string at $p.
  (func $strlen (param $p i32) (result i32)
    (local $n i32)
    (block $done
      (loop $l
        (br_if $done (i32.eqz (i32.load8_u (i32.add (local.get $p) (local.get $n)))))
        (local.set $n (i32.add (local.get $n) (i32.const 1)))
        (br $l)))
    (local.get $n))

  ;; Write the NUL-terminated string at $p to stdout: one wire frame.
  (func $emit (param $p i32)
    (i32.store (i32.const 0) (local.get $p))
    (i32.store (i32.const 4) (call $strlen (local.get $p)))
    (drop (call $fd_write (i32.const 1) (i32.const 0) (i32.const 1) (i32.const 16))))

  ;; Write $len bytes of scratch memory to $fd.
  (func $spew (param $fd i32) (param $len i32)
    (i32.store (i32.const 0) (i32.const 12288))
    (i32.store (i32.const 4) (local.get $len))
    (drop (call $fd_write (local.get $fd) (i32.const 0) (i32.const 1) (i32.const 16))))

  ;; Read one newline-terminated frame from stdin into the input buffer at
  ;; 8192; returns its length, newline excluded.
  (func $read_line (result i32)
    (local $n i32)
    (block $done
      (loop $l
        (i32.store (i32.const 0) (i32.add (i32.const 65536) (local.get $n)))
        (i32.store (i32.const 4) (i32.const 1))
        (i32.store (i32.const 16) (i32.const 0))
        (br_if $done (i32.ne (call $fd_read (i32.const 0) (i32.const 0) (i32.const 1) (i32.const 16)) (i32.const 0)))
        (br_if $done (i32.eqz (i32.load (i32.const 16))))
        (br_if $done (i32.eq (i32.load8_u (i32.add (i32.const 65536) (local.get $n))) (i32.const 10)))
        (local.set $n (i32.add (local.get $n) (i32.const 1)))
        (br_if $done (i32.ge_u (local.get $n) (i32.const 40000)))
        (br $l)))
    (local.get $n))

  ;; Naive substring search of the NUL-terminated needle at $needle inside the
  ;; first $len bytes of the input buffer.
  (func $contains (param $len i32) (param $needle i32) (result i32)
    (local $i i32) (local $j i32) (local $nl i32)
    (local.set $nl (call $strlen (local.get $needle)))
    (if (i32.gt_u (local.get $nl) (local.get $len)) (then (return (i32.const 0))))
    (block $no
      (loop $outer
        (br_if $no (i32.gt_u (i32.add (local.get $i) (local.get $nl)) (local.get $len)))
        (local.set $j (i32.const 0))
        (block $mismatch
          (loop $inner
            (br_if $mismatch (i32.ne
              (i32.load8_u (i32.add (i32.const 65536) (i32.add (local.get $i) (local.get $j))))
              (i32.load8_u (i32.add (local.get $needle) (local.get $j)))))
            (local.set $j (i32.add (local.get $j) (i32.const 1)))
            (br_if $inner (i32.lt_u (local.get $j) (local.get $nl))))
          (return (i32.const 1)))
        (local.set $i (i32.add (local.get $i) (i32.const 1)))
        (br $outer)))
    (i32.const 0))

  ;; Answer the request the way an honest hello-world plugin would.
  (func $answer
    (local $len i32)
    (local.set $len (call $read_line))
    (if (i32.eqz (call $contains (local.get $len) (i32.const 1024)))
      (then (call $emit (i32.const 768)) (return)))
    (if (call $contains (local.get $len) (i32.const 1088))
      (then (call $emit (i32.const 128)) (return)))
    (call $emit (i32.const 512)))
  (func (export "_start")
    (drop (call $system (i32.const 0)))
    (call $answer)
  )
)
"#;

/// Imports a WASI function this shim does not implement at all.
pub const UNDEFINED_WASI: &str = r#"(module
  (import "wasi_snapshot_preview1" "fd_read" (func $fd_read (param i32 i32 i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "fd_write" (func $fd_write (param i32 i32 i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "sock_connect" (func $sock_connect (param i32 i32 i32) (result i32)))
  (memory (export "memory") 2 4)
  (data (i32.const 128) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"aGVsbG8gZnJvbSB0aGUgc2FuZGJveA==\"}\0a\00")
  (data (i32.const 512) "{\"op\":\"response\",\"status\":404,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"bm8gc3VjaCBzYW5kYm94ZWQgcm91dGU=\"}\0a\00")
  (data (i32.const 768) "{\"op\":\"response\",\"status\":405,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"bWV0aG9kIG5vdCBhbGxvd2Vk\"}\0a\00")
  (data (i32.const 1024) "\"method\":\"GET\"\00")
  (data (i32.const 1088) "/greet\00")
  (data (i32.const 1120) "/etc/passwd\00")
  (data (i32.const 1152) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"],[\"set-cookie\",\"session=forged\"]],\"body_b64\":\"Y29va2llIGF0dGVtcHQ=\"}\0a\00")
  (data (i32.const 1600) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"],[\"x-evil\",\"a\\r\\nset-cookie: forged=1\"]],\"body_b64\":\"c3BsaXR0aW5nIGF0dGVtcHQ=\"}\0a\00")
  (data (i32.const 2048) "{\"op\":\"response\",\"status\":200,\0a\00")
  (data (i32.const 2176) "{\"op\":\"exec\",\"cmd\":\"/bin/sh\"}\0a\00")
  (data (i32.const 2304) "{\"op\":\"response\",\"status\":999,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"aW1wb3NzaWJsZSBzdGF0dXM=\"}\0a\00")
  (data (i32.const 2816) "X\00")
  (data (i32.const 3072) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"c2Vjb25kIGFuc3dlcg==\"}\0a\00")
  (data (i32.const 3584) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"],[\"x-autumn-sandboxed\",\"not-this-plugin\"],[\"x-content-type-options\",\"off\"]],\"body_b64\":\"YXR0cmlidXRpb24gYXR0ZW1wdA==\"}\0a\00")
  (data (i32.const 4200) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"aGVsbG8gZnJvbSB0aGUgc2FuZGJveA==\"}\00")
  (data (i32.const 5000) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"ZW50cm9weSBlY2hvZWQgaW4gdGhlIHN0YXR1cw==\"}\0a\00")
  (data (i32.const 6000) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"],[\"x-accel-redirect\",\"/internal/etc/passwd\"],[\"x-sendfile\",\"/etc/passwd\"]],\"body_b64\":\"cHJveHkgcmVkaXJlY3QgYXR0ZW1wdA==\"}\0a\00")
  (data (i32.const 6400) "\ff\fe\fd\fc\0a\00")
  (data (i32.const 6500) "{\"op\":\"error\",\"detail\":\"\\n2026-01-01T00:00:00Z  INFO forged: the host said this\\u001b[2K\\rBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB\"}\0a\00")

  ;; Length of the NUL-terminated string at $p.
  (func $strlen (param $p i32) (result i32)
    (local $n i32)
    (block $done
      (loop $l
        (br_if $done (i32.eqz (i32.load8_u (i32.add (local.get $p) (local.get $n)))))
        (local.set $n (i32.add (local.get $n) (i32.const 1)))
        (br $l)))
    (local.get $n))

  ;; Write the NUL-terminated string at $p to stdout: one wire frame.
  (func $emit (param $p i32)
    (i32.store (i32.const 0) (local.get $p))
    (i32.store (i32.const 4) (call $strlen (local.get $p)))
    (drop (call $fd_write (i32.const 1) (i32.const 0) (i32.const 1) (i32.const 16))))

  ;; Write $len bytes of scratch memory to $fd.
  (func $spew (param $fd i32) (param $len i32)
    (i32.store (i32.const 0) (i32.const 12288))
    (i32.store (i32.const 4) (local.get $len))
    (drop (call $fd_write (local.get $fd) (i32.const 0) (i32.const 1) (i32.const 16))))

  ;; Read one newline-terminated frame from stdin into the input buffer at
  ;; 8192; returns its length, newline excluded.
  (func $read_line (result i32)
    (local $n i32)
    (block $done
      (loop $l
        (i32.store (i32.const 0) (i32.add (i32.const 65536) (local.get $n)))
        (i32.store (i32.const 4) (i32.const 1))
        (i32.store (i32.const 16) (i32.const 0))
        (br_if $done (i32.ne (call $fd_read (i32.const 0) (i32.const 0) (i32.const 1) (i32.const 16)) (i32.const 0)))
        (br_if $done (i32.eqz (i32.load (i32.const 16))))
        (br_if $done (i32.eq (i32.load8_u (i32.add (i32.const 65536) (local.get $n))) (i32.const 10)))
        (local.set $n (i32.add (local.get $n) (i32.const 1)))
        (br_if $done (i32.ge_u (local.get $n) (i32.const 40000)))
        (br $l)))
    (local.get $n))

  ;; Naive substring search of the NUL-terminated needle at $needle inside the
  ;; first $len bytes of the input buffer.
  (func $contains (param $len i32) (param $needle i32) (result i32)
    (local $i i32) (local $j i32) (local $nl i32)
    (local.set $nl (call $strlen (local.get $needle)))
    (if (i32.gt_u (local.get $nl) (local.get $len)) (then (return (i32.const 0))))
    (block $no
      (loop $outer
        (br_if $no (i32.gt_u (i32.add (local.get $i) (local.get $nl)) (local.get $len)))
        (local.set $j (i32.const 0))
        (block $mismatch
          (loop $inner
            (br_if $mismatch (i32.ne
              (i32.load8_u (i32.add (i32.const 65536) (i32.add (local.get $i) (local.get $j))))
              (i32.load8_u (i32.add (local.get $needle) (local.get $j)))))
            (local.set $j (i32.add (local.get $j) (i32.const 1)))
            (br_if $inner (i32.lt_u (local.get $j) (local.get $nl))))
          (return (i32.const 1)))
        (local.set $i (i32.add (local.get $i) (i32.const 1)))
        (br $outer)))
    (i32.const 0))

  ;; Answer the request the way an honest hello-world plugin would.
  (func $answer
    (local $len i32)
    (local.set $len (call $read_line))
    (if (i32.eqz (call $contains (local.get $len) (i32.const 1024)))
      (then (call $emit (i32.const 768)) (return)))
    (if (call $contains (local.get $len) (i32.const 1088))
      (then (call $emit (i32.const 128)) (return)))
    (call $emit (i32.const 512)))
  (func (export "_start")
    (drop (call $sock_connect (i32.const 3) (i32.const 0) (i32.const 0)))
    (call $answer)
  )
)
"#;

/// Answers with a complete line that is not a frame.
pub const MALFORMED_FRAME: &str = r#"(module
  (import "wasi_snapshot_preview1" "fd_read" (func $fd_read (param i32 i32 i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "fd_write" (func $fd_write (param i32 i32 i32 i32) (result i32)))
  (memory (export "memory") 2 4)
  (data (i32.const 128) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"aGVsbG8gZnJvbSB0aGUgc2FuZGJveA==\"}\0a\00")
  (data (i32.const 512) "{\"op\":\"response\",\"status\":404,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"bm8gc3VjaCBzYW5kYm94ZWQgcm91dGU=\"}\0a\00")
  (data (i32.const 768) "{\"op\":\"response\",\"status\":405,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"bWV0aG9kIG5vdCBhbGxvd2Vk\"}\0a\00")
  (data (i32.const 1024) "\"method\":\"GET\"\00")
  (data (i32.const 1088) "/greet\00")
  (data (i32.const 1120) "/etc/passwd\00")
  (data (i32.const 1152) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"],[\"set-cookie\",\"session=forged\"]],\"body_b64\":\"Y29va2llIGF0dGVtcHQ=\"}\0a\00")
  (data (i32.const 1600) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"],[\"x-evil\",\"a\\r\\nset-cookie: forged=1\"]],\"body_b64\":\"c3BsaXR0aW5nIGF0dGVtcHQ=\"}\0a\00")
  (data (i32.const 2048) "{\"op\":\"response\",\"status\":200,\0a\00")
  (data (i32.const 2176) "{\"op\":\"exec\",\"cmd\":\"/bin/sh\"}\0a\00")
  (data (i32.const 2304) "{\"op\":\"response\",\"status\":999,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"aW1wb3NzaWJsZSBzdGF0dXM=\"}\0a\00")
  (data (i32.const 2816) "X\00")
  (data (i32.const 3072) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"c2Vjb25kIGFuc3dlcg==\"}\0a\00")
  (data (i32.const 3584) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"],[\"x-autumn-sandboxed\",\"not-this-plugin\"],[\"x-content-type-options\",\"off\"]],\"body_b64\":\"YXR0cmlidXRpb24gYXR0ZW1wdA==\"}\0a\00")
  (data (i32.const 4200) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"aGVsbG8gZnJvbSB0aGUgc2FuZGJveA==\"}\00")
  (data (i32.const 5000) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"ZW50cm9weSBlY2hvZWQgaW4gdGhlIHN0YXR1cw==\"}\0a\00")
  (data (i32.const 6000) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"],[\"x-accel-redirect\",\"/internal/etc/passwd\"],[\"x-sendfile\",\"/etc/passwd\"]],\"body_b64\":\"cHJveHkgcmVkaXJlY3QgYXR0ZW1wdA==\"}\0a\00")
  (data (i32.const 6400) "\ff\fe\fd\fc\0a\00")
  (data (i32.const 6500) "{\"op\":\"error\",\"detail\":\"\\n2026-01-01T00:00:00Z  INFO forged: the host said this\\u001b[2K\\rBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB\"}\0a\00")

  ;; Length of the NUL-terminated string at $p.
  (func $strlen (param $p i32) (result i32)
    (local $n i32)
    (block $done
      (loop $l
        (br_if $done (i32.eqz (i32.load8_u (i32.add (local.get $p) (local.get $n)))))
        (local.set $n (i32.add (local.get $n) (i32.const 1)))
        (br $l)))
    (local.get $n))

  ;; Write the NUL-terminated string at $p to stdout: one wire frame.
  (func $emit (param $p i32)
    (i32.store (i32.const 0) (local.get $p))
    (i32.store (i32.const 4) (call $strlen (local.get $p)))
    (drop (call $fd_write (i32.const 1) (i32.const 0) (i32.const 1) (i32.const 16))))

  ;; Write $len bytes of scratch memory to $fd.
  (func $spew (param $fd i32) (param $len i32)
    (i32.store (i32.const 0) (i32.const 12288))
    (i32.store (i32.const 4) (local.get $len))
    (drop (call $fd_write (local.get $fd) (i32.const 0) (i32.const 1) (i32.const 16))))

  ;; Read one newline-terminated frame from stdin into the input buffer at
  ;; 8192; returns its length, newline excluded.
  (func $read_line (result i32)
    (local $n i32)
    (block $done
      (loop $l
        (i32.store (i32.const 0) (i32.add (i32.const 65536) (local.get $n)))
        (i32.store (i32.const 4) (i32.const 1))
        (i32.store (i32.const 16) (i32.const 0))
        (br_if $done (i32.ne (call $fd_read (i32.const 0) (i32.const 0) (i32.const 1) (i32.const 16)) (i32.const 0)))
        (br_if $done (i32.eqz (i32.load (i32.const 16))))
        (br_if $done (i32.eq (i32.load8_u (i32.add (i32.const 65536) (local.get $n))) (i32.const 10)))
        (local.set $n (i32.add (local.get $n) (i32.const 1)))
        (br_if $done (i32.ge_u (local.get $n) (i32.const 40000)))
        (br $l)))
    (local.get $n))

  ;; Naive substring search of the NUL-terminated needle at $needle inside the
  ;; first $len bytes of the input buffer.
  (func $contains (param $len i32) (param $needle i32) (result i32)
    (local $i i32) (local $j i32) (local $nl i32)
    (local.set $nl (call $strlen (local.get $needle)))
    (if (i32.gt_u (local.get $nl) (local.get $len)) (then (return (i32.const 0))))
    (block $no
      (loop $outer
        (br_if $no (i32.gt_u (i32.add (local.get $i) (local.get $nl)) (local.get $len)))
        (local.set $j (i32.const 0))
        (block $mismatch
          (loop $inner
            (br_if $mismatch (i32.ne
              (i32.load8_u (i32.add (i32.const 65536) (i32.add (local.get $i) (local.get $j))))
              (i32.load8_u (i32.add (local.get $needle) (local.get $j)))))
            (local.set $j (i32.add (local.get $j) (i32.const 1)))
            (br_if $inner (i32.lt_u (local.get $j) (local.get $nl))))
          (return (i32.const 1)))
        (local.set $i (i32.add (local.get $i) (i32.const 1)))
        (br $outer)))
    (i32.const 0))

  ;; Answer the request the way an honest hello-world plugin would.
  (func $answer
    (local $len i32)
    (local.set $len (call $read_line))
    (if (i32.eqz (call $contains (local.get $len) (i32.const 1024)))
      (then (call $emit (i32.const 768)) (return)))
    (if (call $contains (local.get $len) (i32.const 1088))
      (then (call $emit (i32.const 128)) (return)))
    (call $emit (i32.const 512)))
  (func (export "_start")
    (call $emit (i32.const 2048))
  )
)
"#;

/// Answers with a complete line of bytes that are not UTF-8 — the shape that
/// makes a lossy decode expand each byte threefold while the original is still
/// held.
pub const INVALID_UTF8: &str = r#"(module
  (import "wasi_snapshot_preview1" "fd_read" (func $fd_read (param i32 i32 i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "fd_write" (func $fd_write (param i32 i32 i32 i32) (result i32)))
  (memory (export "memory") 2 4)
  (data (i32.const 128) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"aGVsbG8gZnJvbSB0aGUgc2FuZGJveA==\"}\0a\00")
  (data (i32.const 512) "{\"op\":\"response\",\"status\":404,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"bm8gc3VjaCBzYW5kYm94ZWQgcm91dGU=\"}\0a\00")
  (data (i32.const 768) "{\"op\":\"response\",\"status\":405,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"bWV0aG9kIG5vdCBhbGxvd2Vk\"}\0a\00")
  (data (i32.const 1024) "\"method\":\"GET\"\00")
  (data (i32.const 1088) "/greet\00")
  (data (i32.const 1120) "/etc/passwd\00")
  (data (i32.const 1152) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"],[\"set-cookie\",\"session=forged\"]],\"body_b64\":\"Y29va2llIGF0dGVtcHQ=\"}\0a\00")
  (data (i32.const 1600) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"],[\"x-evil\",\"a\\r\\nset-cookie: forged=1\"]],\"body_b64\":\"c3BsaXR0aW5nIGF0dGVtcHQ=\"}\0a\00")
  (data (i32.const 2048) "{\"op\":\"response\",\"status\":200,\0a\00")
  (data (i32.const 2176) "{\"op\":\"exec\",\"cmd\":\"/bin/sh\"}\0a\00")
  (data (i32.const 2304) "{\"op\":\"response\",\"status\":999,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"aW1wb3NzaWJsZSBzdGF0dXM=\"}\0a\00")
  (data (i32.const 2816) "X\00")
  (data (i32.const 3072) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"c2Vjb25kIGFuc3dlcg==\"}\0a\00")
  (data (i32.const 3584) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"],[\"x-autumn-sandboxed\",\"not-this-plugin\"],[\"x-content-type-options\",\"off\"]],\"body_b64\":\"YXR0cmlidXRpb24gYXR0ZW1wdA==\"}\0a\00")
  (data (i32.const 4200) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"aGVsbG8gZnJvbSB0aGUgc2FuZGJveA==\"}\00")
  (data (i32.const 5000) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"ZW50cm9weSBlY2hvZWQgaW4gdGhlIHN0YXR1cw==\"}\0a\00")
  (data (i32.const 6000) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"],[\"x-accel-redirect\",\"/internal/etc/passwd\"],[\"x-sendfile\",\"/etc/passwd\"]],\"body_b64\":\"cHJveHkgcmVkaXJlY3QgYXR0ZW1wdA==\"}\0a\00")
  (data (i32.const 6400) "\ff\fe\fd\fc\0a\00")
  (data (i32.const 6500) "{\"op\":\"error\",\"detail\":\"\\n2026-01-01T00:00:00Z  INFO forged: the host said this\\u001b[2K\\rBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB\"}\0a\00")

  ;; Length of the NUL-terminated string at $p.
  (func $strlen (param $p i32) (result i32)
    (local $n i32)
    (block $done
      (loop $l
        (br_if $done (i32.eqz (i32.load8_u (i32.add (local.get $p) (local.get $n)))))
        (local.set $n (i32.add (local.get $n) (i32.const 1)))
        (br $l)))
    (local.get $n))

  ;; Write the NUL-terminated string at $p to stdout: one wire frame.
  (func $emit (param $p i32)
    (i32.store (i32.const 0) (local.get $p))
    (i32.store (i32.const 4) (call $strlen (local.get $p)))
    (drop (call $fd_write (i32.const 1) (i32.const 0) (i32.const 1) (i32.const 16))))

  ;; Write $len bytes of scratch memory to $fd.
  (func $spew (param $fd i32) (param $len i32)
    (i32.store (i32.const 0) (i32.const 12288))
    (i32.store (i32.const 4) (local.get $len))
    (drop (call $fd_write (local.get $fd) (i32.const 0) (i32.const 1) (i32.const 16))))

  ;; Read one newline-terminated frame from stdin into the input buffer at
  ;; 8192; returns its length, newline excluded.
  (func $read_line (result i32)
    (local $n i32)
    (block $done
      (loop $l
        (i32.store (i32.const 0) (i32.add (i32.const 65536) (local.get $n)))
        (i32.store (i32.const 4) (i32.const 1))
        (i32.store (i32.const 16) (i32.const 0))
        (br_if $done (i32.ne (call $fd_read (i32.const 0) (i32.const 0) (i32.const 1) (i32.const 16)) (i32.const 0)))
        (br_if $done (i32.eqz (i32.load (i32.const 16))))
        (br_if $done (i32.eq (i32.load8_u (i32.add (i32.const 65536) (local.get $n))) (i32.const 10)))
        (local.set $n (i32.add (local.get $n) (i32.const 1)))
        (br_if $done (i32.ge_u (local.get $n) (i32.const 40000)))
        (br $l)))
    (local.get $n))

  ;; Naive substring search of the NUL-terminated needle at $needle inside the
  ;; first $len bytes of the input buffer.
  (func $contains (param $len i32) (param $needle i32) (result i32)
    (local $i i32) (local $j i32) (local $nl i32)
    (local.set $nl (call $strlen (local.get $needle)))
    (if (i32.gt_u (local.get $nl) (local.get $len)) (then (return (i32.const 0))))
    (block $no
      (loop $outer
        (br_if $no (i32.gt_u (i32.add (local.get $i) (local.get $nl)) (local.get $len)))
        (local.set $j (i32.const 0))
        (block $mismatch
          (loop $inner
            (br_if $mismatch (i32.ne
              (i32.load8_u (i32.add (i32.const 65536) (i32.add (local.get $i) (local.get $j))))
              (i32.load8_u (i32.add (local.get $needle) (local.get $j)))))
            (local.set $j (i32.add (local.get $j) (i32.const 1)))
            (br_if $inner (i32.lt_u (local.get $j) (local.get $nl))))
          (return (i32.const 1)))
        (local.set $i (i32.add (local.get $i) (i32.const 1)))
        (br $outer)))
    (i32.const 0))

  ;; Answer the request the way an honest hello-world plugin would.
  (func $answer
    (local $len i32)
    (local.set $len (call $read_line))
    (if (i32.eqz (call $contains (local.get $len) (i32.const 1024)))
      (then (call $emit (i32.const 768)) (return)))
    (if (call $contains (local.get $len) (i32.const 1088))
      (then (call $emit (i32.const 128)) (return)))
    (call $emit (i32.const 512)))
  (func (export "_start")
    (call $emit (i32.const 6400))
  )
)
"#;

/// Answers with an op the wire does not define.
pub const UNKNOWN_OP: &str = r#"(module
  (import "wasi_snapshot_preview1" "fd_read" (func $fd_read (param i32 i32 i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "fd_write" (func $fd_write (param i32 i32 i32 i32) (result i32)))
  (memory (export "memory") 2 4)
  (data (i32.const 128) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"aGVsbG8gZnJvbSB0aGUgc2FuZGJveA==\"}\0a\00")
  (data (i32.const 512) "{\"op\":\"response\",\"status\":404,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"bm8gc3VjaCBzYW5kYm94ZWQgcm91dGU=\"}\0a\00")
  (data (i32.const 768) "{\"op\":\"response\",\"status\":405,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"bWV0aG9kIG5vdCBhbGxvd2Vk\"}\0a\00")
  (data (i32.const 1024) "\"method\":\"GET\"\00")
  (data (i32.const 1088) "/greet\00")
  (data (i32.const 1120) "/etc/passwd\00")
  (data (i32.const 1152) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"],[\"set-cookie\",\"session=forged\"]],\"body_b64\":\"Y29va2llIGF0dGVtcHQ=\"}\0a\00")
  (data (i32.const 1600) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"],[\"x-evil\",\"a\\r\\nset-cookie: forged=1\"]],\"body_b64\":\"c3BsaXR0aW5nIGF0dGVtcHQ=\"}\0a\00")
  (data (i32.const 2048) "{\"op\":\"response\",\"status\":200,\0a\00")
  (data (i32.const 2176) "{\"op\":\"exec\",\"cmd\":\"/bin/sh\"}\0a\00")
  (data (i32.const 2304) "{\"op\":\"response\",\"status\":999,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"aW1wb3NzaWJsZSBzdGF0dXM=\"}\0a\00")
  (data (i32.const 2816) "X\00")
  (data (i32.const 3072) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"c2Vjb25kIGFuc3dlcg==\"}\0a\00")
  (data (i32.const 3584) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"],[\"x-autumn-sandboxed\",\"not-this-plugin\"],[\"x-content-type-options\",\"off\"]],\"body_b64\":\"YXR0cmlidXRpb24gYXR0ZW1wdA==\"}\0a\00")
  (data (i32.const 4200) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"aGVsbG8gZnJvbSB0aGUgc2FuZGJveA==\"}\00")
  (data (i32.const 5000) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"ZW50cm9weSBlY2hvZWQgaW4gdGhlIHN0YXR1cw==\"}\0a\00")
  (data (i32.const 6000) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"],[\"x-accel-redirect\",\"/internal/etc/passwd\"],[\"x-sendfile\",\"/etc/passwd\"]],\"body_b64\":\"cHJveHkgcmVkaXJlY3QgYXR0ZW1wdA==\"}\0a\00")
  (data (i32.const 6400) "\ff\fe\fd\fc\0a\00")
  (data (i32.const 6500) "{\"op\":\"error\",\"detail\":\"\\n2026-01-01T00:00:00Z  INFO forged: the host said this\\u001b[2K\\rBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB\"}\0a\00")

  ;; Length of the NUL-terminated string at $p.
  (func $strlen (param $p i32) (result i32)
    (local $n i32)
    (block $done
      (loop $l
        (br_if $done (i32.eqz (i32.load8_u (i32.add (local.get $p) (local.get $n)))))
        (local.set $n (i32.add (local.get $n) (i32.const 1)))
        (br $l)))
    (local.get $n))

  ;; Write the NUL-terminated string at $p to stdout: one wire frame.
  (func $emit (param $p i32)
    (i32.store (i32.const 0) (local.get $p))
    (i32.store (i32.const 4) (call $strlen (local.get $p)))
    (drop (call $fd_write (i32.const 1) (i32.const 0) (i32.const 1) (i32.const 16))))

  ;; Write $len bytes of scratch memory to $fd.
  (func $spew (param $fd i32) (param $len i32)
    (i32.store (i32.const 0) (i32.const 12288))
    (i32.store (i32.const 4) (local.get $len))
    (drop (call $fd_write (local.get $fd) (i32.const 0) (i32.const 1) (i32.const 16))))

  ;; Read one newline-terminated frame from stdin into the input buffer at
  ;; 8192; returns its length, newline excluded.
  (func $read_line (result i32)
    (local $n i32)
    (block $done
      (loop $l
        (i32.store (i32.const 0) (i32.add (i32.const 65536) (local.get $n)))
        (i32.store (i32.const 4) (i32.const 1))
        (i32.store (i32.const 16) (i32.const 0))
        (br_if $done (i32.ne (call $fd_read (i32.const 0) (i32.const 0) (i32.const 1) (i32.const 16)) (i32.const 0)))
        (br_if $done (i32.eqz (i32.load (i32.const 16))))
        (br_if $done (i32.eq (i32.load8_u (i32.add (i32.const 65536) (local.get $n))) (i32.const 10)))
        (local.set $n (i32.add (local.get $n) (i32.const 1)))
        (br_if $done (i32.ge_u (local.get $n) (i32.const 40000)))
        (br $l)))
    (local.get $n))

  ;; Naive substring search of the NUL-terminated needle at $needle inside the
  ;; first $len bytes of the input buffer.
  (func $contains (param $len i32) (param $needle i32) (result i32)
    (local $i i32) (local $j i32) (local $nl i32)
    (local.set $nl (call $strlen (local.get $needle)))
    (if (i32.gt_u (local.get $nl) (local.get $len)) (then (return (i32.const 0))))
    (block $no
      (loop $outer
        (br_if $no (i32.gt_u (i32.add (local.get $i) (local.get $nl)) (local.get $len)))
        (local.set $j (i32.const 0))
        (block $mismatch
          (loop $inner
            (br_if $mismatch (i32.ne
              (i32.load8_u (i32.add (i32.const 65536) (i32.add (local.get $i) (local.get $j))))
              (i32.load8_u (i32.add (local.get $needle) (local.get $j)))))
            (local.set $j (i32.add (local.get $j) (i32.const 1)))
            (br_if $inner (i32.lt_u (local.get $j) (local.get $nl))))
          (return (i32.const 1)))
        (local.set $i (i32.add (local.get $i) (i32.const 1)))
        (br $outer)))
    (i32.const 0))

  ;; Answer the request the way an honest hello-world plugin would.
  (func $answer
    (local $len i32)
    (local.set $len (call $read_line))
    (if (i32.eqz (call $contains (local.get $len) (i32.const 1024)))
      (then (call $emit (i32.const 768)) (return)))
    (if (call $contains (local.get $len) (i32.const 1088))
      (then (call $emit (i32.const 128)) (return)))
    (call $emit (i32.const 512)))
  (func (export "_start")
    (call $emit (i32.const 2176))
  )
)
"#;

/// Answers with a status HTTP has no room for.
pub const IMPOSSIBLE_STATUS: &str = r#"(module
  (import "wasi_snapshot_preview1" "fd_read" (func $fd_read (param i32 i32 i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "fd_write" (func $fd_write (param i32 i32 i32 i32) (result i32)))
  (memory (export "memory") 2 4)
  (data (i32.const 128) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"aGVsbG8gZnJvbSB0aGUgc2FuZGJveA==\"}\0a\00")
  (data (i32.const 512) "{\"op\":\"response\",\"status\":404,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"bm8gc3VjaCBzYW5kYm94ZWQgcm91dGU=\"}\0a\00")
  (data (i32.const 768) "{\"op\":\"response\",\"status\":405,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"bWV0aG9kIG5vdCBhbGxvd2Vk\"}\0a\00")
  (data (i32.const 1024) "\"method\":\"GET\"\00")
  (data (i32.const 1088) "/greet\00")
  (data (i32.const 1120) "/etc/passwd\00")
  (data (i32.const 1152) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"],[\"set-cookie\",\"session=forged\"]],\"body_b64\":\"Y29va2llIGF0dGVtcHQ=\"}\0a\00")
  (data (i32.const 1600) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"],[\"x-evil\",\"a\\r\\nset-cookie: forged=1\"]],\"body_b64\":\"c3BsaXR0aW5nIGF0dGVtcHQ=\"}\0a\00")
  (data (i32.const 2048) "{\"op\":\"response\",\"status\":200,\0a\00")
  (data (i32.const 2176) "{\"op\":\"exec\",\"cmd\":\"/bin/sh\"}\0a\00")
  (data (i32.const 2304) "{\"op\":\"response\",\"status\":999,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"aW1wb3NzaWJsZSBzdGF0dXM=\"}\0a\00")
  (data (i32.const 2816) "X\00")
  (data (i32.const 3072) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"c2Vjb25kIGFuc3dlcg==\"}\0a\00")
  (data (i32.const 3584) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"],[\"x-autumn-sandboxed\",\"not-this-plugin\"],[\"x-content-type-options\",\"off\"]],\"body_b64\":\"YXR0cmlidXRpb24gYXR0ZW1wdA==\"}\0a\00")
  (data (i32.const 4200) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"aGVsbG8gZnJvbSB0aGUgc2FuZGJveA==\"}\00")
  (data (i32.const 5000) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"ZW50cm9weSBlY2hvZWQgaW4gdGhlIHN0YXR1cw==\"}\0a\00")
  (data (i32.const 6000) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"],[\"x-accel-redirect\",\"/internal/etc/passwd\"],[\"x-sendfile\",\"/etc/passwd\"]],\"body_b64\":\"cHJveHkgcmVkaXJlY3QgYXR0ZW1wdA==\"}\0a\00")
  (data (i32.const 6400) "\ff\fe\fd\fc\0a\00")
  (data (i32.const 6500) "{\"op\":\"error\",\"detail\":\"\\n2026-01-01T00:00:00Z  INFO forged: the host said this\\u001b[2K\\rBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB\"}\0a\00")

  ;; Length of the NUL-terminated string at $p.
  (func $strlen (param $p i32) (result i32)
    (local $n i32)
    (block $done
      (loop $l
        (br_if $done (i32.eqz (i32.load8_u (i32.add (local.get $p) (local.get $n)))))
        (local.set $n (i32.add (local.get $n) (i32.const 1)))
        (br $l)))
    (local.get $n))

  ;; Write the NUL-terminated string at $p to stdout: one wire frame.
  (func $emit (param $p i32)
    (i32.store (i32.const 0) (local.get $p))
    (i32.store (i32.const 4) (call $strlen (local.get $p)))
    (drop (call $fd_write (i32.const 1) (i32.const 0) (i32.const 1) (i32.const 16))))

  ;; Write $len bytes of scratch memory to $fd.
  (func $spew (param $fd i32) (param $len i32)
    (i32.store (i32.const 0) (i32.const 12288))
    (i32.store (i32.const 4) (local.get $len))
    (drop (call $fd_write (local.get $fd) (i32.const 0) (i32.const 1) (i32.const 16))))

  ;; Read one newline-terminated frame from stdin into the input buffer at
  ;; 8192; returns its length, newline excluded.
  (func $read_line (result i32)
    (local $n i32)
    (block $done
      (loop $l
        (i32.store (i32.const 0) (i32.add (i32.const 65536) (local.get $n)))
        (i32.store (i32.const 4) (i32.const 1))
        (i32.store (i32.const 16) (i32.const 0))
        (br_if $done (i32.ne (call $fd_read (i32.const 0) (i32.const 0) (i32.const 1) (i32.const 16)) (i32.const 0)))
        (br_if $done (i32.eqz (i32.load (i32.const 16))))
        (br_if $done (i32.eq (i32.load8_u (i32.add (i32.const 65536) (local.get $n))) (i32.const 10)))
        (local.set $n (i32.add (local.get $n) (i32.const 1)))
        (br_if $done (i32.ge_u (local.get $n) (i32.const 40000)))
        (br $l)))
    (local.get $n))

  ;; Naive substring search of the NUL-terminated needle at $needle inside the
  ;; first $len bytes of the input buffer.
  (func $contains (param $len i32) (param $needle i32) (result i32)
    (local $i i32) (local $j i32) (local $nl i32)
    (local.set $nl (call $strlen (local.get $needle)))
    (if (i32.gt_u (local.get $nl) (local.get $len)) (then (return (i32.const 0))))
    (block $no
      (loop $outer
        (br_if $no (i32.gt_u (i32.add (local.get $i) (local.get $nl)) (local.get $len)))
        (local.set $j (i32.const 0))
        (block $mismatch
          (loop $inner
            (br_if $mismatch (i32.ne
              (i32.load8_u (i32.add (i32.const 65536) (i32.add (local.get $i) (local.get $j))))
              (i32.load8_u (i32.add (local.get $needle) (local.get $j)))))
            (local.set $j (i32.add (local.get $j) (i32.const 1)))
            (br_if $inner (i32.lt_u (local.get $j) (local.get $nl))))
          (return (i32.const 1)))
        (local.set $i (i32.add (local.get $i) (i32.const 1)))
        (br $outer)))
    (i32.const 0))

  ;; Answer the request the way an honest hello-world plugin would.
  (func $answer
    (local $len i32)
    (local.set $len (call $read_line))
    (if (i32.eqz (call $contains (local.get $len) (i32.const 1024)))
      (then (call $emit (i32.const 768)) (return)))
    (if (call $contains (local.get $len) (i32.const 1088))
      (then (call $emit (i32.const 128)) (return)))
    (call $emit (i32.const 512)))
  (func (export "_start")
    (call $emit (i32.const 2304))
  )
)
"#;

/// Writes to stdout forever without ever ending a line, so nothing the host
/// has bounded so far would bound it.
pub const OUTPUT_FLOOD: &str = r#"(module
  (import "wasi_snapshot_preview1" "fd_read" (func $fd_read (param i32 i32 i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "fd_write" (func $fd_write (param i32 i32 i32 i32) (result i32)))
  (memory (export "memory") 2 4)
  (data (i32.const 128) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"aGVsbG8gZnJvbSB0aGUgc2FuZGJveA==\"}\0a\00")
  (data (i32.const 512) "{\"op\":\"response\",\"status\":404,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"bm8gc3VjaCBzYW5kYm94ZWQgcm91dGU=\"}\0a\00")
  (data (i32.const 768) "{\"op\":\"response\",\"status\":405,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"bWV0aG9kIG5vdCBhbGxvd2Vk\"}\0a\00")
  (data (i32.const 1024) "\"method\":\"GET\"\00")
  (data (i32.const 1088) "/greet\00")
  (data (i32.const 1120) "/etc/passwd\00")
  (data (i32.const 1152) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"],[\"set-cookie\",\"session=forged\"]],\"body_b64\":\"Y29va2llIGF0dGVtcHQ=\"}\0a\00")
  (data (i32.const 1600) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"],[\"x-evil\",\"a\\r\\nset-cookie: forged=1\"]],\"body_b64\":\"c3BsaXR0aW5nIGF0dGVtcHQ=\"}\0a\00")
  (data (i32.const 2048) "{\"op\":\"response\",\"status\":200,\0a\00")
  (data (i32.const 2176) "{\"op\":\"exec\",\"cmd\":\"/bin/sh\"}\0a\00")
  (data (i32.const 2304) "{\"op\":\"response\",\"status\":999,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"aW1wb3NzaWJsZSBzdGF0dXM=\"}\0a\00")
  (data (i32.const 2816) "X\00")
  (data (i32.const 3072) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"c2Vjb25kIGFuc3dlcg==\"}\0a\00")
  (data (i32.const 3584) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"],[\"x-autumn-sandboxed\",\"not-this-plugin\"],[\"x-content-type-options\",\"off\"]],\"body_b64\":\"YXR0cmlidXRpb24gYXR0ZW1wdA==\"}\0a\00")
  (data (i32.const 4200) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"aGVsbG8gZnJvbSB0aGUgc2FuZGJveA==\"}\00")
  (data (i32.const 5000) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"ZW50cm9weSBlY2hvZWQgaW4gdGhlIHN0YXR1cw==\"}\0a\00")
  (data (i32.const 6000) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"],[\"x-accel-redirect\",\"/internal/etc/passwd\"],[\"x-sendfile\",\"/etc/passwd\"]],\"body_b64\":\"cHJveHkgcmVkaXJlY3QgYXR0ZW1wdA==\"}\0a\00")
  (data (i32.const 6400) "\ff\fe\fd\fc\0a\00")
  (data (i32.const 6500) "{\"op\":\"error\",\"detail\":\"\\n2026-01-01T00:00:00Z  INFO forged: the host said this\\u001b[2K\\rBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB\"}\0a\00")

  ;; Length of the NUL-terminated string at $p.
  (func $strlen (param $p i32) (result i32)
    (local $n i32)
    (block $done
      (loop $l
        (br_if $done (i32.eqz (i32.load8_u (i32.add (local.get $p) (local.get $n)))))
        (local.set $n (i32.add (local.get $n) (i32.const 1)))
        (br $l)))
    (local.get $n))

  ;; Write the NUL-terminated string at $p to stdout: one wire frame.
  (func $emit (param $p i32)
    (i32.store (i32.const 0) (local.get $p))
    (i32.store (i32.const 4) (call $strlen (local.get $p)))
    (drop (call $fd_write (i32.const 1) (i32.const 0) (i32.const 1) (i32.const 16))))

  ;; Write $len bytes of scratch memory to $fd.
  (func $spew (param $fd i32) (param $len i32)
    (i32.store (i32.const 0) (i32.const 12288))
    (i32.store (i32.const 4) (local.get $len))
    (drop (call $fd_write (local.get $fd) (i32.const 0) (i32.const 1) (i32.const 16))))

  ;; Read one newline-terminated frame from stdin into the input buffer at
  ;; 8192; returns its length, newline excluded.
  (func $read_line (result i32)
    (local $n i32)
    (block $done
      (loop $l
        (i32.store (i32.const 0) (i32.add (i32.const 65536) (local.get $n)))
        (i32.store (i32.const 4) (i32.const 1))
        (i32.store (i32.const 16) (i32.const 0))
        (br_if $done (i32.ne (call $fd_read (i32.const 0) (i32.const 0) (i32.const 1) (i32.const 16)) (i32.const 0)))
        (br_if $done (i32.eqz (i32.load (i32.const 16))))
        (br_if $done (i32.eq (i32.load8_u (i32.add (i32.const 65536) (local.get $n))) (i32.const 10)))
        (local.set $n (i32.add (local.get $n) (i32.const 1)))
        (br_if $done (i32.ge_u (local.get $n) (i32.const 40000)))
        (br $l)))
    (local.get $n))

  ;; Naive substring search of the NUL-terminated needle at $needle inside the
  ;; first $len bytes of the input buffer.
  (func $contains (param $len i32) (param $needle i32) (result i32)
    (local $i i32) (local $j i32) (local $nl i32)
    (local.set $nl (call $strlen (local.get $needle)))
    (if (i32.gt_u (local.get $nl) (local.get $len)) (then (return (i32.const 0))))
    (block $no
      (loop $outer
        (br_if $no (i32.gt_u (i32.add (local.get $i) (local.get $nl)) (local.get $len)))
        (local.set $j (i32.const 0))
        (block $mismatch
          (loop $inner
            (br_if $mismatch (i32.ne
              (i32.load8_u (i32.add (i32.const 65536) (i32.add (local.get $i) (local.get $j))))
              (i32.load8_u (i32.add (local.get $needle) (local.get $j)))))
            (local.set $j (i32.add (local.get $j) (i32.const 1)))
            (br_if $inner (i32.lt_u (local.get $j) (local.get $nl))))
          (return (i32.const 1)))
        (local.set $i (i32.add (local.get $i) (i32.const 1)))
        (br $outer)))
    (i32.const 0))

  ;; Answer the request the way an honest hello-world plugin would.
  (func $answer
    (local $len i32)
    (local.set $len (call $read_line))
    (if (i32.eqz (call $contains (local.get $len) (i32.const 1024)))
      (then (call $emit (i32.const 768)) (return)))
    (if (call $contains (local.get $len) (i32.const 1088))
      (then (call $emit (i32.const 128)) (return)))
    (call $emit (i32.const 512)))
  (func (export "_start")
    (loop $l (call $emit (i32.const 2816)) (br $l))
  )
)
"#;

/// Writes 64 KiB to stderr forever. Stderr is discarded past its budget, so
/// only a fuel charge on the host-side copy bounds the work it costs.
pub const STDERR_FLOOD: &str = r#"(module
  (import "wasi_snapshot_preview1" "fd_read" (func $fd_read (param i32 i32 i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "fd_write" (func $fd_write (param i32 i32 i32 i32) (result i32)))
  (memory (export "memory") 2 4)
  (data (i32.const 128) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"aGVsbG8gZnJvbSB0aGUgc2FuZGJveA==\"}\0a\00")
  (data (i32.const 512) "{\"op\":\"response\",\"status\":404,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"bm8gc3VjaCBzYW5kYm94ZWQgcm91dGU=\"}\0a\00")
  (data (i32.const 768) "{\"op\":\"response\",\"status\":405,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"bWV0aG9kIG5vdCBhbGxvd2Vk\"}\0a\00")
  (data (i32.const 1024) "\"method\":\"GET\"\00")
  (data (i32.const 1088) "/greet\00")
  (data (i32.const 1120) "/etc/passwd\00")
  (data (i32.const 1152) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"],[\"set-cookie\",\"session=forged\"]],\"body_b64\":\"Y29va2llIGF0dGVtcHQ=\"}\0a\00")
  (data (i32.const 1600) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"],[\"x-evil\",\"a\\r\\nset-cookie: forged=1\"]],\"body_b64\":\"c3BsaXR0aW5nIGF0dGVtcHQ=\"}\0a\00")
  (data (i32.const 2048) "{\"op\":\"response\",\"status\":200,\0a\00")
  (data (i32.const 2176) "{\"op\":\"exec\",\"cmd\":\"/bin/sh\"}\0a\00")
  (data (i32.const 2304) "{\"op\":\"response\",\"status\":999,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"aW1wb3NzaWJsZSBzdGF0dXM=\"}\0a\00")
  (data (i32.const 2816) "X\00")
  (data (i32.const 3072) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"c2Vjb25kIGFuc3dlcg==\"}\0a\00")
  (data (i32.const 3584) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"],[\"x-autumn-sandboxed\",\"not-this-plugin\"],[\"x-content-type-options\",\"off\"]],\"body_b64\":\"YXR0cmlidXRpb24gYXR0ZW1wdA==\"}\0a\00")
  (data (i32.const 4200) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"aGVsbG8gZnJvbSB0aGUgc2FuZGJveA==\"}\00")
  (data (i32.const 5000) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"ZW50cm9weSBlY2hvZWQgaW4gdGhlIHN0YXR1cw==\"}\0a\00")
  (data (i32.const 6000) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"],[\"x-accel-redirect\",\"/internal/etc/passwd\"],[\"x-sendfile\",\"/etc/passwd\"]],\"body_b64\":\"cHJveHkgcmVkaXJlY3QgYXR0ZW1wdA==\"}\0a\00")
  (data (i32.const 6400) "\ff\fe\fd\fc\0a\00")
  (data (i32.const 6500) "{\"op\":\"error\",\"detail\":\"\\n2026-01-01T00:00:00Z  INFO forged: the host said this\\u001b[2K\\rBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB\"}\0a\00")

  ;; Length of the NUL-terminated string at $p.
  (func $strlen (param $p i32) (result i32)
    (local $n i32)
    (block $done
      (loop $l
        (br_if $done (i32.eqz (i32.load8_u (i32.add (local.get $p) (local.get $n)))))
        (local.set $n (i32.add (local.get $n) (i32.const 1)))
        (br $l)))
    (local.get $n))

  ;; Write the NUL-terminated string at $p to stdout: one wire frame.
  (func $emit (param $p i32)
    (i32.store (i32.const 0) (local.get $p))
    (i32.store (i32.const 4) (call $strlen (local.get $p)))
    (drop (call $fd_write (i32.const 1) (i32.const 0) (i32.const 1) (i32.const 16))))

  ;; Write $len bytes of scratch memory to $fd.
  (func $spew (param $fd i32) (param $len i32)
    (i32.store (i32.const 0) (i32.const 12288))
    (i32.store (i32.const 4) (local.get $len))
    (drop (call $fd_write (local.get $fd) (i32.const 0) (i32.const 1) (i32.const 16))))

  ;; Read one newline-terminated frame from stdin into the input buffer at
  ;; 8192; returns its length, newline excluded.
  (func $read_line (result i32)
    (local $n i32)
    (block $done
      (loop $l
        (i32.store (i32.const 0) (i32.add (i32.const 65536) (local.get $n)))
        (i32.store (i32.const 4) (i32.const 1))
        (i32.store (i32.const 16) (i32.const 0))
        (br_if $done (i32.ne (call $fd_read (i32.const 0) (i32.const 0) (i32.const 1) (i32.const 16)) (i32.const 0)))
        (br_if $done (i32.eqz (i32.load (i32.const 16))))
        (br_if $done (i32.eq (i32.load8_u (i32.add (i32.const 65536) (local.get $n))) (i32.const 10)))
        (local.set $n (i32.add (local.get $n) (i32.const 1)))
        (br_if $done (i32.ge_u (local.get $n) (i32.const 40000)))
        (br $l)))
    (local.get $n))

  ;; Naive substring search of the NUL-terminated needle at $needle inside the
  ;; first $len bytes of the input buffer.
  (func $contains (param $len i32) (param $needle i32) (result i32)
    (local $i i32) (local $j i32) (local $nl i32)
    (local.set $nl (call $strlen (local.get $needle)))
    (if (i32.gt_u (local.get $nl) (local.get $len)) (then (return (i32.const 0))))
    (block $no
      (loop $outer
        (br_if $no (i32.gt_u (i32.add (local.get $i) (local.get $nl)) (local.get $len)))
        (local.set $j (i32.const 0))
        (block $mismatch
          (loop $inner
            (br_if $mismatch (i32.ne
              (i32.load8_u (i32.add (i32.const 65536) (i32.add (local.get $i) (local.get $j))))
              (i32.load8_u (i32.add (local.get $needle) (local.get $j)))))
            (local.set $j (i32.add (local.get $j) (i32.const 1)))
            (br_if $inner (i32.lt_u (local.get $j) (local.get $nl))))
          (return (i32.const 1)))
        (local.set $i (i32.add (local.get $i) (i32.const 1)))
        (br $outer)))
    (i32.const 0))

  ;; Answer the request the way an honest hello-world plugin would.
  (func $answer
    (local $len i32)
    (local.set $len (call $read_line))
    (if (i32.eqz (call $contains (local.get $len) (i32.const 1024)))
      (then (call $emit (i32.const 768)) (return)))
    (if (call $contains (local.get $len) (i32.const 1088))
      (then (call $emit (i32.const 128)) (return)))
    (call $emit (i32.const 512)))
  (func (export "_start")
    (loop $l (call $spew (i32.const 2) (i32.const 49152)) (br $l))
  )
)
"#;

/// Writes 1 MiB to stdout in 64 KiB chunks, never ending a line. Used to prove
/// the host-side copy is charged against fuel rather than being free.
pub const STDOUT_BULK: &str = r#"(module
  (import "wasi_snapshot_preview1" "fd_read" (func $fd_read (param i32 i32 i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "fd_write" (func $fd_write (param i32 i32 i32 i32) (result i32)))
  (memory (export "memory") 2 4)
  (data (i32.const 128) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"aGVsbG8gZnJvbSB0aGUgc2FuZGJveA==\"}\0a\00")
  (data (i32.const 512) "{\"op\":\"response\",\"status\":404,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"bm8gc3VjaCBzYW5kYm94ZWQgcm91dGU=\"}\0a\00")
  (data (i32.const 768) "{\"op\":\"response\",\"status\":405,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"bWV0aG9kIG5vdCBhbGxvd2Vk\"}\0a\00")
  (data (i32.const 1024) "\"method\":\"GET\"\00")
  (data (i32.const 1088) "/greet\00")
  (data (i32.const 1120) "/etc/passwd\00")
  (data (i32.const 1152) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"],[\"set-cookie\",\"session=forged\"]],\"body_b64\":\"Y29va2llIGF0dGVtcHQ=\"}\0a\00")
  (data (i32.const 1600) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"],[\"x-evil\",\"a\\r\\nset-cookie: forged=1\"]],\"body_b64\":\"c3BsaXR0aW5nIGF0dGVtcHQ=\"}\0a\00")
  (data (i32.const 2048) "{\"op\":\"response\",\"status\":200,\0a\00")
  (data (i32.const 2176) "{\"op\":\"exec\",\"cmd\":\"/bin/sh\"}\0a\00")
  (data (i32.const 2304) "{\"op\":\"response\",\"status\":999,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"aW1wb3NzaWJsZSBzdGF0dXM=\"}\0a\00")
  (data (i32.const 2816) "X\00")
  (data (i32.const 3072) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"c2Vjb25kIGFuc3dlcg==\"}\0a\00")
  (data (i32.const 3584) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"],[\"x-autumn-sandboxed\",\"not-this-plugin\"],[\"x-content-type-options\",\"off\"]],\"body_b64\":\"YXR0cmlidXRpb24gYXR0ZW1wdA==\"}\0a\00")
  (data (i32.const 4200) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"aGVsbG8gZnJvbSB0aGUgc2FuZGJveA==\"}\00")
  (data (i32.const 5000) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"ZW50cm9weSBlY2hvZWQgaW4gdGhlIHN0YXR1cw==\"}\0a\00")
  (data (i32.const 6000) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"],[\"x-accel-redirect\",\"/internal/etc/passwd\"],[\"x-sendfile\",\"/etc/passwd\"]],\"body_b64\":\"cHJveHkgcmVkaXJlY3QgYXR0ZW1wdA==\"}\0a\00")
  (data (i32.const 6400) "\ff\fe\fd\fc\0a\00")
  (data (i32.const 6500) "{\"op\":\"error\",\"detail\":\"\\n2026-01-01T00:00:00Z  INFO forged: the host said this\\u001b[2K\\rBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB\"}\0a\00")

  ;; Length of the NUL-terminated string at $p.
  (func $strlen (param $p i32) (result i32)
    (local $n i32)
    (block $done
      (loop $l
        (br_if $done (i32.eqz (i32.load8_u (i32.add (local.get $p) (local.get $n)))))
        (local.set $n (i32.add (local.get $n) (i32.const 1)))
        (br $l)))
    (local.get $n))

  ;; Write the NUL-terminated string at $p to stdout: one wire frame.
  (func $emit (param $p i32)
    (i32.store (i32.const 0) (local.get $p))
    (i32.store (i32.const 4) (call $strlen (local.get $p)))
    (drop (call $fd_write (i32.const 1) (i32.const 0) (i32.const 1) (i32.const 16))))

  ;; Write $len bytes of scratch memory to $fd.
  (func $spew (param $fd i32) (param $len i32)
    (i32.store (i32.const 0) (i32.const 12288))
    (i32.store (i32.const 4) (local.get $len))
    (drop (call $fd_write (local.get $fd) (i32.const 0) (i32.const 1) (i32.const 16))))

  ;; Read one newline-terminated frame from stdin into the input buffer at
  ;; 8192; returns its length, newline excluded.
  (func $read_line (result i32)
    (local $n i32)
    (block $done
      (loop $l
        (i32.store (i32.const 0) (i32.add (i32.const 65536) (local.get $n)))
        (i32.store (i32.const 4) (i32.const 1))
        (i32.store (i32.const 16) (i32.const 0))
        (br_if $done (i32.ne (call $fd_read (i32.const 0) (i32.const 0) (i32.const 1) (i32.const 16)) (i32.const 0)))
        (br_if $done (i32.eqz (i32.load (i32.const 16))))
        (br_if $done (i32.eq (i32.load8_u (i32.add (i32.const 65536) (local.get $n))) (i32.const 10)))
        (local.set $n (i32.add (local.get $n) (i32.const 1)))
        (br_if $done (i32.ge_u (local.get $n) (i32.const 40000)))
        (br $l)))
    (local.get $n))

  ;; Naive substring search of the NUL-terminated needle at $needle inside the
  ;; first $len bytes of the input buffer.
  (func $contains (param $len i32) (param $needle i32) (result i32)
    (local $i i32) (local $j i32) (local $nl i32)
    (local.set $nl (call $strlen (local.get $needle)))
    (if (i32.gt_u (local.get $nl) (local.get $len)) (then (return (i32.const 0))))
    (block $no
      (loop $outer
        (br_if $no (i32.gt_u (i32.add (local.get $i) (local.get $nl)) (local.get $len)))
        (local.set $j (i32.const 0))
        (block $mismatch
          (loop $inner
            (br_if $mismatch (i32.ne
              (i32.load8_u (i32.add (i32.const 65536) (i32.add (local.get $i) (local.get $j))))
              (i32.load8_u (i32.add (local.get $needle) (local.get $j)))))
            (local.set $j (i32.add (local.get $j) (i32.const 1)))
            (br_if $inner (i32.lt_u (local.get $j) (local.get $nl))))
          (return (i32.const 1)))
        (local.set $i (i32.add (local.get $i) (i32.const 1)))
        (br $outer)))
    (i32.const 0))

  ;; Answer the request the way an honest hello-world plugin would.
  (func $answer
    (local $len i32)
    (local.set $len (call $read_line))
    (if (i32.eqz (call $contains (local.get $len) (i32.const 1024)))
      (then (call $emit (i32.const 768)) (return)))
    (if (call $contains (local.get $len) (i32.const 1088))
      (then (call $emit (i32.const 128)) (return)))
    (call $emit (i32.const 512)))
  (func (export "_start")
    (local $i i32)
    (loop $l
      (call $spew (i32.const 1) (i32.const 49152))
      (local.set $i (i32.add (local.get $i) (i32.const 1)))
      (br_if $l (i32.lt_u (local.get $i) (i32.const 32))))
  )
)
"#;

/// Answers with a `Set-Cookie`, which would be a forged session in the host
/// application's own origin.
pub const FORGE_COOKIE: &str = r#"(module
  (import "wasi_snapshot_preview1" "fd_read" (func $fd_read (param i32 i32 i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "fd_write" (func $fd_write (param i32 i32 i32 i32) (result i32)))
  (memory (export "memory") 2 4)
  (data (i32.const 128) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"aGVsbG8gZnJvbSB0aGUgc2FuZGJveA==\"}\0a\00")
  (data (i32.const 512) "{\"op\":\"response\",\"status\":404,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"bm8gc3VjaCBzYW5kYm94ZWQgcm91dGU=\"}\0a\00")
  (data (i32.const 768) "{\"op\":\"response\",\"status\":405,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"bWV0aG9kIG5vdCBhbGxvd2Vk\"}\0a\00")
  (data (i32.const 1024) "\"method\":\"GET\"\00")
  (data (i32.const 1088) "/greet\00")
  (data (i32.const 1120) "/etc/passwd\00")
  (data (i32.const 1152) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"],[\"set-cookie\",\"session=forged\"]],\"body_b64\":\"Y29va2llIGF0dGVtcHQ=\"}\0a\00")
  (data (i32.const 1600) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"],[\"x-evil\",\"a\\r\\nset-cookie: forged=1\"]],\"body_b64\":\"c3BsaXR0aW5nIGF0dGVtcHQ=\"}\0a\00")
  (data (i32.const 2048) "{\"op\":\"response\",\"status\":200,\0a\00")
  (data (i32.const 2176) "{\"op\":\"exec\",\"cmd\":\"/bin/sh\"}\0a\00")
  (data (i32.const 2304) "{\"op\":\"response\",\"status\":999,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"aW1wb3NzaWJsZSBzdGF0dXM=\"}\0a\00")
  (data (i32.const 2816) "X\00")
  (data (i32.const 3072) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"c2Vjb25kIGFuc3dlcg==\"}\0a\00")
  (data (i32.const 3584) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"],[\"x-autumn-sandboxed\",\"not-this-plugin\"],[\"x-content-type-options\",\"off\"]],\"body_b64\":\"YXR0cmlidXRpb24gYXR0ZW1wdA==\"}\0a\00")
  (data (i32.const 4200) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"aGVsbG8gZnJvbSB0aGUgc2FuZGJveA==\"}\00")
  (data (i32.const 5000) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"ZW50cm9weSBlY2hvZWQgaW4gdGhlIHN0YXR1cw==\"}\0a\00")
  (data (i32.const 6000) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"],[\"x-accel-redirect\",\"/internal/etc/passwd\"],[\"x-sendfile\",\"/etc/passwd\"]],\"body_b64\":\"cHJveHkgcmVkaXJlY3QgYXR0ZW1wdA==\"}\0a\00")
  (data (i32.const 6400) "\ff\fe\fd\fc\0a\00")
  (data (i32.const 6500) "{\"op\":\"error\",\"detail\":\"\\n2026-01-01T00:00:00Z  INFO forged: the host said this\\u001b[2K\\rBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB\"}\0a\00")

  ;; Length of the NUL-terminated string at $p.
  (func $strlen (param $p i32) (result i32)
    (local $n i32)
    (block $done
      (loop $l
        (br_if $done (i32.eqz (i32.load8_u (i32.add (local.get $p) (local.get $n)))))
        (local.set $n (i32.add (local.get $n) (i32.const 1)))
        (br $l)))
    (local.get $n))

  ;; Write the NUL-terminated string at $p to stdout: one wire frame.
  (func $emit (param $p i32)
    (i32.store (i32.const 0) (local.get $p))
    (i32.store (i32.const 4) (call $strlen (local.get $p)))
    (drop (call $fd_write (i32.const 1) (i32.const 0) (i32.const 1) (i32.const 16))))

  ;; Write $len bytes of scratch memory to $fd.
  (func $spew (param $fd i32) (param $len i32)
    (i32.store (i32.const 0) (i32.const 12288))
    (i32.store (i32.const 4) (local.get $len))
    (drop (call $fd_write (local.get $fd) (i32.const 0) (i32.const 1) (i32.const 16))))

  ;; Read one newline-terminated frame from stdin into the input buffer at
  ;; 8192; returns its length, newline excluded.
  (func $read_line (result i32)
    (local $n i32)
    (block $done
      (loop $l
        (i32.store (i32.const 0) (i32.add (i32.const 65536) (local.get $n)))
        (i32.store (i32.const 4) (i32.const 1))
        (i32.store (i32.const 16) (i32.const 0))
        (br_if $done (i32.ne (call $fd_read (i32.const 0) (i32.const 0) (i32.const 1) (i32.const 16)) (i32.const 0)))
        (br_if $done (i32.eqz (i32.load (i32.const 16))))
        (br_if $done (i32.eq (i32.load8_u (i32.add (i32.const 65536) (local.get $n))) (i32.const 10)))
        (local.set $n (i32.add (local.get $n) (i32.const 1)))
        (br_if $done (i32.ge_u (local.get $n) (i32.const 40000)))
        (br $l)))
    (local.get $n))

  ;; Naive substring search of the NUL-terminated needle at $needle inside the
  ;; first $len bytes of the input buffer.
  (func $contains (param $len i32) (param $needle i32) (result i32)
    (local $i i32) (local $j i32) (local $nl i32)
    (local.set $nl (call $strlen (local.get $needle)))
    (if (i32.gt_u (local.get $nl) (local.get $len)) (then (return (i32.const 0))))
    (block $no
      (loop $outer
        (br_if $no (i32.gt_u (i32.add (local.get $i) (local.get $nl)) (local.get $len)))
        (local.set $j (i32.const 0))
        (block $mismatch
          (loop $inner
            (br_if $mismatch (i32.ne
              (i32.load8_u (i32.add (i32.const 65536) (i32.add (local.get $i) (local.get $j))))
              (i32.load8_u (i32.add (local.get $needle) (local.get $j)))))
            (local.set $j (i32.add (local.get $j) (i32.const 1)))
            (br_if $inner (i32.lt_u (local.get $j) (local.get $nl))))
          (return (i32.const 1)))
        (local.set $i (i32.add (local.get $i) (i32.const 1)))
        (br $outer)))
    (i32.const 0))

  ;; Answer the request the way an honest hello-world plugin would.
  (func $answer
    (local $len i32)
    (local.set $len (call $read_line))
    (if (i32.eqz (call $contains (local.get $len) (i32.const 1024)))
      (then (call $emit (i32.const 768)) (return)))
    (if (call $contains (local.get $len) (i32.const 1088))
      (then (call $emit (i32.const 128)) (return)))
    (call $emit (i32.const 512)))
  (func (export "_start")
    (call $emit (i32.const 1152))
  )
)
"#;

/// Answers with a header value carrying CRLF.
pub const SPLIT_RESPONSE: &str = r#"(module
  (import "wasi_snapshot_preview1" "fd_read" (func $fd_read (param i32 i32 i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "fd_write" (func $fd_write (param i32 i32 i32 i32) (result i32)))
  (memory (export "memory") 2 4)
  (data (i32.const 128) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"aGVsbG8gZnJvbSB0aGUgc2FuZGJveA==\"}\0a\00")
  (data (i32.const 512) "{\"op\":\"response\",\"status\":404,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"bm8gc3VjaCBzYW5kYm94ZWQgcm91dGU=\"}\0a\00")
  (data (i32.const 768) "{\"op\":\"response\",\"status\":405,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"bWV0aG9kIG5vdCBhbGxvd2Vk\"}\0a\00")
  (data (i32.const 1024) "\"method\":\"GET\"\00")
  (data (i32.const 1088) "/greet\00")
  (data (i32.const 1120) "/etc/passwd\00")
  (data (i32.const 1152) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"],[\"set-cookie\",\"session=forged\"]],\"body_b64\":\"Y29va2llIGF0dGVtcHQ=\"}\0a\00")
  (data (i32.const 1600) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"],[\"x-evil\",\"a\\r\\nset-cookie: forged=1\"]],\"body_b64\":\"c3BsaXR0aW5nIGF0dGVtcHQ=\"}\0a\00")
  (data (i32.const 2048) "{\"op\":\"response\",\"status\":200,\0a\00")
  (data (i32.const 2176) "{\"op\":\"exec\",\"cmd\":\"/bin/sh\"}\0a\00")
  (data (i32.const 2304) "{\"op\":\"response\",\"status\":999,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"aW1wb3NzaWJsZSBzdGF0dXM=\"}\0a\00")
  (data (i32.const 2816) "X\00")
  (data (i32.const 3072) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"c2Vjb25kIGFuc3dlcg==\"}\0a\00")
  (data (i32.const 3584) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"],[\"x-autumn-sandboxed\",\"not-this-plugin\"],[\"x-content-type-options\",\"off\"]],\"body_b64\":\"YXR0cmlidXRpb24gYXR0ZW1wdA==\"}\0a\00")
  (data (i32.const 4200) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"aGVsbG8gZnJvbSB0aGUgc2FuZGJveA==\"}\00")
  (data (i32.const 5000) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"ZW50cm9weSBlY2hvZWQgaW4gdGhlIHN0YXR1cw==\"}\0a\00")
  (data (i32.const 6000) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"],[\"x-accel-redirect\",\"/internal/etc/passwd\"],[\"x-sendfile\",\"/etc/passwd\"]],\"body_b64\":\"cHJveHkgcmVkaXJlY3QgYXR0ZW1wdA==\"}\0a\00")
  (data (i32.const 6400) "\ff\fe\fd\fc\0a\00")
  (data (i32.const 6500) "{\"op\":\"error\",\"detail\":\"\\n2026-01-01T00:00:00Z  INFO forged: the host said this\\u001b[2K\\rBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB\"}\0a\00")

  ;; Length of the NUL-terminated string at $p.
  (func $strlen (param $p i32) (result i32)
    (local $n i32)
    (block $done
      (loop $l
        (br_if $done (i32.eqz (i32.load8_u (i32.add (local.get $p) (local.get $n)))))
        (local.set $n (i32.add (local.get $n) (i32.const 1)))
        (br $l)))
    (local.get $n))

  ;; Write the NUL-terminated string at $p to stdout: one wire frame.
  (func $emit (param $p i32)
    (i32.store (i32.const 0) (local.get $p))
    (i32.store (i32.const 4) (call $strlen (local.get $p)))
    (drop (call $fd_write (i32.const 1) (i32.const 0) (i32.const 1) (i32.const 16))))

  ;; Write $len bytes of scratch memory to $fd.
  (func $spew (param $fd i32) (param $len i32)
    (i32.store (i32.const 0) (i32.const 12288))
    (i32.store (i32.const 4) (local.get $len))
    (drop (call $fd_write (local.get $fd) (i32.const 0) (i32.const 1) (i32.const 16))))

  ;; Read one newline-terminated frame from stdin into the input buffer at
  ;; 8192; returns its length, newline excluded.
  (func $read_line (result i32)
    (local $n i32)
    (block $done
      (loop $l
        (i32.store (i32.const 0) (i32.add (i32.const 65536) (local.get $n)))
        (i32.store (i32.const 4) (i32.const 1))
        (i32.store (i32.const 16) (i32.const 0))
        (br_if $done (i32.ne (call $fd_read (i32.const 0) (i32.const 0) (i32.const 1) (i32.const 16)) (i32.const 0)))
        (br_if $done (i32.eqz (i32.load (i32.const 16))))
        (br_if $done (i32.eq (i32.load8_u (i32.add (i32.const 65536) (local.get $n))) (i32.const 10)))
        (local.set $n (i32.add (local.get $n) (i32.const 1)))
        (br_if $done (i32.ge_u (local.get $n) (i32.const 40000)))
        (br $l)))
    (local.get $n))

  ;; Naive substring search of the NUL-terminated needle at $needle inside the
  ;; first $len bytes of the input buffer.
  (func $contains (param $len i32) (param $needle i32) (result i32)
    (local $i i32) (local $j i32) (local $nl i32)
    (local.set $nl (call $strlen (local.get $needle)))
    (if (i32.gt_u (local.get $nl) (local.get $len)) (then (return (i32.const 0))))
    (block $no
      (loop $outer
        (br_if $no (i32.gt_u (i32.add (local.get $i) (local.get $nl)) (local.get $len)))
        (local.set $j (i32.const 0))
        (block $mismatch
          (loop $inner
            (br_if $mismatch (i32.ne
              (i32.load8_u (i32.add (i32.const 65536) (i32.add (local.get $i) (local.get $j))))
              (i32.load8_u (i32.add (local.get $needle) (local.get $j)))))
            (local.set $j (i32.add (local.get $j) (i32.const 1)))
            (br_if $inner (i32.lt_u (local.get $j) (local.get $nl))))
          (return (i32.const 1)))
        (local.set $i (i32.add (local.get $i) (i32.const 1)))
        (br $outer)))
    (i32.const 0))

  ;; Answer the request the way an honest hello-world plugin would.
  (func $answer
    (local $len i32)
    (local.set $len (call $read_line))
    (if (i32.eqz (call $contains (local.get $len) (i32.const 1024)))
      (then (call $emit (i32.const 768)) (return)))
    (if (call $contains (local.get $len) (i32.const 1088))
      (then (call $emit (i32.const 128)) (return)))
    (call $emit (i32.const 512)))
  (func (export "_start")
    (call $emit (i32.const 1600))
  )
)
"#;

/// Answers with the host's own attribution header and a bogus
/// `x-content-type-options`, trying to misattribute the response and defeat
/// the host's sniffing guard.
pub const FORGE_ATTRIBUTION: &str = r#"(module
  (import "wasi_snapshot_preview1" "fd_read" (func $fd_read (param i32 i32 i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "fd_write" (func $fd_write (param i32 i32 i32 i32) (result i32)))
  (memory (export "memory") 2 4)
  (data (i32.const 128) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"aGVsbG8gZnJvbSB0aGUgc2FuZGJveA==\"}\0a\00")
  (data (i32.const 512) "{\"op\":\"response\",\"status\":404,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"bm8gc3VjaCBzYW5kYm94ZWQgcm91dGU=\"}\0a\00")
  (data (i32.const 768) "{\"op\":\"response\",\"status\":405,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"bWV0aG9kIG5vdCBhbGxvd2Vk\"}\0a\00")
  (data (i32.const 1024) "\"method\":\"GET\"\00")
  (data (i32.const 1088) "/greet\00")
  (data (i32.const 1120) "/etc/passwd\00")
  (data (i32.const 1152) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"],[\"set-cookie\",\"session=forged\"]],\"body_b64\":\"Y29va2llIGF0dGVtcHQ=\"}\0a\00")
  (data (i32.const 1600) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"],[\"x-evil\",\"a\\r\\nset-cookie: forged=1\"]],\"body_b64\":\"c3BsaXR0aW5nIGF0dGVtcHQ=\"}\0a\00")
  (data (i32.const 2048) "{\"op\":\"response\",\"status\":200,\0a\00")
  (data (i32.const 2176) "{\"op\":\"exec\",\"cmd\":\"/bin/sh\"}\0a\00")
  (data (i32.const 2304) "{\"op\":\"response\",\"status\":999,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"aW1wb3NzaWJsZSBzdGF0dXM=\"}\0a\00")
  (data (i32.const 2816) "X\00")
  (data (i32.const 3072) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"c2Vjb25kIGFuc3dlcg==\"}\0a\00")
  (data (i32.const 3584) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"],[\"x-autumn-sandboxed\",\"not-this-plugin\"],[\"x-content-type-options\",\"off\"]],\"body_b64\":\"YXR0cmlidXRpb24gYXR0ZW1wdA==\"}\0a\00")
  (data (i32.const 4200) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"aGVsbG8gZnJvbSB0aGUgc2FuZGJveA==\"}\00")
  (data (i32.const 5000) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"ZW50cm9weSBlY2hvZWQgaW4gdGhlIHN0YXR1cw==\"}\0a\00")
  (data (i32.const 6000) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"],[\"x-accel-redirect\",\"/internal/etc/passwd\"],[\"x-sendfile\",\"/etc/passwd\"]],\"body_b64\":\"cHJveHkgcmVkaXJlY3QgYXR0ZW1wdA==\"}\0a\00")
  (data (i32.const 6400) "\ff\fe\fd\fc\0a\00")
  (data (i32.const 6500) "{\"op\":\"error\",\"detail\":\"\\n2026-01-01T00:00:00Z  INFO forged: the host said this\\u001b[2K\\rBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB\"}\0a\00")

  ;; Length of the NUL-terminated string at $p.
  (func $strlen (param $p i32) (result i32)
    (local $n i32)
    (block $done
      (loop $l
        (br_if $done (i32.eqz (i32.load8_u (i32.add (local.get $p) (local.get $n)))))
        (local.set $n (i32.add (local.get $n) (i32.const 1)))
        (br $l)))
    (local.get $n))

  ;; Write the NUL-terminated string at $p to stdout: one wire frame.
  (func $emit (param $p i32)
    (i32.store (i32.const 0) (local.get $p))
    (i32.store (i32.const 4) (call $strlen (local.get $p)))
    (drop (call $fd_write (i32.const 1) (i32.const 0) (i32.const 1) (i32.const 16))))

  ;; Write $len bytes of scratch memory to $fd.
  (func $spew (param $fd i32) (param $len i32)
    (i32.store (i32.const 0) (i32.const 12288))
    (i32.store (i32.const 4) (local.get $len))
    (drop (call $fd_write (local.get $fd) (i32.const 0) (i32.const 1) (i32.const 16))))

  ;; Read one newline-terminated frame from stdin into the input buffer at
  ;; 8192; returns its length, newline excluded.
  (func $read_line (result i32)
    (local $n i32)
    (block $done
      (loop $l
        (i32.store (i32.const 0) (i32.add (i32.const 65536) (local.get $n)))
        (i32.store (i32.const 4) (i32.const 1))
        (i32.store (i32.const 16) (i32.const 0))
        (br_if $done (i32.ne (call $fd_read (i32.const 0) (i32.const 0) (i32.const 1) (i32.const 16)) (i32.const 0)))
        (br_if $done (i32.eqz (i32.load (i32.const 16))))
        (br_if $done (i32.eq (i32.load8_u (i32.add (i32.const 65536) (local.get $n))) (i32.const 10)))
        (local.set $n (i32.add (local.get $n) (i32.const 1)))
        (br_if $done (i32.ge_u (local.get $n) (i32.const 40000)))
        (br $l)))
    (local.get $n))

  ;; Naive substring search of the NUL-terminated needle at $needle inside the
  ;; first $len bytes of the input buffer.
  (func $contains (param $len i32) (param $needle i32) (result i32)
    (local $i i32) (local $j i32) (local $nl i32)
    (local.set $nl (call $strlen (local.get $needle)))
    (if (i32.gt_u (local.get $nl) (local.get $len)) (then (return (i32.const 0))))
    (block $no
      (loop $outer
        (br_if $no (i32.gt_u (i32.add (local.get $i) (local.get $nl)) (local.get $len)))
        (local.set $j (i32.const 0))
        (block $mismatch
          (loop $inner
            (br_if $mismatch (i32.ne
              (i32.load8_u (i32.add (i32.const 65536) (i32.add (local.get $i) (local.get $j))))
              (i32.load8_u (i32.add (local.get $needle) (local.get $j)))))
            (local.set $j (i32.add (local.get $j) (i32.const 1)))
            (br_if $inner (i32.lt_u (local.get $j) (local.get $nl))))
          (return (i32.const 1)))
        (local.set $i (i32.add (local.get $i) (i32.const 1)))
        (br $outer)))
    (i32.const 0))

  ;; Answer the request the way an honest hello-world plugin would.
  (func $answer
    (local $len i32)
    (local.set $len (call $read_line))
    (if (i32.eqz (call $contains (local.get $len) (i32.const 1024)))
      (then (call $emit (i32.const 768)) (return)))
    (if (call $contains (local.get $len) (i32.const 1088))
      (then (call $emit (i32.const 128)) (return)))
    (call $emit (i32.const 512)))
  (func (export "_start")
    (call $emit (i32.const 3584))
  )
)
"#;

/// Answers with `X-Accel-Redirect` and `X-Sendfile`, asking the reverse proxy
/// in front of the host to serve a local file on its behalf — a filesystem the
/// sandbox withheld, one hop upstream.
pub const PROXY_REDIRECT: &str = r#"(module
  (import "wasi_snapshot_preview1" "fd_read" (func $fd_read (param i32 i32 i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "fd_write" (func $fd_write (param i32 i32 i32 i32) (result i32)))
  (memory (export "memory") 2 4)
  (data (i32.const 128) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"aGVsbG8gZnJvbSB0aGUgc2FuZGJveA==\"}\0a\00")
  (data (i32.const 512) "{\"op\":\"response\",\"status\":404,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"bm8gc3VjaCBzYW5kYm94ZWQgcm91dGU=\"}\0a\00")
  (data (i32.const 768) "{\"op\":\"response\",\"status\":405,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"bWV0aG9kIG5vdCBhbGxvd2Vk\"}\0a\00")
  (data (i32.const 1024) "\"method\":\"GET\"\00")
  (data (i32.const 1088) "/greet\00")
  (data (i32.const 1120) "/etc/passwd\00")
  (data (i32.const 1152) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"],[\"set-cookie\",\"session=forged\"]],\"body_b64\":\"Y29va2llIGF0dGVtcHQ=\"}\0a\00")
  (data (i32.const 1600) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"],[\"x-evil\",\"a\\r\\nset-cookie: forged=1\"]],\"body_b64\":\"c3BsaXR0aW5nIGF0dGVtcHQ=\"}\0a\00")
  (data (i32.const 2048) "{\"op\":\"response\",\"status\":200,\0a\00")
  (data (i32.const 2176) "{\"op\":\"exec\",\"cmd\":\"/bin/sh\"}\0a\00")
  (data (i32.const 2304) "{\"op\":\"response\",\"status\":999,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"aW1wb3NzaWJsZSBzdGF0dXM=\"}\0a\00")
  (data (i32.const 2816) "X\00")
  (data (i32.const 3072) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"c2Vjb25kIGFuc3dlcg==\"}\0a\00")
  (data (i32.const 3584) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"],[\"x-autumn-sandboxed\",\"not-this-plugin\"],[\"x-content-type-options\",\"off\"]],\"body_b64\":\"YXR0cmlidXRpb24gYXR0ZW1wdA==\"}\0a\00")
  (data (i32.const 4200) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"aGVsbG8gZnJvbSB0aGUgc2FuZGJveA==\"}\00")
  (data (i32.const 5000) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"ZW50cm9weSBlY2hvZWQgaW4gdGhlIHN0YXR1cw==\"}\0a\00")
  (data (i32.const 6000) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"],[\"x-accel-redirect\",\"/internal/etc/passwd\"],[\"x-sendfile\",\"/etc/passwd\"]],\"body_b64\":\"cHJveHkgcmVkaXJlY3QgYXR0ZW1wdA==\"}\0a\00")
  (data (i32.const 6400) "\ff\fe\fd\fc\0a\00")
  (data (i32.const 6500) "{\"op\":\"error\",\"detail\":\"\\n2026-01-01T00:00:00Z  INFO forged: the host said this\\u001b[2K\\rBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB\"}\0a\00")

  ;; Length of the NUL-terminated string at $p.
  (func $strlen (param $p i32) (result i32)
    (local $n i32)
    (block $done
      (loop $l
        (br_if $done (i32.eqz (i32.load8_u (i32.add (local.get $p) (local.get $n)))))
        (local.set $n (i32.add (local.get $n) (i32.const 1)))
        (br $l)))
    (local.get $n))

  ;; Write the NUL-terminated string at $p to stdout: one wire frame.
  (func $emit (param $p i32)
    (i32.store (i32.const 0) (local.get $p))
    (i32.store (i32.const 4) (call $strlen (local.get $p)))
    (drop (call $fd_write (i32.const 1) (i32.const 0) (i32.const 1) (i32.const 16))))

  ;; Write $len bytes of scratch memory to $fd.
  (func $spew (param $fd i32) (param $len i32)
    (i32.store (i32.const 0) (i32.const 12288))
    (i32.store (i32.const 4) (local.get $len))
    (drop (call $fd_write (local.get $fd) (i32.const 0) (i32.const 1) (i32.const 16))))

  ;; Read one newline-terminated frame from stdin into the input buffer at
  ;; 8192; returns its length, newline excluded.
  (func $read_line (result i32)
    (local $n i32)
    (block $done
      (loop $l
        (i32.store (i32.const 0) (i32.add (i32.const 65536) (local.get $n)))
        (i32.store (i32.const 4) (i32.const 1))
        (i32.store (i32.const 16) (i32.const 0))
        (br_if $done (i32.ne (call $fd_read (i32.const 0) (i32.const 0) (i32.const 1) (i32.const 16)) (i32.const 0)))
        (br_if $done (i32.eqz (i32.load (i32.const 16))))
        (br_if $done (i32.eq (i32.load8_u (i32.add (i32.const 65536) (local.get $n))) (i32.const 10)))
        (local.set $n (i32.add (local.get $n) (i32.const 1)))
        (br_if $done (i32.ge_u (local.get $n) (i32.const 40000)))
        (br $l)))
    (local.get $n))

  ;; Naive substring search of the NUL-terminated needle at $needle inside the
  ;; first $len bytes of the input buffer.
  (func $contains (param $len i32) (param $needle i32) (result i32)
    (local $i i32) (local $j i32) (local $nl i32)
    (local.set $nl (call $strlen (local.get $needle)))
    (if (i32.gt_u (local.get $nl) (local.get $len)) (then (return (i32.const 0))))
    (block $no
      (loop $outer
        (br_if $no (i32.gt_u (i32.add (local.get $i) (local.get $nl)) (local.get $len)))
        (local.set $j (i32.const 0))
        (block $mismatch
          (loop $inner
            (br_if $mismatch (i32.ne
              (i32.load8_u (i32.add (i32.const 65536) (i32.add (local.get $i) (local.get $j))))
              (i32.load8_u (i32.add (local.get $needle) (local.get $j)))))
            (local.set $j (i32.add (local.get $j) (i32.const 1)))
            (br_if $inner (i32.lt_u (local.get $j) (local.get $nl))))
          (return (i32.const 1)))
        (local.set $i (i32.add (local.get $i) (i32.const 1)))
        (br $outer)))
    (i32.const 0))

  ;; Answer the request the way an honest hello-world plugin would.
  (func $answer
    (local $len i32)
    (local.set $len (call $read_line))
    (if (i32.eqz (call $contains (local.get $len) (i32.const 1024)))
      (then (call $emit (i32.const 768)) (return)))
    (if (call $contains (local.get $len) (i32.const 1088))
      (then (call $emit (i32.const 128)) (return)))
    (call $emit (i32.const 512)))
  (func (export "_start")
    (call $emit (i32.const 6000))
  )
)
"#;

/// Reports its own failure with a detail that is long, starts a line of its
/// own, and carries an ANSI escape — everything a guest needs to flood a log
/// or forge a record in it.
pub const FORGE_LOG: &str = r#"(module
  (import "wasi_snapshot_preview1" "fd_read" (func $fd_read (param i32 i32 i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "fd_write" (func $fd_write (param i32 i32 i32 i32) (result i32)))
  (memory (export "memory") 2 4)
  (data (i32.const 128) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"aGVsbG8gZnJvbSB0aGUgc2FuZGJveA==\"}\0a\00")
  (data (i32.const 512) "{\"op\":\"response\",\"status\":404,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"bm8gc3VjaCBzYW5kYm94ZWQgcm91dGU=\"}\0a\00")
  (data (i32.const 768) "{\"op\":\"response\",\"status\":405,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"bWV0aG9kIG5vdCBhbGxvd2Vk\"}\0a\00")
  (data (i32.const 1024) "\"method\":\"GET\"\00")
  (data (i32.const 1088) "/greet\00")
  (data (i32.const 1120) "/etc/passwd\00")
  (data (i32.const 1152) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"],[\"set-cookie\",\"session=forged\"]],\"body_b64\":\"Y29va2llIGF0dGVtcHQ=\"}\0a\00")
  (data (i32.const 1600) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"],[\"x-evil\",\"a\\r\\nset-cookie: forged=1\"]],\"body_b64\":\"c3BsaXR0aW5nIGF0dGVtcHQ=\"}\0a\00")
  (data (i32.const 2048) "{\"op\":\"response\",\"status\":200,\0a\00")
  (data (i32.const 2176) "{\"op\":\"exec\",\"cmd\":\"/bin/sh\"}\0a\00")
  (data (i32.const 2304) "{\"op\":\"response\",\"status\":999,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"aW1wb3NzaWJsZSBzdGF0dXM=\"}\0a\00")
  (data (i32.const 2816) "X\00")
  (data (i32.const 3072) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"c2Vjb25kIGFuc3dlcg==\"}\0a\00")
  (data (i32.const 3584) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"],[\"x-autumn-sandboxed\",\"not-this-plugin\"],[\"x-content-type-options\",\"off\"]],\"body_b64\":\"YXR0cmlidXRpb24gYXR0ZW1wdA==\"}\0a\00")
  (data (i32.const 4200) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"aGVsbG8gZnJvbSB0aGUgc2FuZGJveA==\"}\00")
  (data (i32.const 5000) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"ZW50cm9weSBlY2hvZWQgaW4gdGhlIHN0YXR1cw==\"}\0a\00")
  (data (i32.const 6000) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"],[\"x-accel-redirect\",\"/internal/etc/passwd\"],[\"x-sendfile\",\"/etc/passwd\"]],\"body_b64\":\"cHJveHkgcmVkaXJlY3QgYXR0ZW1wdA==\"}\0a\00")
  (data (i32.const 6400) "\ff\fe\fd\fc\0a\00")
  (data (i32.const 6500) "{\"op\":\"error\",\"detail\":\"\\n2026-01-01T00:00:00Z  INFO forged: the host said this\\u001b[2K\\rBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB\"}\0a\00")

  ;; Length of the NUL-terminated string at $p.
  (func $strlen (param $p i32) (result i32)
    (local $n i32)
    (block $done
      (loop $l
        (br_if $done (i32.eqz (i32.load8_u (i32.add (local.get $p) (local.get $n)))))
        (local.set $n (i32.add (local.get $n) (i32.const 1)))
        (br $l)))
    (local.get $n))

  ;; Write the NUL-terminated string at $p to stdout: one wire frame.
  (func $emit (param $p i32)
    (i32.store (i32.const 0) (local.get $p))
    (i32.store (i32.const 4) (call $strlen (local.get $p)))
    (drop (call $fd_write (i32.const 1) (i32.const 0) (i32.const 1) (i32.const 16))))

  ;; Write $len bytes of scratch memory to $fd.
  (func $spew (param $fd i32) (param $len i32)
    (i32.store (i32.const 0) (i32.const 12288))
    (i32.store (i32.const 4) (local.get $len))
    (drop (call $fd_write (local.get $fd) (i32.const 0) (i32.const 1) (i32.const 16))))

  ;; Read one newline-terminated frame from stdin into the input buffer at
  ;; 8192; returns its length, newline excluded.
  (func $read_line (result i32)
    (local $n i32)
    (block $done
      (loop $l
        (i32.store (i32.const 0) (i32.add (i32.const 65536) (local.get $n)))
        (i32.store (i32.const 4) (i32.const 1))
        (i32.store (i32.const 16) (i32.const 0))
        (br_if $done (i32.ne (call $fd_read (i32.const 0) (i32.const 0) (i32.const 1) (i32.const 16)) (i32.const 0)))
        (br_if $done (i32.eqz (i32.load (i32.const 16))))
        (br_if $done (i32.eq (i32.load8_u (i32.add (i32.const 65536) (local.get $n))) (i32.const 10)))
        (local.set $n (i32.add (local.get $n) (i32.const 1)))
        (br_if $done (i32.ge_u (local.get $n) (i32.const 40000)))
        (br $l)))
    (local.get $n))

  ;; Naive substring search of the NUL-terminated needle at $needle inside the
  ;; first $len bytes of the input buffer.
  (func $contains (param $len i32) (param $needle i32) (result i32)
    (local $i i32) (local $j i32) (local $nl i32)
    (local.set $nl (call $strlen (local.get $needle)))
    (if (i32.gt_u (local.get $nl) (local.get $len)) (then (return (i32.const 0))))
    (block $no
      (loop $outer
        (br_if $no (i32.gt_u (i32.add (local.get $i) (local.get $nl)) (local.get $len)))
        (local.set $j (i32.const 0))
        (block $mismatch
          (loop $inner
            (br_if $mismatch (i32.ne
              (i32.load8_u (i32.add (i32.const 65536) (i32.add (local.get $i) (local.get $j))))
              (i32.load8_u (i32.add (local.get $needle) (local.get $j)))))
            (local.set $j (i32.add (local.get $j) (i32.const 1)))
            (br_if $inner (i32.lt_u (local.get $j) (local.get $nl))))
          (return (i32.const 1)))
        (local.set $i (i32.add (local.get $i) (i32.const 1)))
        (br $outer)))
    (i32.const 0))

  ;; Answer the request the way an honest hello-world plugin would.
  (func $answer
    (local $len i32)
    (local.set $len (call $read_line))
    (if (i32.eqz (call $contains (local.get $len) (i32.const 1024)))
      (then (call $emit (i32.const 768)) (return)))
    (if (call $contains (local.get $len) (i32.const 1088))
      (then (call $emit (i32.const 128)) (return)))
    (call $emit (i32.const 512)))
  (func (export "_start")
    (call $emit (i32.const 6500))
  )
)
"#;

/// Writes a complete, valid frame and forgets the trailing newline — the most
/// likely author mistake there is.
pub const PARTIAL_FRAME: &str = r#"(module
  (import "wasi_snapshot_preview1" "fd_read" (func $fd_read (param i32 i32 i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "fd_write" (func $fd_write (param i32 i32 i32 i32) (result i32)))
  (memory (export "memory") 2 4)
  (data (i32.const 128) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"aGVsbG8gZnJvbSB0aGUgc2FuZGJveA==\"}\0a\00")
  (data (i32.const 512) "{\"op\":\"response\",\"status\":404,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"bm8gc3VjaCBzYW5kYm94ZWQgcm91dGU=\"}\0a\00")
  (data (i32.const 768) "{\"op\":\"response\",\"status\":405,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"bWV0aG9kIG5vdCBhbGxvd2Vk\"}\0a\00")
  (data (i32.const 1024) "\"method\":\"GET\"\00")
  (data (i32.const 1088) "/greet\00")
  (data (i32.const 1120) "/etc/passwd\00")
  (data (i32.const 1152) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"],[\"set-cookie\",\"session=forged\"]],\"body_b64\":\"Y29va2llIGF0dGVtcHQ=\"}\0a\00")
  (data (i32.const 1600) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"],[\"x-evil\",\"a\\r\\nset-cookie: forged=1\"]],\"body_b64\":\"c3BsaXR0aW5nIGF0dGVtcHQ=\"}\0a\00")
  (data (i32.const 2048) "{\"op\":\"response\",\"status\":200,\0a\00")
  (data (i32.const 2176) "{\"op\":\"exec\",\"cmd\":\"/bin/sh\"}\0a\00")
  (data (i32.const 2304) "{\"op\":\"response\",\"status\":999,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"aW1wb3NzaWJsZSBzdGF0dXM=\"}\0a\00")
  (data (i32.const 2816) "X\00")
  (data (i32.const 3072) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"c2Vjb25kIGFuc3dlcg==\"}\0a\00")
  (data (i32.const 3584) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"],[\"x-autumn-sandboxed\",\"not-this-plugin\"],[\"x-content-type-options\",\"off\"]],\"body_b64\":\"YXR0cmlidXRpb24gYXR0ZW1wdA==\"}\0a\00")
  (data (i32.const 4200) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"aGVsbG8gZnJvbSB0aGUgc2FuZGJveA==\"}\00")
  (data (i32.const 5000) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"ZW50cm9weSBlY2hvZWQgaW4gdGhlIHN0YXR1cw==\"}\0a\00")
  (data (i32.const 6000) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"],[\"x-accel-redirect\",\"/internal/etc/passwd\"],[\"x-sendfile\",\"/etc/passwd\"]],\"body_b64\":\"cHJveHkgcmVkaXJlY3QgYXR0ZW1wdA==\"}\0a\00")
  (data (i32.const 6400) "\ff\fe\fd\fc\0a\00")
  (data (i32.const 6500) "{\"op\":\"error\",\"detail\":\"\\n2026-01-01T00:00:00Z  INFO forged: the host said this\\u001b[2K\\rBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB\"}\0a\00")

  ;; Length of the NUL-terminated string at $p.
  (func $strlen (param $p i32) (result i32)
    (local $n i32)
    (block $done
      (loop $l
        (br_if $done (i32.eqz (i32.load8_u (i32.add (local.get $p) (local.get $n)))))
        (local.set $n (i32.add (local.get $n) (i32.const 1)))
        (br $l)))
    (local.get $n))

  ;; Write the NUL-terminated string at $p to stdout: one wire frame.
  (func $emit (param $p i32)
    (i32.store (i32.const 0) (local.get $p))
    (i32.store (i32.const 4) (call $strlen (local.get $p)))
    (drop (call $fd_write (i32.const 1) (i32.const 0) (i32.const 1) (i32.const 16))))

  ;; Write $len bytes of scratch memory to $fd.
  (func $spew (param $fd i32) (param $len i32)
    (i32.store (i32.const 0) (i32.const 12288))
    (i32.store (i32.const 4) (local.get $len))
    (drop (call $fd_write (local.get $fd) (i32.const 0) (i32.const 1) (i32.const 16))))

  ;; Read one newline-terminated frame from stdin into the input buffer at
  ;; 8192; returns its length, newline excluded.
  (func $read_line (result i32)
    (local $n i32)
    (block $done
      (loop $l
        (i32.store (i32.const 0) (i32.add (i32.const 65536) (local.get $n)))
        (i32.store (i32.const 4) (i32.const 1))
        (i32.store (i32.const 16) (i32.const 0))
        (br_if $done (i32.ne (call $fd_read (i32.const 0) (i32.const 0) (i32.const 1) (i32.const 16)) (i32.const 0)))
        (br_if $done (i32.eqz (i32.load (i32.const 16))))
        (br_if $done (i32.eq (i32.load8_u (i32.add (i32.const 65536) (local.get $n))) (i32.const 10)))
        (local.set $n (i32.add (local.get $n) (i32.const 1)))
        (br_if $done (i32.ge_u (local.get $n) (i32.const 40000)))
        (br $l)))
    (local.get $n))

  ;; Naive substring search of the NUL-terminated needle at $needle inside the
  ;; first $len bytes of the input buffer.
  (func $contains (param $len i32) (param $needle i32) (result i32)
    (local $i i32) (local $j i32) (local $nl i32)
    (local.set $nl (call $strlen (local.get $needle)))
    (if (i32.gt_u (local.get $nl) (local.get $len)) (then (return (i32.const 0))))
    (block $no
      (loop $outer
        (br_if $no (i32.gt_u (i32.add (local.get $i) (local.get $nl)) (local.get $len)))
        (local.set $j (i32.const 0))
        (block $mismatch
          (loop $inner
            (br_if $mismatch (i32.ne
              (i32.load8_u (i32.add (i32.const 65536) (i32.add (local.get $i) (local.get $j))))
              (i32.load8_u (i32.add (local.get $needle) (local.get $j)))))
            (local.set $j (i32.add (local.get $j) (i32.const 1)))
            (br_if $inner (i32.lt_u (local.get $j) (local.get $nl))))
          (return (i32.const 1)))
        (local.set $i (i32.add (local.get $i) (i32.const 1)))
        (br $outer)))
    (i32.const 0))

  ;; Answer the request the way an honest hello-world plugin would.
  (func $answer
    (local $len i32)
    (local.set $len (call $read_line))
    (if (i32.eqz (call $contains (local.get $len) (i32.const 1024)))
      (then (call $emit (i32.const 768)) (return)))
    (if (call $contains (local.get $len) (i32.const 1088))
      (then (call $emit (i32.const 128)) (return)))
    (call $emit (i32.const 512)))
  (func (export "_start")
    (call $emit (i32.const 4200))
  )
)
"#;

/// Answers twice. The first answer is the answer.
pub const DOUBLE_ANSWER: &str = r#"(module
  (import "wasi_snapshot_preview1" "fd_read" (func $fd_read (param i32 i32 i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "fd_write" (func $fd_write (param i32 i32 i32 i32) (result i32)))
  (memory (export "memory") 2 4)
  (data (i32.const 128) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"aGVsbG8gZnJvbSB0aGUgc2FuZGJveA==\"}\0a\00")
  (data (i32.const 512) "{\"op\":\"response\",\"status\":404,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"bm8gc3VjaCBzYW5kYm94ZWQgcm91dGU=\"}\0a\00")
  (data (i32.const 768) "{\"op\":\"response\",\"status\":405,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"bWV0aG9kIG5vdCBhbGxvd2Vk\"}\0a\00")
  (data (i32.const 1024) "\"method\":\"GET\"\00")
  (data (i32.const 1088) "/greet\00")
  (data (i32.const 1120) "/etc/passwd\00")
  (data (i32.const 1152) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"],[\"set-cookie\",\"session=forged\"]],\"body_b64\":\"Y29va2llIGF0dGVtcHQ=\"}\0a\00")
  (data (i32.const 1600) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"],[\"x-evil\",\"a\\r\\nset-cookie: forged=1\"]],\"body_b64\":\"c3BsaXR0aW5nIGF0dGVtcHQ=\"}\0a\00")
  (data (i32.const 2048) "{\"op\":\"response\",\"status\":200,\0a\00")
  (data (i32.const 2176) "{\"op\":\"exec\",\"cmd\":\"/bin/sh\"}\0a\00")
  (data (i32.const 2304) "{\"op\":\"response\",\"status\":999,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"aW1wb3NzaWJsZSBzdGF0dXM=\"}\0a\00")
  (data (i32.const 2816) "X\00")
  (data (i32.const 3072) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"c2Vjb25kIGFuc3dlcg==\"}\0a\00")
  (data (i32.const 3584) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"],[\"x-autumn-sandboxed\",\"not-this-plugin\"],[\"x-content-type-options\",\"off\"]],\"body_b64\":\"YXR0cmlidXRpb24gYXR0ZW1wdA==\"}\0a\00")
  (data (i32.const 4200) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"aGVsbG8gZnJvbSB0aGUgc2FuZGJveA==\"}\00")
  (data (i32.const 5000) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"ZW50cm9weSBlY2hvZWQgaW4gdGhlIHN0YXR1cw==\"}\0a\00")
  (data (i32.const 6000) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"],[\"x-accel-redirect\",\"/internal/etc/passwd\"],[\"x-sendfile\",\"/etc/passwd\"]],\"body_b64\":\"cHJveHkgcmVkaXJlY3QgYXR0ZW1wdA==\"}\0a\00")
  (data (i32.const 6400) "\ff\fe\fd\fc\0a\00")
  (data (i32.const 6500) "{\"op\":\"error\",\"detail\":\"\\n2026-01-01T00:00:00Z  INFO forged: the host said this\\u001b[2K\\rBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB\"}\0a\00")

  ;; Length of the NUL-terminated string at $p.
  (func $strlen (param $p i32) (result i32)
    (local $n i32)
    (block $done
      (loop $l
        (br_if $done (i32.eqz (i32.load8_u (i32.add (local.get $p) (local.get $n)))))
        (local.set $n (i32.add (local.get $n) (i32.const 1)))
        (br $l)))
    (local.get $n))

  ;; Write the NUL-terminated string at $p to stdout: one wire frame.
  (func $emit (param $p i32)
    (i32.store (i32.const 0) (local.get $p))
    (i32.store (i32.const 4) (call $strlen (local.get $p)))
    (drop (call $fd_write (i32.const 1) (i32.const 0) (i32.const 1) (i32.const 16))))

  ;; Write $len bytes of scratch memory to $fd.
  (func $spew (param $fd i32) (param $len i32)
    (i32.store (i32.const 0) (i32.const 12288))
    (i32.store (i32.const 4) (local.get $len))
    (drop (call $fd_write (local.get $fd) (i32.const 0) (i32.const 1) (i32.const 16))))

  ;; Read one newline-terminated frame from stdin into the input buffer at
  ;; 8192; returns its length, newline excluded.
  (func $read_line (result i32)
    (local $n i32)
    (block $done
      (loop $l
        (i32.store (i32.const 0) (i32.add (i32.const 65536) (local.get $n)))
        (i32.store (i32.const 4) (i32.const 1))
        (i32.store (i32.const 16) (i32.const 0))
        (br_if $done (i32.ne (call $fd_read (i32.const 0) (i32.const 0) (i32.const 1) (i32.const 16)) (i32.const 0)))
        (br_if $done (i32.eqz (i32.load (i32.const 16))))
        (br_if $done (i32.eq (i32.load8_u (i32.add (i32.const 65536) (local.get $n))) (i32.const 10)))
        (local.set $n (i32.add (local.get $n) (i32.const 1)))
        (br_if $done (i32.ge_u (local.get $n) (i32.const 40000)))
        (br $l)))
    (local.get $n))

  ;; Naive substring search of the NUL-terminated needle at $needle inside the
  ;; first $len bytes of the input buffer.
  (func $contains (param $len i32) (param $needle i32) (result i32)
    (local $i i32) (local $j i32) (local $nl i32)
    (local.set $nl (call $strlen (local.get $needle)))
    (if (i32.gt_u (local.get $nl) (local.get $len)) (then (return (i32.const 0))))
    (block $no
      (loop $outer
        (br_if $no (i32.gt_u (i32.add (local.get $i) (local.get $nl)) (local.get $len)))
        (local.set $j (i32.const 0))
        (block $mismatch
          (loop $inner
            (br_if $mismatch (i32.ne
              (i32.load8_u (i32.add (i32.const 65536) (i32.add (local.get $i) (local.get $j))))
              (i32.load8_u (i32.add (local.get $needle) (local.get $j)))))
            (local.set $j (i32.add (local.get $j) (i32.const 1)))
            (br_if $inner (i32.lt_u (local.get $j) (local.get $nl))))
          (return (i32.const 1)))
        (local.set $i (i32.add (local.get $i) (i32.const 1)))
        (br $outer)))
    (i32.const 0))

  ;; Answer the request the way an honest hello-world plugin would.
  (func $answer
    (local $len i32)
    (local.set $len (call $read_line))
    (if (i32.eqz (call $contains (local.get $len) (i32.const 1024)))
      (then (call $emit (i32.const 768)) (return)))
    (if (call $contains (local.get $len) (i32.const 1088))
      (then (call $emit (i32.const 128)) (return)))
    (call $emit (i32.const 512)))
  (func (export "_start")
    (call $answer)
    (call $emit (i32.const 3072))
  )
)
"#;

/// Answers and then spins forever. The answer stands, and the host must not
/// wait out the whole fuel budget to serve it.
pub const ANSWER_THEN_SPIN: &str = r#"(module
  (import "wasi_snapshot_preview1" "fd_read" (func $fd_read (param i32 i32 i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "fd_write" (func $fd_write (param i32 i32 i32 i32) (result i32)))
  (memory (export "memory") 2 4)
  (data (i32.const 128) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"aGVsbG8gZnJvbSB0aGUgc2FuZGJveA==\"}\0a\00")
  (data (i32.const 512) "{\"op\":\"response\",\"status\":404,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"bm8gc3VjaCBzYW5kYm94ZWQgcm91dGU=\"}\0a\00")
  (data (i32.const 768) "{\"op\":\"response\",\"status\":405,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"bWV0aG9kIG5vdCBhbGxvd2Vk\"}\0a\00")
  (data (i32.const 1024) "\"method\":\"GET\"\00")
  (data (i32.const 1088) "/greet\00")
  (data (i32.const 1120) "/etc/passwd\00")
  (data (i32.const 1152) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"],[\"set-cookie\",\"session=forged\"]],\"body_b64\":\"Y29va2llIGF0dGVtcHQ=\"}\0a\00")
  (data (i32.const 1600) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"],[\"x-evil\",\"a\\r\\nset-cookie: forged=1\"]],\"body_b64\":\"c3BsaXR0aW5nIGF0dGVtcHQ=\"}\0a\00")
  (data (i32.const 2048) "{\"op\":\"response\",\"status\":200,\0a\00")
  (data (i32.const 2176) "{\"op\":\"exec\",\"cmd\":\"/bin/sh\"}\0a\00")
  (data (i32.const 2304) "{\"op\":\"response\",\"status\":999,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"aW1wb3NzaWJsZSBzdGF0dXM=\"}\0a\00")
  (data (i32.const 2816) "X\00")
  (data (i32.const 3072) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"c2Vjb25kIGFuc3dlcg==\"}\0a\00")
  (data (i32.const 3584) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"],[\"x-autumn-sandboxed\",\"not-this-plugin\"],[\"x-content-type-options\",\"off\"]],\"body_b64\":\"YXR0cmlidXRpb24gYXR0ZW1wdA==\"}\0a\00")
  (data (i32.const 4200) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"aGVsbG8gZnJvbSB0aGUgc2FuZGJveA==\"}\00")
  (data (i32.const 5000) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"ZW50cm9weSBlY2hvZWQgaW4gdGhlIHN0YXR1cw==\"}\0a\00")
  (data (i32.const 6000) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"],[\"x-accel-redirect\",\"/internal/etc/passwd\"],[\"x-sendfile\",\"/etc/passwd\"]],\"body_b64\":\"cHJveHkgcmVkaXJlY3QgYXR0ZW1wdA==\"}\0a\00")
  (data (i32.const 6400) "\ff\fe\fd\fc\0a\00")
  (data (i32.const 6500) "{\"op\":\"error\",\"detail\":\"\\n2026-01-01T00:00:00Z  INFO forged: the host said this\\u001b[2K\\rBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB\"}\0a\00")

  ;; Length of the NUL-terminated string at $p.
  (func $strlen (param $p i32) (result i32)
    (local $n i32)
    (block $done
      (loop $l
        (br_if $done (i32.eqz (i32.load8_u (i32.add (local.get $p) (local.get $n)))))
        (local.set $n (i32.add (local.get $n) (i32.const 1)))
        (br $l)))
    (local.get $n))

  ;; Write the NUL-terminated string at $p to stdout: one wire frame.
  (func $emit (param $p i32)
    (i32.store (i32.const 0) (local.get $p))
    (i32.store (i32.const 4) (call $strlen (local.get $p)))
    (drop (call $fd_write (i32.const 1) (i32.const 0) (i32.const 1) (i32.const 16))))

  ;; Write $len bytes of scratch memory to $fd.
  (func $spew (param $fd i32) (param $len i32)
    (i32.store (i32.const 0) (i32.const 12288))
    (i32.store (i32.const 4) (local.get $len))
    (drop (call $fd_write (local.get $fd) (i32.const 0) (i32.const 1) (i32.const 16))))

  ;; Read one newline-terminated frame from stdin into the input buffer at
  ;; 8192; returns its length, newline excluded.
  (func $read_line (result i32)
    (local $n i32)
    (block $done
      (loop $l
        (i32.store (i32.const 0) (i32.add (i32.const 65536) (local.get $n)))
        (i32.store (i32.const 4) (i32.const 1))
        (i32.store (i32.const 16) (i32.const 0))
        (br_if $done (i32.ne (call $fd_read (i32.const 0) (i32.const 0) (i32.const 1) (i32.const 16)) (i32.const 0)))
        (br_if $done (i32.eqz (i32.load (i32.const 16))))
        (br_if $done (i32.eq (i32.load8_u (i32.add (i32.const 65536) (local.get $n))) (i32.const 10)))
        (local.set $n (i32.add (local.get $n) (i32.const 1)))
        (br_if $done (i32.ge_u (local.get $n) (i32.const 40000)))
        (br $l)))
    (local.get $n))

  ;; Naive substring search of the NUL-terminated needle at $needle inside the
  ;; first $len bytes of the input buffer.
  (func $contains (param $len i32) (param $needle i32) (result i32)
    (local $i i32) (local $j i32) (local $nl i32)
    (local.set $nl (call $strlen (local.get $needle)))
    (if (i32.gt_u (local.get $nl) (local.get $len)) (then (return (i32.const 0))))
    (block $no
      (loop $outer
        (br_if $no (i32.gt_u (i32.add (local.get $i) (local.get $nl)) (local.get $len)))
        (local.set $j (i32.const 0))
        (block $mismatch
          (loop $inner
            (br_if $mismatch (i32.ne
              (i32.load8_u (i32.add (i32.const 65536) (i32.add (local.get $i) (local.get $j))))
              (i32.load8_u (i32.add (local.get $needle) (local.get $j)))))
            (local.set $j (i32.add (local.get $j) (i32.const 1)))
            (br_if $inner (i32.lt_u (local.get $j) (local.get $nl))))
          (return (i32.const 1)))
        (local.set $i (i32.add (local.get $i) (i32.const 1)))
        (br $outer)))
    (i32.const 0))

  ;; Answer the request the way an honest hello-world plugin would.
  (func $answer
    (local $len i32)
    (local.set $len (call $read_line))
    (if (i32.eqz (call $contains (local.get $len) (i32.const 1024)))
      (then (call $emit (i32.const 768)) (return)))
    (if (call $contains (local.get $len) (i32.const 1088))
      (then (call $emit (i32.const 128)) (return)))
    (call $emit (i32.const 512)))
  (func (export "_start")
    (local $i i32)
    (call $answer)
    (loop $l (local.set $i (i32.add (local.get $i) (i32.const 1))) (br $l))
  )
)
"#;

/// Exports no `_start`, so there is nothing to run.
pub const NO_START: &str = r#"(module
  (import "wasi_snapshot_preview1" "fd_write" (func $fd_write (param i32 i32 i32 i32) (result i32)))
  (memory (export "memory") 1)
  (func (export "other") (nop))
)
"#;

/// Reads one entropy byte and folds it into its status code, so two runs of
/// the same artifact are only identical if the host's entropy is.
pub const ENTROPY: &str = r#"(module
  (import "wasi_snapshot_preview1" "fd_read" (func $fd_read (param i32 i32 i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "fd_write" (func $fd_write (param i32 i32 i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "random_get" (func $random_get (param i32 i32) (result i32)))
  (memory (export "memory") 2 4)
  (data (i32.const 128) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"aGVsbG8gZnJvbSB0aGUgc2FuZGJveA==\"}\0a\00")
  (data (i32.const 512) "{\"op\":\"response\",\"status\":404,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"bm8gc3VjaCBzYW5kYm94ZWQgcm91dGU=\"}\0a\00")
  (data (i32.const 768) "{\"op\":\"response\",\"status\":405,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"bWV0aG9kIG5vdCBhbGxvd2Vk\"}\0a\00")
  (data (i32.const 1024) "\"method\":\"GET\"\00")
  (data (i32.const 1088) "/greet\00")
  (data (i32.const 1120) "/etc/passwd\00")
  (data (i32.const 1152) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"],[\"set-cookie\",\"session=forged\"]],\"body_b64\":\"Y29va2llIGF0dGVtcHQ=\"}\0a\00")
  (data (i32.const 1600) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"],[\"x-evil\",\"a\\r\\nset-cookie: forged=1\"]],\"body_b64\":\"c3BsaXR0aW5nIGF0dGVtcHQ=\"}\0a\00")
  (data (i32.const 2048) "{\"op\":\"response\",\"status\":200,\0a\00")
  (data (i32.const 2176) "{\"op\":\"exec\",\"cmd\":\"/bin/sh\"}\0a\00")
  (data (i32.const 2304) "{\"op\":\"response\",\"status\":999,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"aW1wb3NzaWJsZSBzdGF0dXM=\"}\0a\00")
  (data (i32.const 2816) "X\00")
  (data (i32.const 3072) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"c2Vjb25kIGFuc3dlcg==\"}\0a\00")
  (data (i32.const 3584) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"],[\"x-autumn-sandboxed\",\"not-this-plugin\"],[\"x-content-type-options\",\"off\"]],\"body_b64\":\"YXR0cmlidXRpb24gYXR0ZW1wdA==\"}\0a\00")
  (data (i32.const 4200) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"aGVsbG8gZnJvbSB0aGUgc2FuZGJveA==\"}\00")
  (data (i32.const 5000) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"ZW50cm9weSBlY2hvZWQgaW4gdGhlIHN0YXR1cw==\"}\0a\00")
  (data (i32.const 6000) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"],[\"x-accel-redirect\",\"/internal/etc/passwd\"],[\"x-sendfile\",\"/etc/passwd\"]],\"body_b64\":\"cHJveHkgcmVkaXJlY3QgYXR0ZW1wdA==\"}\0a\00")
  (data (i32.const 6400) "\ff\fe\fd\fc\0a\00")
  (data (i32.const 6500) "{\"op\":\"error\",\"detail\":\"\\n2026-01-01T00:00:00Z  INFO forged: the host said this\\u001b[2K\\rBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB\"}\0a\00")

  ;; Length of the NUL-terminated string at $p.
  (func $strlen (param $p i32) (result i32)
    (local $n i32)
    (block $done
      (loop $l
        (br_if $done (i32.eqz (i32.load8_u (i32.add (local.get $p) (local.get $n)))))
        (local.set $n (i32.add (local.get $n) (i32.const 1)))
        (br $l)))
    (local.get $n))

  ;; Write the NUL-terminated string at $p to stdout: one wire frame.
  (func $emit (param $p i32)
    (i32.store (i32.const 0) (local.get $p))
    (i32.store (i32.const 4) (call $strlen (local.get $p)))
    (drop (call $fd_write (i32.const 1) (i32.const 0) (i32.const 1) (i32.const 16))))

  ;; Write $len bytes of scratch memory to $fd.
  (func $spew (param $fd i32) (param $len i32)
    (i32.store (i32.const 0) (i32.const 12288))
    (i32.store (i32.const 4) (local.get $len))
    (drop (call $fd_write (local.get $fd) (i32.const 0) (i32.const 1) (i32.const 16))))

  ;; Read one newline-terminated frame from stdin into the input buffer at
  ;; 8192; returns its length, newline excluded.
  (func $read_line (result i32)
    (local $n i32)
    (block $done
      (loop $l
        (i32.store (i32.const 0) (i32.add (i32.const 65536) (local.get $n)))
        (i32.store (i32.const 4) (i32.const 1))
        (i32.store (i32.const 16) (i32.const 0))
        (br_if $done (i32.ne (call $fd_read (i32.const 0) (i32.const 0) (i32.const 1) (i32.const 16)) (i32.const 0)))
        (br_if $done (i32.eqz (i32.load (i32.const 16))))
        (br_if $done (i32.eq (i32.load8_u (i32.add (i32.const 65536) (local.get $n))) (i32.const 10)))
        (local.set $n (i32.add (local.get $n) (i32.const 1)))
        (br_if $done (i32.ge_u (local.get $n) (i32.const 40000)))
        (br $l)))
    (local.get $n))

  ;; Naive substring search of the NUL-terminated needle at $needle inside the
  ;; first $len bytes of the input buffer.
  (func $contains (param $len i32) (param $needle i32) (result i32)
    (local $i i32) (local $j i32) (local $nl i32)
    (local.set $nl (call $strlen (local.get $needle)))
    (if (i32.gt_u (local.get $nl) (local.get $len)) (then (return (i32.const 0))))
    (block $no
      (loop $outer
        (br_if $no (i32.gt_u (i32.add (local.get $i) (local.get $nl)) (local.get $len)))
        (local.set $j (i32.const 0))
        (block $mismatch
          (loop $inner
            (br_if $mismatch (i32.ne
              (i32.load8_u (i32.add (i32.const 65536) (i32.add (local.get $i) (local.get $j))))
              (i32.load8_u (i32.add (local.get $needle) (local.get $j)))))
            (local.set $j (i32.add (local.get $j) (i32.const 1)))
            (br_if $inner (i32.lt_u (local.get $j) (local.get $nl))))
          (return (i32.const 1)))
        (local.set $i (i32.add (local.get $i) (i32.const 1)))
        (br $outer)))
    (i32.const 0))

  ;; Answer the request the way an honest hello-world plugin would.
  (func $answer
    (local $len i32)
    (local.set $len (call $read_line))
    (if (i32.eqz (call $contains (local.get $len) (i32.const 1024)))
      (then (call $emit (i32.const 768)) (return)))
    (if (call $contains (local.get $len) (i32.const 1088))
      (then (call $emit (i32.const 128)) (return)))
    (call $emit (i32.const 512)))
  (func (export "_start")
    (drop (call $random_get (i32.const 12288) (i32.const 1)))
    (i32.store8 (i32.const 5028)
      (i32.add (i32.const 48) (i32.and (i32.load8_u (i32.const 12288)) (i32.const 7))))
    (call $emit (i32.const 5000))
  )
)
"#;

/// A guest that calls `fd_write` `calls` times with `iovecs` **zero-length**
/// descriptors, then answers honestly.
///
/// Parameterised rather than written out twice because the whole point is a
/// controlled comparison: two modules from this function differ in exactly one
/// `i32.const` operand, so they execute an identical number of guest
/// instructions and any gap in `fuel_used` between them is host work priced per
/// descriptor. A pair of hand-written fixtures could not promise that.
///
/// The descriptor array is never initialised on purpose. Linear memory starts
/// zeroed, so every entry is already `{ pointer: 0, length: 0 }` — in bounds,
/// and with nothing to copy, which is precisely the shape that walks the host's
/// iovec loop without reaching its byte charge.
#[must_use]
pub fn empty_iovecs(iovecs: u32, calls: u32) -> String {
    format!(
        r#"(module
  (import "wasi_snapshot_preview1" "fd_write" (func $fd_write (param i32 i32 i32 i32) (result i32)))
  (memory (export "memory") 2 4)
  (data (i32.const 128) "{{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"aGVsbG8gZnJvbSB0aGUgc2FuZGJveA==\"}}\0a\00")

  ;; Length of the NUL-terminated string at $p.
  (func $strlen (param $p i32) (result i32)
    (local $n i32)
    (block $done
      (loop $l
        (br_if $done (i32.eqz (i32.load8_u (i32.add (local.get $p) (local.get $n)))))
        (local.set $n (i32.add (local.get $n) (i32.const 1)))
        (br $l)))
    (local.get $n))

  ;; Write the NUL-terminated string at $p to stdout: one wire frame.
  (func $emit (param $p i32)
    (i32.store (i32.const 0) (local.get $p))
    (i32.store (i32.const 4) (call $strlen (local.get $p)))
    (drop (call $fd_write (i32.const 1) (i32.const 0) (i32.const 1) (i32.const 16))))

  (func (export "_start")
    (local $i i32)
    (loop $l
      ;; The array at 4096 is untouched zeroed memory: {iovecs} descriptors that
      ;; each point nowhere and ask for nothing.
      (drop (call $fd_write (i32.const 1) (i32.const 4096) (i32.const {iovecs}) (i32.const 16)))
      (local.set $i (i32.add (local.get $i) (i32.const 1)))
      (br_if $l (i32.lt_u (local.get $i) (i32.const {calls}))))
    (call $emit (i32.const 128)))
)
"#
    )
}

/// A guest whose single `fd_write` carries a complete `kv-set` frame and then
/// a line that never ends, long enough to overrun the stdout budget.
///
/// The shape that made the defect reachable: `write_stdout` parses and queues
/// the first frame, refuses at the second and answers `false`. Both land in one
/// host-side chunk, so the queued call and the rejection are decided together —
/// and the request is going to end as `OutputBudget` either way. If the queue is
/// serviced anyway, the `kv-set` commits on a request whose caller is told the
/// write failed and may retry it.
///
/// `pad` is the length of the unterminated tail; it must exceed
/// `2 × max_response_bytes + 4096` for the write to be refused.
#[must_use]
pub fn call_then_overrun(pad: usize) -> String {
    // Written out rather than escaped from a Rust literal: the WAT string needs
    // `\"` for its quotes and `\0a` for the newline, and getting that wrong
    // silently produces a frame the host cannot parse — which would make this
    // guest prove nothing while still passing.
    const FRAME: &str = r#"{\"op\":\"call\",\"call\":\"kv-set\",\"id\":1,\"key\":\"cart\",\"value\":\"one item\"}\0a"#;
    // The frame's length in bytes, not in source characters: `\"` is one byte
    // and `\0a` is one byte.
    let frame_len =
        FRAME.len() - (FRAME.matches(r#"\""#).count() + 2 * FRAME.matches(r"\0a").count());
    let start = 4096;
    let frame_end = start + frame_len;
    let total = frame_len + pad;
    format!(
        r#"(module
  (import "wasi_snapshot_preview1" "fd_write" (func $fd_write (param i32 i32 i32 i32) (result i32)))
  (memory (export "memory") 4 4)
  (data (i32.const {start}) "{FRAME}")

  (func (export "_start")
    (local $i i32)
    ;; A tail of 'X' immediately after the frame, with no newline anywhere in
    ;; it, so the host's pending line grows past its ceiling.
    (loop $l
      (i32.store8 (i32.add (i32.const {frame_end}) (local.get $i)) (i32.const 88))
      (local.set $i (i32.add (local.get $i) (i32.const 1)))
      (br_if $l (i32.lt_u (local.get $i) (i32.const {pad}))))
    ;; One write, one chunk: the call and the overrun arrive together.
    (i32.store (i32.const 0) (i32.const {start}))
    (i32.store (i32.const 4) (i32.const {total}))
    (drop (call $fd_write (i32.const 1) (i32.const 0) (i32.const 1) (i32.const 16))))
)
"#
    )
}

// ── Capability-channel guests (issue #1632) ──────────────────────────────

/// A plugin that uses the capability channel: it reads the request frame,
/// picks a call from the path, writes the call frame, reads the answer, and
/// reports whether the host allowed it.
///
/// The answer is deliberately coarse — 200 for `"status":"ok"`, 403 otherwise —
/// because the point of these tests is *which calls the host allows*, and a
/// guest that had to base64 the reply back out would be a hundred lines of WAT
/// whose bugs would look like containment failures.
///
/// One guest with a dispatch table rather than one per call: the escape corpus
/// above is one module per misbehaviour because each *module* is the thing
/// under test, and here it is each *call*.
pub const CAPABILITY_CLIENT: &str = r#"(module
  (import "wasi_snapshot_preview1" "fd_read" (func $fd_read (param i32 i32 i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "fd_write" (func $fd_write (param i32 i32 i32 i32) (result i32)))
  (memory (export "memory") 2 4)

  ;; Answers.
  (data (i32.const 128) "{\"op\":\"response\",\"status\":200,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"YWxsb3dlZA==\"}\0a\00")
  (data (i32.const 384) "{\"op\":\"response\",\"status\":403,\"headers\":[[\"content-type\",\"text/plain; charset=utf-8\"]],\"body_b64\":\"cmVmdXNlZA==\"}\0a\00")

  ;; Needles matched against the request frame, and the call each selects.
  (data (i32.const 640) "/kv-write\00")
  (data (i32.const 660) "/kv-read\00")
  (data (i32.const 680) "/kv-other-tenant\00")
  (data (i32.const 704) "/fetch-granted\00")
  (data (i32.const 724) "/fetch-undeclared\00")
  (data (i32.const 748) "/db-insert\00")
  (data (i32.const 768) "/db-host-table\00")
  (data (i32.const 788) "/db-tenant-column\00")
  (data (i32.const 812) "/job-granted\00")
  (data (i32.const 832) "/job-undeclared\00")
  (data (i32.const 856) "/kv-flood\00")
  (data (i32.const 876) "\"status\":\"ok\"\00")
  (data (i32.const 896) "\"op\":\"render\"\00")


  ;; Call frames.
  (data (i32.const 1024) "{\"op\":\"call\",\"call\":\"kv-set\",\"id\":1,\"key\":\"cart\",\"value\":\"one item\"}\0a\00")
  (data (i32.const 1280) "{\"op\":\"call\",\"call\":\"kv-get\",\"id\":2,\"key\":\"cart\"}\0a\00")
  (data (i32.const 1536) "{\"op\":\"call\",\"call\":\"kv-get\",\"id\":3,\"key\":\"beta:cart\"}\0a\00")
  (data (i32.const 1792) "{\"op\":\"call\",\"call\":\"http-fetch\",\"id\":4,\"method\":\"GET\",\"url\":\"https://api.example.com/v1\"}\0a\00")
  (data (i32.const 2048) "{\"op\":\"call\",\"call\":\"http-fetch\",\"id\":5,\"method\":\"GET\",\"url\":\"https://api.example.com.attacker.test/v1\"}\0a\00")
  (data (i32.const 2304) "{\"op\":\"call\",\"call\":\"db-insert\",\"id\":6,\"table\":\"orders\",\"row\":{\"sku\":\"A-1\"}}\0a\00")
  (data (i32.const 2560) "{\"op\":\"call\",\"call\":\"db-query\",\"id\":7,\"table\":\"users\"}\0a\00")
  (data (i32.const 2816) "{\"op\":\"call\",\"call\":\"db-insert\",\"id\":8,\"table\":\"orders\",\"row\":{\"tenant_id\":\"beta\"}}\0a\00")
  (data (i32.const 3072) "{\"op\":\"call\",\"call\":\"job-enqueue\",\"id\":9,\"job_type\":\"reindex\"}\0a\00")
  (data (i32.const 3328) "{\"op\":\"call\",\"call\":\"job-enqueue\",\"id\":10,\"job_type\":\"drain-accounts\"}\0a\00")
  ;; Two `http-fetch`es to the granted host, so a per-request `outbound_calls`
  ;; quota of one can actually be exceeded — a guest that makes a single call
  ;; can never demonstrate a ceiling.
  (data (i32.const 3584) "{\"op\":\"call\",\"call\":\"http-fetch\",\"id\":11,\"method\":\"GET\",\"url\":\"https://api.example.com/v1\"}\0a\00")
  ;; The render answer, kept clear of every call frame above: data segments
  ;; apply in order, so a block that overruns the next one's offset silently
  ;; truncates itself.
  (data (i32.const 4096) "{\"op\":\"fragment\",\"nodes\":[{\"node\":\"element\",\"tag\":\"p\",\"attributes\":[[\"class\",\"panel\"]],\"children\":[{\"node\":\"text\",\"text\":\"3 orders\"}]}]}\0a\00")
  ;; Needles for the two routes that need a *hit* rather than a call.
  (data (i32.const 4608) "\"found\":true\00")
  (data (i32.const 4640) "/kv-read-hit\00")
  (data (i32.const 4672) "/fetch-twice\00")

  (func $strlen (param $p i32) (result i32)
    (local $n i32)
    (block $done
      (loop $l
        (br_if $done (i32.eqz (i32.load8_u (i32.add (local.get $p) (local.get $n)))))
        (local.set $n (i32.add (local.get $n) (i32.const 1)))
        (br $l)))
    (local.get $n))

  (func $emit (param $p i32)
    (i32.store (i32.const 0) (local.get $p))
    (i32.store (i32.const 4) (call $strlen (local.get $p)))
    (drop (call $fd_write (i32.const 1) (i32.const 0) (i32.const 1) (i32.const 16))))

  ;; Read one newline-terminated frame into the buffer at 65536.
  (func $read_line (result i32)
    (local $n i32)
    (block $done
      (loop $l
        (i32.store (i32.const 0) (i32.add (i32.const 65536) (local.get $n)))
        (i32.store (i32.const 4) (i32.const 1))
        (i32.store (i32.const 16) (i32.const 0))
        (br_if $done (i32.ne (call $fd_read (i32.const 0) (i32.const 0) (i32.const 1) (i32.const 16)) (i32.const 0)))
        (br_if $done (i32.eqz (i32.load (i32.const 16))))
        (br_if $done (i32.eq (i32.load8_u (i32.add (i32.const 65536) (local.get $n))) (i32.const 10)))
        (local.set $n (i32.add (local.get $n) (i32.const 1)))
        (br_if $done (i32.ge_u (local.get $n) (i32.const 40000)))
        (br $l)))
    (local.get $n))

  (func $contains (param $len i32) (param $needle i32) (result i32)
    (local $i i32) (local $j i32) (local $nl i32)
    (local.set $nl (call $strlen (local.get $needle)))
    (if (i32.gt_u (local.get $nl) (local.get $len)) (then (return (i32.const 0))))
    (block $no
      (loop $outer
        (br_if $no (i32.gt_u (i32.add (local.get $i) (local.get $nl)) (local.get $len)))
        (local.set $j (i32.const 0))
        (block $mismatch
          (loop $inner
            (br_if $mismatch (i32.ne
              (i32.load8_u (i32.add (i32.const 65536) (i32.add (local.get $i) (local.get $j))))
              (i32.load8_u (i32.add (local.get $needle) (local.get $j)))))
            (local.set $j (i32.add (local.get $j) (i32.const 1)))
            (br_if $inner (i32.lt_u (local.get $j) (local.get $nl))))
          (return (i32.const 1)))
        (local.set $i (i32.add (local.get $i) (i32.const 1)))
        (br $outer)))
    (i32.const 0))

  ;; Which call frame this request selects, or 0 for none.
  (func $select (param $len i32) (result i32)
    (if (call $contains (local.get $len) (i32.const 640)) (then (return (i32.const 1024))))
    (if (call $contains (local.get $len) (i32.const 660)) (then (return (i32.const 1280))))
    (if (call $contains (local.get $len) (i32.const 724)) (then (return (i32.const 2048))))
    (if (call $contains (local.get $len) (i32.const 704)) (then (return (i32.const 1792))))
    (if (call $contains (local.get $len) (i32.const 768)) (then (return (i32.const 2560))))
    (if (call $contains (local.get $len) (i32.const 788)) (then (return (i32.const 2816))))
    (if (call $contains (local.get $len) (i32.const 748)) (then (return (i32.const 2304))))
    (if (call $contains (local.get $len) (i32.const 832)) (then (return (i32.const 3328))))
    (if (call $contains (local.get $len) (i32.const 812)) (then (return (i32.const 3072))))
    (i32.const 0))

  (func (export "_start")
    (local $len i32) (local $call i32) (local $i i32)
    (local.set $len (call $read_line))
    ;; A render frame is answered with a fragment. One plugin, one manifest,
    ;; every capability — which is the reference plugin the issue's success
    ;; metric asks for rather than five plugins that each do one thing.
    (if (call $contains (local.get $len) (i32.const 896))
      (then (call $emit (i32.const 4096)) (return)))
    ;; The flood case never reads a reply, which is how the host's ceiling on
    ;; unread replies gets exercised at all.
    (if (call $contains (local.get $len) (i32.const 856))
      (then
        (block $stop
          (loop $l
            (call $emit (i32.const 1024))
            (local.set $i (i32.add (local.get $i) (i32.const 1)))
            (br_if $stop (i32.ge_u (local.get $i) (i32.const 100000)))
            (br $l)))
        (call $emit (i32.const 128))
        (return)))
    ;; Two `http-fetch`es in one request, so a per-request quota of one is
    ;; exceeded on the second. Answers 200 only if the second was refused.
    (if (call $contains (local.get $len) (i32.const 4672))
      (then
        (call $emit (i32.const 3584))
        (drop (call $read_line))
        (call $emit (i32.const 3584))
        (local.set $len (call $read_line))
        (if (call $contains (local.get $len) (i32.const 876))
          (then (call $emit (i32.const 384)))
          (else (call $emit (i32.const 128))))
        (return)))
    ;; A *hit*, not merely a call that was allowed. `kv-get` for a key another
    ;; tenant owns is correctly ALLOWED and correctly MISSES — the escaping is
    ;; what makes it miss — so a test that read the status alone could not tell
    ;; containment from success.
    ;; `/kv-read-hit` asks for its own key; `/kv-other-tenant` asks for
    ;; `beta:cart`, which is what a naive `format!("{plugin}:{tenant}:{key}")`
    ;; would have made reachable. Both answer 200 only on a hit.
    (local.set $call (i32.const 0))
    (if (call $contains (local.get $len) (i32.const 4640))
      (then (local.set $call (i32.const 1280))))
    (if (call $contains (local.get $len) (i32.const 680))
      (then (local.set $call (i32.const 1536))))
    (if (local.get $call)
      (then
        (call $emit (local.get $call))
        (local.set $len (call $read_line))
        (if (call $contains (local.get $len) (i32.const 4608))
          (then (call $emit (i32.const 128)))
          (else (call $emit (i32.const 384))))
        (return)))
    (local.set $call (call $select (local.get $len)))
    (if (i32.eqz (local.get $call)) (then (call $emit (i32.const 384)) (return)))
    (call $emit (local.get $call))
    (local.set $len (call $read_line))
    (if (call $contains (local.get $len) (i32.const 876))
      (then (call $emit (i32.const 128)))
      (else (call $emit (i32.const 384)))))
)
"#;

/// A plugin that answers a render frame with a fragment tree.
///
/// The fragment carries one legal element and, on the `/unsafe` path, a tag and
/// an attribute the renderer will not emit — so the same guest proves both that
/// a hook works and that a hostile one produces no page.
pub const RENDER_CLIENT: &str = r#"(module
  (import "wasi_snapshot_preview1" "fd_read" (func $fd_read (param i32 i32 i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "fd_write" (func $fd_write (param i32 i32 i32 i32) (result i32)))
  (memory (export "memory") 2 4)

  (data (i32.const 128) "{\"op\":\"fragment\",\"nodes\":[{\"node\":\"element\",\"tag\":\"p\",\"attributes\":[[\"class\",\"panel\"]],\"children\":[{\"node\":\"text\",\"text\":\"3 orders <b>& counting</b>\"}]}]}\0a\00")
  (data (i32.const 640) "{\"op\":\"fragment\",\"nodes\":[{\"node\":\"element\",\"tag\":\"script\",\"children\":[{\"node\":\"text\",\"text\":\"alert(1)\"}]}]}\0a\00")
  (data (i32.const 1024) "{\"op\":\"fragment\",\"nodes\":[{\"node\":\"element\",\"tag\":\"a\",\"attributes\":[[\"href\",\"javascript:alert(1)\"]],\"children\":[]}]}\0a\00")
  (data (i32.const 1536) "{\"op\":\"response\",\"status\":200,\"headers\":[],\"body_b64\":\"\"}\0a\00")
  (data (i32.const 1792) "\"slot\":\"unsafe-tag\"\00")
  (data (i32.const 1824) "\"slot\":\"unsafe-href\"\00")
  (data (i32.const 1856) "\"slot\":\"wrong-frame\"\00")

  (func $strlen (param $p i32) (result i32)
    (local $n i32)
    (block $done
      (loop $l
        (br_if $done (i32.eqz (i32.load8_u (i32.add (local.get $p) (local.get $n)))))
        (local.set $n (i32.add (local.get $n) (i32.const 1)))
        (br $l)))
    (local.get $n))

  (func $emit (param $p i32)
    (i32.store (i32.const 0) (local.get $p))
    (i32.store (i32.const 4) (call $strlen (local.get $p)))
    (drop (call $fd_write (i32.const 1) (i32.const 0) (i32.const 1) (i32.const 16))))

  (func $read_line (result i32)
    (local $n i32)
    (block $done
      (loop $l
        (i32.store (i32.const 0) (i32.add (i32.const 65536) (local.get $n)))
        (i32.store (i32.const 4) (i32.const 1))
        (i32.store (i32.const 16) (i32.const 0))
        (br_if $done (i32.ne (call $fd_read (i32.const 0) (i32.const 0) (i32.const 1) (i32.const 16)) (i32.const 0)))
        (br_if $done (i32.eqz (i32.load (i32.const 16))))
        (br_if $done (i32.eq (i32.load8_u (i32.add (i32.const 65536) (local.get $n))) (i32.const 10)))
        (local.set $n (i32.add (local.get $n) (i32.const 1)))
        (br_if $done (i32.ge_u (local.get $n) (i32.const 40000)))
        (br $l)))
    (local.get $n))

  (func $contains (param $len i32) (param $needle i32) (result i32)
    (local $i i32) (local $j i32) (local $nl i32)
    (local.set $nl (call $strlen (local.get $needle)))
    (if (i32.gt_u (local.get $nl) (local.get $len)) (then (return (i32.const 0))))
    (block $no
      (loop $outer
        (br_if $no (i32.gt_u (i32.add (local.get $i) (local.get $nl)) (local.get $len)))
        (local.set $j (i32.const 0))
        (block $mismatch
          (loop $inner
            (br_if $mismatch (i32.ne
              (i32.load8_u (i32.add (i32.const 65536) (i32.add (local.get $i) (local.get $j))))
              (i32.load8_u (i32.add (local.get $needle) (local.get $j)))))
            (local.set $j (i32.add (local.get $j) (i32.const 1)))
            (br_if $inner (i32.lt_u (local.get $j) (local.get $nl))))
          (return (i32.const 1)))
        (local.set $i (i32.add (local.get $i) (i32.const 1)))
        (br $outer)))
    (i32.const 0))

  (func (export "_start")
    (local $len i32)
    (local.set $len (call $read_line))
    (if (call $contains (local.get $len) (i32.const 1792))
      (then (call $emit (i32.const 640)) (return)))
    (if (call $contains (local.get $len) (i32.const 1824))
      (then (call $emit (i32.const 1024)) (return)))
    (if (call $contains (local.get $len) (i32.const 1856))
      (then (call $emit (i32.const 1536)) (return)))
    (call $emit (i32.const 128)))
)
"#;
