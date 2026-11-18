# Ollama's `/api/embed`, `/v1/systemone` and `/api/generate`, the contract gopola depends on

Contract version: 2

gopola makes three kinds of request to Ollama, all from `Embedder` in `src/embed.rs`: `POST /api/embed`
(`Embedder::embed`), `POST /v1/systemone` (`Embedder::noul`: the decision model d3 answers one yes/no question
about a text as a probability) and `POST /api/generate` with `"keep_alive": 0` (`Embedder::unload`: free the
model's memory at once). Two implementations answer them: the real server, and the fake in
`tests/fake_ollama/mod.rs` that every `cargo test` runs against. Each `*.json` file here is one request and what
Ollama answered, with the embedding vectors cut to their first four numbers. One runner,
`replay_ollama_fixtures` in `tests/cli.rs`, replays every file against both: `ollama_fixtures_hold_for_the_fake`
on every `cargo test`, and `ollama_contract_real_server` (ignored; run by hand) against `GOPOLA_OLLAMA`.

gopola adds `"keep_alive": "30m"` to every embed request (`KEEP_ALIVE` in `src/embed.rs`), so each embed file's
`request` carries it; the captures themselves were made without it. The `/v1/systemone` requests carry
`"keep_alive": "30s"` and `"options": {"num_ctx": 4096}`: d3 is about 4 GB loaded, so it is held only briefly.
Ollama 0.40.16 answers the same keys. `model-missing.json` was recaptured from 0.40.16 on 2026-11-17, because its
not-found text changed from 0.40.3's (`model "x" not found, try pulling it first` became `model 'x' not found`);
the other embed files were replayed against 0.40.16 and still hold. gopola checks only the 404 and the words
"not found".

The files, embed: `embed-array-2.json` (a list of two inputs), `embed-single-string.json` (one string, not a
list), `embed-empty.json` (an empty list), `model-missing.json` (a model that is not pulled). d3's
`/v1/systemone`: `systemone-noul.json` (one noul question: 200 with a probability), `systemone-model-missing.json`
(a model that is not pulled: 404 naming it), `systemone-no-questions.json` (a request with no `questions`: 400).
And `unload-d3.json` (`/api/generate` with `keep_alive` 0: the model is unloaded at once).

## Keys, and how strictly each is checked

| Key | What it is | Checked |
|---|---|---|
| `about`, `source` | what the file pins; where and when it was captured | no |
| `path` | the route the file is sent to; absent means `/api/embed` | sent there |
| `request` | the JSON body sent to `path` | sent as it is |
| `status` | the HTTP status | exactly |
| `keys_include` | top-level keys of the answer | as a subset: each must be there, others may be too |
| `rows` | the length of `embeddings` | exactly |
| `row_len` | the length of every row | exactly against the real server; against the fake only that it is not 0 (the test picks the fake's length) |
| `noul` | `true`: `answers.friction.noul` is a number in [0, 1] | the range, not the value |
| `error_contains` | text inside the answer's `error` | as a substring |
| `response` | the captured answer, rows cut | no: the fake answers from it |

A key a file does not have is not checked.

## How the fake answers

- `POST /api/embed`, a request whose model is the one the fake was started with: `embed-array-2.json`'s `response`
  for a non-empty list, `embed-single-string.json`'s for a string, `embed-empty.json`'s for `[]`. Every key is
  kept; `model` is the request's and `embeddings` holds one topic vector per input.
- `POST /api/embed`, any other model: `model-missing.json`'s status and `error`, naming the requested model.
- `POST /v1/systemone`, model `DECISION_MODEL` (`d3:4b-nvfp4`, a constant in the fake): 200 with `Content-Length`
  (not chunked, as captured), `answers.friction.noul` 0.9 when the lowercased `state` contains `wrong` or
  `told you`, else 0.1; every other key from `systemone-noul.json`'s `response`, with `model` the request's.
- `POST /v1/systemone`, another model: `systemone-model-missing.json`'s 404 and `error`, naming the requested
  model. A request with no `questions` (or not 1 to 64 of them): `systemone-no-questions.json`'s 400.
- `POST /api/generate` with `keep_alive` 0: `unload-d3.json`'s `response`, with `model` the request's; each one is
  counted in `unloads`. The fake records every `/v1/systemone` body in `systemone`.
- Any other request: 404 with a plain-text body (not captured).
- The connection stays open for the next request, as the real server's does; it never sends `Connection: close`.
  An embed 200 that carries rows is sent `Transfer-Encoding: chunked`, as captured; every other answer carries
  `Content-Length`.
- Rude modes (`Rude` in the fake): `DelayMs` waits before answering (the real server took 1.87 s to load the
  model cold: `load_duration` in `embed-array-2.json`), `Status500` answers 500 with a body that is not JSON,
  `WrongRowCount` answers one row fewer than inputs, `HangUp` closes the connection without answering; these four
  apply to `/api/embed` and `/v1/systemone` alike, except that `WrongRowCount` changes only embed. Three more
  change only `/v1/systemone`: `NoDecisionModel` answers the not-pulled 404 whatever the model, `NoulOutOfRange`
  answers a `noul` of 1.5, and `RejectState(text)` answers `systemone-no-questions.json`'s kind of 400 (`{"error":
  "rejected"}`) for a state whose lowercase holds `text` and the normal answer for any other state, which is how a
  test makes one prompt of several refused. `UnloadFails` changes only the unload: it is counted in `unloads`, then
  answered 500 `{"error":"boom"}`, so a test can show that a run does not depend on the unload's answer. `Redirect { route, status, location }`
  answers a `POST` to `route` with that status line and a `Location` and counts nothing, so a test can show that no
  call follows a redirect.
- A 4xx from `/v1/systemone` other than the not-pulled 404 is a refusal of that one input (`EmbedError::Rejected`);
  so is a 200 whose `noul` is missing, not finite or outside [0, 1]. A 5xx, a body that is not JSON and a hang-up
  are `Bad`: the server, not the input.

## Against the real server

The replay of `systemone-noul.json` loads d3 (about 4 GB, held 30 s). Files run in name order, so
`unload-d3.json` runs after it and unloads d3 at once. Read `sysctl -n kern.memorystatus_vm_pressure_level` first
(below 4), and do not run it on a machine that is short of memory.

## Captured but not checked by the runner

- The 200s and `/api/version` came as `Content-Type: application/json; charset=utf-8` (the scan recorded the
  404 as `application/json`). The 200s with rows came chunked; the empty answer with `Content-Length: 49`.
- Rows arrive unit length (norm within 1e-7 of 1.0). gopola normalizes every row anyway, so this is not relied on.
- `GET /api/version` answered `{"version":"0.40.3"}` when `embed-array-2.json`, `embed-single-string.json` and
  `embed-empty.json` were captured (2026-10-13). The other files came from 0.40.16, and `model-missing.json` was
  recaptured from it; the real-server run prints the version it ran against.

## Changing the contract

This README first, then the fixture, then the fake and gopola. Bump the version when the format changes, not
when a file is added. Every `EmbedError` variant in `src/embed.rs` must be produced by a fixture or a rude mode:
`every_embed_error_is_produced_by_a_fixture_or_a_rude_mode` fails until one is: it runs `gopola embed` for the
embed cases and `gopola friction --session` (with a scripted memory-pressure source) for the d3 cases.
