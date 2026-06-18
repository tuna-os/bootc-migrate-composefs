#!/bin/bash
# E2E test watcher — tails the log, exits on error words or idle timeout.
# Usage: ./watcher.sh <logfile> [poll_secs=30] [idle_limit_secs=120] [error_words] [whitelist_words]

LOG="$1"
POLL="${2:-30}"
IDLE_LIMIT="${3:-120}"
ERROR_WORDS="${4:-ERROR|FAIL:|FAILED|Timeout|timed out|could not|No such|Permission denied|not found|exit code|exited with}"
WHITELIST="${5:-WARN:|Warning:|warn:|WARNING}"

if [ -z "$LOG" ]; then
    echo "Usage: $0 <logfile> [poll_secs=30] [idle_limit_secs=120] [error_words] [whitelist_words]"
    echo ""
    echo "Watches log, exits 1 on any ERROR word (not in whitelist) or idle timeout."
    echo "Exits 0 on completion (PASSED/SUCCESS message found)."
    exit 1
fi

echo "Watching $LOG (poll=${POLL}s, idle_limit=${IDLE_LIMIT}s)"
echo "Error patterns: $ERROR_WORDS"
echo "Whitelist:      $WHITELIST"
echo "---"

LAST_SIZE=0
IDLE_SECONDS=0

while true; do
    if [ -f "$LOG" ]; then
        CURRENT_SIZE=$(stat -c%s "$LOG" 2>/dev/null || echo 0)
        
        if [ "$CURRENT_SIZE" != "$LAST_SIZE" ]; then
            # Print new lines since last check
            if [ "$LAST_SIZE" -gt 0 ] 2>/dev/null; then
                tail -c +$((LAST_SIZE + 1)) "$LOG" 2>/dev/null
            else
                cat "$LOG" 2>/dev/null
            fi
            LAST_SIZE=$CURRENT_SIZE
            IDLE_SECONDS=0

            # Check for un-whitelisted error words in the new content
            # Take last 8KB of log, remove whitelisted lines, then check for error patterns
            NEW_TAIL=$(tail -c 8192 "$LOG" 2>/dev/null)
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
    fi
    
    # Check if the E2E script process is still running
    if ! pgrep -f 'run-e2e.sh' > /dev/null 2>&1; then
        echo ""
        echo "=== E2E SCRIPT EXITED ==="
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
        echo "Last lines:"
        tail -5 "$LOG"
        echo "Processes:"
        ps aux | grep -E 'run-e2e|qemu-system' | grep -v grep
        exit 1
    fi
    
    sleep "$POLL"
done
