#!/bin/sh
# Install or update agent CLIs into the /data volume (not into the image).
# Usage: agents install <cli>[@<version>]...
#        agents update [<cli>...]
#        agents versions
set -eu

DATA_DIR="${DATA_DIR:-/data}"
TOOLS_PREFIX="${DATA_DIR}/tools"
LOCAL_BIN="${DATA_DIR}/.local/bin"
LOCK="${DATA_DIR}/agents.lock"
PATH="${TOOLS_PREFIX}/bin:${LOCAL_BIN}:${PATH}"
export PATH HOME="${DATA_DIR}"

mkdir -p "${TOOLS_PREFIX}/bin" "${LOCAL_BIN}"

lock_exclusive() {
    # Wait until summarize releases its shared lock, then take exclusive.
    exec 9>"${LOCK}"
    flock -x 9
}

npm_pkg() {
    case "$1" in
        claude) echo "@anthropic-ai/claude-code" ;;
        codex) echo "@openai/codex" ;;
        opencode) echo "@opencode/cli" ;;
        *) return 1 ;;
    esac
}

install_npm() {
    name="$1"
    ver="${2:-latest}"
    pkg="$(npm_pkg "$name")"
    echo "installing ${name} (${pkg}@${ver}) into ${TOOLS_PREFIX}"
    npm i -g --prefix "${TOOLS_PREFIX}" "${pkg}@${ver}"
}

install_cursor() {
    echo "installing cursor-agent into ${LOCAL_BIN} (installer; version pin not supported)"
    tmp="$(mktemp)"
    # Download first: without pipefail, a failed curl would still leave bash with
    # empty stdin and exit 0.
    curl -fsSo "$tmp" https://cursor.com/install
    bash "$tmp"
    rm -f "$tmp"
}

install_agy() {
    # Installer URL can change; check agy docs if this fails.
    echo "installing agy into ${LOCAL_BIN}"
    if command -v agy >/dev/null 2>&1; then
        echo "agy already on PATH: $(command -v agy)"
        return 0
    fi
    echo "agy: no installer wired yet; install the binary into ${LOCAL_BIN} by hand" >&2
    return 1
}

install_one() {
    spec="$1"
    name="${spec%%@*}"
    ver=
    case "$spec" in
        *@*) ver="${spec#*@}" ;;
    esac
    case "$name" in
        claude|codex|opencode)
            if [ -n "$ver" ]; then install_npm "$name" "$ver"; else install_npm "$name"; fi
            ;;
        cursor) install_cursor ;;
        agy) install_agy ;;
        *)
            echo "unknown CLI: ${name} (claude|codex|opencode|cursor|agy)" >&2
            return 1
            ;;
    esac
}

list_installed() {
    for name in claude codex opencode cursor-agent agy; do
        if command -v "$name" >/dev/null 2>&1; then
            echo "$name"
        fi
    done
}

cmd_versions() {
    for name in claude codex opencode cursor-agent agy; do
        if command -v "$name" >/dev/null 2>&1; then
            ver="$("$name" --version 2>/dev/null | head -n 1 || true)"
            if [ -z "$ver" ]; then ver="version unknown"; fi
            echo "${name}: ${ver}"
        else
            echo "${name}: not installed"
        fi
    done
}

cmd_install() {
    if [ "$#" -eq 0 ]; then
        echo "usage: agents install <cli>[@version]..." >&2
        exit 2
    fi
    lock_exclusive
    for spec in "$@"; do
        install_one "$spec"
    done
}

cmd_update() {
    lock_exclusive
    if [ "$#" -eq 0 ]; then
        set -- $(list_installed)
        if [ "$#" -eq 0 ]; then
            echo "no CLIs installed yet; try: agents install claude"
            exit 0
        fi
    fi
    for name in "$@"; do
        # Map binary name cursor-agent back to the install key.
        case "$name" in
            cursor-agent) name=cursor ;;
        esac
        install_one "$name"
    done
}

case "${1:-}" in
    install) shift; cmd_install "$@" ;;
    update) shift; cmd_update "$@" ;;
    versions) cmd_versions ;;
    "")
        echo "usage: agents install|update|versions" >&2
        exit 2
        ;;
    *)
        echo "unknown agents command: $1" >&2
        exit 2
        ;;
esac
