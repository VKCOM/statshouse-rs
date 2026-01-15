# statshouse-rs

Rust client library for StatsHouse.

This repository is a Cargo workspace with:
- `statshouse/`: the client library crate
- `xtask/`: developer tooling (`cargo x`)

## Usage

Add the dependency:

```toml
[dependencies]
statshouse = "0.1.0"
```

Send a counter metric (UDP by default):

```rust
use statshouse::{MetricBuilder, Transport};

fn main() {
    let mut transport = Transport::default();

    MetricBuilder::new(b"requests_total")
        .tag(b"env", b"staging")
        .tag(b"service", b"api")
        .write_count(&mut transport, 1.0, 0);
}
```

Send value metric over TCP:

```rust
use statshouse::{MetricBuilder, Transport};

fn main() {
    let mut transport = Transport::tcp("127.0.0.1:13337");

    MetricBuilder::new(b"latency_ms")
        .tag(b"env", b"staging")
        .write_values(&mut transport, &[12.3, 18.9, 7.4], 0.0, 0);

}
```

Use UDP explicitly:

```rust
use statshouse::{MetricBuilder, Transport};

fn main() {
    let mut transport = Transport::udp("127.0.0.1:13337");

    MetricBuilder::new(b"requests_total")
        .tag(b"env", b"staging")
        .write_count(&mut transport, 1.0, 0);
}
```

## Development

Single command to run all checks: `cargo x ci`

## License

MPL-2.0
