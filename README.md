# Quill-Core

The high-performance native engine for the [Quill PHP Framework](https://github.com/quillphp/quill).

`quill-core` provides a Rust-powered, FFI-compatible routing and validation engine designed for sub-microsecond overhead in PHP applications.

## Prerequisites

- [Rust](https://www.rust-lang.org/tools/install) (stable)
- `cargo`

## Building Locally

To build the shared library for your current platform:

```bash
# Debug build (includes symbols)
./scripts/build.sh

# Optimized release build
./scripts/build.sh --release
```

The artifacts will be generated in the `bin/` directory:
- `libquill.so` (Linux) or `libquill.dylib` (macOS)
- `quill.h` (C-header for FFI)

## Integration with Quill PHP

The Quill PHP framework automatically discovers this library if it is placed in the `build/` or `vendor/` directories of your application. You can also specify a custom path using the `QUILL_CORE_BINARY` environment variable.

## CI/CD

This repository includes GitHub Actions for:
- **Continuous Integration**: Auto-testing on every push.
- **Automated Releases**: Building and uploading optimized binaries for Linux and macOS on every new tag (e.g., `v1.0.0`).

## License

MIT
