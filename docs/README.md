# AttaCore Documentation

## Architecture

AttaCore is a 16-crate workspace implementing an AI agent runtime.

See `crates/` for each crate's source and inline docs.

## Crate map

| Layer | Crate | Purpose |
|---|---|---|
| L0 | auth, hooks, plugin, telemetry | Zero-dependency leaf crates |
| L1 | core | Foundation types, traits, interface |
| L2 | model, history, permissions, mcp, compaction, session | Single-dependency on core |
| L3 | tools, skills, scene, task, team | Multi-dependency |
| L4 | runtime | Agent + turn loop, wires everything |

## Developer Guides

| 使用方式 | 文档 | 说明 |
|---|---|---|
| **Daemon 模式** | [DAEMON_DEV_GUIDE.md](./DAEMON_DEV_GUIDE.md) | 独立进程 + JSON-RPC 2.0 over Unix socket / TCP —— 面向 IDE 插件、CLI 工具 |
| **库模式** | [LIBRARY_DEV_GUIDE.md](./LIBRARY_DEV_GUIDE.md) | 嵌入式 Rust API —— 面向桌面应用、定制 CLI、服务端 |

## Daemon

`daemon/` is a sample application demonstrating how to use AttaCore
crates to build a JSON-RPC 2.0 agent service over Unix sockets.

## Testing

```sh
# All crates
cargo test --workspace

# Single crate
cargo test -p tools

# Daemon tests
cargo test -p daemon
```
