#!/usr/bin/env bash
# Copyright (c) 2026 Henrik Widlund
# SPDX-License-Identifier: MPL-2.0
#
# Guided manual test with a real Remote and receiver.
#
# The program tools/remote-sim tests most of the driver without a Remote. This script covers what
# only the Remote shows: the setup pages, the entity lists and the buttons.
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
#   Steps: numbers and ranges, for example: 12 14-16 20
#   A number with + runs that step and all steps after it, for example: 30+

set -u

# Bash reads a script while it runs, so a change to the file breaks a running test.
# The braces make bash read the whole script first.
{

cd "$(dirname "$0")/.." || exit 1

RESULTS="$PWD/manual-test-results-$(date +%Y%m%d-%H%M%S).md"
BIN="$PWD/target/release/uc-intg-denonmarantz"
DRIVER_PID=""
SELECTION="$*"
STEP=0
# Debug lines of the driver and the library.
DRIVER_LOG_LEVEL="info,uc_intg_denonmarantz=debug,denon_marantz=debug"

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
    sudo chown "$(id -u):$(id -g)" "$CONFIG_DIR"/config.json* "$LOG" 2>/dev/null
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

break_config() {
    # Make config.json invalid, start the driver, then put the file back.
    if [[ "$IDE" = 1 ]]; then
        read -r -p "Stop the driver in RustRover now, then press Enter: " _
    else
        stop_driver
    fi
    give_files_back
    cp "$CONFIG_DIR/config.json" "$CONFIG_DIR/config.json.before-broken-test"
    printf '[{"broken": ' >"$CONFIG_DIR/config.json"
    start_driver
    sleep 3
    recent_log 'Invalid|moved|WARN|ERROR'
    ls -l "$CONFIG_DIR"/config.json*
    if [[ "$IDE" = 1 ]]; then
        read -r -p "Stop the driver in RustRover now, then press Enter: " _
    else
        stop_driver
    fi
    mv "$CONFIG_DIR/config.json.before-broken-test" "$CONFIG_DIR/config.json"
    rm -f "$CONFIG_DIR"/config.json.bad-*
    echo "config.json is back. The moved file was deleted."
    start_driver
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
echo "  - Steps that can make sound or change the volume say so. Say no if you do not want it."
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
    "- Library: $(git -C ../rusty-denon describe --always --dirty 2>/dev/null || echo unknown)" \
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
Pick the receiver. Keep the defaults. Press Next." \
"The page 'Setup mode' says that the address is optional and that the search finds receivers only on the
same network as the Remote. A page 'Please choose your Denon or Marantz AVR' has the receiver and these
fields: show all sources, manufacturer, connection mode, volume step, Dirac, signal read time.
'Device supports Dirac' shows the real value of the receiver.
The setup ends with success in about a second." \
"recent_log 'discovering|adding|not found|WARN|ERROR'"

step "Setup: entities" \
"Setup is done." \
"Add all entities of the receiver. Open the media player and the remote." \
"All entities are available. The media player shows the source, the sound mode and the volume.
With telnet there are 15 selects (14 without Dirac), 30 sensors (29 without Dirac), 6 switches and
2 numbers, besides the media player and the remote." \
"recent_log 'connected|WARN|ERROR'"

step "Configuration file" \
"Setup is done." \
"Press Enter to show the configuration file." \
"The file has the fields of the Python integration (id = serial number, address, use_telnet, ...).
The mode is -rw------- and there is no config.json.tmp file left." \
"ls -l \"$CONFIG_DIR\"/config.json* && cat \"$CONFIG_DIR/config.json\""

step "Take over a configuration of the Python integration" \
"Copy config.json of the Python integration into the config folder of this driver. Stop the driver first." \
"Start the driver. Open the Remote and look at the entities of the receiver." \
"The receiver is set up without a new setup. It has the entity ids and names of the Python integration
(media_player.<serial>, sensor.<kind>.<serial>) and the new entities of this driver." \
"recent_log 'configured|connected|WARN|ERROR'"

# ---------------------------------------------------------------- control

step "Power off and on" \
"The receiver is on." \
"Media player: press Off. Wait 5 s. Press On." \
"Off: the receiver goes to standby, the state is Off, the media info and the source are empty.
Sensors show '--', the mute sensor shows 'off'. Selects show '--'.
On: the receiver wakes, the state is On, values come back." \
"recent_log 'WARN|ERROR'"

step "Input source and sound mode" \
"ASK FIRST: this can start sound. The receiver is on." \
"Select another input source in the media player and in the select entity.
Select a sound mode, for example Stereo and Dolby Surround." \
"The receiver switches. The media player, the select and the sensor show the new value.
The sound mode shows the name that the receiver reports for the mode." \
"recent_log 'WARN|ERROR'"

step "Selects and sensors" \
"The receiver is on." \
"Change dimmer and eco mode with the select entities. Change the HDMI monitor output only if
you have two TVs." \
"The receiver changes. The sensor of the same value follows within a second." \
"recent_log 'WARN|ERROR'"

step "Remote entity commands" \
"The receiver is on." \
"Send these with the remote entity: DIMMER_TOGGLE, OUTPUT_AUTO, INFO_MENU, and one UI page button
(for example Sound Modes)." \
"Each command changes the receiver, and the UI pages of the remote entity have 15 pages." \
"recent_log 'WARN|ERROR'"

step "Manual address" \
"The receiver is on." \
"Start the setup again and type the IP address of the receiver on the page 'Setup mode'. Then type a
text with a space. Then an address with no receiver." \
"The right address finds the receiver. The text with a space shows the page again with an error. The
address with no receiver ends the setup with a not found message of the Remote." \
"recent_log 'discovering|not found|WARN|ERROR'"

step "Tone, Audyssey and Dirac" \
"ASK FIRST: this changes the sound. The receiver is on." \
"Use the switches Tone Control and Dynamic EQ, and the numbers Bass and Treble. Use the selects Audyssey
MultEQ, Reference Level and Dynamic Volume. If the receiver has Dirac: use the select Dirac Filter and look at
the sensor Active Dirac Filter." \
"The receiver changes and the entities follow. With a Dirac filter selected, the tone control and the
Audyssey settings are refused with a message that says why. The Dirac select has the slots that have a
filter, and the saved filter names. A receiver without Audyssey or Dirac shows '--' or has no Dirac
entities." \
"recent_log 'WARN|ERROR'"

step "Quick select, triggers, tuner and Bluetooth" \
"ASK FIRST: a quick select can change the volume and the source. The triggers can switch other equipment." \
"Select a quick select. Switch a trigger on and off. Select the tuner band. Switch the Bluetooth
transmitter and select the Bluetooth output." \
"Each change shows on the receiver and in the entity. The tuner sensor shows the frequency with its unit." \
"recent_log 'WARN|ERROR'"

step "Media info and navigation" \
"Play something from a network source, for example HEOS or internet radio." \
"Look at the media player: title, artist, album and cover. Then use play and pause, next and previous,
the cursor keys, menu, back, info, channel up and down. Then select a source that has no media, and the
tuner." \
"Title, artist, album and cover show for network audio. A source without media shows its name as the title.
The tuner shows the frequency as the title and the band as the artist. The keys work on the receiver." \
"recent_log 'WARN|ERROR'"

step "Signal sensors and the signal read option" \
"A source with a video and audio signal is playing." \
"Look at the sensors for audio and video signal, channels, resolution, HDR, pixel depth, FRL and color space.
Change the source. Then set 'Read the signal info every N seconds' to 10 in the setup." \
"The sensors show the signal of the receiver. They follow a change of source. With the option on, they
follow also when the receiver does not report a change." \
"recent_log 'signal|WARN|ERROR'"

step "Simple commands and pages" \
"The receiver is on." \
"Open the pages of the remote entity (15 pages). Press a few buttons on each page, also a repeated
command and a sequence of two commands with a pause." \
"The buttons work. A held button sends one command, not four. A long sequence is answered at once and
the volume buttons still work while it runs." \
"recent_log 'WARN|ERROR'"

step "Configure, remove and options" \
"At least one receiver is configured." \
"Setup > Configure the receiver: change the volume step, 'Show all sources' and the debug log option.
Then add and remove a receiver if you have a second one." \
"The changes work without a restart. A new volume step is used at once. With 'Show all sources' the
input source select has more sources. The debug log option writes more lines and stays on after a restart." \
"recent_log 'WARN|ERROR'"

step "Settings that the receiver does not accept" \
"The receiver is in standby, or a Dirac filter is selected." \
"Switch the tone control on. In standby, change a switch, a number and a select." \
"With a Dirac filter the tone control is refused with a clear message. In standby the commands are
accepted and do nothing. The log shows no WARN line for the refused command." \
"recent_log 'WARN|ERROR'"

step "Volume (ask first)" \
"ASK FIRST: this changes the main volume. Set the volume low." \
"Press volume up and down on the Remote. Change the volume step in the setup and try again." \
"One press changes the volume by the step. The volume sensors follow." \
"recent_log 'WARN|ERROR'"

# ---------------------------------------------------------------- wake and loss

step "Sleep and wake of the Remote" \
"The receiver is on." \
"Let the Remote go to sleep for a minute. Wake it and press a button at once." \
"The command works at once, or in less than 3 seconds.
The log shows 'connected in' and the time." \
"recent_log 'connected in|network not ready|WARN|ERROR'"

step "Receiver not reachable" \
"The receiver is on." \
"Unplug the network cable or the power of the receiver. Press volume up on the Remote at once, and
again after 30 seconds." \
"The entities become unavailable. A command fails at once (not after many seconds).
After the receiver is back, the entities become available again without a restart." \
"recent_log 'connect failed|not found|connected|WARN|ERROR'"

step "HTTP connection mode" \
"ASK FIRST: this changes dimmer and the input source. The receiver is on." \
"Configure the receiver with 'Use HTTP connection'. Check the entities. Change the dimmer and select
another input source. Select a sound mode group (for example STEREO). Switch back to telnet." \
"With HTTP there are 7 sensors and 6 selects, and no switches or numbers. Commands work. The new value shows within about 3 seconds
(source and sound mode can take 2 s). The sound mode select lists groups such as MOVIE and STEREO.
Other changes show within 10 seconds." \
"recent_log 'WARN|ERROR'"

step "Backup and restore" \
"At least one receiver is configured." \
"Setup > Backup configuration: copy the text. Then Setup > Reset. Then Restore from backup with the text." \
"The receiver is back with the same entity ids." \
"recent_log 'WARN|ERROR'"

step "Broken configuration file" \
"The driver runs." \
"Press Enter. The script stops the driver, makes config.json invalid and starts the driver.
It then shows the log, puts the old config.json back and starts the driver again." \
"The log shows 'Invalid ... The file was moved to ...config.json.bad-<time>'.
The driver starts with no devices. After the restore, the receiver connects again." \
"break_config"

step "Check the log for errors again" \
"The steps above are done." \
"Press Enter to show warnings and errors from the driver log." \
"Each WARN or ERROR line is about a step above where something did not work.
The broken configuration step gives one expected ERROR line." \
"recent_log 'WARN|ERROR' 40"

save "" "Done."
echo
echo "All steps done. Results: $RESULTS"
exit
}
