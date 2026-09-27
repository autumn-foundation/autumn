### Performance

- **⚡ Bolt: cache the keyed HMAC on `ResolvedSigningKeys` (instructions
  -5.9%, `sha256::compress256` calls roughly halved):** `ResolvedSigningKeys::sign`/
  `verify` (`autumn/src/security/config.rs`) — the CSRF-cookie and
  session-cookie signature check driven on every request, safe methods
  included — called `hmac_sha256_hex`, which built a fresh
  `Hmac::<Sha256>::new_from_slice(key)` from the raw key bytes on every
  single call. `Hmac::new_from_slice` XORs the key into the block-sized
  ipad/opad pads and absorbs each into its own `Sha256` state — one
  `compress256` call per pad — even though `ResolvedSigningKeys` is built
  once at startup and its keys never change for the life of the process.
  `ResolvedSigningKeys` now keys an `Hmac<Sha256>` once per key (`current_mac`,
  `previous_macs`) and `sign`/`verify` clone that already-keyed `Hmac` per
  call instead — cloning just copies the two small pre-absorbed digest
  states (no hashing), so the ipad/opad compressions run once at startup
  instead of on every request. `hmac_sha256_hex` itself is unchanged and
  still used as-is by call sites that sign once or rarely (webhook
  delivery, mail, alerts). Measured on `autumn/benches/csrf_verify.rs`
  (1000 rounds of a real `GET` + two `POST`s through the production
  `CsrfLayer`, `valgrind --tool=callgrind`): instructions 380,084,104 →
  ~357,571,091 (**-5.9%**, averaged over two after-runs to bound
  ~0.7% run-to-run noise), with `sha2::sha256::compress256` dropping from
  11.16% to 5.91% of the profile — consistent with removing 2 of the
  ~4 compressions per HMAC call. `valgrind --tool=dhat` allocation block
  count is unchanged (111,636 blocks before and after), so this is an
  instruction-only win, not an allocation one. Behavior is unchanged: all
  `security::config`, `security::csrf`, and `session` unit tests pass
  unmodified.
