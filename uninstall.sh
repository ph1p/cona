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
    rm -fv "$HOME/.local/bin/cona" "$HOME/.local/bin/cona-resolve-helper"
    # Files cona owns outright: safe to delete wholesale.
    # Honour XDG_CONFIG_HOME only when it really sits under the HOME we are
    # cleaning; an inherited value pointing elsewhere would make us list config
    # belonging to a different home entirely.
    XDG="$HOME/.config"
    case "${XDG_CONFIG_HOME:-}" in
        "$HOME"/*) XDG="$XDG_CONFIG_HOME" ;;
    esac
    rm -rfv "$HOME/.claude/skills/cona" "$HOME/.claude/CONA.md" \
        "$HOME/.cursor/rules/cona.mdc" \
        "$HOME/.windsurf/rules/cona.md" 2>/dev/null || true
    # scan ALL args (not just $1), and stay safe under `set -eu`
    case " $* " in
        *" --purge "*) rm -rfv "$HOME/.cona" ;;
    esac
    # Everything else is a marker block or one entry inside a file you also own,
    # so it is listed rather than deleted — removing the file would take your
    # config with it.
    echo "left for manual review (marker blocks / MCP + hook entries):"
    for f in \
        "$HOME/.claude/CLAUDE.md" "$HOME/.claude/settings.json" \
        "$HOME/.codex/AGENTS.md" "$HOME/.codex/config.toml" \
        "$HOME/.cursor/mcp.json" \
        "$HOME/.gemini/GEMINI.md" "$HOME/.gemini/settings.json" \
        "$HOME/.pi/agent/AGENTS.md" \
        "$XDG/opencode/AGENTS.md" "$XDG/opencode/opencode.json" \
        "$XDG/zed/AGENTS.md" "$XDG/zed/settings.json" \
        "$XDG/crush/CRUSH.md" "$XDG/crush/crush.json" \
        "$HOME/.codeium/windsurf/memories/global_rules.md" \
        "$HOME/.codeium/windsurf/mcp_config.json" \
        "$HOME/.qwen/QWEN.md" "$HOME/.qwen/settings.json" \
        "$HOME/.copilot/copilot-instructions.md" "$HOME/.copilot/mcp-config.json"
    do
        # `[ -e ] && echo` would exit non-zero when the LAST file is missing,
        # which `set -e` turns into a failed uninstall. Use a real if.
        if [ -e "$f" ]; then echo "  $f"; fi
    done
    echo "  …plus per-project AGENTS.md/CLAUDE.md/.mcp.json and .git/hooks in your checkouts"
fi
