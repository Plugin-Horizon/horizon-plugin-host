# Contributing to horizon-plugin-host

Pull requests and improvements are always welcome! Bug reports, ideas and documentation fixes count just as much as code.

## Building and testing the core

The core crate(s) are plain Rust and build without the Horizon engine, so you only need a stable Rust toolchain:

- `.`

```sh
cargo test --workspace
```

CI runs exactly these commands (job name `test`) on every push and pull request. Parts of the repository that depend on the Horizon engine are not built by CI; if you change them, please say in the PR how you tested the change.

## Workflow

1. Fork the repository (or create a branch if you have write access) and branch from `main`.
2. Make your change. Keep it focused, and add or update tests where it makes sense.
3. Run the tests locally.
4. Open a pull request against `main` and fill in the template.
5. One approving review is needed before merging. Direct pushes to `main` are disabled.

## Licence of contributions

By submitting a contribution you agree that it is licensed under the same licence as this repository (see the `LICENSE` file). Only submit code you wrote yourself or that comes from a source whose licence is compatible; mention the source in the code.

## Be nice

Please follow the [Code of Conduct](CODE_OF_CONDUCT.md). Pull requests and improvements are always welcome!
