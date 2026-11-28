#!/usr/bin/env bash
# Copyright (c) 2026 Henrik Widlund
# SPDX-License-Identifier: MPL-2.0
#
# Guided manual test with a real Remote and receiver.
#
# This script covers only what needs a real Remote: the driver is found with mDNS, the setup
# pages, the entities on the Remote, the pages and buttons of the remote entity (also a held
# button), adding and removing a second receiver, and the sleep and wake of the Remote. The
# program tools/remote-sim tests everything else, also the checks that ask the owner a question.
# Run it first.
#
# The script shows one numbered step at a time: what to prepare, what to do and what to watch
# for. Enter runs the step command (if the step has one). You then type what happened. All
# answers are saved to a results file.
#
# The driver runs in one of two ways:
# - script: the script starts the driver with sudo (mDNS needs it) and writes the log to a file.
# - IDE: the driver runs in RustRover (or another IDE). The script asks for the log file.
#
# Usage: tools/manual_test.sh [steps]
#   No steps: run all steps.
#   Steps: numbers and ranges, for example: 2 4-6 8
#   A number with + runs that step and all steps after it, for example: 5+

set -u

# Bash reads a script while it runs, so a change to the file breaks a running test.
# The braces make bash read the whole script first.
{

cd "$(dirname "$0")/.." || exit 1

mkdir -p test-results || exit 1
RESULTS="$PWD/test-results/manual-test-results-$(date +%Y%m%d-%H%M%S).md"
BIN="$PWD/target/release/uc-intg-denonmarantz"
DRIVER_PID=""
SELECTION="$*"
STEP=0
# Debug lines of the driver and the library.
DRIVER_LOG_LEVEL="info,uc_intg_denonmarantz=debug,denon_marantz=debug"
# Texts that many steps use.
RECEIVER_ON="The receiver is on."
LOG_PROBLEMS="recent_log 'WARN|ERROR'"

echo "Where does the driver run?"
echo "  1 + Enter = this script builds and starts it"
echo "  2 + Enter = RustRover or another IDE"
read -r -p "Choice: " mode
if [[ "$mode" = "2" ]]; then
    IDE=1
    echo
    echo "In the RustRover run configuration:"
    echo "  - Environment variables: RUST_LOG=$DRIVER_LOG_LEVEL"
    echo "  - Logs tab: turn on 'Save console output to file' and choose a file."
    echo "  - The working directory is UC_CONFIG_HOME if it is not set."
    read -r -p "Config folder of the driver (UC_CONFIG_HOME), Enter = $PWD: " CONFIG_DIR
    CONFIG_DIR="${CONFIG_DIR:-$PWD}"
    read -r -p "Log file from 'Save console output to file', Enter = no file: " LOG
else
    IDE=0
    # Same default as the driver in RustRover, so both find the same configuration.
    read -r -p "Config folder of the driver (UC_CONFIG_HOME), Enter = ${UC_CONFIG_HOME:-$PWD}: " CONFIG_DIR
    CONFIG_DIR="${CONFIG_DIR:-${UC_CONFIG_HOME:-$PWD}}"
    LOG="$CONFIG_DIR/driver.log"
    mkdir -p "$CONFIG_DIR"
    touch "$LOG"
fi
# recent_log shows only lines written after this point.
LOG_START=0
if [[ -n "$LOG" ]] && [[ -f "$LOG" ]]; then
    LOG_START=$(wc -c <"$LOG" | tr -d ' ')
fi

# True if step $1 is in the selection.
selected() {
    local n="$1" s
    [[ -z "$SELECTION" ]] && return 0
    for s in $SELECTION; do
        case "$s" in
            *+) [[ "$n" -ge "${s%+}" ]] && return 0 ;;
            *-*) [[ "$n" -ge "${s%-*}" ]] && [[ "$n" -le "${s#*-}" ]] && return 0 ;;
            *) [[ "$n" -eq "$s" ]] && return 0 ;;
        esac
    done
    return 1
}

# The driver needs root for mDNS. Files that it wrote as root are given back to the user.
give_files_back() {
    sudo chown "$(id -u):$(id -g)" "$CONFIG_DIR"/config.json* "$CONFIG_DIR/settings.json" "$LOG" 2>/dev/null
    return 0
}

stop_driver() {
    if [[ -n "$DRIVER_PID" ]] && sudo kill -0 "$DRIVER_PID" 2>/dev/null; then
        sudo kill "$DRIVER_PID"
        wait "$DRIVER_PID" 2>/dev/null
        echo "Driver stopped."
    fi
    DRIVER_PID=""
    give_files_back
}

start_driver() {
    if [[ "$IDE" = 1 ]]; then
        read -r -p "Restart the driver in RustRover now, then press Enter: " _
        return
    fi
    stop_driver
    echo "The driver starts with sudo (mDNS needs it). Enter your password if asked."
    sudo -v || return
    # No terminal for sudo (stdin, stdout and stderr are not a terminal): sudo then does not take
    # over the terminal of the script.
    sudo -n env UC_CONFIG_HOME="$CONFIG_DIR" RUST_LOG="${RUST_LOG:-$DRIVER_LOG_LEVEL}" "$BIN" \
        </dev/null >>"$LOG" 2>&1 &
    DRIVER_PID=$!
    sleep 1
    if sudo kill -0 "$DRIVER_PID" 2>/dev/null; then
        echo "Driver started (pid $DRIVER_PID). Log: $LOG"
        grep "Listening on" "$LOG" | tail -n 1
    else
        echo "Driver did not start. Last log lines:"
        tail -n 20 "$LOG"
        DRIVER_PID=""
    fi
}


recent_log() {
    # Log lines of the driver from this run, without the HTTP server lines.
    if [[ -z "$LOG" ]] || [[ ! -f "$LOG" ]]; then
        echo "No log file. Look in the RustRover console for lines with: ${1:-any text}"
        return
    fi
    tail -c "+$((LOG_START + 1))" "$LOG" | grep -v "actix_server" | grep -E "${1:-.}" | tail -n "${2:-15}"
}

trap stop_driver EXIT

save() {
    printf '%s\n' "$@" >>"$RESULTS"
}

# step TITLE PREPARE DO WATCH [COMMAND]
step() {
    STEP=$((STEP + 1))
    local title="$1" prepare="$2" todo="$3" watch="$4" cmd="${5:-}"
    if ! selected "$STEP"; then
        return
    fi
    while true; do
        echo
        echo "================================================================"
        echo "Step $STEP: $title"
        echo "================================================================"
        echo "Prepare:"
        echo "$prepare" | sed 's/^/  /'
        echo "Do:"
        echo "$todo" | sed 's/^/  /'
        echo "Watch for:"
        echo "$watch" | sed 's/^/  /'
        if [[ -n "$cmd" ]]; then
            echo "Command:"
            echo "  $cmd"
        fi
        echo
        if [[ -n "$cmd" ]]; then
            read -r -p "Enter = run the command, s + Enter = skip this step, q + Enter = quit: " key
        else
            read -r -p "Enter = start this step, s + Enter = skip this step, q + Enter = quit: " key
        fi
        case "$key" in
            q | Q)
                save "" "Stopped at step $STEP."
                echo "Results: $RESULTS"
                exit 0
                ;;
            s | S)
                save "" "## $STEP. $title" "" "- Result: SKIPPED"
                return
                ;;
            *)
                # Enter or any other key: run the step.
                ;;
        esac
        if [[ -n "$cmd" ]]; then
            echo "----- command output -----"
            eval "$cmd"
            echo "--------------------------"
        fi
        echo
        read -r -p "Result: y + Enter = as expected, n + Enter = not as expected, r + Enter = run the step again: " answer
        case "$answer" in
            r | R) continue ;;
            y | Y)
                read -r -p "Note (optional), then Enter: " note
                save "" "## $STEP. $title" "" "- Result: OK"
                ;;
            *)
                read -r -p "What happened? Type it, then Enter: " note
                save "" "## $STEP. $title" "" "- Result: NOT OK"
                ;;
        esac
        if [[ -n "${note:-}" ]]; then
            save "- Note: $note"
        fi
        note=""
        return
    done
}

echo "Manual test of the Denon and Marantz integration."
echo "Results are saved to: $RESULTS"
echo "Driver configuration: $CONFIG_DIR"
echo "Driver log: ${LOG:-RustRover console}"
echo
echo "Rules for this test:"
echo "  - Steps that can make sound or change the volume say so. Press s + Enter to skip a step you do not want."
echo
# The answers are kept for the next run. Enter keeps the last answer.
SETUP_FILE="$PWD/.manual-test-setup"
avr="" remote="" address=""
if [[ -f "$SETUP_FILE" ]]; then
    { read -r avr; read -r remote; read -r address; } <"$SETUP_FILE"
fi
read -r -p "Receiver model, Enter = '${avr:-unknown}': " answer
avr="${answer:-${avr:-unknown}}"
read -r -p "Remote model and firmware version, Enter = '${remote:-unknown}': " answer
remote="${answer:-${remote:-unknown}}"
read -r -p "Receiver IP address, Enter = '${address:-unknown}': " answer
address="${answer:-${address:-unknown}}"
printf '%s\n' "$avr" "$remote" "$address" >"$SETUP_FILE"

save "# Manual test $(date '+%Y-%m-%d %H:%M')" "" \
    "- Driver: $(git describe --always --dirty 2>/dev/null || echo unknown)" \
    "- Library: $(awk '/^name = "denon-marantz"$/ { f = 1 } f && /^source/ { sub(/.*rev=/, ""); sub(/#.*/, ""); print; exit }' Cargo.lock 2>/dev/null || echo unknown)" \
    "- Receiver: $avr" \
    "- Remote: $remote"

# ---------------------------------------------------------------- driver and setup

# Step 1 starts the driver. When step 1 is not selected, start it here.
if [[ "$IDE" = 0 ]] && ! selected 1; then
    echo
    echo "Building and starting the driver..."
    cargo build --release && start_driver
fi

if [[ "$IDE" = 1 ]]; then
    step "Start the driver" \
"The Mac and the Remote are on the same network as the receiver.
The receiver is removed from the Python integration on the Remote. The Python integration cannot
be stopped, but only one integration should be connected to the receiver." \
"Start the driver in RustRover with the run configuration from above." \
"The log shows 'Listening on: 0.0.0.0:9090' and 'configured receivers'." \
"recent_log 'Listening on|configured|mDNS|WARN|ERROR'"
else
    step "Build and start the driver" \
"The Mac and the Remote are on the same network as the receiver.
The receiver is removed from the Python integration on the Remote. The Python integration cannot
be stopped, but only one integration should be connected to the receiver." \
"Press Enter. The script builds the driver and starts it in the background." \
"The build ends without errors.
The output shows 'Driver started' and 'Listening on: 0.0.0.0:9090'." \
"cargo build --release && start_driver"
fi

step "Find the driver on the Remote" \
"Open the web configurator of the Remote." \
"Integrations > Add new > look for 'Denon and Marantz AVR Network Receivers (Rust)'." \
"The driver is listed with the id denonavr_rs. If it is not listed, the mDNS publish failed: check the log." \
"recent_log 'mDNS|WARN|ERROR'"

step "Setup: discovery and options" \
"The receiver is on or in network standby." \
"Start the setup. On the page 'Restore': leave 'Restore from backup' unticked.
On the page 'Setup mode': leave the address empty. Press Next.
Pick the receiver. Keep the defaults. Press Next.
Then start the setup again: 'Configuration mode' > 'Configure selected device' > Next. Press Next on the
next page without a change.
Then once more 'Configure selected device': write the host name of the receiver in the address field (for
example the name that the router shows). Press Next. Open 'Configure selected device' again, then cancel." \
"The page 'Setup mode' says that the address is optional and that the search finds receivers only on the
same network as the Remote. A page 'Please choose your Denon or Marantz AVR' has the receiver and these
fields: show all sources, manufacturer, connection mode, Dirac, AM/FM tuner, signal check time (5 s; not
on receivers from before about 2016) and a note.
'Device supports Dirac' shows the real value of the receiver.
The setup ends with success in about a second.
The page 'Configure your AVR' shows the address and the same fields with the saved values.
With the host name the setup also ends with success, and the address field then shows the host name." \
"recent_log 'discovering|adding|not found|host name|WARN|ERROR'"

step "Setup: entities" \
"Setup is done." \
"Add all entities of the receiver. Open the media player and the remote." \
"All entities are available. The media player shows the source, the sound mode and the volume.
With telnet the AVC-A10H (Dirac, no tuner, no picture mode) has 13 selects, 30 sensors, 6 switches and 2
numbers, besides the media player and the remote. A tuner adds 2 selects and 1 sensor. Without Dirac there
is 1 select and 1 sensor less." \
"recent_log 'connected|WARN|ERROR'"

# ---------------------------------------------------------------- remote entity and setup

step "Simple commands and pages" \
"$RECEIVER_ON
Select a source that plays nothing, or pause the playback: some page buttons (for example Pure
Direct, MultiEQ Off or ECO Off) can make the sound louder.
Buttons on these pages can make sound and change the volume. Press volume up only while the
receiver shows -45 dB or lower. Do not press the 'Up' buttons on the page 'Channel Levels'. Do not
press buttons on the pages 'Quick Select', 'Smart Select' and 'Zone Favorites': they can select a
source and a volume, or overwrite a saved favorite. Do not press buttons on the pages 'Inputs' and
'Tuner': they can change the source, and remote-sim checks the source changes. Do not press buttons
on the page 'Triggers': they can switch other devices. Buttons on the pages 'Main', 'Sound Modes',
'Channel Levels', 'Sound Tweaks', 'Standby', 'Dirac', 'Audyssey', 'ECO', 'Picture Mode' and 'Sleep
Timer' change settings: set them back by hand, and set the sleep timer back to Off. The repeated
command and the sequence must not use volume up or a source button." \
"Open the pages of the remote entity (up to 15 pages). Press a few buttons on each page that the text
above allows, also a repeated command and a sequence of two commands with a pause of 5 seconds (the
longest pause that the driver allows). Press volume down while the sequence runs. Hold a button that
does not change the volume or the source (for example cursor down) for about 2 seconds." \
"The buttons work. A held button sends one command, not four. A long sequence is answered at once and
the volume buttons still work while it runs." \
"$LOG_PROBLEMS"

step "Add and remove a second receiver" \
"At least one receiver is configured. A second receiver is on the network. Skip this step if you
do not have one." \
"Start the setup. On 'Configuration mode' choose the action 'Add a new device' and add the second
receiver. Add its entities. Then start the setup again, choose the second receiver in 'Configured
devices' and the action 'Delete selected device'." \
"The second receiver and its entities are added without a restart. After the delete, the first
receiver still works. Note what the Remote shows for the entities of the second receiver." \
"$LOG_PROBLEMS"

# ---------------------------------------------------------------- wake

step "Sleep and wake of the Remote" \
"$RECEIVER_ON" \
"Let the Remote go to sleep for a minute. Wake it and at once press a button that does not change
the volume or the source, for example cursor down." \
"The command works at once, or in less than 3 seconds.
The log shows 'connected in' and the time." \
"recent_log 'connected in|network not ready|WARN|ERROR'"

step "Check the log for errors again" \
"The steps above are done." \
"Press Enter to show warnings and errors from the driver log." \
"Each WARN or ERROR line is about a step above where something did not work." \
"recent_log 'WARN|ERROR' 40"

save "" "Done."
echo
echo "All steps done. Results: $RESULTS"
exit
}
