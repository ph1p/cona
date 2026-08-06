#!/bin/sh
# cona uninstall: removes everything — binary, upgrade hooks, and
# agent files + git hooks globally and in every registered project.
# Usage: ./uninstall.sh [--purge]   (--purge also deletes ~/.cona)
set -e

# prefer the installed binary, fall back to a local build, else manual cleanup
if command -v cona >/dev/null 2>&1; then
    cona uninstall "$@"
elif [ -x "$HOME/.local/bin/cona" ]; then
    "$HOME/.local/bin/cona" uninstall "$@"
elif [ -x "$(dirname "$0")/target/release/cona" ]; then
    "$(dirname "$0")/target/release/cona" uninstall "$@"
else
    echo "no cona binary found — manual cleanup:"
    rm -fv "$HOME/.local/bin/cona"
    rm -rfv "$HOME/.claude/skills/cona" "$HOME/.claude/CONA.md" \
        "$HOME/.cursor/rules/cona.mdc" 2>/dev/null || true
    # scan ALL args (not just $1), and stay safe under `set -eu`
    case " $* " in
        *" --purge "*) rm -rfv "$HOME/.cona" ;;
    esac
    echo "left for manual review (marker blocks / hook entries):"
    echo "  ~/.claude/CLAUDE.md, ~/.claude/settings.json, ~/.codex/AGENTS.md,"
    echo "  ~/.gemini/GEMINI.md, and .git/hooks in your project checkouts"
fi
