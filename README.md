# KRPC Performance Analysis: Sync vs Async

## Test Results

| Implementation | Packet Loss |
| -------------- | ----------- |
| Sync           | ±45%        |
| Async          | 0%          |

**Load**: 5 clients × 10,000 requests, 500μs intervals

## Technical Findings

**Possible sync version bottlenecks:**

- 10ms read timeout with blocking I/O
- Thread scheduling delays under load
- `Arc<Mutex<>>` contention serializing socket operations

**Async advantages:**

- Non-blocking I/O with event loop scheduling
- Eliminates thread contention
- Better handling of sustained concurrent load

## Conclusion

Blocking I/O creates significant packet loss under sustained load. Event-driven I/O eliminates scheduling delays that cause dropped packets.

## Run

```
cargo test --release --bin sync -- --nocapture
cargo test --release --bin async -- --nocapture
```
