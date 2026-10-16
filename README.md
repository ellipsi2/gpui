# GPUI for Draw

This branch (`draw`) holds the GPUI crates that [Draw](https://github.com/ellipsi2/draw) builds against, maintained as a fork of [zed-industries/zed](https://github.com/zed-industries/zed).

It is not a zed checkout. GPUI 0.2.2 was published to crates.io from zed commit `69e2130295c2649963eb639fc70b4f2ee8ea1624` (from a modified tree) as twelve crates with renamed internal dependencies (`gpui_util`, `gpui_collections`, ...). The first commit of this branch is exactly those twelve `.crate` archives, unpacked; their SHA-256 sums match the crates.io index. Building the zed workspace directly would pull in zed's whole dependency set instead, so changes are made on top of the published sources.

The branch shares no history with zed's `main`, so a Cargo git dependency on it fetches only these sources.

## Crates

| Directory | Package |
| --- | --- |
| `crates/gpui` | `gpui` |
| `crates/gpui-macros` | `gpui-macros` |
| `crates/gpui_collections` | `gpui_collections` |
| `crates/gpui_derive_refineable` | `gpui_derive_refineable` |
| `crates/gpui_http_client` | `gpui_http_client` |
| `crates/gpui_media` | `gpui_media` |
| `crates/gpui_perf` | `gpui_perf` |
| `crates/gpui_refineable` | `gpui_refineable` |
| `crates/gpui_semantic_version` | `gpui_semantic_version` |
| `crates/gpui_sum_tree` | `gpui_sum_tree` |
| `crates/gpui_util` | `gpui_util` |
| `crates/gpui_util_macros` | `gpui_util_macros` |

Each crate keeps its published `Cargo.toml`, `Cargo.toml.orig`, `.cargo_vcs_info.json` and `Cargo.lock`. The root `Cargo.toml` is a workspace that patches the crates onto each other, so `cargo check` here builds the local copies.

## Use from Draw

Draw's root `Cargo.toml` replaces the crates.io packages with this branch:

```toml
[patch.crates-io]
gpui = { git = "https://github.com/ellipsi2/gpui", branch = "draw" }
# ... one line per package above
```

After pushing here, run `cargo update -p gpui` (and the other packages) in Draw to move its lock file to the new commit.

## Bringing in upstream changes

Fetch zed into a local clone (`git remote add upstream https://github.com/zed-industries/zed`, `git fetch upstream main`) and port the needed commits by hand: paths differ (`crates/gpui/...` is the same, but internal crates are renamed) and the published `Cargo.toml` files use plain crates.io versions instead of `workspace = true`.

## License

Apache-2.0, as upstream (`LICENSE-APACHE` in each crate).
