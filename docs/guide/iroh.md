# iroh: Dial Nodes by Public Key

iroh gives each process a QUIC endpoint whose address is an ed25519 public
key: the **endpoint id**. A peer dials the id. The TLS handshake proves that
the remote end holds the key. With a relay, iroh also goes through NAT.

Autumn uses one iroh endpoint per process. In this release, the cluster
(`[cluster] transport = "iroh"`) runs on it. Serving the app and calling
services over iroh come later (#3818).

Build with the `iroh` feature:

```toml
[dependencies]
autumn-web = { version = "...", features = ["iroh"] }
```

## Enable it

```toml
[iroh]
enabled = true
```

At boot the app logs its endpoint id and a dial address:

```text
INFO iroh endpoint bound endpoint_id=ae58ff88… dial_addr=ae58ff88…@10.0.0.4:51820
```

The endpoint id is public. Give it to the peers that dial this node.

### Configuration keys

| Key | Default | Meaning |
| --- | --- | --- |
| `enabled` | `false` | Bind the endpoint. |
| `secret_key_path` | `.autumn/iroh/secret.key` | The secret key: the node's identity. |
| `bind_addr` | `0.0.0.0:0` | The UDP `ip:port` to bind. Port `0` takes a free port. |
| `relay` | `"disabled"` | `"disabled"`, `"n0"` (n0's public relays) or `"custom"`. |
| `relay_urls` | `[]` | The relays for `relay = "custom"`. Each starts with `https://`. |
| `address_lookup` | `"static"` | `"static"` (from config and from connections) or `"n0-dns"`. |
| `allowed_peers` | `[]` | Endpoint ids that may connect. Empty means no list. |

Each key has an `AUTUMN_IROH__<KEY>` env override. `relay_urls` and
`allowed_peers` take comma-separated values.

### The secret key

The app creates the key file on first boot, with mode `0600`, in a directory
with mode `0700`. The file holds 64 hex characters. A file with 32 raw bytes
is read too. On Unix, the app refuses to boot when group or others can read
the file. The app never logs the key.

Keep the file across restarts: a new key is a new endpoint id. In a container,
put the file on a volume, or mount it from a secret store.

### Peer addresses

A peer address is one of:

- `<endpoint-id>`: dial by id. This needs an address from somewhere: a relay,
  `address_lookup = "n0-dns"`, or a connection that the peer opened.
- `<endpoint-id>@<ip:port>`: dial by id at a direct address. The handshake
  still checks the id.

The id is 64 lowercase hex characters. The address is an IP literal; host
names are not resolved.

## Defaults talk to no third party

`relay = "disabled"` and `address_lookup = "static"` are the defaults. The
endpoint then sends nothing to any server but its peers. Peers need a direct
address.

To go through NAT, set a relay. iroh uses the relay to coordinate hole
punching, and to carry traffic when a direct path fails. Traffic through a
relay stays end-to-end encrypted.

- `relay = "n0"` uses n0's public relays.
- `relay = "custom"` with `relay_urls` uses your own `iroh-relay` servers.

`address_lookup = "n0-dns"` publishes the endpoint's addresses to n0's
DNS, and resolves peers from it. Then a bare `<endpoint-id>` is enough to
dial.

## Who may connect

`allowed_peers` applies to every protocol on the endpoint. A peer not on the
list completes the handshake, then the node closes the connection. No
protocol handler sees it. When you set the list, add the cluster nodes too.

An unknown protocol (ALPN) fails the handshake.

## The cluster on iroh

```toml
[iroh]
enabled = true
bind_addr = "0.0.0.0:7947"

[cluster]
enabled = true
transport = "iroh"
secret = "a-shared-secret-at-least-16-bytes"
seed_peers = ["ae58ff8833241ac82d6ff7611046ed67b5072d142c588d0063e942d9a75502b6@10.0.0.4:7947"]
```

See [Embedded Clustering](clustering.md#the-iroh-transport).

## Check it

`autumn doctor` has an `iroh` check. It fails when the key file is readable by
others or holds no key, and when `[cluster] transport = "iroh"` is set with
`[iroh]` off. It shows the relay and lookup modes.
