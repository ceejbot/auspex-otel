# Contributing to auspex

Thank you for your interest in auspex! This document describes how we work so that contributions are smooth and the project stays true to its goals.

## Project Philosophy

auspex is **deliberately opinionated and simple**.

- When faced with a configuration choice, we ask: "What do 90% of users want?" and then remove the knob.
- We are a **tracing-only** crate for **axum only**.
- We reject the full complexity and performance cost of the official OpenTelemetry SDK.
- We prioritize **low runtime overhead** and **ergonomic setup** above all else.

If you need maximum flexibility, please use `tracing-opentelemetry` + the full SDK instead.

## Development Workflow

### Before you start

Run `just ci` (or the equivalent individual commands) and make sure everything is green.

### How we work (Design / Validate / Implement / Test / Verify)

We follow a strict **DVI + TDD + Verify** cycle for every non-trivial piece of work:

- **Design** — Write down the types, module boundaries, and invariants first (often as code comments).
- **Validate** — Define how we will know the change is correct *before* writing the implementation (tests, property tests, benchmarks, `cargo clippy -D warnings`, etc.).
- **Implement** — Write the code using **Test-Driven Development** (red → green → refactor).
- **Test** — Make sure the assertions actually run, actually pass, and actually prove the Validate bullets.
- **Verify** — `just ci` green, runtime smoke when it matters, trivia reconciled with reality.

Strong types are our primary tool for preventing bugs. Prefer enums over strings, newtypes for IDs, and `Cow<'static, str>` for hot-path data.

### Review-clean discipline

When a non-trivial task lands, do a deliberate review pass against the just-shipped diff. List every blocker (not nitpicks). Land the fixes as a **single review-clean commit** with a message that enumerates each blocker and the fix for each. `7a8f13d` is the template. Don't add new feature work until the review-clean commit is in.

### Running checks locally

```bash
just ci          # The one command you should run before committing
```

Individual recipes also exist:

- `just check`
- `just clippy`
- `just test`
- `just fmt`
- `just deny`

We use `cargo nextest` and strict Clippy (`-D warnings`).
Both the default feature set *and* `--features axum` are checked (the `just ci` / `just clippy` targets run the full matrix so feature-gated code such as MatchedPath handling is covered). The `aws-lc-rs` crypto-provider combo (`--no-default-features --features aws-lc-rs,axum`) is checked in GitHub Actions CI only — it compiles a cmake-built C library, so it stays out of the local `just ci` loop.

### Commit messages & PRs

- Keep PRs focused. One logical change per PR.
- Describe the *why* as well as the *what*.

## Code Style & Linting

- We use the configuration in `rustfmt.toml`, `clippy.toml`, and the `[lints]` tables in `Cargo.toml`.
- All new public API must be documented.
- Internal modules (`pub(crate)`) are allowed some leniency on docs, but the hot paths and public types must be crystal clear.

## Adding Dependencies

We are extremely conservative about dependencies. Before adding one, ask:

1. Can we do this with the standard library or a very small crate we already depend on?
2. What is the impact on compile time, binary size, and runtime performance?
3. Does this help the 90% user story?

`cargo-deny` runs in CI and will catch many problems automatically.

## Reporting Issues

Please include:

- Rust version + `cargo --version`
- Output of `just ci` (or the failing command)
- A minimal reproduction if possible

## License

Dual-licensed under MIT or Apache-2.0 at your option.

---

Thank you for helping keep auspex small, fast, and delightful to use.
