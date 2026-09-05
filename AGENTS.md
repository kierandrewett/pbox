# pbox contributor rules

For any CLI output change, follow [the CLI design system](docs/cli-design.md).
Use `crates/pbox-cli/src/ui.rs` for human output, prompts, colours, and layout.
Do not print directly or add ANSI escapes in command handlers. Do not suppress
the crate's output lints outside `ui.rs`. Preserve JSON and guest data streams.

Before completing a code change, run the relevant tests and `just check`.
For changes across commands, run `just test` and inspect terminal, plain, and JSON output.
