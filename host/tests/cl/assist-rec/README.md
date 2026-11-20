Recorded provider streams for the `assist` tests (CL-3a): server-sent events as each provider kind sends them.

They are written by hand after the providers' documented stream formats. No real provider was contacted to make
them, and none is contacted by any test (brief of CL-3a; R9). Used by the host tests of `user/services/assist`
(`tests/adapters.rs`, `tests/engine.rs`, `tests/cli.rs`) and by `assist_fake_api.py` for the scenarios.

| file | what it is |
|---|---|
| `<kind>-text.sse` | a text answer in several deltas, with umlauts (Anthropic: after a reasoning block with a signature, with a ping) |
| `<kind>-tools.sse` | text, then `settings_get` and `settings_set` (theme.scale to 150); the second call's arguments are split across deltas (Gemini sends a call whole: there the two calls come in two chunks, the first with a `thoughtSignature`) |
| `<kind>-done.sse` | the closing text after tool results |
| `anthropic-undo.sse` | one `settings_undo` call |
| `anthropic-version.sse` | text, then one call of the machine's own tool `version` without arguments (CL-T) |
| `<kind>-cut.sse` | a stream that stops in the middle of an event |
| `anthropic-overloaded.sse`, `openai-error.sse` | an error event inside a stream that had begun |
