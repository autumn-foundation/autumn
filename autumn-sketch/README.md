# autumn-sketch

Fixed-memory stream sketches for apps built on the
[Autumn](https://github.com/autumn-foundation/autumn) web framework. Each one
answers a question about an endless stream (requests, client IPs, tenants,
latencies) in memory you pick up front, whatever the traffic does.

| Module | Type (shared form) | Question |
| --- | --- | --- |
| `heavy_hitters` | `SpaceSaving` (`HeavyHitters`) | Which keys are most of the traffic? |
| `frequency` | `CountMinSketch` (`FrequencyCounter`) | How often has *this* key been seen? |
| `distinct` | `HyperLogLog` (`DistinctCounter`) | How many distinct keys? |
| `quantiles` | `DdSketch` (`Quantiles`) | What are p50, p99 and p99.9? |
| `reservoir` | `Reservoir` (`Sampler`) | A fair sample of `k` items |
| `bloom` | `BloomFilter` (`SeenFilter`) | Have we seen this key before? |
| `minhash` | `MinHash` | How alike are these two sets? |

Each shared form is a mutex around the plain sketch, to put in an `Arc` and
record from every handler. Sketches of one shape merge, so each replica records
its own traffic and the merged sketch answers for the fleet.

The crate is std only, with no dependencies, and does not depend on
`autumn-web`. Add it to an app with:

```toml
[dependencies]
autumn-sketch = "0.11"
```
