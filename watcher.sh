#!/bin/bash
# E2E test watcher — tails the log, exits on error words or idle timeout.
# Usage: ./watcher.sh <logfile> [poll_secs=30] [idle_limit_secs=120] [error_words] [whitelist_words]

LOG="$1"
POLL="${2:-30}"
IDLE_LIMIT="${3:-300}"
ERROR_WORDS="${4:-error|ERROR|FAIL:|FAILED|Timeout|timed out|could not|No such|Permission denied|not found|exit code|exited with|blocked by}"
WHITELIST="${5:-WARN:|Warning:|warn:|WARNING|Timeout: 0 seconds|Read-only file system}"

if [ -z "$LOG" ]; then
    echo "Usage: $0 <logfile> [poll_secs=30] [idle_limit_secs=120] [error_words] [whitelist_words]"
    exit 1
fi

echo "Watching $LOG (poll=${POLL}s, idle_limit=${IDLE_LIMIT}s)"
echo "Error patterns: $ERROR_WORDS"
echo "Whitelist:      $WHITELIST"
echo "---"

LAST_SIZE=0
IDLE_SECONDS=0
STARTED=false

# Wait for log to appear (E2E may be building first)
for i in $(seq 1 10); do
    if [ -f "$LOG" ] && [ "$(stat -c%s "$LOG" 2>/dev/null || echo 0)" -gt 0 ]; then
        STARTED=true
        # Store first good size
        LAST_SIZE=$(stat -c%s "$LOG" 2>/dev/null || echo 0)
        break
    fi
    sleep 3
done
if ! $STARTED; then
    echo "ERROR: log $LOG never appeared" >&2
    exit 1
fi

while true; do
    CURRENT_SIZE=$(stat -c%s "$LOG" 2>/dev/null || echo 0)

    if [ "$CURRENT_SIZE" != "$LAST_SIZE" ]; then
        if [ "$CURRENT_SIZE" -lt "$LAST_SIZE" ]; then
            # File was truncated (log recreated) — print everything
            tr -d '\0' < "$LOG" 2>/dev/null
        elif [ "$LAST_SIZE" -gt 0 ] 2>/dev/null; then
            # Print only the new bytes
            tail -c +$((LAST_SIZE + 1)) "$LOG" 2>/dev/null | tr -d '\0'
        else
            tr -d '\0' < "$LOG" 2>/dev/null
        fi
        LAST_SIZE=$CURRENT_SIZE
        IDLE_SECONDS=0

        # Check for un-whitelisted error words in tail of log
        NEW_TAIL=$(tail -c 8192 "$LOG" 2>/dev/null | tr -d '\0')
        CLEAN=$(echo "$NEW_TAIL" | grep -vE "$WHITELIST" 2>/dev/null || true)
        if echo "$CLEAN" | grep -qE "$ERROR_WORDS" 2>/dev/null; then
            echo ""
            echo "=== ERROR DETECTED ==="
            echo "$CLEAN" | grep -E "$ERROR_WORDS" | head -5
            exit 1
        fi
    else
        IDLE_SECONDS=$((IDLE_SECONDS + POLL))
    fi

    # Check if a relevant process is still running
    PROC_RUNNING=false
    for pat in 'run-e2e.sh' 'qemu-system' 'just.*e2e'; do
        if pgrep -f "$pat" > /dev/null 2>&1; then
            PROC_RUNNING=true; break
        fi
    done

    if ! $PROC_RUNNING; then
        echo ""
        echo "=== E2E PROCESSES EXIT ==="
        tail -10 "$LOG"
        if grep -qE 'PASSED|TEST.*SUCCESS|passed successfully' "$LOG" 2>/dev/null; then
            echo "=== TEST PASSED ==="
            exit 0
        fi
        exit 1
    fi

    if [ "$IDLE_SECONDS" -ge "$IDLE_LIMIT" ]; then
        echo ""
        echo "=== IDLE TIMEOUT (${IDLE_LIMIT}s without output) ==="
        tail -5 "$LOG"
        ps aux | grep -E 'run-e2e|qemu-system|just.*e2e' | grep -v grep
        exit 1
    fi

    sleep "$POLL"
done
