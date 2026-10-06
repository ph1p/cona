#!/bin/sh
# A/B benchmark: the same agent tasks with and without cona, measured by the
# harness's own token/cost accounting — not cona's modeled baseline.
#
#   ANTHROPIC_API_KEY=… scripts/bench/ab.sh [tasks-file] [runs-per-arm] [repo]
#
# Each task runs RUNS times per arm in a fresh `claude -p --bare` session
# (no hooks, plugins, CLAUDE.md or memory, so neither arm inherits your setup):
#   control  no cona: its CLI and MCP tools are disallowed
#   cona     cona's MCP server + the agent guide (SKILL.md) appended to the
#            system prompt. No hooks — `--bare` skips them — so this measures
#            the floor of what cona buys; the redirect hooks only add to it.
# tasks file: one `id<TAB>regex<TAB>prompt` per line (tabs, so a regex may use
# `|`). A task passes when the final answer matches its regex. Tasks must not edit
# files (both arms share the checkout); the repo must be `cona index`ed.
#
# Costs real money: RUNS × 2 × tasks sessions, each capped by BUDGET_USD.
# Override arms with CONTROL_ARGS / CONA_ARGS, the model with MODEL.
# Output: one TSV row per session in $OUT (default bench-<ts>.tsv) + a
# per-arm median summary on stdout.
set -eu
here="$(cd "$(dirname "$0")" && pwd)"
tasks="${1:-$here/tasks.txt}"
runs="${2:-3}"
repo="${3:-$(cd "$here/../.." && pwd)}"
budget="${BUDGET_USD:-1.00}"
model="${MODEL:-}"
out="${OUT:-bench-$(date +%Y%m%d-%H%M%S).tsv}"
guide="$here/../../plugin/skills/cona/SKILL.md"
mcp="$here/../../plugin/.mcp.json"

[ -n "${ANTHROPIC_API_KEY:-}" ] || { echo "error: --bare sessions need ANTHROPIC_API_KEY" >&2; exit 1; }
command -v jq >/dev/null || { echo "error: jq not found" >&2; exit 1; }
command -v cona >/dev/null || { echo "error: cona not on PATH" >&2; exit 1; }

control_args="${CONTROL_ARGS:---disallowedTools Bash(cona:*) mcp__cona}"
cona_args="${CONA_ARGS:---strict-mcp-config --mcp-config $mcp}"
model_args=""
[ -n "$model" ] && model_args="--model $model"

printf 'task\tarm\trun\tok\tinput_tok\tcache_read_tok\toutput_tok\tcost_usd\tturns\tms\n' > "$out"

grep -v '^\s*#' "$tasks" | grep -v '^\s*$' | while IFS='	' read -r id regex prompt; do
    for arm in control cona; do
        r=1
        while [ "$r" -le "$runs" ]; do
            if [ "$arm" = control ]; then
                # shellcheck disable=SC2086 # args are word lists by design
                json="$(cd "$repo" && claude -p --bare --output-format json \
                    --max-budget-usd "$budget" $model_args $control_args \
                    -- "$prompt" </dev/null 2>/dev/null || true)"
            else
                # shellcheck disable=SC2086
                json="$(cd "$repo" && claude -p --bare --output-format json \
                    --max-budget-usd "$budget" $model_args $cona_args \
                    --append-system-prompt "$(cat "$guide")" \
                    -- "$prompt" </dev/null 2>/dev/null || true)"
            fi
            ok=0
            if printf '%s' "$json" | jq -er '.result // ""' 2>/dev/null | grep -Eqi -- "$regex"; then
                ok=1
            fi
            row="$(printf '%s' "$json" | jq -r '[
                (.usage.input_tokens // 0) + (.usage.cache_creation_input_tokens // 0),
                (.usage.cache_read_input_tokens // 0),
                (.usage.output_tokens // 0),
                (.total_cost_usd // 0), (.num_turns // 0), (.duration_ms // 0)
              ] | @tsv' 2>/dev/null || printf '0\t0\t0\t0\t0\t0')"
            printf '%s\t%s\t%s\t%s\t%s\n' "$id" "$arm" "$r" "$ok" "$row" >> "$out"
            printf '%-24s %-8s run %s  ok=%s  %s\n' "$id" "$arm" "$r" "$ok" "$row" >&2
            r=$((r + 1))
        done
    done
done

# Medians per arm (cost and tokens are skewed by the odd runaway session —
# a median is the honest middle), plus the pass rate.
echo "── $out ──"
for arm in control cona; do
    awk -F'\t' -v arm="$arm" '
        NR > 1 && $2 == arm { n++; ok += $4; inp[n] = $5 + $6; o[n] = $7; c[n] = $8; t[n] = $9 }
        function med(a, k,   i, j, x) {
            for (i = 2; i <= k; i++) { x = a[i]; for (j = i - 1; j >= 1 && a[j] > x; j--) a[j+1] = a[j]; a[j+1] = x }
            return k % 2 ? a[(k+1)/2] : (a[k/2] + a[k/2+1]) / 2
        }
        END {
            if (!n) exit
            printf "%-8s sessions %d  pass %d%%  median: input %d tok · output %d tok · $%.4f · %d turns\n",
                arm, n, ok * 100 / n, med(inp, n), med(o, n), med(c, n), med(t, n)
        }' "$out"
done
