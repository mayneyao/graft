#!/usr/bin/env bash
set -uo pipefail

GIT_ROOT="$(git rev-parse --show-toplevel)"
EXAMPLES_LIB_PATH="${GIT_ROOT}/target/debug/examples"
LIB_PATH="${GIT_ROOT}/target/debug"

# invert diff colors so that expected lines are green
DIFF_PALETTE="ad=1;38;5;9:de=1;38;5;154"

FILTER="${1:-}"

# set the PWD to the git root directory - sql tests expect this
cd "${GIT_ROOT}"

# make sure sqlite can find the vfs
export LD_LIBRARY_PATH=${LIB_PATH}:${EXAMPLES_LIB_PATH}${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}
export DYLD_LIBRARY_PATH=${LIB_PATH}${DYLD_LIBRARY_PATH:+:$DYLD_LIBRARY_PATH}

cargo build
if [ $? -ne 0 ]; then
    echo "Cargo build failed"
    exit 1
fi

cargo build --examples --features dynamic
if [ $? -ne 0 ]; then
    echo "Cargo build failed"
    exit 1
fi

# select tests from *.sql files in GIT_ROOT, filtered by the first argument (substring)
declare TESTS=$(find "${GIT_ROOT}" -name "test_*.sql" | grep "${FILTER}")

if [ -z "${TESTS}" ]; then
    echo "No tests found using filter: ${FILTER}"
    echo "usage: $0 <filter>"
    echo "available tests:"
    find "${GIT_ROOT}" -name "test_*.sql"
    exit 1
fi

ANY_FAILED=0

for TEST in ${TESTS}; do
    echo "Running test: ${TEST}"
    EXPECTED="${TEST}.expected"

    export GRAFT_DIR="$(mktemp -d)"
    export GRAFT_AUTOSYNC=0;

    # We add the exit code to the output since SQLite returns the number of
    # errors encountered while executing, and some of the tests want to trigger
    # errors on purpose (thus we can't just fail on non-zero exit code).
    OUTPUT=$(sqlite3 -cmd '.log stderr' -header -table -echo 2>&1 <"${TEST}")
    EXIT_CODE=$?
    OUTPUT=$(printf "%s\n\n%s\n" "${OUTPUT}" "SQLite Exit Code = ${EXIT_CODE}")

    if [ -n "${GRAFT_DIR}" ] && [ -z "${SKIP_CLEANUP:-}" ]; then
        rm -rf "${GRAFT_DIR}"
    fi

    # if UPDATE_EXPECTED is set in the env, then write out expected files
    if [ -n "${UPDATE_EXPECTED:-}" ]; then
        echo "Updating expected output for: ${TEST}"
        echo "${OUTPUT}" >"${EXPECTED}"
    elif [ -f "${EXPECTED}" ]; then
        DIFF=$(echo "${OUTPUT}" | diff --color=always --palette="${DIFF_PALETTE}" -u --label "Expected Output" "${EXPECTED}" --label "Actual output" -)
        if [ -n "${DIFF}" ]; then
            echo "TEST FAILURE: Diff between actual and expected output for: ${TEST}"
            echo "${DIFF}"
            echo "Test failed: ${TEST}"
            echo "Expected file: ${EXPECTED}"
            echo "You can update the expected file by running:"
            echo "UPDATE_EXPECTED=1 $0 ${FILTER}"
            ANY_FAILED=1
        fi
    else
        echo "${OUTPUT}"

        # no expected file, fail
        echo "No expected output found for: ${TEST}"
        echo "You can create one by setting the UPDATE_EXPECTED environment variable and re-running the tests"
        exit 1
    fi
done

exit $ANY_FAILED
