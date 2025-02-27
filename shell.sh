#!/usr/bin/env bash

GIT_ROOT="$(git rev-parse --show-toplevel)"

VID="GontkHa6QVLMYnkyk16wUP"

GDB=${GDB:-0}
export RUST_LOG=${RUST_LOG:-warn}

cargo build

# parse flags
while [[ $# -gt 0 ]]; do
    case $1 in
        -v*)
            VID="${1:2}"
            shift
            ;;
        --trace)
            RUST_LOG="trace"
            shift
            ;;
        --temp)
            export GRAFT_DIR="$(mktemp -d)"
            shift
            ;;
        -p*)
            export GRAFT_DIR="/tmp/graft-shell/${1:2}"
            shift
            ;;
        --gdb)
            GDB=1
            shift
            ;;
        *)
            echo "Unknown option: $1"
            echo "Usage: $0 [-vVID] [--trace] [--temp] [-pPROFILE] [--gdb]"
            exit 1
            ;;
    esac
done

ARGS=(
    -header
    -table
    -cmd '.log stderr'
    -cmd '.load libgraft'
    -cmd ".open 'file:${VID}?vfs=graft'"
)

if [ "${GDB}" == 1 ]; then
    GDB_ARGS=(
        --eval-command="set breakpoint pending on"
        --eval-command="break rust_panic"
        -ex run
        --args sqlite3
        "${ARGS[@]}"
    )
    exec rust-gdb "${GDB_ARGS[@]}"
else
    exec ${GIT_ROOT}/sqlite3.sh "${ARGS[@]}"
fi
