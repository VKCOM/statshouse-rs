# StatsHouse Rust Client

A Rust client library for the [StatsHouse](https://github.com/VKCOM/statshouse) metrics collection system.

## Features

- Efficient metrics collection and batching
- Thread-safe client implementation
- Configurable batch sizes and timeouts
- Support for various metric types

## Usage

Add this to your `Cargo.toml`:

```toml
[dependencies]
statshouse = "0.1.1"
```

Basic usage example:

```rust
use statshouse::Client;

// Create a client
let client = Client::new("localhost:13337")?;

// Send metrics
client.send_metric("my_metric", 42.0)?;
```

## License

This project is licensed under the Mozilla Public License 2.0 - see the [LICENSE](LICENSE) file for details.

## Contributing

Contributions are welcome! Please feel free to submit a Pull Request.
