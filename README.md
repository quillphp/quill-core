<div align="center">
  <h1>Quill-Core</h1>
  <p><strong>The high-performance native engine behind the Quill PHP Framework.</strong></p>

  [![CI](https://github.com/quillphp/quill-core/actions/workflows/ci.yml/badge.svg)](https://github.com/quillphp/quill-core/actions/workflows/ci.yml)
  [![Release](https://img.shields.io/github/v/release/quillphp/quill-core)](https://github.com/quillphp/quill-core/releases)
  [![Rust](https://img.shields.io/badge/rust-stable-orange.svg)](https://www.rust-lang.org)
  [![License](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
</div>

---

**Quill-Core** is the specialized native library that offloads heavy lifting from PHP userland to a thread-safe, memory-safe Rust engine. Built for sub-microsecond overhead, it powers the routing, validation, and JSON processing of the Quill PHP Framework.

## Key Features

- **Blazing Fast Routing**: Uses a Radix-tree based router (`matchit`) for O(1) performance in both PHP and CLI modes.
- **Native DTO Validation**: Decouples validation from PHP userland, performing schema checks at native speeds before your code even runs.
- **FFI-First Architecture**: Seamlessly integrates into any PHP environment via the `FFI` extension, with zero build-step requirements for the end user.
- **Cross-Platform**: Automated release pipeline providing pre-built binaries for **Linux** and **macOS** (Intel & Apple Silicon).
- **Efficient JSON Compaction**: Specialized native methods for ultra-fast JSON transformations between boundaries.

---

## Architecture Overview

Quill Core owns the entire I/O stack. PHP never touches a socket — it only executes business logic, orchestrated by the native engine through a lock-free FFI bridge.

### Multi-Worker Model

The parent process compiles routes and pre-binds the TCP port **before** forking. Each child inherits the socket via `dup(2)` and independently initialises its own Rust heap — no shared state across workers.

```mermaid
flowchart TD
    A[PHP App] -->|compile manifest JSON| B["quill_router_build()"]
    A -->|register DTO schemas| C["quill_validator_register()"]
    A -->|pre-bind TCP port| D["quill_server_prebind()"]

    B --> Router[(matchit\nradix trie)]
    C --> Validator[(ValidatorRegistry)]
    D --> Sock[[Shared Socket fd]]

    Sock -.->|"dup(2) per worker"| W1 & W2 & WN

    subgraph W1 ["Worker 1 — parent process"]
        direction LR
        AX1["Axum / Tokio\n(single-threaded rt)"] <-->|"mpsc + oneshot"| PH1[PHP Poll Loop]
    end
    subgraph W2 ["Worker 2 — pcntl_fork"]
        direction LR
        AX2["Axum / Tokio\n(single-threaded rt)"] <-->|"mpsc + oneshot"| PH2[PHP Poll Loop]
    end
    subgraph WN ["Worker N — pcntl_fork"]
        direction LR
        AXN["Axum / Tokio\n(single-threaded rt)"] <-->|"mpsc + oneshot"| PHN[PHP Poll Loop]
    end
```

### Request Lifecycle

```mermaid
sequenceDiagram
    participant C  as Client
    participant AX as Axum / Tokio
    participant RT as matchit Router
    participant VL as ValidatorRegistry
    participant MP as mpsc channel
    participant PHP as PHP Poll Loop

    C->>+AX: HTTP Request

    AX->>RT: match_route(method, path)
    RT-->>AX: RouteMetadata { handler_id, dto_class }

    opt route has dto_class
        AX->>VL: validate(dto_name, body_bytes)
        VL-->>AX: validated JSON  —or—  400 Bad Request
    end

    AX->>MP: send(PendingRequest { id, handler_id, params, dto_data })
    Note over MP,PHP: 10 000-slot buffered channel

    MP-->>PHP: quill_server_poll()
    PHP->>PHP: execute handler
    PHP->>MP: quill_server_respond(id, response_json)

    MP-->>AX: oneshot::recv() → response_json
    AX->>AX: parse { status, headers, body }
    AX-->>-C: HTTP Response
```

---

## Installation

### Option 1: Using Pre-built Binaries (Recommended)
You can download the optimized shared libraries (`.so` or `.dylib`) and the required C-header (`quill.h`) directly from the [GitHub Releases](https://github.com/quillphp/quill-core/releases) page.

### Option 2: Building from Source
If you are contributing or need a custom build, you can compile from source using `cargo`:

```bash
# Clone the repository
git clone https://github.com/quillphp/quill-core.git
cd quill-core

# Build the shared library (bin/ folder)
./scripts/build.sh --release
```

---

## Integration with Quill PHP

By default, the Quill PHP framework will automatically discover the core library if it's placed in any of these locations:
1.  `build/libquill.so` (Local Development)
2.  `vendor/quillphp/quill-core/bin/libquill.so` (Composer Integration)
3.  `/usr/local/lib/libquill.so` (Global System Level)

You can override the discovery behavior using the **`QUILL_CORE_BINARY`** environment variable:

```bash
export QUILL_CORE_BINARY=/path/to/your/libquill.so
```

---

## Development & Testing

We maintain strict code quality standards to ensure consistency and performance.

```bash
# Run unit tests
cargo test

# Run Clippy (linter)
cargo clippy -- -D warnings

# Apply formatting
cargo fmt --all
```

---

## License

This project is open-sourced under the **MIT License**.
