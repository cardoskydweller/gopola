# Ollama's `/api/embed`, the contract gopola depends on

Contract version: 1

gopola makes one kind of request to Ollama: `POST /api/embed`, from `Embedder::embed` in `src/embed.rs`. Two
implementations answer it: the real server, and the fake in `tests/fake_ollama/mod.rs` that every `cargo test`
runs against. Each `*.json` file here is one request and what Ollama 0.40.3 answered on 2026-10-13, with its
vectors cut to their first four numbers. One runner, `replay_ollama_fixtures` in `tests/cli.rs`, replays every
file against both: `ollama_fixtures_hold_for_the_fake` on every `cargo test`, and `ollama_contract_real_server`
(ignored; run by hand) against `GOPOLA_OLLAMA`.

The files: `embed-array-2.json` (a list of two inputs), `embed-single-string.json` (one string, not a list),
`embed-empty.json` (an empty list), `model-missing.json` (a model that is not pulled).

## Keys, and how strictly each is checked

| Key | What it is | Checked |
|---|---|---|
| `about`, `source` | what the file pins; where and when it was captured | no |
| `request` | the JSON body sent to `POST /api/embed` | sent as it is |
| `status` | the HTTP status | exactly |
| `keys_include` | top-level keys of the answer | as a subset: each must be there, others may be too |
| `rows` | the length of `embeddings` | exactly |
| `row_len` | the length of every row | exactly against the real server; against the fake only that it is not 0 (the test picks the fake's length) |
| `error_contains` | text inside the answer's `error` | as a substring |
| `response` | the captured answer, rows cut | no: the fake answers from it |

A key a file does not have is not checked.

## How the fake answers

- A request whose model is the one the fake was started with: `embed-array-2.json`'s `response` for a non-empty
  list, `embed-single-string.json`'s for a string, `embed-empty.json`'s for `[]`. Every key is kept; `model` is
  the request's and `embeddings` holds one topic vector per input.
- Any other model: `model-missing.json`'s status and `error`, naming the requested model.
- Any request other than `POST /api/embed`: 404 with a plain-text body (not captured).
- The connection stays open for the next request, as the real server's does; it never sends `Connection: close`.
  A 200 that carries rows is sent `Transfer-Encoding: chunked`, as captured; every other answer carries
  `Content-Length`.
- Rude modes (`Rude` in the fake): `DelayMs` waits before answering (the real server took 1.87 s to load the
  model cold: `load_duration` in `embed-array-2.json`), `Status500` answers 500 with a body that is not JSON,
  `WrongRowCount` answers one row fewer than inputs, `HangUp` closes the connection without answering.

## Captured but not checked by the runner

- The 200s and `/api/version` came as `Content-Type: application/json; charset=utf-8` (the scan recorded the
  404 as `application/json`). The 200s with rows came chunked; the empty answer with `Content-Length: 49`.
- Rows arrive unit length (norm within 1e-7 of 1.0). gopola normalizes every row anyway, so this is not relied on.
- `GET /api/version` answered `{"version":"0.40.3"}`; the real-server run prints it.

## Changing the contract

This README first, then the fixture, then the fake and gopola. Bump the version when the format changes, not
when a file is added. Every `EmbedError` variant in `src/embed.rs` must be produced by a fixture or a rude mode:
`every_embed_error_is_produced_by_a_fixture_or_a_rude_mode` fails until one is.
