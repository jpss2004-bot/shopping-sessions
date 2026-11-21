#!/bin/sh
# Entry point. Runs with no arguments; exit 0 means "done, check my output".
set -eu
: "${DATA_DIR:=/data}" "${OUTPUT_DIR:=/output}" "${SCRATCH_DIR:=/scratch}"
export DATA_DIR OUTPUT_DIR SCRATCH_DIR
# Read-only root filesystem: everything that wants a temp or home dir gets /scratch.
export TMPDIR="$SCRATCH_DIR" HOME="$SCRATCH_DIR"
case "${ENGINE:-native}" in
    native)
        # Set LD_PRELOAD to an empty value to measure the system allocator.
        export LD_PRELOAD="${LD_PRELOAD-libmimalloc.so.2}"
        exec /app/shopping-sessions
        ;;
    duckdb) exec python3 /app/pipeline/pipeline.py ;;
    *) echo "unknown ENGINE: $ENGINE" >&2; exit 2 ;;
esac
