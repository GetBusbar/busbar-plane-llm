<!-- fleet:header:begin (rendered by `cargo xtask fleet render` from GetBusbar/busbar's plugins.yaml; edit it there) -->
# busbar-plane-llm

First-party signed kind:plane plugin cdylib: the llm plane, packaged as a droppable busbar plugin. Drop the signed tarball into plugins/.

| kind | alias | crate | busbar | license |
|---|---|---|---|---|
| `plane` | `llm` | `busbar-plane-llm-plugin` | 1.6.0 (pinned in `.busbar-ref`) | Apache-2.0 |

[![ci](https://github.com/GetBusbar/busbar-plane-llm/actions/workflows/ci.yml/badge.svg?branch=dev)](https://github.com/GetBusbar/busbar-plane-llm/actions/workflows/ci.yml)
<!-- fleet:header:end -->

## What it is for

`busbar-plane-llm` is a `kind: plane` busbar plugin.

## Config

Configured under the `llm` module name.

## Build

```bash
cargo build --release -p busbar-plane-llm-plugin
```

## Tests

```bash
cargo test --workspace --locked
```

## License

Apache-2.0. See [LICENSE](LICENSE).
