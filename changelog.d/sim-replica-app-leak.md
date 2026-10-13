### Fixed

- **sim:** a dropped `Sim` now frees the apps of its replicas when it has a
  `SimNet`. A replica is a host of the network and its app holds the
  network, so the two kept each other alive. A long seed sweep with replicas
  kept about 0.5 MiB per seed, until the runner ran out of memory.

### Testing

- **reddit-clone:** the DST nightly sweep fails with a memory-leak message
  when its process grows by more than 256 MiB, before the runner runs out of
  memory.
