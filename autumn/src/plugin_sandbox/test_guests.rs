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


/// A well-behaved plugin: it reads the request frame, dispatches on
/// its content, and answers. Everything else in this corpus is a
/// variation on it that misbehaves in exactly one way.
pub const HELLO: &str = r##"(module
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

  ;; Read one newline-terminated frame from stdin into the input buffer at
  ;; 8192; returns its length, newline excluded.
  (func $read_line (result i32)
    (local $n i32)
    (block $done
      (loop $l
        (i32.store (i32.const 0) (i32.add (i32.const 8192) (local.get $n)))
        (i32.store (i32.const 4) (i32.const 1))
        (i32.store (i32.const 16) (i32.const 0))
        (br_if $done (i32.ne (call $fd_read (i32.const 0) (i32.const 0) (i32.const 1) (i32.const 16)) (i32.const 0)))
        (br_if $done (i32.eqz (i32.load (i32.const 16))))
        (br_if $done (i32.eq (i32.load8_u (i32.add (i32.const 8192) (local.get $n))) (i32.const 10)))
        (local.set $n (i32.add (local.get $n) (i32.const 1)))
        (br_if $done (i32.ge_u (local.get $n) (i32.const 100000)))
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
              (i32.load8_u (i32.add (i32.const 8192) (i32.add (local.get $i) (local.get $j))))
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
"##;

/// Never stops computing. Bounded only by the fuel ceiling.
pub const CPU_SPIN: &str = r##"(module
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

  ;; Read one newline-terminated frame from stdin into the input buffer at
  ;; 8192; returns its length, newline excluded.
  (func $read_line (result i32)
    (local $n i32)
    (block $done
      (loop $l
        (i32.store (i32.const 0) (i32.add (i32.const 8192) (local.get $n)))
        (i32.store (i32.const 4) (i32.const 1))
        (i32.store (i32.const 16) (i32.const 0))
        (br_if $done (i32.ne (call $fd_read (i32.const 0) (i32.const 0) (i32.const 1) (i32.const 16)) (i32.const 0)))
        (br_if $done (i32.eqz (i32.load (i32.const 16))))
        (br_if $done (i32.eq (i32.load8_u (i32.add (i32.const 8192) (local.get $n))) (i32.const 10)))
        (local.set $n (i32.add (local.get $n) (i32.const 1)))
        (br_if $done (i32.ge_u (local.get $n) (i32.const 100000)))
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
              (i32.load8_u (i32.add (i32.const 8192) (i32.add (local.get $i) (local.get $j))))
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
"##;

/// Grows linear memory until the host refuses, then keeps asking.
pub const MEMORY_BOMB: &str = r##"(module
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

  ;; Read one newline-terminated frame from stdin into the input buffer at
  ;; 8192; returns its length, newline excluded.
  (func $read_line (result i32)
    (local $n i32)
    (block $done
      (loop $l
        (i32.store (i32.const 0) (i32.add (i32.const 8192) (local.get $n)))
        (i32.store (i32.const 4) (i32.const 1))
        (i32.store (i32.const 16) (i32.const 0))
        (br_if $done (i32.ne (call $fd_read (i32.const 0) (i32.const 0) (i32.const 1) (i32.const 16)) (i32.const 0)))
        (br_if $done (i32.eqz (i32.load (i32.const 16))))
        (br_if $done (i32.eq (i32.load8_u (i32.add (i32.const 8192) (local.get $n))) (i32.const 10)))
        (local.set $n (i32.add (local.get $n) (i32.const 1)))
        (br_if $done (i32.ge_u (local.get $n) (i32.const 100000)))
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
              (i32.load8_u (i32.add (i32.const 8192) (i32.add (local.get $i) (local.get $j))))
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
"##;

/// Traps immediately — what a Rust panic compiles to on wasm.
pub const TRAP: &str = r##"(module
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

  ;; Read one newline-terminated frame from stdin into the input buffer at
  ;; 8192; returns its length, newline excluded.
  (func $read_line (result i32)
    (local $n i32)
    (block $done
      (loop $l
        (i32.store (i32.const 0) (i32.add (i32.const 8192) (local.get $n)))
        (i32.store (i32.const 4) (i32.const 1))
        (i32.store (i32.const 16) (i32.const 0))
        (br_if $done (i32.ne (call $fd_read (i32.const 0) (i32.const 0) (i32.const 1) (i32.const 16)) (i32.const 0)))
        (br_if $done (i32.eqz (i32.load (i32.const 16))))
        (br_if $done (i32.eq (i32.load8_u (i32.add (i32.const 8192) (local.get $n))) (i32.const 10)))
        (local.set $n (i32.add (local.get $n) (i32.const 1)))
        (br_if $done (i32.ge_u (local.get $n) (i32.const 100000)))
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
              (i32.load8_u (i32.add (i32.const 8192) (i32.add (local.get $i) (local.get $j))))
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
"##;

/// Calls `proc_exit`, the closest a guest can get to killing the process.
pub const EXIT: &str = r##"(module
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

  ;; Read one newline-terminated frame from stdin into the input buffer at
  ;; 8192; returns its length, newline excluded.
  (func $read_line (result i32)
    (local $n i32)
    (block $done
      (loop $l
        (i32.store (i32.const 0) (i32.add (i32.const 8192) (local.get $n)))
        (i32.store (i32.const 4) (i32.const 1))
        (i32.store (i32.const 16) (i32.const 0))
        (br_if $done (i32.ne (call $fd_read (i32.const 0) (i32.const 0) (i32.const 1) (i32.const 16)) (i32.const 0)))
        (br_if $done (i32.eqz (i32.load (i32.const 16))))
        (br_if $done (i32.eq (i32.load8_u (i32.add (i32.const 8192) (local.get $n))) (i32.const 10)))
        (local.set $n (i32.add (local.get $n) (i32.const 1)))
        (br_if $done (i32.ge_u (local.get $n) (i32.const 100000)))
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
              (i32.load8_u (i32.add (i32.const 8192) (i32.add (local.get $i) (local.get $j))))
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
"##;

/// Returns without ever answering.
pub const SILENT: &str = r##"(module
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

  ;; Read one newline-terminated frame from stdin into the input buffer at
  ;; 8192; returns its length, newline excluded.
  (func $read_line (result i32)
    (local $n i32)
    (block $done
      (loop $l
        (i32.store (i32.const 0) (i32.add (i32.const 8192) (local.get $n)))
        (i32.store (i32.const 4) (i32.const 1))
        (i32.store (i32.const 16) (i32.const 0))
        (br_if $done (i32.ne (call $fd_read (i32.const 0) (i32.const 0) (i32.const 1) (i32.const 16)) (i32.const 0)))
        (br_if $done (i32.eqz (i32.load (i32.const 16))))
        (br_if $done (i32.eq (i32.load8_u (i32.add (i32.const 8192) (local.get $n))) (i32.const 10)))
        (local.set $n (i32.add (local.get $n) (i32.const 1)))
        (br_if $done (i32.ge_u (local.get $n) (i32.const 100000)))
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
              (i32.load8_u (i32.add (i32.const 8192) (i32.add (local.get $i) (local.get $j))))
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
"##;

/// Tries to open `/etc/passwd`, then answers, so the denial is
/// observable without the guest dying of it.
pub const READ_FILE: &str = r##"(module
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

  ;; Read one newline-terminated frame from stdin into the input buffer at
  ;; 8192; returns its length, newline excluded.
  (func $read_line (result i32)
    (local $n i32)
    (block $done
      (loop $l
        (i32.store (i32.const 0) (i32.add (i32.const 8192) (local.get $n)))
        (i32.store (i32.const 4) (i32.const 1))
        (i32.store (i32.const 16) (i32.const 0))
        (br_if $done (i32.ne (call $fd_read (i32.const 0) (i32.const 0) (i32.const 1) (i32.const 16)) (i32.const 0)))
        (br_if $done (i32.eqz (i32.load (i32.const 16))))
        (br_if $done (i32.eq (i32.load8_u (i32.add (i32.const 8192) (local.get $n))) (i32.const 10)))
        (local.set $n (i32.add (local.get $n) (i32.const 1)))
        (br_if $done (i32.ge_u (local.get $n) (i32.const 100000)))
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
              (i32.load8_u (i32.add (i32.const 8192) (i32.add (local.get $i) (local.get $j))))
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
"##;

/// Asks for the pre-opened directories a WASI host normally hands out.
pub const DISCOVER_PREOPENS: &str = r##"(module
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

  ;; Read one newline-terminated frame from stdin into the input buffer at
  ;; 8192; returns its length, newline excluded.
  (func $read_line (result i32)
    (local $n i32)
    (block $done
      (loop $l
        (i32.store (i32.const 0) (i32.add (i32.const 8192) (local.get $n)))
        (i32.store (i32.const 4) (i32.const 1))
        (i32.store (i32.const 16) (i32.const 0))
        (br_if $done (i32.ne (call $fd_read (i32.const 0) (i32.const 0) (i32.const 1) (i32.const 16)) (i32.const 0)))
        (br_if $done (i32.eqz (i32.load (i32.const 16))))
        (br_if $done (i32.eq (i32.load8_u (i32.add (i32.const 8192) (local.get $n))) (i32.const 10)))
        (local.set $n (i32.add (local.get $n) (i32.const 1)))
        (br_if $done (i32.ge_u (local.get $n) (i32.const 100000)))
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
              (i32.load8_u (i32.add (i32.const 8192) (i32.add (local.get $i) (local.get $j))))
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
"##;

/// Reads from a descriptor it was never given.
pub const READ_STRAY_FD: &str = r##"(module
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

  ;; Read one newline-terminated frame from stdin into the input buffer at
  ;; 8192; returns its length, newline excluded.
  (func $read_line (result i32)
    (local $n i32)
    (block $done
      (loop $l
        (i32.store (i32.const 0) (i32.add (i32.const 8192) (local.get $n)))
        (i32.store (i32.const 4) (i32.const 1))
        (i32.store (i32.const 16) (i32.const 0))
        (br_if $done (i32.ne (call $fd_read (i32.const 0) (i32.const 0) (i32.const 1) (i32.const 16)) (i32.const 0)))
        (br_if $done (i32.eqz (i32.load (i32.const 16))))
        (br_if $done (i32.eq (i32.load8_u (i32.add (i32.const 8192) (local.get $n))) (i32.const 10)))
        (local.set $n (i32.add (local.get $n) (i32.const 1)))
        (br_if $done (i32.ge_u (local.get $n) (i32.const 100000)))
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
              (i32.load8_u (i32.add (i32.const 8192) (i32.add (local.get $i) (local.get $j))))
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
"##;

/// Tries to send on a socket.
pub const NETWORK: &str = r##"(module
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

  ;; Read one newline-terminated frame from stdin into the input buffer at
  ;; 8192; returns its length, newline excluded.
  (func $read_line (result i32)
    (local $n i32)
    (block $done
      (loop $l
        (i32.store (i32.const 0) (i32.add (i32.const 8192) (local.get $n)))
        (i32.store (i32.const 4) (i32.const 1))
        (i32.store (i32.const 16) (i32.const 0))
        (br_if $done (i32.ne (call $fd_read (i32.const 0) (i32.const 0) (i32.const 1) (i32.const 16)) (i32.const 0)))
        (br_if $done (i32.eqz (i32.load (i32.const 16))))
        (br_if $done (i32.eq (i32.load8_u (i32.add (i32.const 8192) (local.get $n))) (i32.const 10)))
        (local.set $n (i32.add (local.get $n) (i32.const 1)))
        (br_if $done (i32.ge_u (local.get $n) (i32.const 100000)))
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
              (i32.load8_u (i32.add (i32.const 8192) (i32.add (local.get $i) (local.get $j))))
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
"##;

/// Reads the host process's environment.
pub const ENVIRONMENT: &str = r##"(module
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

  ;; Read one newline-terminated frame from stdin into the input buffer at
  ;; 8192; returns its length, newline excluded.
  (func $read_line (result i32)
    (local $n i32)
    (block $done
      (loop $l
        (i32.store (i32.const 0) (i32.add (i32.const 8192) (local.get $n)))
        (i32.store (i32.const 4) (i32.const 1))
        (i32.store (i32.const 16) (i32.const 0))
        (br_if $done (i32.ne (call $fd_read (i32.const 0) (i32.const 0) (i32.const 1) (i32.const 16)) (i32.const 0)))
        (br_if $done (i32.eqz (i32.load (i32.const 16))))
        (br_if $done (i32.eq (i32.load8_u (i32.add (i32.const 8192) (local.get $n))) (i32.const 10)))
        (local.set $n (i32.add (local.get $n) (i32.const 1)))
        (br_if $done (i32.ge_u (local.get $n) (i32.const 100000)))
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
              (i32.load8_u (i32.add (i32.const 8192) (i32.add (local.get $i) (local.get $j))))
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
"##;

/// Reads the host process's argv.
pub const ARGUMENTS: &str = r##"(module
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

  ;; Read one newline-terminated frame from stdin into the input buffer at
  ;; 8192; returns its length, newline excluded.
  (func $read_line (result i32)
    (local $n i32)
    (block $done
      (loop $l
        (i32.store (i32.const 0) (i32.add (i32.const 8192) (local.get $n)))
        (i32.store (i32.const 4) (i32.const 1))
        (i32.store (i32.const 16) (i32.const 0))
        (br_if $done (i32.ne (call $fd_read (i32.const 0) (i32.const 0) (i32.const 1) (i32.const 16)) (i32.const 0)))
        (br_if $done (i32.eqz (i32.load (i32.const 16))))
        (br_if $done (i32.eq (i32.load8_u (i32.add (i32.const 8192) (local.get $n))) (i32.const 10)))
        (local.set $n (i32.add (local.get $n) (i32.const 1)))
        (br_if $done (i32.ge_u (local.get $n) (i32.const 100000)))
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
              (i32.load8_u (i32.add (i32.const 8192) (i32.add (local.get $i) (local.get $j))))
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
"##;

/// Asks the host to block it on a poll subscription.
pub const POLL: &str = r##"(module
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

  ;; Read one newline-terminated frame from stdin into the input buffer at
  ;; 8192; returns its length, newline excluded.
  (func $read_line (result i32)
    (local $n i32)
    (block $done
      (loop $l
        (i32.store (i32.const 0) (i32.add (i32.const 8192) (local.get $n)))
        (i32.store (i32.const 4) (i32.const 1))
        (i32.store (i32.const 16) (i32.const 0))
        (br_if $done (i32.ne (call $fd_read (i32.const 0) (i32.const 0) (i32.const 1) (i32.const 16)) (i32.const 0)))
        (br_if $done (i32.eqz (i32.load (i32.const 16))))
        (br_if $done (i32.eq (i32.load8_u (i32.add (i32.const 8192) (local.get $n))) (i32.const 10)))
        (local.set $n (i32.add (local.get $n) (i32.const 1)))
        (br_if $done (i32.ge_u (local.get $n) (i32.const 100000)))
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
              (i32.load8_u (i32.add (i32.const 8192) (i32.add (local.get $i) (local.get $j))))
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
"##;

/// Imports a database seam that does not exist. Refused at load: the
/// linker's world is closed, so an import nothing defines is not a
/// runtime error a guest can retry — the artifact never runs.
pub const DATABASE: &str = r##"(module
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

  ;; Read one newline-terminated frame from stdin into the input buffer at
  ;; 8192; returns its length, newline excluded.
  (func $read_line (result i32)
    (local $n i32)
    (block $done
      (loop $l
        (i32.store (i32.const 0) (i32.add (i32.const 8192) (local.get $n)))
        (i32.store (i32.const 4) (i32.const 1))
        (i32.store (i32.const 16) (i32.const 0))
        (br_if $done (i32.ne (call $fd_read (i32.const 0) (i32.const 0) (i32.const 1) (i32.const 16)) (i32.const 0)))
        (br_if $done (i32.eqz (i32.load (i32.const 16))))
        (br_if $done (i32.eq (i32.load8_u (i32.add (i32.const 8192) (local.get $n))) (i32.const 10)))
        (local.set $n (i32.add (local.get $n) (i32.const 1)))
        (br_if $done (i32.ge_u (local.get $n) (i32.const 100000)))
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
              (i32.load8_u (i32.add (i32.const 8192) (i32.add (local.get $i) (local.get $j))))
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
"##;

/// Imports a host escape from a namespace of its own invention.
pub const HOST_COMMAND: &str = r##"(module
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

  ;; Read one newline-terminated frame from stdin into the input buffer at
  ;; 8192; returns its length, newline excluded.
  (func $read_line (result i32)
    (local $n i32)
    (block $done
      (loop $l
        (i32.store (i32.const 0) (i32.add (i32.const 8192) (local.get $n)))
        (i32.store (i32.const 4) (i32.const 1))
        (i32.store (i32.const 16) (i32.const 0))
        (br_if $done (i32.ne (call $fd_read (i32.const 0) (i32.const 0) (i32.const 1) (i32.const 16)) (i32.const 0)))
        (br_if $done (i32.eqz (i32.load (i32.const 16))))
        (br_if $done (i32.eq (i32.load8_u (i32.add (i32.const 8192) (local.get $n))) (i32.const 10)))
        (local.set $n (i32.add (local.get $n) (i32.const 1)))
        (br_if $done (i32.ge_u (local.get $n) (i32.const 100000)))
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
              (i32.load8_u (i32.add (i32.const 8192) (i32.add (local.get $i) (local.get $j))))
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
"##;

/// Imports a WASI function this shim does not implement at all.
pub const UNDEFINED_WASI: &str = r##"(module
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

  ;; Read one newline-terminated frame from stdin into the input buffer at
  ;; 8192; returns its length, newline excluded.
  (func $read_line (result i32)
    (local $n i32)
    (block $done
      (loop $l
        (i32.store (i32.const 0) (i32.add (i32.const 8192) (local.get $n)))
        (i32.store (i32.const 4) (i32.const 1))
        (i32.store (i32.const 16) (i32.const 0))
        (br_if $done (i32.ne (call $fd_read (i32.const 0) (i32.const 0) (i32.const 1) (i32.const 16)) (i32.const 0)))
        (br_if $done (i32.eqz (i32.load (i32.const 16))))
        (br_if $done (i32.eq (i32.load8_u (i32.add (i32.const 8192) (local.get $n))) (i32.const 10)))
        (local.set $n (i32.add (local.get $n) (i32.const 1)))
        (br_if $done (i32.ge_u (local.get $n) (i32.const 100000)))
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
              (i32.load8_u (i32.add (i32.const 8192) (i32.add (local.get $i) (local.get $j))))
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
"##;

/// Answers with something that is not a frame.
pub const MALFORMED_FRAME: &str = r##"(module
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

  ;; Read one newline-terminated frame from stdin into the input buffer at
  ;; 8192; returns its length, newline excluded.
  (func $read_line (result i32)
    (local $n i32)
    (block $done
      (loop $l
        (i32.store (i32.const 0) (i32.add (i32.const 8192) (local.get $n)))
        (i32.store (i32.const 4) (i32.const 1))
        (i32.store (i32.const 16) (i32.const 0))
        (br_if $done (i32.ne (call $fd_read (i32.const 0) (i32.const 0) (i32.const 1) (i32.const 16)) (i32.const 0)))
        (br_if $done (i32.eqz (i32.load (i32.const 16))))
        (br_if $done (i32.eq (i32.load8_u (i32.add (i32.const 8192) (local.get $n))) (i32.const 10)))
        (local.set $n (i32.add (local.get $n) (i32.const 1)))
        (br_if $done (i32.ge_u (local.get $n) (i32.const 100000)))
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
              (i32.load8_u (i32.add (i32.const 8192) (i32.add (local.get $i) (local.get $j))))
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
    (call $emit (i32.const 2816))
  )
)
"##;

/// Answers with an op the wire does not define.
pub const UNKNOWN_OP: &str = r##"(module
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

  ;; Read one newline-terminated frame from stdin into the input buffer at
  ;; 8192; returns its length, newline excluded.
  (func $read_line (result i32)
    (local $n i32)
    (block $done
      (loop $l
        (i32.store (i32.const 0) (i32.add (i32.const 8192) (local.get $n)))
        (i32.store (i32.const 4) (i32.const 1))
        (i32.store (i32.const 16) (i32.const 0))
        (br_if $done (i32.ne (call $fd_read (i32.const 0) (i32.const 0) (i32.const 1) (i32.const 16)) (i32.const 0)))
        (br_if $done (i32.eqz (i32.load (i32.const 16))))
        (br_if $done (i32.eq (i32.load8_u (i32.add (i32.const 8192) (local.get $n))) (i32.const 10)))
        (local.set $n (i32.add (local.get $n) (i32.const 1)))
        (br_if $done (i32.ge_u (local.get $n) (i32.const 100000)))
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
              (i32.load8_u (i32.add (i32.const 8192) (i32.add (local.get $i) (local.get $j))))
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
"##;

/// Answers with a status HTTP has no room for.
pub const IMPOSSIBLE_STATUS: &str = r##"(module
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

  ;; Read one newline-terminated frame from stdin into the input buffer at
  ;; 8192; returns its length, newline excluded.
  (func $read_line (result i32)
    (local $n i32)
    (block $done
      (loop $l
        (i32.store (i32.const 0) (i32.add (i32.const 8192) (local.get $n)))
        (i32.store (i32.const 4) (i32.const 1))
        (i32.store (i32.const 16) (i32.const 0))
        (br_if $done (i32.ne (call $fd_read (i32.const 0) (i32.const 0) (i32.const 1) (i32.const 16)) (i32.const 0)))
        (br_if $done (i32.eqz (i32.load (i32.const 16))))
        (br_if $done (i32.eq (i32.load8_u (i32.add (i32.const 8192) (local.get $n))) (i32.const 10)))
        (local.set $n (i32.add (local.get $n) (i32.const 1)))
        (br_if $done (i32.ge_u (local.get $n) (i32.const 100000)))
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
              (i32.load8_u (i32.add (i32.const 8192) (i32.add (local.get $i) (local.get $j))))
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
"##;

/// Writes to stdout forever without ever ending a line, so nothing
/// the host has bounded so far would bound it.
pub const OUTPUT_FLOOD: &str = r##"(module
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

  ;; Read one newline-terminated frame from stdin into the input buffer at
  ;; 8192; returns its length, newline excluded.
  (func $read_line (result i32)
    (local $n i32)
    (block $done
      (loop $l
        (i32.store (i32.const 0) (i32.add (i32.const 8192) (local.get $n)))
        (i32.store (i32.const 4) (i32.const 1))
        (i32.store (i32.const 16) (i32.const 0))
        (br_if $done (i32.ne (call $fd_read (i32.const 0) (i32.const 0) (i32.const 1) (i32.const 16)) (i32.const 0)))
        (br_if $done (i32.eqz (i32.load (i32.const 16))))
        (br_if $done (i32.eq (i32.load8_u (i32.add (i32.const 8192) (local.get $n))) (i32.const 10)))
        (local.set $n (i32.add (local.get $n) (i32.const 1)))
        (br_if $done (i32.ge_u (local.get $n) (i32.const 100000)))
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
              (i32.load8_u (i32.add (i32.const 8192) (i32.add (local.get $i) (local.get $j))))
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
"##;

/// Answers with a `Set-Cookie`, which would be a forged session in
/// the host application's own origin.
pub const FORGE_COOKIE: &str = r##"(module
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

  ;; Read one newline-terminated frame from stdin into the input buffer at
  ;; 8192; returns its length, newline excluded.
  (func $read_line (result i32)
    (local $n i32)
    (block $done
      (loop $l
        (i32.store (i32.const 0) (i32.add (i32.const 8192) (local.get $n)))
        (i32.store (i32.const 4) (i32.const 1))
        (i32.store (i32.const 16) (i32.const 0))
        (br_if $done (i32.ne (call $fd_read (i32.const 0) (i32.const 0) (i32.const 1) (i32.const 16)) (i32.const 0)))
        (br_if $done (i32.eqz (i32.load (i32.const 16))))
        (br_if $done (i32.eq (i32.load8_u (i32.add (i32.const 8192) (local.get $n))) (i32.const 10)))
        (local.set $n (i32.add (local.get $n) (i32.const 1)))
        (br_if $done (i32.ge_u (local.get $n) (i32.const 100000)))
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
              (i32.load8_u (i32.add (i32.const 8192) (i32.add (local.get $i) (local.get $j))))
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
"##;

/// Answers with a header value carrying CRLF.
pub const SPLIT_RESPONSE: &str = r##"(module
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

  ;; Read one newline-terminated frame from stdin into the input buffer at
  ;; 8192; returns its length, newline excluded.
  (func $read_line (result i32)
    (local $n i32)
    (block $done
      (loop $l
        (i32.store (i32.const 0) (i32.add (i32.const 8192) (local.get $n)))
        (i32.store (i32.const 4) (i32.const 1))
        (i32.store (i32.const 16) (i32.const 0))
        (br_if $done (i32.ne (call $fd_read (i32.const 0) (i32.const 0) (i32.const 1) (i32.const 16)) (i32.const 0)))
        (br_if $done (i32.eqz (i32.load (i32.const 16))))
        (br_if $done (i32.eq (i32.load8_u (i32.add (i32.const 8192) (local.get $n))) (i32.const 10)))
        (local.set $n (i32.add (local.get $n) (i32.const 1)))
        (br_if $done (i32.ge_u (local.get $n) (i32.const 100000)))
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
              (i32.load8_u (i32.add (i32.const 8192) (i32.add (local.get $i) (local.get $j))))
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
"##;

/// Answers twice. The first answer is the answer.
pub const DOUBLE_ANSWER: &str = r##"(module
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

  ;; Read one newline-terminated frame from stdin into the input buffer at
  ;; 8192; returns its length, newline excluded.
  (func $read_line (result i32)
    (local $n i32)
    (block $done
      (loop $l
        (i32.store (i32.const 0) (i32.add (i32.const 8192) (local.get $n)))
        (i32.store (i32.const 4) (i32.const 1))
        (i32.store (i32.const 16) (i32.const 0))
        (br_if $done (i32.ne (call $fd_read (i32.const 0) (i32.const 0) (i32.const 1) (i32.const 16)) (i32.const 0)))
        (br_if $done (i32.eqz (i32.load (i32.const 16))))
        (br_if $done (i32.eq (i32.load8_u (i32.add (i32.const 8192) (local.get $n))) (i32.const 10)))
        (local.set $n (i32.add (local.get $n) (i32.const 1)))
        (br_if $done (i32.ge_u (local.get $n) (i32.const 100000)))
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
              (i32.load8_u (i32.add (i32.const 8192) (i32.add (local.get $i) (local.get $j))))
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
"##;

/// Exports no `_start`, so there is nothing to run.
pub const NO_START: &str = r##"(module
  (import "wasi_snapshot_preview1" "fd_write" (func $fd_write (param i32 i32 i32 i32) (result i32)))
  (memory (export "memory") 1)
  (func (export "other") (nop))
)
"##;

/// Reads entropy and then answers normally. Entropy is not authority, so
/// the host answers it — deterministically — rather than denying it.
pub const ENTROPY: &str = r##"(module
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

  ;; Read one newline-terminated frame from stdin into the input buffer at
  ;; 8192; returns its length, newline excluded.
  (func $read_line (result i32)
    (local $n i32)
    (block $done
      (loop $l
        (i32.store (i32.const 0) (i32.add (i32.const 8192) (local.get $n)))
        (i32.store (i32.const 4) (i32.const 1))
        (i32.store (i32.const 16) (i32.const 0))
        (br_if $done (i32.ne (call $fd_read (i32.const 0) (i32.const 0) (i32.const 1) (i32.const 16)) (i32.const 0)))
        (br_if $done (i32.eqz (i32.load (i32.const 16))))
        (br_if $done (i32.eq (i32.load8_u (i32.add (i32.const 8192) (local.get $n))) (i32.const 10)))
        (local.set $n (i32.add (local.get $n) (i32.const 1)))
        (br_if $done (i32.ge_u (local.get $n) (i32.const 100000)))
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
              (i32.load8_u (i32.add (i32.const 8192) (i32.add (local.get $i) (local.get $j))))
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
    (drop (call $random_get (i32.const 4096) (i32.const 32)))
    (call $answer)
  )
)
"##;
