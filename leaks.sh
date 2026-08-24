#!/bin/sh
# Refcount-balance check: retain/release crossing .so boundaries must net to
# zero. The C harness exercises the whole surface — snapshots retained and
# released across liba/libb, COW detaches, capture and stream callbacks,
# cancellation, dma-buf fds — so if the boundary refcounting is wrong, it
# leaks here and nowhere else a test would notice.
#
# Why a leak checker rather than an assertion on a counter: an internal
# counter can only report what the library believes it did. valgrind reports
# what actually happened to the allocator, including anything leaked by the
# OTHER side of the boundary.
#
#   ./leaks.sh          check the C harness
#   ./leaks.sh <prog>   check something else
set -e
cd "$(dirname "$0")"

PROG=${1:-target/debug/harness}
[ -x "$PROG" ] || { echo "leaks: $PROG not built — run ./run.sh first"; exit 1; }

case "$(uname)" in
Darwin)
    command -v leaks >/dev/null || { echo "leaks: 'leaks' not available, skipping"; exit 0; }
    MallocStackLogging=1 leaks --atExit -- "$PROG" >/dev/null
    echo "leaks: no leaks at exit"
    ;;
Linux)
    command -v valgrind >/dev/null || {
        echo "leaks: valgrind not installed, skipping"
        echo "  install with: sudo apt install valgrind"
        exit 0
    }
    # Only definite and indirect losses are errors. "Still reachable" is
    # NOT a leak and must not be one here: the library installs its panic
    # hook exactly once through a std::sync::Once (see a_rt::err), which
    # boxes a closure that lives for the life of the process by design.
    # One bounded block, deliberately never freed — failing on it would
    # only teach the next person to delete this check.
    OUT=$(mktemp)
    trap 'rm -f "$OUT"' EXIT
    if valgrind --leak-check=full \
        --show-leak-kinds=definite,indirect \
        --errors-for-leak-kinds=definite,indirect \
        --error-exitcode=99 \
        "$PROG" >/dev/null 2>"$OUT"; then
        grep -E "definitely lost|indirectly lost|ERROR SUMMARY|total heap usage" "$OUT" | sed 's/^==[0-9]*== */  /'
        echo "leaks: refcounts across the .so boundary net to zero"
    else
        code=$?
        echo "FAIL leaks: valgrind reported errors (exit $code)"
        cat "$OUT"
        exit 1
    fi
    ;;
*)
    echo "leaks: no leak checker wired up for $(uname), skipping"
    ;;
esac
