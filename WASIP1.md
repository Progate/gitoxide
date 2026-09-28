# The `wasip1` branch of Progate/gitoxide

This branch carries the changes [`@progate/browser-git`](https://github.com/Progate/packages/tree/main/packages/browser-git)
needs to run `gix` as a `wasm32-wasip1` command inside BrowserOS, on top of upstream
[GitoxideLabs/gitoxide](https://github.com/GitoxideLabs/gitoxide). Everything else is upstream as-is.

## Building for WASI

| Change | Why |
| --- | --- |
| `gix-worktree-state`: a local `Close` for `std::fs::File`, executable bits treated as non-unix | `io-close` has no WASI implementation |
| `gix-pack`: `gix-tempfile` is only excluded on `wasm32-unknown-unknown` | streaming pack input (`fetch`) is needed on WASI |
| `gix-tempfile`: no `std::process::id()` on WASI | it panics there; WASI has neither pids nor `fork()` |
| `gix-index`: modification time of the index file through `std` on WASI | `filetime` panics on WASI |
| `vendor/memmap2`: memmap2 0.9.11 plus `src/wasi.rs` | WASI has no `mmap`; maps are emulated by reading the file. Consumers use it through `[patch.crates-io]` |

All of these are scoped to `target_os = "wasi"` and change nothing for other targets.

## Leniency

`gix-transport` accepts the capabilities of the dummy `capabilities^{}` ref (sent by empty repositories)
when a minimal server separates them with a space instead of a null byte, as libgit2 does. Other lines
without a null byte are still rejected.

## Client-side push

Upstream gitoxide can't push. This branch adds it for blocking clients:

- `gix_protocol::push()` sends the commands and a caller-provided pack to `receive-pack` (protocol V0/V1)
  and parses `report-status`, with `side-band-64k` demultiplexing. It works with any
  `gix_transport::client::blocking_io::Transport`, so callers can bring their own HTTP implementation.
- `gix::remote::Connection::push()` performs the `receive-pack` handshake, applies `git push`'s
  fast-forward rules (`fetch first` / `non-fast-forward` rejections, `force`), computes the objects
  the remote lacks and generates the pack with `gix-pack`.

Tests: `cargo test -p gix --features blocking-network-client --test gix remote::push` pushes to a real
`git receive-pack` over `file://`, and `cargo test -p gix-protocol --features blocking-client,sha1 --lib push`
covers the report parsing.
