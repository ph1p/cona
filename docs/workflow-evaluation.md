# Offline workflow evaluation

The workflow fixtures are deterministic Rust tests and require no model or network
service.

## Baseline

On main before this feature (`e5053dc`), `find` ranks symbol names, not behavior in bodies or documentation.
`tests` reports direct identifier references only; absence of a reference cannot
establish absence of execution coverage. Default `context` includes the full
target body outside its related-symbol budget. Default `diff` maps current
changed ranges, without an impact report or deleted-definition source evidence.

## Tasks and assertions

| Workflow | Offline task | Evidence to check |
| --- | --- | --- |
| Discovery | Search for behavior whose implementation name is unknown, with distractor names present | Intended implementation ranks ahead of distractors; reasons and bounded excerpts identify lexical matches |
| Discovery | Search exact names, split identifiers, Unicode, empty queries, nested definitions | Stable ranking and selectors; deterministic ties; scope and result limits hold |
| Understanding | Request editing context for a small function and an oversized target | Whole serialized output fits its estimated budget; omitted bodies have a usable `show` selector |
| Selection | Select tests through direct, indirect, cyclic and ambiguous relationships | Evidence paths, depth bounds and deduplication; no execution-coverage claim |
| Review | Compare a temporary Git repo with added/deleted functions and signature changes | Old/new evidence, outside-symbol changes, deduplicated static impact and bounded output |

Use `cargo test --locked` for the combined fixture suite. Run the release binary
for representative CLI checks after `cargo build --release`. Record actual
commands, exit codes and environmental failures in the executor report; do not
replace a failed gate with a benchmark success or claim measured token savings
from these fixtures.
