#!/bin/sh
# Resolve the versioned runtime without changing the caller's working directory.
set -eu
workflow_skill_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
workflow_expected="workflow $(cat "$workflow_skill_root/VERSION")"
workflow_compatible() {
    [ -x "$1" ] && [ "$("$1" --version 2>/dev/null)" = "$workflow_expected" ]
}
if [ -n "${WORKFLOW_BIN:-}" ]; then
    workflow_selected=$WORKFLOW_BIN
    if ! workflow_compatible "$workflow_selected"; then
        echo "WORKFLOW_BIN must point to an executable reporting $workflow_expected" >&2
        exit 2
    fi
else
    workflow_selected=
    case "$(uname -s):$(uname -m)" in
        Linux:x86_64|Linux:amd64)
            workflow_asset="$workflow_skill_root/assets/linux-x86_64/workflow"
            if workflow_compatible "$workflow_asset"; then
                workflow_selected=$workflow_asset
            fi
            ;;
    esac
    if [ -z "$workflow_selected" ]; then
        workflow_path=$(command -v workflow || true)
        if [ -n "$workflow_path" ] && workflow_compatible "$workflow_path"; then
            workflow_selected=$workflow_path
        else
            echo "No compatible $workflow_expected runtime; install the matching release for this platform." >&2
            exit 2
        fi
    fi
fi
exec "$workflow_selected" "$@"
