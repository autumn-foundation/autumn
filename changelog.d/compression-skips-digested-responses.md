### Fixed

- **compression:** a response that carries `Content-Digest` or `Repr-Digest`
  is no longer compressed. The digest, and any HTTP message signature over
  it, covers the bytes the handler sent; encoding them afterwards broke both.
