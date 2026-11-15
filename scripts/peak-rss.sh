#!/bin/sh
# Report the peak resident set (VmHWM) of the first process whose argv
# starts with NAME and contains WORD, sampling once a second until it exits.
# Usage: scripts/peak-rss.sh osmium sort
name="$1"
word="$2"
pid=""
while [ -z "$pid" ]; do
    for d in /proc/[0-9]*; do
        if [ -r "$d/cmdline" ]; then
            cmd=$(tr '\0' ' ' < "$d/cmdline" 2>/dev/null)
            case "$cmd" in
                "$name $word "* | */"$name $word "*) pid="${d#/proc/}"; break ;;
            esac
        fi
    done
    [ -z "$pid" ] && sleep 1
done
echo "watching pid $pid: $cmd"
peak=""
while [ -r "/proc/$pid/status" ]; do
    hwm=$(grep VmHWM "/proc/$pid/status" 2>/dev/null)
    [ -n "$hwm" ] && peak="$hwm"
    sleep 1
done
echo "last $peak"
