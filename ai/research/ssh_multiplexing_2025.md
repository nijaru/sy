# SSH Multiplexing Research (2025)

## Summary
SSH ControlMaster multiplexing can save ~2.5s per connection (3.3s → 0.8s), but is **NOT recommended for heavy data transfer** because multiple transfers share ONE TCP connection, creating a bottleneck.

## Current State in sy
- ✅ SSH config parsing implemented (src/ssh/config.rs)
- ✅ ControlMaster, ControlPath, ControlPersist parsing complete
- ✅ Full test coverage
- ⚠️ Marked `#[allow(dead_code)]` - not yet used

## Recommendation for sy

**DON'T use ControlMaster for parallel file transfers**

### Why:
- sy uses parallel workers (--workers flag) for performance
- ControlMaster serializes all transfers on one TCP connection
- This defeats the purpose of parallel transfers

### Better approach:
1. **Metadata operations**: Use ControlMaster for stat(), readdir(), checksum requests
2. **Data transfer**: Each worker uses separate SSH connection for parallel throughput

## Alternative: SSH Connection Pooling

Instead of ControlMaster, consider:
- Pool of N persistent SSH connections (N = --workers count)
- Each worker gets dedicated connection from pool
- Connections persist across multiple files (avoid 2.5s handshake per file)
- True parallel data transfer (not bottlenecked on one TCP conn)

## Best Practices (2025)

Standard ControlMaster config:
```
Host *
  ControlMaster auto
  ControlPath ~/.ssh/controlmasters/%r@%h:%p
  ControlPersist 600  # 10 minutes
```

**ControlPersist timing**:
- 30s: Minimal stale socket risk
- 600s (10m): Balanced performance
- 3600s (1h): Maximum reuse

**Limitations**:
- MaxSessions: Usually 10 per master connection
- Security: Control socket allows connections without re-auth

## Sources
- OpenSSH 9.x-10.1: Bug fixes, diagnostic improvements, no major architectural changes
- Community consensus: ControlMaster great for interactive sessions, poor for bulk data transfer
